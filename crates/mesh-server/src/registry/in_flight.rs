use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// A count of the requests being relayed through one account.
///
/// Clones share the count. [`InFlight::start`] counts one request until its
/// guard is dropped.
#[derive(Clone, Debug, Default)]
pub struct InFlight(Arc<AtomicU32>);

impl InFlight {
    /// The number of requests being relayed now.
    pub fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    /// Count one more request until the returned guard is dropped.
    pub fn start(&self) -> InFlightGuard {
        self.0.fetch_add(1, Ordering::Relaxed);
        InFlightGuard(self.0.clone())
    }
}

/// Keeps one request in the [`InFlight`] count. Dropping it ends the count,
/// so it works for every way a request can end.
#[derive(Debug)]
pub struct InFlightGuard(Arc<AtomicU32>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
