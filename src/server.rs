//! Host-testable scheduling, framing, deadlines and response ownership.
//!
//! One OS thread executes capture, one job may wait behind it, and two permits
//! cover all queued/executing/sending responses. Network senders cannot block
//! the capture owner. Deadlines cannot pre-empt an executing Win32 call.

use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinSet,
    time::{sleep, timeout, timeout_at},
};
use xsar::tcp::protocol::{
    FrameHeader, REQUEST_BYTES,
    v3::{REQUEST_LEN, Request, ResponseHeader, Status},
};


/// Native dimensions remain fixed; no scaling or cropping is supported here.
pub(crate) const WIDTH: u32 = 1920;


/// Native display height.
pub(crate) const HEIGHT: u32 = 1080;


/// This process supports one fixed compatible display generation.
const DISPLAY_GENERATION: u64 = 1;


/// Maximum rolling samples per stage.
const SAMPLE_LIMIT: usize = 256;


/// Production deadlines; wire inputs never select or extend these values.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    /// First-byte idle deadline between requests.
    idle: Duration,
    /// Absolute deadline for all request bytes after the first.
    request: Duration,
    /// Maximum queue residence before synchronous capture begins.
    queue: Duration,
    /// Capture-result wait budget, then a shared budget for header/pixels/flush.
    response: Duration,
    /// Periodic statistics reporting interval.
    summary: Duration,
}


impl Default for Limits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(120),
            request: Duration::from_secs(10),
            queue: Duration::from_secs(2),
            response: Duration::from_secs(10),
            summary: Duration::from_secs(60),
        }
    }
}


/// Explicit format selected by a fully validated request, never payload length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PixelMode {
    /// Top-down tightly packed BGR8 for version 3.
    Bgr8,
    /// Exact legacy version 2; opaque BGRA and one frame per connection.
    LegacyBgra8,
}


impl PixelMode {
    /// Fixed maximum decoded bytes, independent of untrusted allocation hints.
    pub(crate) const fn bytes(self) -> usize {
        WIDTH as usize
            * HEIGHT as usize
            * match self {
                Self::Bgr8 => 3,
                Self::LegacyBgra8 => 4,
            }
    }
}


/// Prepare one owned native BGRX capture in place, preserving every RGB byte.
/// BGR8 compacts forward after reading each complete source pixel; its backing
/// allocation retains the bounded 32-bit capacity until the response is dropped.
pub(crate) fn prepare_pixels(pixels: &mut Vec<u8>, mode: PixelMode) -> Result<(), Failure> {
    if pixels.len() != PixelMode::LegacyBgra8.bytes() {
        return Err(Failure::new(
            Status::CaptureFailed,
            "GDI pixel length mismatch",
        ));
    }
    match mode {
        PixelMode::LegacyBgra8 => {
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }
        PixelMode::Bgr8 => {
            for index in 0..(WIDTH as usize * HEIGHT as usize) {
                let source = index * 4;
                let colour = [pixels[source], pixels[source + 1], pixels[source + 2]];
                let destination = index * 3;
                pixels[destination..destination + 3].copy_from_slice(&colour);
            }
            pixels.truncate(mode.bytes());
        }
    }
    Ok(())
}


/// Capture-owner stage durations; setup, BitBlt, readback and packing do not overlap.
#[derive(Default, Clone, Copy)]
pub(crate) struct CaptureStages {
    /// Buffer allocation and GDI setup before BitBlt.
    pub(crate) allocation_setup: Duration,
    /// The BitBlt call alone.
    pub(crate) bitblt: Duration,
    /// The deselected-bitmap GetDIBits call alone.
    pub(crate) readback: Duration,
    /// BGR packing or legacy fourth-byte normalisation.
    pub(crate) packing: Duration,
    /// Entire capture/preparation interval, including cleanup and other overhead.
    pub(crate) total: Duration,
}


/// Exclusively owned immutable response pixels with local measurements.
pub(crate) struct Captured {
    /// Complete top-down native payload in its explicit format.
    pub(crate) pixels: Vec<u8>,
    /// Monotonic measurements; no host/guest wall-clock subtraction.
    pub(crate) stages: CaptureStages,
}


/// Fixed wire category and bounded trusted diagnostic.
#[derive(Debug)]
pub(crate) struct Failure {
    /// Versioned protocol status.
    pub(crate) status: Status,
    /// Static local reason, never arbitrary text from the client.
    pub(crate) reason: &'static str,
}


impl Failure {
    /// Refuse without publishing partial or uninitialised pixels.
    pub(crate) const fn new(status: Status, reason: &'static str) -> Self {
        Self { status, reason }
    }
}


/// Providers and their resources live exclusively on the capture OS thread.
pub(crate) trait Capture: Send + 'static {
    /// Execute one fresh acquisition for this accepted request.
    fn capture(&mut self, mode: PixelMode) -> Result<Captured, Failure>;
}


/// Fixed sample ring with distinct lifetime count and maximum.
struct Window {
    /// Rolling duration samples.
    samples: [Duration; SAMPLE_LIMIT],
    /// Next entry replaced.
    next: usize,
    /// Valid entries, capped at SAMPLE_LIMIT.
    used: usize,
    /// Saturating lifetime count.
    count: u64,
    /// Lifetime maximum, unaffected by eviction.
    maximum: Duration,
}


impl Default for Window {
    fn default() -> Self {
        Self {
            samples: [Duration::ZERO; SAMPLE_LIMIT],
            next: 0,
            used: 0,
            count: 0,
            maximum: Duration::ZERO,
        }
    }
}


impl Window {
    /// Record without growing storage.
    fn add(&mut self, value: Duration) {
        self.samples[self.next] = value;
        self.next = (self.next + 1) % SAMPLE_LIMIT;
        self.used = (self.used + 1).min(SAMPLE_LIMIT);
        self.count = self.count.saturating_add(1);
        self.maximum = self.maximum.max(value);
    }


    /// Report rolling upper median/p95/max alongside explicit lifetime metrics.
    fn print(&self, name: &str) {
        if self.used == 0 {
            return;
        }
        let mut ordered = self.samples;
        ordered[..self.used].sort_unstable();
        let p95 = (self.used * 95).div_ceil(100) - 1;
        crate::logging::info!(
            "stats stage={name} lifetime_count={} rolling_count={} rolling_median_us={} rolling_p95_us={} rolling_max_us={} lifetime_max_us={}",
            self.count,
            self.used,
            ordered[self.used / 2].as_micros(),
            ordered[p95].as_micros(),
            ordered[self.used - 1].as_micros(),
            self.maximum.as_micros()
        );
    }
}


/// Fixed measurement windows and lifetime outcomes.
#[derive(Default)]
struct Statistics {
    /// Queue/setup/BitBlt/readback/packing/capture/header/payload/request totals.
    stages: [Window; 9],
    /// Saturating complete response count.
    successes: u64,
    /// Fixed failure counters indexed by the agreed status codes 0..6.
    failures: [u64; 7],
}


impl Statistics {
    /// Count an outcome without retaining requests or pixels.
    fn fail(&mut self, status: Status) {
        self.failures[status as usize] = self.failures[status as usize].saturating_add(1);
    }


    /// Emit periodic/shutdown summaries with explicit rolling versus lifetime labels.
    fn print(&self, cause: &str) {
        crate::logging::info!(
            "stats cause={cause} lifetime_successes={} lifetime_failures={:?} rolling_capacity={SAMPLE_LIMIT}",
            self.successes,
            self.failures
        );
        for (window, name) in self.stages.iter().zip([
            "queue_wait",
            "allocation_setup",
            "bitblt",
            "readback",
            "packing",
            "capture_preparation_total",
            "header_write",
            "payload_write_flush",
            "request_to_response_complete",
        ]) {
            window.print(name);
        }
    }
}


/// Waiting work owns its response quota through completion or cancellation.
struct Job {
    /// Validated requested format.
    mode: PixelMode,
    /// Queue entry time.
    submitted: Instant,
    /// Last permitted start time.
    deadline: Instant,
    /// Revoked on disconnect, invalid pipelining, timeout or handler cancellation.
    cancelled: Arc<AtomicBool>,
    /// True after leaving the queue for synchronous capture.
    started: Arc<AtomicBool>,
    /// One of exactly two payload ownership permits.
    lease: OwnedSemaphorePermit,
    /// Closed receivers invalidate queued work.
    reply: oneshot::Sender<Result<LeasedFrame, Failure>>,
}


/// A completed response retains its quota until the final write or drop.
struct LeasedFrame {
    /// Validated complete pixels and measurements.
    captured: Captured,
    /// Monotonic identity assigned after successful fresh acquisition.
    frame: u64,
    /// Queue residence, excluding capture and socket writes.
    queued: Duration,
    /// Returned only after the immutable payload is no longer referenced.
    _lease: OwnedSemaphorePermit,
}


/// Bounded submission handle; the OS worker never waits on a network sender.
#[derive(Clone)]
struct Scheduler {
    /// One waiting request, independent of the executing request.
    jobs: mpsc::SyncSender<Job>,
    /// Two total queued/executing/sending response leases.
    leases: Arc<Semaphore>,
    /// Cooperative process shutdown.
    shutdown: Arc<AtomicBool>,
}


/// Result receiver and cancellation/start flags for one bounded queued request.
type Submitted = (
    oneshot::Receiver<Result<LeasedFrame, Failure>>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
);


impl Scheduler {
    /// Enqueue once; overload is refused rather than accumulated.
    fn submit(&self, mode: PixelMode, queue_limit: Duration) -> Result<Submitted, Failure> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(Failure::new(Status::ShuttingDown, "server shutting down"));
        }
        let lease = Arc::clone(&self.leases)
            .try_acquire_owned()
            .map_err(|_| Failure::new(Status::Busy, "response ownership limit reached"))?;
        let (reply, receiver) = oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let submitted = Instant::now();
        self.jobs
            .try_send(Job {
                mode,
                submitted,
                deadline: submitted + queue_limit,
                cancelled: Arc::clone(&cancelled),
                started: Arc::clone(&started),
                lease,
                reply,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => Failure::new(Status::Busy, "capture queue full"),
                mpsc::TrySendError::Disconnected(_) => {
                    Failure::new(Status::ShuttingDown, "capture owner unavailable")
                }
            })?;
        Ok((receiver, cancelled, started))
    }
}


/// Spawn one capture owner, checking queue freshness immediately before execution.
fn capture_owner(
    mut capture: impl Capture,
    shutdown: Arc<AtomicBool>,
) -> io::Result<(Scheduler, std::thread::JoinHandle<()>)> {
    let (jobs, receiver) = mpsc::sync_channel::<Job>(1);
    let scheduler = Scheduler {
        jobs,
        leases: Arc::new(Semaphore::new(2)),
        shutdown: Arc::clone(&shutdown),
    };
    let thread = std::thread::Builder::new()
        .name("desktop-capture".to_owned())
        .spawn(move || {
            let mut frame = 0u64;
            while !shutdown.load(Ordering::Acquire) {
                let job = match receiver.recv_timeout(Duration::from_millis(20)) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if job.cancelled.load(Ordering::Acquire) || job.reply.is_closed() {
                    continue;
                }
                if shutdown.load(Ordering::Acquire) {
                    let _ = job.reply.send(Err(Failure::new(
                        Status::ShuttingDown,
                        "shutdown before capture",
                    )));
                    continue;
                }
                if Instant::now() >= job.deadline {
                    let _ = job.reply.send(Err(Failure::new(
                        Status::DeadlineExceeded,
                        "queue deadline expired",
                    )));
                    continue;
                }
                job.started.store(true, Ordering::Release);
                let queued = job.submitted.elapsed();
                let result = capture.capture(job.mode).and_then(|captured| {
                    if captured.pixels.len() != job.mode.bytes() {
                        return Err(Failure::new(
                            Status::CaptureFailed,
                            "capture payload length mismatch",
                        ));
                    }
                    frame = frame.checked_add(1).ok_or_else(|| {
                        Failure::new(Status::CaptureFailed, "frame identity exhausted")
                    })?;
                    Ok(LeasedFrame {
                        captured,
                        frame,
                        queued,
                        _lease: job.lease,
                    })
                });
                let _ = job.reply.send(result);
            }
        })?;
    Ok((scheduler, thread))
}


/// Revoke queued work when an async handler returns or is aborted.
struct CancelJob(Arc<AtomicBool>);


impl Drop for CancelJob {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}


/// Fully validated request with local correlation and measurement boundaries.
struct Incoming {
    /// Explicit format/compatibility mode.
    mode: PixelMode,
    /// Requested session; zero only during the v3 initial handshake.
    session: u64,
    /// Strictly increasing request identity; legacy uses one.
    request: u64,
    /// Expected server display generation.
    generation: u64,
    /// First byte arrival; excludes idle time and includes request remainder.
    started: Instant,
}


/// Idle deadline covers first-byte arrival; one absolute deadline covers the remainder.
async fn read_request(
    socket: &mut TcpStream,
    limits: Limits,
    legacy_session: u64,
) -> Result<Option<Incoming>, Failure> {
    let mut first = [0u8; 1];
    let read = timeout(limits.idle, socket.read(&mut first))
        .await
        .map_err(|_| Failure::new(Status::DeadlineExceeded, "idle deadline"))?
        .map_err(|_| Failure::new(Status::InvalidRequest, "request read failed"))?;
    if read == 0 {
        return Ok(None);
    }
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + limits.request;
    if first[0] == REQUEST_BYTES[0] {
        let mut bytes = REQUEST_BYTES;
        timeout_at(deadline, socket.read_exact(&mut bytes[1..]))
            .await
            .map_err(|_| Failure::new(Status::DeadlineExceeded, "legacy request deadline"))?
            .map_err(|_| Failure::new(Status::InvalidRequest, "truncated legacy request"))?;
        if bytes != REQUEST_BYTES {
            return Err(Failure::new(
                Status::InvalidRequest,
                "unsupported legacy request",
            ));
        }
        return Ok(Some(Incoming {
            mode: PixelMode::LegacyBgra8,
            session: legacy_session,
            request: 1,
            generation: 0,
            started,
        }));
    }
    if first[0] != b'X' {
        return Err(Failure::new(
            Status::InvalidRequest,
            "unknown protocol prefix",
        ));
    }
    let mut bytes = [0u8; REQUEST_LEN];
    bytes[0] = first[0];
    timeout_at(deadline, socket.read_exact(&mut bytes[1..]))
        .await
        .map_err(|_| Failure::new(Status::DeadlineExceeded, "request remainder deadline"))?
        .map_err(|_| Failure::new(Status::InvalidRequest, "truncated v3 request"))?;
    let request = Request::from_bytes(bytes)
        .map_err(|_| Failure::new(Status::InvalidRequest, "invalid v3 request"))?;
    Ok(Some(Incoming {
        mode: PixelMode::Bgr8,
        session: request.session_id(),
        request: request.request_id(),
        generation: request.display_generation(),
        started,
    }))
}


/// Watch socket closure/pipelining and queue expiry without blocking the capture owner.
async fn await_capture(
    socket: &mut TcpStream,
    scheduler: &Scheduler,
    mode: PixelMode,
    limits: Limits,
) -> Result<LeasedFrame, Failure> {
    // Refuse already buffered pipelining or EOF before enqueueing capture work.
    check_waiting_peer(socket)?;
    let (mut reply, cancelled, started) = scheduler.submit(mode, limits.queue)?;
    let _cancel = CancelJob(cancelled);
    let queued = Instant::now();
    let result_deadline = tokio::time::Instant::now() + limits.response;
    loop {
        tokio::select! {
            biased;
            readable = socket.readable() => {
                readable.map_err(|_| Failure::new(Status::InvalidRequest, "socket readiness failed"))?;
                check_waiting_peer(socket)?;
            }
            result = &mut reply => return finish_capture_result(result, result_deadline, &scheduler.shutdown),
            () = tokio::time::sleep_until(result_deadline) => {
                return Err(Failure::new(Status::DeadlineExceeded, "capture result deadline"));
            }
            () = sleep(Duration::from_millis(10)) => {
                if scheduler.shutdown.load(Ordering::Acquire) { return Err(Failure::new(Status::ShuttingDown, "shutdown while awaiting capture")); }
                if !started.load(Ordering::Acquire) && queued.elapsed() >= limits.queue { return Err(Failure::new(Status::DeadlineExceeded, "queue wait deadline")); }
            }
        }
    }
}


/// A ready reply cannot override an expired absolute deadline or shutdown.
/// Validate again after selection because several branches may be ready together.
fn finish_capture_result(
    result: Result<Result<LeasedFrame, Failure>, oneshot::error::RecvError>,
    deadline: tokio::time::Instant,
    shutdown: &AtomicBool,
) -> Result<LeasedFrame, Failure> {
    if shutdown.load(Ordering::Acquire) {
        return Err(Failure::new(
            Status::ShuttingDown,
            "shutdown while awaiting capture",
        ));
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(Failure::new(
            Status::DeadlineExceeded,
            "capture result deadline",
        ));
    }
    result.unwrap_or_else(|_| Err(Failure::new(Status::ShuttingDown, "capture owner stopped")))
}


/// While a response is outstanding, EOF and any extra request bytes revoke it.
fn check_waiting_peer(socket: &TcpStream) -> Result<(), Failure> {
    let mut byte = [0u8; 1];
    match socket.try_read(&mut byte) {
        Ok(0) => Err(Failure::new(
            Status::InvalidRequest,
            "client disconnected before response",
        )),
        Ok(_) => Err(Failure::new(
            Status::InvalidRequest,
            "one outstanding request required",
        )),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(_) => Err(Failure::new(
            Status::InvalidRequest,
            "client read failure before response",
        )),
    }
}


/// Send canonical bounded failure data only for a complete validated v3 request.
async fn send_failure(
    socket: &mut TcpStream,
    request: &Incoming,
    failure: &Failure,
    budget: Duration,
) {
    crate::logging::info!(
        "capture session={} request={} frame=0 protocol={} command=full format={:?} compression=raw outcome={:?} reason={}",
        request.session,
        request.request,
        if request.mode == PixelMode::Bgr8 {
            3
        } else {
            2
        },
        request.mode,
        failure.status,
        failure.reason
    );
    if request.mode == PixelMode::Bgr8
        && let Ok(header) = ResponseHeader::error(failure.status, request.session, request.request)
    {
        let _ = timeout(budget, socket.write_all(&header.to_bytes())).await;
    }
}


/// Sequential persistent requests share one assigned session; framing failures close it.
async fn handle_client(
    mut socket: TcpStream,
    scheduler: Scheduler,
    stats: Arc<Mutex<Statistics>>,
    limits: Limits,
    connection: u64,
) -> Result<(), Failure> {
    let mut established = false;
    let mut previous_request = 0;
    loop {
        let Some(mut request) = read_request(&mut socket, limits, connection).await? else {
            return Ok(());
        };
        let valid_session = if request.mode == PixelMode::LegacyBgra8 {
            !established
        } else if established {
            request.session == connection
        } else {
            request.session == 0 && request.generation == 0
        };
        if !valid_session || request.request <= previous_request {
            let failure = Failure::new(
                Status::InvalidRequest,
                "session/request/display identity mismatch",
            );
            send_failure(&mut socket, &request, &failure, limits.response).await;
            return Err(failure);
        }
        request.session = connection;
        if request.mode == PixelMode::Bgr8
            && established
            && request.generation != DISPLAY_GENERATION
        {
            let failure = Failure::new(Status::IncompatibleDisplay, "display generation mismatch");
            send_failure(&mut socket, &request, &failure, limits.response).await;
            return Err(failure);
        }
        established = true;
        previous_request = request.request;
        let frame = match await_capture(&mut socket, &scheduler, request.mode, limits).await {
            Ok(frame) => frame,
            Err(failure) => {
                send_failure(&mut socket, &request, &failure, limits.response).await;
                return Err(failure);
            }
        };
        let header = if request.mode == PixelMode::Bgr8 {
            ResponseHeader::success(
                connection,
                request.request,
                frame.frame,
                DISPLAY_GENERATION,
                WIDTH,
                HEIGHT,
            )
            .map_err(|_| Failure::new(Status::CaptureFailed, "response metadata rejected"))?
            .to_bytes()
            .to_vec()
        } else {
            let ordinal = u32::try_from(frame.frame).map_err(|_| {
                Failure::new(Status::CaptureFailed, "legacy frame identity exhausted")
            })?;
            FrameHeader::new(WIDTH, HEIGHT, (WIDTH * 4) as u16, ordinal)
                .map_err(|_| {
                    Failure::new(Status::CaptureFailed, "legacy response metadata rejected")
                })?
                .to_bytes()
                .to_vec()
        };
        let sent = async {
            let deadline = tokio::time::Instant::now() + limits.response;
            let started = Instant::now();
            timeout_at(deadline, socket.write_all(&header))
                .await
                .map_err(|_| {
                    Failure::new(Status::DeadlineExceeded, "response header write deadline")
                })?
                .map_err(|_| {
                    Failure::new(Status::InvalidRequest, "response header write failed")
                })?;
            let header_time = started.elapsed();
            let started = Instant::now();
            timeout_at(deadline, async {
                socket.write_all(&frame.captured.pixels).await?;
                socket.flush().await
            })
            .await
            .map_err(|_| Failure::new(Status::DeadlineExceeded, "response payload write deadline"))?
            .map_err(|_| Failure::new(Status::InvalidRequest, "response payload write failed"))?;
            Ok::<_, Failure>((header_time, started.elapsed()))
        }
        .await;
        let (header_time, payload_time) = match sent {
            Ok(timings) => timings,
            Err(failure) => {
                crate::logging::info!(
                    "capture session={} request={} frame={} protocol={} command=full format={:?} compression=raw outcome={:?} reason={}",
                    request.session,
                    request.request,
                    frame.frame,
                    if request.mode == PixelMode::Bgr8 {
                        3
                    } else {
                        2
                    },
                    request.mode,
                    failure.status,
                    failure.reason
                );
                return Err(failure);
            }
        };
        let stages = frame.captured.stages;
        let total = request.started.elapsed();
        {
            let mut stats = stats.lock().expect("statistics mutex");
            stats.successes = stats.successes.saturating_add(1);
            for (window, duration) in stats.stages.iter_mut().zip([
                frame.queued,
                stages.allocation_setup,
                stages.bitblt,
                stages.readback,
                stages.packing,
                stages.total,
                header_time,
                payload_time,
                total,
            ]) {
                window.add(duration);
            }
        }
        crate::logging::info!(
            "capture connection={connection} session={} request={} frame={} protocol={} command=full format={:?} compression=raw display_generation={DISPLAY_GENERATION} display={WIDTH}x{HEIGHT} rect=0,0,{WIDTH},{HEIGHT} stride={} decoded_bytes={} transmitted_bytes={} queue_us={} allocation_setup_us={} bitblt_us={} readback_us={} packing_us={} capture_preparation_total_us={} header_write_us={} payload_write_flush_us={} request_to_response_complete_us={} outcome=ok",
            request.session,
            request.request,
            frame.frame,
            if request.mode == PixelMode::Bgr8 {
                3
            } else {
                2
            },
            request.mode,
            WIDTH
                * if request.mode == PixelMode::Bgr8 {
                    3
                } else {
                    4
                },
            frame.captured.pixels.len(),
            frame.captured.pixels.len(),
            frame.queued.as_micros(),
            stages.allocation_setup.as_micros(),
            stages.bitblt.as_micros(),
            stages.readback.as_micros(),
            stages.packing.as_micros(),
            stages.total.as_micros(),
            header_time.as_micros(),
            payload_time.as_micros(),
            total.as_micros()
        );
        drop(frame);
        if request.mode == PixelMode::LegacyBgra8 {
            return Ok(());
        }
    }
}


/// Run two handlers, one capture owner and bounded periodic/shutdown statistics.
pub(crate) async fn run(
    listener: TcpListener,
    capture: impl Capture,
    shutdown: Arc<AtomicBool>,
    limits: Limits,
) -> io::Result<()> {
    run_with_shutdown_reporter(listener, capture, shutdown, limits, |message| {
        crate::logging::info!("{message}");
    })
    .await
}


/// Report shutdown from the server task, keeping the signal callback atomic-only.
/// Completion is reported only after handlers drain, the capture owner joins and
/// final statistics are printed. Owner/join errors return without success feedback.
async fn run_with_shutdown_reporter(
    listener: TcpListener,
    capture: impl Capture,
    shutdown: Arc<AtomicBool>,
    limits: Limits,
    mut report: impl FnMut(&str),
) -> io::Result<()> {
    let (scheduler, owner) = capture_owner(capture, Arc::clone(&shutdown))?;
    let clients = Arc::new(Semaphore::new(2));
    let stats = Arc::new(Mutex::new(Statistics::default()));
    let mut connection = 1u64;
    let mut handlers = JoinSet::new();
    let mut summary = tokio::time::interval(limits.summary);
    summary.tick().await;
    while !shutdown.load(Ordering::Acquire) {
        while let Some(result) = handlers.try_join_next() {
            if result.is_err() {
                stats
                    .lock()
                    .expect("statistics mutex")
                    .fail(Status::CaptureFailed);
            }
        }
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = match accepted { Ok(accepted) => accepted, Err(error) => { crate::logging::error!("accept failed: {error}"); continue; } };
                let Ok(permit) = Arc::clone(&clients).try_acquire_owned() else {
                    stats.lock().expect("statistics mutex").fail(Status::Busy);
                    drop(socket);
                    continue;
                };
                let scheduler = scheduler.clone();
                let stats = Arc::clone(&stats);
                let identity = connection;
                let Some(next) = connection.checked_add(1) else { shutdown.store(true, Ordering::Release); break; };
                connection = next;
                handlers.spawn(async move {
                    let _permit = permit;
                    if let Err(failure) = handle_client(socket, scheduler, Arc::clone(&stats), limits, identity).await {
                        crate::logging::info!("capture connection={identity} outcome={:?} reason={}", failure.status, failure.reason);
                        stats.lock().expect("statistics mutex").fail(failure.status);
                    }
                });
            }
            () = sleep(Duration::from_millis(20)) => {}
            _ = summary.tick() => stats.lock().expect("statistics mutex").print("periodic"),
            Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                if result.is_err() { stats.lock().expect("statistics mutex").fail(Status::CaptureFailed); }
            }
        }
    }
    report("Shutdown signal received.");
    report("Shutting down gracefully.");
    handlers.abort_all();
    while handlers.join_next().await.is_some() {}
    drop(scheduler);
    // Synchronous GDI cannot be pre-empted: await its return and resource cleanup.
    tokio::task::spawn_blocking(move || owner.join())
        .await
        .map_err(|_| io::Error::other("capture owner join task failed"))?
        .map_err(|_| io::Error::other("capture owner panicked"))?;
    stats.lock().expect("statistics mutex").print("shutdown");
    report("Shutdown complete.");
    Ok(())
}


#[cfg(test)]
mod tests;
