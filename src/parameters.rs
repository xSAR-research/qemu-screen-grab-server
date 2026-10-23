//! Capture-service timing policy shared by defaults and cooperative polling.
//!
//! These established values are preserved, not newly measured or tuned. Network
//! deadlines bound ownership; polling intervals bound shutdown responsiveness.
//! None can interrupt a GDI call already executing on the capture owner.

use std::time::Duration;


/// Idle first-byte budget so persistent clients cannot retain a connection forever.
pub(crate) const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);


/// Absolute budget for the remaining bytes of one request after its first byte.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);


/// Maximum queue residence before capture starts, bounding obsolete waiting work.
pub(crate) const CAPTURE_QUEUE_TIMEOUT: Duration = Duration::from_secs(2);


/// Capture-result wait budget and then the shared header/pixels/flush send budget.
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);


/// Interval for aggregate service statistics, independent of request pacing.
pub(crate) const STATISTICS_INTERVAL: Duration = Duration::from_secs(60);


/// Capture-owner and listener polling interval for cooperative shutdown checks.
pub(crate) const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(20);


/// Result-wait polling interval for abandoned requests and shutdown detection.
pub(crate) const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(10);
