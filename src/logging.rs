//! Per-process UTF-8 diagnostics mirrored to console and a new working-directory file.
//!
//! Whole lines are serialised across the capture owner and server tasks. File writes
//! are flushed after each line, without claiming power-loss durability. No queue or
//! screenshot storage is introduced. File failure disables further file writes,
//! reports once to stderr and requests cooperative server shutdown.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};


/// One process-wide sink, installed before the listener or capture owner starts.
static SESSION: OnceLock<Session<File>> = OnceLock::new();


/// At most the unsuffixed name and 99 numbered collision alternatives are tried.
const NAME_ATTEMPTS: u32 = 100;


/// Already formatted, validated Windows-local date/time, including milliseconds.
struct LocalStamp(String);


impl LocalStamp {
    /// Validate calendar fields before using them in a Windows-safe filename.
    fn from_fields(date: [u16; 3], time: [u16; 4]) -> io::Result<Self> {
        let [year, month, day] = date;
        let [hour, minute, second, millis] = time;
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let days = match month {
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            _ => 0,
        };
        if !(1601..=9999).contains(&year)
            || day == 0
            || day > days
            || hour > 23
            || minute > 59
            || second > 59
            || millis > 999
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid local log timestamp",
            ));
        }
        Ok(Self(format!(
            "{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{millis:03}"
        )))
    }


    /// Read Windows local civil time without changing timezone or clock settings.
    #[cfg(windows)]
    fn now() -> io::Result<Self> {
        // SAFETY: SYSTEMTIME contains integer fields; GetLocalTime fills this
        // exclusively borrowed, correctly sized structure synchronously.
        let mut time: winapi::um::minwinbase::SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { winapi::um::sysinfoapi::GetLocalTime(&mut time) };
        Self::from_fields(
            [time.wYear, time.wMonth, time.wDay],
            [time.wHour, time.wMinute, time.wSecond, time.wMilliseconds],
        )
    }
}


/// File state shares the same lock as console emission to preserve line order.
struct State<W> {
    /// Exclusively owned file or test writer.
    writer: W,
    /// Permanent latch: never append more bytes after a partial write/flush failure.
    failed: bool,
}


/// One log with no asynchronous queue or references to capture pixels.
struct Session<W> {
    /// Absolute selected log path, retained for failure diagnostics.
    path: PathBuf,
    /// Serialises complete lines from all producer threads.
    state: Mutex<State<W>>,
    /// Uses the existing cooperative cancellation path on file failure.
    shutdown: Arc<AtomicBool>,
}


impl<W: Write> Session<W> {
    /// Mirror a complete UTF-8 line and report the first file failure once.
    /// Console and diagnostic callbacks must not recursively enter this session.
    fn emit(&self, line: &str, console: impl FnOnce(&str), diagnostic: impl FnOnce(&str)) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        console(line);
        if state.failed {
            return;
        }
        if let Err(error) = state
            .writer
            .write_all(line.as_bytes())
            .and_then(|()| state.writer.flush())
        {
            state.failed = true;
            self.shutdown.store(true, Ordering::Release);
            diagnostic(&format!(
                "Session log write/flush failed at {}; requesting shutdown; file logging is incomplete: {error}\n",
                self.path.display()
            ));
        }
    }


    /// Flush after final output, returning failure if any line was already lost.
    fn finish(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.failed {
            return Err(io::Error::other(
                "session log is incomplete after a write/flush failure",
            ));
        }
        state.writer.flush().inspect_err(|_| {
            state.failed = true;
            self.shutdown.store(true, Ordering::Release);
        })
    }
}


/// Exclusively create one file; existing files and symlinks are never truncated.
/// Only AlreadyExists permits another name, with a fixed 100-attempt limit.
fn create_in(directory: &Path, stamp: &LocalStamp, pid: u32) -> io::Result<(PathBuf, File)> {
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log process ID must be positive",
        ));
    }
    for attempt in 0..NAME_ATTEMPTS {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt:03}")
        };
        let name = format!("qemu-screen-grab-server-{}-pid{pid}{suffix}.log", stamp.0);
        let path = directory.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("cannot create session log {}: {error}", path.display()),
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "all {NAME_ATTEMPTS} session log names already exist in {}",
            directory.display()
        ),
    ))
}


/// Initialise before binding TCP; errors refuse startup and name the destination.
#[cfg(windows)]
pub(crate) fn initialise(shutdown: Arc<AtomicBool>) -> io::Result<()> {
    let directory = std::env::current_dir()?;
    let stamp = LocalStamp::now()?;
    let (path, file) = create_in(&directory, &stamp, std::process::id())?;
    let session = Session {
        path: path.clone(),
        state: Mutex::new(State {
            writer: file,
            failed: false,
        }),
        shutdown,
    };
    SESSION
        .set(session)
        .map_err(|_| io::Error::other("session logger already initialised"))?;
    line(
        false,
        format_args!(
            "Session log: {}; filename time=Windows local; encoding=UTF-8",
            path.display()
        ),
    );
    if SESSION
        .get()
        .expect("installed session logger")
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .failed
    {
        return Err(io::Error::other("initial session log write failed"));
    }
    Ok(())
}


/// Write console bytes without println's broken-pipe panic behaviour.
fn console(error: bool, line: &str) {
    if error {
        let _ = io::stderr().lock().write_all(line.as_bytes());
    } else {
        let _ = io::stdout().lock().write_all(line.as_bytes());
    }
}


/// Mirror one existing server message, leaving its diagnostic fields unchanged.
pub(crate) fn line(error: bool, message: fmt::Arguments<'_>) {
    let text = format!("{message}\n");
    if let Some(session) = SESSION.get() {
        session.emit(
            &text,
            |text| console(error, text),
            |text| console(true, text),
        );
    } else {
        console(error, &text);
    }
}


/// Flush the final server message and expose incomplete logs through exit status.
#[cfg(windows)]
pub(crate) fn finish() -> io::Result<()> {
    SESSION.get().map_or(Ok(()), Session::finish)
}


/// Keep existing stdout message syntax while mirroring through the shared sink.
macro_rules! info {
    ($($argument:tt)*) => { $crate::logging::line(false, format_args!($($argument)*)) };
}


/// Keep existing stderr message syntax while mirroring through the shared sink.
macro_rules! error {
    ($($argument:tt)*) => { $crate::logging::line(true, format_args!($($argument)*)) };
}


pub(crate) use {error, info};


#[cfg(test)]
mod tests;
