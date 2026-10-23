//! A flag that is fired once and that a thread can wait on with a timeout.
//!
//! The scan loop and the watch loop of `serve` sleep between their passes. They
//! wait on a `Stop` instead of calling `thread::sleep`, so that a stop and
//! the snapshot, which have to join those threads before the mint's stores can
//! be dropped, wake them at once and do not wait out an interval.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Stop {
    fired: Mutex<bool>,
    wake: Condvar,
}

impl Stop {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fire the flag and wake every thread waiting on it. Firing again changes
    /// nothing.
    pub fn fire(&self) {
        *self.fired.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }

    pub fn is_fired(&self) -> bool {
        *self.fired.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait until the flag is fired or `timeout` has passed. Returns whether the
    /// flag is fired.
    pub fn wait(&self, timeout: Duration) -> bool {
        let guard = self.fired.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, timeout, |fired| !*fired)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_flag_that_is_not_fired_times_out_and_a_fired_one_does_not_wait() {
        let stop = Stop::new();
        assert!(!stop.is_fired());
        assert!(!stop.wait(Duration::from_millis(0)));
        stop.fire();
        assert!(stop.is_fired());
        // An hour is never waited: the flag is already fired.
        assert!(stop.wait(Duration::from_secs(3600)));
        // Firing again changes nothing.
        stop.fire();
        assert!(stop.is_fired());
    }

    #[test]
    fn firing_wakes_a_thread_that_is_waiting_a_long_time() {
        let stop = Arc::new(Stop::new());
        let waiter = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || stop.wait(Duration::from_secs(3600)))
        };
        stop.fire();
        // If the wakeup were lost this would be false only after the hour; the
        // test would not end, which is how it fails.
        assert!(waiter.join().unwrap());
    }
}
