//! The PSP's HTTP API.
//!
//! The handlers are mint-server's, the same ones `mintd` uses, mounted over the
//! PSP's own `MintLogic` in the same process. One process owns the database, so
//! the API and the payment scanner share the mint behind one mutex.
//!
//! Routes:
//!
//! - `GET /keys`: the public key for each denomination.
//! - `POST /melt`: burn notes, bound to a memo that every input signs. The ESP
//!   mint uses this to burn PSP in an exchange.
//! - `POST /check-state`: which nonces are spent.
//! - `GET /audit`: issued, redeemed and outstanding totals.
//! - `GET /health`: `200` while the scanner is keeping up and the mint lock is
//!   sound, `503` otherwise, with the time of the last successful scan.
//!
//! Not mounted, on purpose:
//!
//! - `/admin/issue`: the PSP issues only from confirmed BTC payments.
//! - `/swap`: PSP to FSP is out of scope, and Service Points are not exchanged peer to
//!   peer.
//!
//! There is no authentication and no TLS. A note authorizes its own spend, so a
//! caller can only burn notes they hold. Bind to localhost, or put it behind
//! something that adds TLS.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use mint_core::MintLogic;
use mint_server::api;
use mint_server::state::AppState;

/// Wrap a mint for the API and the scanner to share.
pub fn app_state(logic: MintLogic) -> Arc<AppState> {
    Arc::new(AppState {
        logic: Mutex::new(logic),
        // Only `/admin/issue` reads this, and that route is not mounted. It is a
        // random value nobody is given, so even a mistaken mount would not open
        // unilateral issuance.
        admin_token: hex::encode(rand::random::<[u8; 32]>()),
    })
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// When the scanner last finished a pass, for `/health`.
///
/// A pass counts as successful when the wallet was read and every payment was
/// processed as far as it could go. A payment waiting on confirmations or on a
/// price is not a failed pass.
pub struct Health {
    started: u64,
    last_scan: AtomicU64,
    stale_after_secs: u64,
}

impl Health {
    /// `stale_after_secs`: how long without a successful pass before `/health`
    /// reports a failure.
    pub fn new(stale_after_secs: u64) -> Arc<Self> {
        Arc::new(Health {
            started: now_secs(),
            last_scan: AtomicU64::new(0),
            stale_after_secs,
        })
    }

    pub fn record_scan(&self) {
        self.record_scan_at(now_secs());
    }

    pub fn record_scan_at(&self, unix_secs: u64) {
        self.last_scan.store(unix_secs, Ordering::Relaxed);
    }

    /// Unix time of the last successful pass, or `None` if there has not been one.
    pub fn last_scan(&self) -> Option<u64> {
        match self.last_scan.load(Ordering::Relaxed) {
            0 => None,
            t => Some(t),
        }
    }

    /// The status code and body for `/health` at time `now`.
    pub fn report(&self, lock_poisoned: bool, now: u64) -> (StatusCode, serde_json::Value) {
        let last = self.last_scan();
        let age = last.map(|t| now.saturating_sub(t));
        let since_start = now.saturating_sub(self.started);
        let status = if lock_poisoned {
            "mint_lock_poisoned"
        } else {
            match age {
                Some(a) if a > self.stale_after_secs => "scan_stale",
                Some(_) => "ok",
                // no pass yet: give the first one the same allowance
                None if since_start > self.stale_after_secs => "scan_stale",
                None => "starting",
            }
        };
        let code = match status {
            "ok" | "starting" => StatusCode::OK,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        (
            code,
            serde_json::json!({
                "status": status,
                "last_scan_unix": last,
                "seconds_since_last_scan": age,
                "stale_after_secs": self.stale_after_secs,
            }),
        )
    }
}

/// Why the process should exit, if it should. A poisoned lock means a thread
/// panicked while holding the mint, so every later request would panic too. A
/// finished scan thread means payments are no longer being settled. Either way
/// the process is no good and a supervisor should start a fresh one.
pub fn fatal(state: &AppState, scan_finished: bool) -> Option<&'static str> {
    if state.logic.is_poisoned() {
        Some("the mint lock is poisoned")
    } else if scan_finished {
        Some("the scan thread has stopped")
    } else {
        None
    }
}

async fn health_handler(
    State((state, health)): State<(Arc<AppState>, Arc<Health>)>,
) -> (StatusCode, Json<serde_json::Value>) {
    // `is_poisoned` does not take the lock, so this answers while a long
    // request or scan holds the mint.
    let (code, body) = health.report(state.logic.is_poisoned(), now_secs());
    (code, Json(body))
}

pub fn router(state: Arc<AppState>, health: Arc<Health>) -> Router {
    let health_routes = Router::new()
        .route("/health", get(health_handler))
        .with_state((Arc::clone(&state), health));
    Router::new()
        .route("/keys", get(api::get_keys))
        .route("/melt", post(api::melt))
        .route("/check-state", post(api::check_state))
        .route("/audit", get(api::audit))
        .with_state(state)
        .merge(health_routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(h: &Health, poisoned: bool, now: u64) -> (u16, String) {
        let (code, body) = h.report(poisoned, now);
        (code.as_u16(), body["status"].as_str().unwrap().to_string())
    }

    #[test]
    fn starting_until_the_first_scan_then_ok() {
        let h = Health::new(100);
        let t = h.started;
        assert_eq!(status(&h, false, t + 5), (200, "starting".into()));
        h.record_scan_at(t + 10);
        assert_eq!(status(&h, false, t + 20), (200, "ok".into()));
        assert_eq!(h.last_scan(), Some(t + 10));
    }

    #[test]
    fn no_scan_for_too_long_is_a_failure() {
        let h = Health::new(100);
        let t = h.started;
        assert_eq!(status(&h, false, t + 101), (503, "scan_stale".into()));
    }

    #[test]
    fn an_old_scan_is_a_failure_and_the_boundary_is_ok() {
        let h = Health::new(100);
        h.record_scan_at(1_000);
        assert_eq!(status(&h, false, 1_100), (200, "ok".into()));
        assert_eq!(status(&h, false, 1_101), (503, "scan_stale".into()));
    }

    #[test]
    fn a_poisoned_lock_is_a_failure_even_after_a_fresh_scan() {
        let h = Health::new(100);
        h.record_scan_at(1_000);
        assert_eq!(status(&h, true, 1_001), (503, "mint_lock_poisoned".into()));
    }

    #[test]
    fn the_body_reports_the_last_scan_time() {
        let h = Health::new(100);
        h.record_scan_at(1_000);
        let (_, body) = h.report(false, 1_030);
        assert_eq!(body["last_scan_unix"], 1_000);
        assert_eq!(body["seconds_since_last_scan"], 30);
        let (_, none) = Health::new(100).report(false, now_secs());
        assert!(none["last_scan_unix"].is_null());
        assert!(none["seconds_since_last_scan"].is_null());
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_underflow() {
        let h = Health::new(100);
        h.record_scan_at(2_000);
        assert_eq!(status(&h, false, 1_000), (200, "ok".into()));
    }
}
