//! Bounds the connection worker enforces: queue depths, poll cadence, and the
//! timeouts that turn a stalled peer into a typed error rather than a hang.

use std::time::Duration;

pub(crate) const COMMAND_CAPACITY: usize = 64;
pub(crate) const EVENT_CAPACITY: usize = 8;
pub(crate) const POLL: Duration = Duration::from_millis(10);
pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
// Completed API round trips over 250 ms are noteworthy; queue wait is excluded.
pub(crate) const SLOW_REQUEST: Duration = Duration::from_millis(250);
pub(crate) const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// The daemon drops a client whose host palette update exceeds the 256
/// indexed colors a terminal has.
pub(crate) const MAX_HOST_PALETTE_COLORS: usize = 256;
