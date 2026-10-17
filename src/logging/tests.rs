//! File publication, whole-line ordering and injected logging failure regressions.

use super::*;
use std::sync::atomic::AtomicU64;


/// Disjoint temporary roots for this process's logger tests.
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);


/// Removes only the directory exclusively created by this test helper.
struct TestDirectory(PathBuf);


impl TestDirectory {
    /// Create a fresh test root without replacing any existing path.
    fn new() -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "capture-log-test-{}-{nanos}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}


impl Drop for TestDirectory {
    /// Test-owned temporary files have no repository or operator data.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}


/// A fixed civil time makes exclusive-creation collisions deterministic.
fn stamp() -> LocalStamp {
    LocalStamp::from_fields([2026, 10, 17], [9, 8, 7, 6]).unwrap()
}


/// Use the production locking/writing path with an injected owned writer.
fn session<W: Write>(writer: W) -> Session<W> {
    Session {
        path: PathBuf::from("test-session.log"),
        state: Mutex::new(State {
            writer,
            failed: false,
        }),
        shutdown: Arc::new(AtomicBool::new(false)),
    }
}


/// Valid calendar output sorts by local civil time and contains no colons.
#[test]
fn filename_stamp_validates_calendar_and_millisecond_fields() {
    assert_eq!(stamp().0, "20261017-090807-006");
    assert!(LocalStamp::from_fields([2024, 2, 29], [23, 59, 59, 999]).is_ok());
    for (date, time) in [
        ([2026, 2, 29], [0, 0, 0, 0]),
        ([2026, 0, 1], [0, 0, 0, 0]),
        ([2026, 4, 31], [0, 0, 0, 0]),
        ([2026, 1, 0], [0, 0, 0, 0]),
        ([2026, 1, 1], [24, 0, 0, 0]),
        ([2026, 1, 1], [0, 60, 0, 0]),
        ([2026, 1, 1], [0, 0, 60, 0]),
        ([2026, 1, 1], [0, 0, 0, 1000]),
    ] {
        assert!(LocalStamp::from_fields(date, time).is_err());
    }
}


/// The second identical launch creates a suffix and preserves preceding bytes.
#[test]
fn exclusive_creation_preserves_existing_logs_and_uses_collision_suffix() {
    let directory = TestDirectory::new();
    let (first, mut file) = create_in(&directory.0, &stamp(), 42).unwrap();
    file.write_all(b"original evidence\n").unwrap();
    drop(file);
    let (second, file) = create_in(&directory.0, &stamp(), 42).unwrap();
    drop(file);
    assert_eq!(
        first.file_name().unwrap(),
        "qemu-screen-grab-server-20261017-090807-006-pid42.log"
    );
    assert_eq!(
        second.file_name().unwrap(),
        "qemu-screen-grab-server-20261017-090807-006-pid42-001.log"
    );
    assert_eq!(std::fs::read(first).unwrap(), b"original evidence\n");
    assert_eq!(std::fs::read(second).unwrap(), b"");
}


/// Exhaustion refuses after exactly the documented 100 names, without overwrite.
#[test]
fn collision_budget_and_creation_errors_are_bounded_and_report_paths() {
    let directory = TestDirectory::new();
    for _ in 0..NAME_ATTEMPTS {
        drop(create_in(&directory.0, &stamp(), 7).unwrap());
    }
    let error = create_in(&directory.0, &stamp(), 7).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 100);
    assert!(error.to_string().contains(directory.0.to_str().unwrap()));
    let missing = directory.0.join("absent-directory");
    let error = create_in(&missing, &stamp(), 7).unwrap_err();
    assert!(error.to_string().contains(missing.to_str().unwrap()));
    assert!(!missing.exists());
    assert!(create_in(&directory.0, &stamp(), 0).is_err());
}


/// Concurrent producers preserve exactly the same complete UTF-8 lines in both sinks.
#[test]
fn concurrent_output_and_shutdown_messages_match_file_and_console_order() {
    let log = Arc::new(session(Vec::<u8>::new()));
    let console = Arc::new(Mutex::new(String::new()));
    let threads: Vec<_> = (0..4)
        .map(|producer| {
            let log = Arc::clone(&log);
            let console = Arc::clone(&console);
            std::thread::spawn(move || {
                for line in 0..25 {
                    let text = format!("capture producer={producer}; frame={line}; café\n");
                    log.emit(
                        &text,
                        |text| console.lock().unwrap().push_str(text),
                        |_| panic!("unexpected file error"),
                    );
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    for text in [
        "Shutdown signal received.\n",
        "Shutting down gracefully.\n",
        "stats cause=shutdown\n",
        "Shutdown complete.\n",
    ] {
        log.emit(
            text,
            |text| console.lock().unwrap().push_str(text),
            |_| panic!("unexpected file error"),
        );
    }
    log.finish().unwrap();
    let state = log.state.lock().unwrap();
    assert_eq!(state.writer, console.lock().unwrap().as_bytes());
    assert_eq!(
        String::from_utf8(state.writer.clone())
            .unwrap()
            .lines()
            .count(),
        104
    );
    assert!(state.writer.ends_with(b"Shutdown complete.\n"));
    assert!(!log.shutdown.load(Ordering::Acquire));
}


/// Model a partial disk write or flush failure without depending on the host disk.
struct FailingWriter {
    /// Maximum bytes accepted before a deterministic write failure.
    remaining: usize,
    /// Instead inject an error on flushing a completely written line.
    fail_flush: bool,
    /// Number of write calls, including the failing one.
    writes: usize,
}


impl Write for FailingWriter {
    /// Accept a bounded prefix, then behave like a full or failed destination.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.remaining == 0 {
            return Err(io::Error::other("injected disk write failure"));
        }
        let written = bytes.len().min(self.remaining);
        self.remaining -= written;
        Ok(written)
    }


    /// Inject the separate error boundary after a successful line write.
    fn flush(&mut self) -> io::Result<()> {
        if self.fail_flush {
            Err(io::Error::other("injected flush failure"))
        } else {
            Ok(())
        }
    }
}


/// A failed file never receives later bytes; console continues and shutdown latches.
#[test]
fn write_and_flush_failure_warn_once_and_request_shutdown() {
    for (remaining, fail_flush) in [(3, false), (usize::MAX, true)] {
        let log = session(FailingWriter {
            remaining,
            fail_flush,
            writes: 0,
        });
        let mut console = String::new();
        let mut warnings = Vec::new();
        log.emit(
            "first line\n",
            |line| console.push_str(line),
            |line| warnings.push(line.to_owned()),
        );
        assert!(log.shutdown.load(Ordering::Acquire));
        let writes = log.state.lock().unwrap().writer.writes;
        log.emit(
            "second line\n",
            |line| console.push_str(line),
            |line| warnings.push(line.to_owned()),
        );
        assert_eq!(log.state.lock().unwrap().writer.writes, writes);
        assert_eq!(console, "first line\nsecond line\n");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("test-session.log"));
        assert!(warnings[0].contains("logging is incomplete"));
        assert!(log.finish().is_err());
    }
}
