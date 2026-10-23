//! Bounded full-desktop capture service for the interactive Windows guest.

#[cfg(any(windows, test))]
mod server;


#[cfg(any(windows, test))]
mod logging;


#[cfg(any(windows, test))]
mod parameters;


#[cfg(windows)]
mod windows_capture;


/// Preserve the existing listener address; network exposure is an operator decision.
#[cfg(windows)]
const LISTEN_ADDR: &str = "0.0.0.0:9999";


/// Run the bounded capture owner and persistent handlers until Ctrl+C.
#[cfg(windows)]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Err(error) = logging::initialise(std::sync::Arc::clone(&shutdown)) {
        eprintln!("Cannot start capture server logging: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let result = run_windows(shutdown).await;
    if let Err(ref error) = result {
        logging::error!("Capture server failed: {error}");
    }
    let flushed = logging::finish();
    if let Err(ref error) = flushed {
        logging::error!("Capture server log finalisation failed: {error}");
    }
    if result.is_ok() && flushed.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}


/// Start native serving only after the working-directory log has been opened.
#[cfg(windows)]
async fn run_windows(
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let signal = std::sync::Arc::clone(&shutdown);
    ctrlc::set_handler(move || signal.store(true, std::sync::atomic::Ordering::Release))?;
    let listener = tokio::net::TcpListener::bind(LISTEN_ADDR).await?;
    logging::info!(
        "Guest TCP capture server v{} on {LISTEN_ADDR}; full 1920x1080 only; v3 raw BGR8, explicit legacy v2 BGRA8 comparison mode",
        env!("CARGO_PKG_VERSION")
    );
    server::run(
        listener,
        windows_capture::GdiCapture,
        shutdown,
        server::Limits::default(),
    )
    .await?;
    Ok(())
}


/// Linux supports runtime tests, but cannot supply native GDI pixels.
#[cfg(not(windows))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("Screen capture requires Windows GDI; run this server inside the Windows guest".into())
}
