//! Generic fake-provider evidence; these tests cannot establish Win32 freshness.

use super::*;
use std::sync::{Condvar, atomic::AtomicUsize};
use xsar::tcp::protocol::v3::HEADER_LEN;


/// Fake provider runs on the same real dedicated-thread path as native GDI.
struct FakeCapture {
    /// Counts actual executions, including any incorrectly executed stale job.
    calls: Arc<AtomicUsize>,
    /// Optional gate holds the first capture while another request queues.
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
}


impl Capture for FakeCapture {
    fn capture(&mut self, mode: PixelMode) -> Result<Captured, Failure> {
        let ordinal = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        if ordinal == 1
            && let Some(gate) = &self.gate
        {
            let (ready, wake) = &**gate;
            let mut ready = ready.lock().unwrap();
            while !*ready {
                ready = wake.wait(ready).unwrap();
            }
        }
        Ok(Captured {
            pixels: vec![ordinal as u8; mode.bytes()],
            stages: CaptureStages::default(),
        })
    }
}


/// Track provider destruction on the actual owner thread, optionally failing it.
struct ShutdownCapture {
    /// Existing provider supplies bounded pixels and an optional acquisition gate.
    capture: FakeCapture,
    /// Records resource destruction before completion can be reported.
    dropped: Arc<AtomicBool>,
    /// Inject an owner-thread cleanup panic to exercise failed join feedback.
    fail_cleanup: bool,
}


impl Capture for ShutdownCapture {
    /// Delegate acquisition without changing the capture owner's execution path.
    fn capture(&mut self, mode: PixelMode) -> Result<Captured, Failure> {
        self.capture.capture(mode)
    }
}


impl Drop for ShutdownCapture {
    /// Mark cleanup on the owner thread and optionally inject its failure.
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
        assert!(!self.fail_cleanup, "injected capture cleanup failure");
    }
}


/// Wait on observable test state with a finite wall-clock safety bound.
async fn wait_for(mut condition: impl FnMut() -> bool) {
    timeout(Duration::from_secs(2), async {
        while !condition() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("bounded test progress");
}


/// Release an in-flight fake capture without involving a network sender.
fn release(gate: &Arc<(Mutex<bool>, Condvar)>) {
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}


/// Stop and join a worker after all test-owned response leases are released.
async fn stop_owner(scheduler: Scheduler, owner: std::thread::JoinHandle<()>) {
    scheduler.shutdown.store(true, Ordering::Release);
    drop(scheduler);
    tokio::task::spawn_blocking(move || owner.join().unwrap())
        .await
        .unwrap();
}


/// One executing job plus one queued job consumes all ownership; a sender cannot
/// obtain a third buffer even after capture has completed and returned its pixels.
#[tokio::test]
async fn capture_queue_and_response_ownership_are_independently_bounded() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (scheduler, owner) = capture_owner(
        FakeCapture {
            calls: Arc::clone(&calls),
            gate: Some(Arc::clone(&gate)),
        },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let (first, _, _) = scheduler
        .submit(PixelMode::Bgr8, Duration::from_secs(1))
        .unwrap();
    wait_for(|| calls.load(Ordering::Acquire) == 1).await;
    let (second, _, _) = scheduler
        .submit(PixelMode::Bgr8, Duration::from_secs(1))
        .unwrap();
    assert!(matches!(
        scheduler.submit(PixelMode::Bgr8, Duration::from_secs(1)),
        Err(Failure {
            status: Status::Busy,
            ..
        })
    ));
    release(&gate);
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();
    assert_eq!((first.frame, second.frame), (1, 2));
    assert_eq!(
        (first.captured.pixels[0], second.captured.pixels[0]),
        (1, 2)
    );
    assert_eq!(scheduler.leases.available_permits(), 0);
    assert!(
        scheduler
            .submit(PixelMode::Bgr8, Duration::from_secs(1))
            .is_err(),
        "slow senders retain both leases"
    );
    drop(first);
    drop(second);
    assert_eq!(scheduler.leases.available_permits(), 2);
    stop_owner(scheduler, owner).await;
}


/// Expiry, explicit cancellation and a dropped result receiver each revoke a
/// queued request before the owning thread can execute its capture.
#[tokio::test]
async fn stale_cancelled_and_disconnected_queued_jobs_never_capture() {
    for case in 0..4 {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (scheduler, owner) = capture_owner(
            FakeCapture {
                calls: Arc::clone(&calls),
                gate: Some(Arc::clone(&gate)),
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let (first, _, _) = scheduler
            .submit(PixelMode::Bgr8, Duration::from_secs(1))
            .unwrap();
        wait_for(|| calls.load(Ordering::Acquire) == 1).await;
        let (second, cancelled, _) = scheduler
            .submit(
                PixelMode::Bgr8,
                if case == 0 {
                    Duration::ZERO
                } else {
                    Duration::from_secs(1)
                },
            )
            .unwrap();
        if case == 1 {
            cancelled.store(true, Ordering::Release);
        }
        if case == 3 {
            scheduler.shutdown.store(true, Ordering::Release);
        }
        if case == 2 {
            drop(second);
        } else {
            release(&gate);
            assert!(
                second
                    .await
                    .unwrap_or_else(|_| Err(Failure::new(Status::InvalidRequest, "cancelled")))
                    .is_err()
            );
        }
        release(&gate);
        drop(first.await.unwrap().unwrap());
        wait_for(|| scheduler.leases.available_permits() == 2).await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        stop_owner(scheduler, owner).await;
    }
}


/// A real disconnected socket revokes its queued request before the capture
/// owner is released, so the next physical capture is never performed for it.
#[tokio::test]
async fn disconnected_socket_cancels_queued_capture_before_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (scheduler, owner) = capture_owner(
        FakeCapture {
            calls: Arc::clone(&calls),
            gate: Some(Arc::clone(&gate)),
        },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let (first, _, _) = scheduler
        .submit(PixelMode::Bgr8, Duration::from_secs(1))
        .unwrap();
    wait_for(|| calls.load(Ordering::Acquire) == 1).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let handler = tokio::spawn(handle_client(
        socket,
        scheduler.clone(),
        Arc::new(Mutex::new(Statistics::default())),
        Limits::default(),
        1,
    ));
    client
        .write_all(&Request::new(0, 1, 0).unwrap().to_bytes())
        .await
        .unwrap();
    wait_for(|| scheduler.leases.available_permits() == 0).await;
    drop(client);
    assert!(
        timeout(Duration::from_secs(2), handler)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    release(&gate);
    drop(first.await.unwrap().unwrap());
    wait_for(|| scheduler.leases.available_permits() == 2).await;
    assert_eq!(calls.load(Ordering::Acquire), 1);
    stop_owner(scheduler, owner).await;
}


/// A slow synchronous provider cannot keep a socket task waiting indefinitely.
/// Its executing lease stays owned until the OS work returns; the late frame is
/// dropped when the timed-out receiver has gone, without becoming a response.
#[tokio::test]
async fn capture_result_timeout_revokes_receiver_and_drops_late_pixels() {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (scheduler, owner) = capture_owner(
        FakeCapture {
            calls: Arc::clone(&calls),
            gate: Some(Arc::clone(&gate)),
        },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut socket, _) = listener.accept().await.unwrap();
    let limits = Limits {
        response: Duration::from_millis(50),
        ..Limits::default()
    };
    let result = await_capture(&mut socket, &scheduler, PixelMode::Bgr8, limits).await;
    assert!(matches!(
        result,
        Err(Failure {
            status: Status::DeadlineExceeded,
            reason: "capture result deadline"
        })
    ));
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(
        scheduler.leases.available_permits(),
        1,
        "in-flight native work keeps its own lease"
    );
    release(&gate);
    wait_for(|| scheduler.leases.available_permits() == 2).await;
    stop_owner(scheduler, owner).await;
}


/// A receiver and its absolute timer can both be ready after executor delay.
/// Even when biased selection chooses the reply, expiry or shutdown discards
/// that owned frame and releases its response lease instead of publishing it.
#[tokio::test]
async fn ready_capture_reply_cannot_override_expiry_or_shutdown() {
    for shutting_down in [false, true] {
        let shutdown = Arc::new(AtomicBool::new(false));
        let (scheduler, owner) = capture_owner(
            FakeCapture {
                calls: Arc::new(AtomicUsize::new(0)),
                gate: None,
            },
            Arc::clone(&shutdown),
        )
        .unwrap();
        let (captured, _, _) = scheduler
            .submit(PixelMode::Bgr8, Duration::from_secs(1))
            .unwrap();
        let frame = captured.await.unwrap().unwrap();
        let (send, mut reply) = oneshot::channel();
        assert!(send.send(Ok(frame)).is_ok());
        shutdown.store(shutting_down, Ordering::Release);
        let deadline = if shutting_down {
            tokio::time::Instant::now() + Duration::from_secs(1)
        } else {
            tokio::time::Instant::now()
        };
        let result = tokio::select! {
            biased;
            result = &mut reply => finish_capture_result(result, deadline, &shutdown),
            () = tokio::time::sleep_until(deadline) => panic!("ready reply must win biased selection"),
        };
        let expected = if shutting_down {
            Status::ShuttingDown
        } else {
            Status::DeadlineExceeded
        };
        assert!(matches!(result, Err(Failure { status, .. }) if status == expected));
        assert_eq!(scheduler.leases.available_permits(), 2);
        stop_owner(scheduler, owner).await;
    }
}


/// Start a loopback service with short test deadlines and no Win32 assumptions.
async fn start(
    limits: Limits,
) -> (
    std::net::SocketAddr,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let task = tokio::spawn(run(
        listener,
        FakeCapture {
            calls: Arc::clone(&calls),
            gate: None,
        },
        Arc::clone(&stop),
        limits,
    ));
    (address, stop, calls, task)
}


/// Read one complete versioned response and its bounded payload.
async fn response(socket: &mut TcpStream) -> (ResponseHeader, Vec<u8>) {
    let mut bytes = [0u8; HEADER_LEN];
    timeout(Duration::from_secs(2), socket.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let header = ResponseHeader::from_bytes(bytes).unwrap();
    let mut pixels = vec![0; header.pixel_bytes()];
    timeout(Duration::from_secs(2), socket.read_exact(&mut pixels))
        .await
        .unwrap()
        .unwrap();
    (header, pixels)
}


/// Two requests on one connection acquire distinct fresh pixels/frame IDs;
/// reconnecting assigns a new session and continues global frame identity.
#[tokio::test]
async fn persistent_requests_are_fresh_and_reconnect_has_a_new_session() {
    let (address, stop, calls, task) = start(Limits::default()).await;
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket
        .write_all(&Request::new(0, 1, 0).unwrap().to_bytes())
        .await
        .unwrap();
    let (first, pixels) = response(&mut socket).await;
    assert_eq!(first.status(), Status::Ok);
    assert_eq!(pixels.len(), PixelMode::Bgr8.bytes());
    assert!(pixels.iter().all(|pixel| *pixel == 1));
    socket
        .write_all(
            &Request::new(first.session_id(), 2, first.display_generation())
                .unwrap()
                .to_bytes(),
        )
        .await
        .unwrap();
    let (second, pixels) = response(&mut socket).await;
    assert!(second.frame_id() > first.frame_id());
    assert!(pixels.iter().all(|pixel| *pixel == 2));
    drop(socket);
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket
        .write_all(&Request::new(0, 1, 0).unwrap().to_bytes())
        .await
        .unwrap();
    let (third, _) = response(&mut socket).await;
    assert_ne!(third.session_id(), first.session_id());
    assert!(third.frame_id() > second.frame_id());
    assert_eq!(calls.load(Ordering::Acquire), 3);
    stop.store(true, Ordering::Release);
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}


/// A repeated request identity is a canonical bounded error and closes the
/// connection without capturing another frame.
#[tokio::test]
async fn repeated_identity_is_rejected_before_capture() {
    let (address, stop, calls, task) = start(Limits::default()).await;
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket
        .write_all(&Request::new(0, 1, 0).unwrap().to_bytes())
        .await
        .unwrap();
    let (first, _) = response(&mut socket).await;
    socket
        .write_all(
            &Request::new(first.session_id(), 1, first.display_generation())
                .unwrap()
                .to_bytes(),
        )
        .await
        .unwrap();
    let (error, pixels) = response(&mut socket).await;
    assert_eq!(error.status(), Status::InvalidRequest);
    assert!(pixels.is_empty());
    assert_eq!(calls.load(Ordering::Acquire), 1);
    stop.store(true, Ordering::Release);
    task.await.unwrap().unwrap();
}


/// Explicit legacy compatibility starts its own connection; switching an
/// established v3 socket to v2 closes it without another capture or guessed header.
#[tokio::test]
async fn established_v3_connection_cannot_switch_to_legacy_v2() {
    let (address, stop, calls, task) = start(Limits::default()).await;
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket
        .write_all(&Request::new(0, 1, 0).unwrap().to_bytes())
        .await
        .unwrap();
    let (first, _) = response(&mut socket).await;
    assert_eq!(first.status(), Status::Ok);
    socket.write_all(&REQUEST_BYTES).await.unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(
        timeout(Duration::from_secs(2), socket.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(calls.load(Ordering::Acquire), 1);
    stop.store(true, Ordering::Release);
    task.await.unwrap().unwrap();
}


/// The third connected client is closed while two idle handlers own all slots;
/// idle and partial-header deadlines eventually release those original slots.
#[tokio::test]
async fn client_slots_idle_and_absolute_request_deadlines_are_bounded() {
    let limits = Limits {
        idle: Duration::from_millis(80),
        request: Duration::from_millis(40),
        ..Limits::default()
    };
    let (address, stop, calls, task) = start(limits).await;
    let mut first = TcpStream::connect(address).await.unwrap();
    first.write_all(b"X").await.unwrap();
    let mut second = TcpStream::connect(address).await.unwrap();
    sleep(Duration::from_millis(10)).await;
    let mut third = TcpStream::connect(address).await.unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(
        timeout(Duration::from_secs(1), third.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(
        timeout(Duration::from_secs(1), first.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(
        timeout(Duration::from_secs(1), second.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
    stop.store(true, Ordering::Release);
    task.await.unwrap().unwrap();
}


/// A client that never reads its multi-megabyte response loses its lease at the
/// absolute write deadline; another accepted connection still captures independently.
#[tokio::test]
async fn stalled_sender_does_not_block_capture_owner_and_shutdown_drops_handlers() {
    let limits = Limits {
        response: Duration::from_millis(300),
        ..Limits::default()
    };
    let (address, stop, calls, task) = start(limits).await;
    let mut stalled = TcpStream::connect(address).await.unwrap();
    stalled
        .write_all(&Request::new(0, 1, 0).unwrap().to_bytes())
        .await
        .unwrap();
    wait_for(|| calls.load(Ordering::Acquire) == 1).await;
    let mut active = TcpStream::connect(address).await.unwrap();
    active
        .write_all(&Request::new(0, 1, 0).unwrap().to_bytes())
        .await
        .unwrap();
    let (fresh, _) = response(&mut active).await;
    assert_eq!(fresh.status(), Status::Ok);
    assert_eq!(calls.load(Ordering::Acquire), 2);
    sleep(Duration::from_millis(400)).await;
    let mut pixels = Vec::new();
    timeout(Duration::from_secs(2), stalled.read_to_end(&mut pixels))
        .await
        .unwrap()
        .unwrap();
    assert!(
        pixels.len() < HEADER_LEN + PixelMode::Bgr8.bytes(),
        "the stalled transfer must not complete"
    );
    stop.store(true, Ordering::Release);
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(active.read(&mut byte).await.unwrap(), 0);
}


/// Shutdown is visible while capture is blocked; completion waits for actual
/// owner destruction and does not require a second Ctrl+C or another request.
#[tokio::test]
async fn shutdown_feedback_waits_for_capture_cleanup_and_reports_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let dropped = Arc::new(AtomicBool::new(false));
    let messages = Arc::new(Mutex::new(Vec::new()));
    let reported = Arc::clone(&messages);
    let cleanup = Arc::clone(&dropped);
    let task = tokio::spawn(run_with_shutdown_reporter(
        listener,
        ShutdownCapture {
            capture: FakeCapture {
                calls: Arc::clone(&calls),
                gate: Some(Arc::clone(&gate)),
            },
            dropped: Arc::clone(&dropped),
            fail_cleanup: false,
        },
        Arc::clone(&stop),
        Limits::default(),
        move |message| {
            reported
                .lock()
                .unwrap()
                .push((message.to_owned(), cleanup.load(Ordering::Acquire)));
        },
    ));
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket
        .write_all(&Request::new(0, 1, 0).unwrap().to_bytes())
        .await
        .unwrap();
    wait_for(|| calls.load(Ordering::Acquire) == 1).await;
    stop.store(true, Ordering::Release);
    let notified = timeout(Duration::from_secs(2), async {
        while messages.lock().unwrap().len() < 2 {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    let before_cleanup = messages.lock().unwrap().clone();
    let finished_early = task.is_finished();
    release(&gate);
    notified.unwrap();
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        !finished_early,
        "synchronous capture still owns its resources"
    );
    assert_eq!(
        before_cleanup,
        [
            ("Shutdown signal received.".to_owned(), false),
            ("Shutting down gracefully.".to_owned(), false),
        ]
    );
    assert_eq!(
        *messages.lock().unwrap(),
        [
            ("Shutdown signal received.".to_owned(), false),
            ("Shutting down gracefully.".to_owned(), false),
            ("Shutdown complete.".to_owned(), true),
        ]
    );
}


/// An owner cleanup failure propagates and cannot produce a completion message.
#[tokio::test]
async fn shutdown_owner_failure_never_reports_success() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let mut messages = Vec::new();
    let result = run_with_shutdown_reporter(
        listener,
        ShutdownCapture {
            capture: FakeCapture {
                calls: Arc::new(AtomicUsize::new(0)),
                gate: None,
            },
            dropped: Arc::clone(&dropped),
            fail_cleanup: true,
        },
        Arc::new(AtomicBool::new(true)),
        Limits::default(),
        |message| messages.push(message.to_owned()),
    )
    .await;
    assert_eq!(result.unwrap_err().to_string(), "capture owner panicked");
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(
        messages,
        ["Shutdown signal received.", "Shutting down gracefully."]
    );
}


/// In-place packing retains BGR byte order and exact/near-black values, while
/// the legacy comparison format independently normalises only its unused byte.
#[test]
fn packing_preserves_colours_and_ignores_the_unused_gdi_byte() {
    let mut original = vec![0; PixelMode::LegacyBgra8.bytes()];
    original[..16].copy_from_slice(&[0, 0, 0, 0, 2, 2, 2, 7, 11, 29, 253, 128, 255, 17, 42, 0]);
    let mut bgr = original.clone();
    prepare_pixels(&mut bgr, PixelMode::Bgr8).unwrap();
    assert_eq!(bgr.len(), PixelMode::Bgr8.bytes());
    assert_eq!(&bgr[..12], &[0, 0, 0, 2, 2, 2, 11, 29, 253, 255, 17, 42]);
    prepare_pixels(&mut original, PixelMode::LegacyBgra8).unwrap();
    assert_eq!(
        &original[..16],
        &[
            0, 0, 0, 255, 2, 2, 2, 255, 11, 29, 253, 255, 255, 17, 42, 255
        ]
    );
    assert!(prepare_pixels(&mut vec![0; 16], PixelMode::Bgr8).is_err());
}


/// Overnight sampling keeps exactly 256 rolling entries, distinct from lifetime totals.
#[test]
fn statistics_window_does_not_grow_and_retains_lifetime_maximum() {
    let mut samples = Window::default();
    samples.add(Duration::from_secs(5));
    for _ in 0..10_000 {
        samples.add(Duration::from_millis(1));
    }
    assert_eq!(samples.used, SAMPLE_LIMIT);
    assert_eq!(samples.count, 10_001);
    assert_eq!(samples.maximum, Duration::from_secs(5));
    assert!(
        samples
            .samples
            .iter()
            .all(|sample| *sample == Duration::from_millis(1))
    );
}
