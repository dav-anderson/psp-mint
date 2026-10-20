//! The AIR plumbing the integration tests share: a real `air_server` in this
//! process (`sp_wire::air_request::local_server`), the PSP mint's own listener on
//! it, and a wallet-side `call` that sends one request the way the wallet does
//! (`sp_wire::air_request::ask`) and hands back the status and JSON the mint's
//! outcome has.
//!
//! Wallets reach the PSP mint over AIR only (keys, keysets, rotation, rollover
//! and check-state), so the tests of those calls call the AIR listener instead of
//! the HTTP routes they used to have. A reply is read as the HTTP route's answer
//! used to be: an answered request is `200` and its JSON; a signed refusal is
//! `400` and its JSON (the PSP's refusals carry no `http_status`; a body over a
//! ceiling is told apart by its `code`, `request_too_large`); a signed "cannot
//! answer now" is `503` and no JSON.
//!
//! The AIR client always dials port 5702 (upstream item 12), so these tests need
//! that port free; `local_server::start` says so when it is not. One server
//! serves every test of a binary, and the tests take turns on it (it has a single
//! storage worker, so tests running in parallel starve each other past their
//! timeouts). Each test has its own mint key and its own slot counter, so no two
//! tests touch the same cell. A test that holds an [`Air`] drops it before it
//! starts another.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use mint_server::state::AppState;
use psp_mint::air_listener::{self, Serve, Settings};
use psp_mint::server::{self, Health, Limits, SharedRotating};
use serde_json::Value;
use sp_wire::ServiceDid;
use sp_wire::WIRE_VERSION;
use sp_wire::air_request::addr;
use sp_wire::air_request::ask::{AskConfig, AskError, InFlight, Verified, ask};
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::{
    KIND_CHECK_STATE, KIND_KEYS, KIND_KEYSETS, KIND_MINT_INFO, KIND_ROLLOVER, KIND_ROTATION,
    STATUS_UNAVAILABLE, VerifyError, local_server,
};
use sp_wire::seal::SecretKey as DidKey;

/// One test at a time on the shared AIR server.
static AIR_TURN: Mutex<()> = Mutex::new(());

/// The shared server plus this test's turn on it; the turn ends when this drops,
/// whether the test passed, failed or panicked.
pub struct Server {
    server: ServerRef,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for Server {
    type Target = ServerRef;

    fn deref(&self) -> &ServerRef {
        &self.server
    }
}

pub fn server() -> Server {
    let turn = AIR_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let server = local_server::start().expect("a local AIR server (is port 5702 free?)");
    Server {
        server,
        _turn: turn,
    }
}

/// The listener on a thread of its own, as `psp-mint serve` runs it. Dropping it
/// stops it.
pub struct Listener {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

pub fn start_serving(
    server: &ServerRef,
    key: &DidKey,
    dir: PathBuf,
    serve: Arc<Serve>,
) -> Listener {
    let settings = Settings {
        server: server.clone(),
        held_reads: 16,
        timeout_secs: 10,
        state_dir: dir,
    };
    let key = key.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let thread = thread::Builder::new()
        .name("test-air-listener".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(air_listener::run(&settings, &key, &*serve, async move {
                let _ = stopped.await;
            }))
        })
        .unwrap();
    Listener {
        stop: Some(stop),
        thread: Some(thread),
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One PSP mint on the AIR: its DID, its listener and the wallet side to call it.
pub struct Air {
    pub did: ServiceDid,
    cfg: AskConfig,
    /// Dropped before `server`, so the listener has stopped when the turn ends.
    _listener: Listener,
    pub server: Server,
}

/// The kind of request a route of the old HTTP API stands for.
fn kind_of(route: &str) -> u8 {
    match route {
        "/keys" => KIND_KEYS,
        "/keysets" => KIND_KEYSETS,
        "/rotation" => KIND_ROTATION,
        "/rollover" => KIND_ROLLOVER,
        "/check-state" => KIND_CHECK_STATE,
        "/health" => KIND_MINT_INFO,
        other => panic!("{other} is not a call the PSP mint answers over AIR"),
    }
}

fn scratch() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "psp-mint-air-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl Air {
    /// Take the turn on the shared server and start the listener for this mint,
    /// under a new mint key and a slot counter of its own. The turn is held until
    /// this drops.
    pub fn start(state: &Arc<AppState>, rot: &SharedRotating, limits: Limits) -> Air {
        let server = server();
        let key = DidKey::new();
        let did = addr::key_to_did(&key.public_key());
        let state = Arc::clone(state);
        let rot = Arc::clone(rot);
        let health = Health::new(100);
        let serve: Arc<Serve> = Arc::new(move |kind: u8, body: &[u8]| {
            server::air_served(&state, &rot, &health, &limits, kind, body)
        });
        let listener = start_serving(&server, &key, scratch(), serve);
        let cfg = AskConfig {
            server: server.server.clone(),
            mint: did.clone(),
            wire_version: WIRE_VERSION,
            network: "regtest".to_string(),
            reply_wait_secs: 30,
            attempts: 3,
            max_skew_secs: 300,
            timeout_secs: 10,
        };
        Air {
            did,
            cfg,
            _listener: listener,
            server,
        }
    }

    /// Send one request of `kind` with `body` and return the verified reply.
    pub fn ask(&self, kind: u8, body: &[u8]) -> Result<Verified, AskError> {
        async fn go(cfg: &AskConfig, flight: &mut InFlight) -> Result<Verified, AskError> {
            let client = client::connect(&cfg.server).await?;
            ask(&client, cfg, flight, &mut |_| Ok(())).await
        }
        let cfg = &self.cfg;
        let body = body.to_vec();
        block_on_new_runtime(move || async move {
            let mut flight = InFlight::with_body(kind, body);
            go(cfg, &mut flight).await
        })
        .map_err(AskError::from)
        .and_then(|r| r)
    }

    /// The route's call over AIR: the status and JSON of the outcome (see the
    /// module docs). A reply that fails a check, or no reply, fails the test.
    pub fn call(&self, route: &str, body: &[u8]) -> (u16, Value) {
        match self.ask(kind_of(route), body) {
            Ok(v) => {
                let json = serde_json::from_slice(v.body()).unwrap_or(Value::Null);
                if v.refused() {
                    (400, json)
                } else {
                    (200, json)
                }
            }
            // The mint said it cannot answer now: what a 5xx is over AIR.
            Err(AskError::Verify(VerifyError::Refused(STATUS_UNAVAILABLE))) => (503, Value::Null),
            Err(e) => panic!("{route} over AIR: {e}"),
        }
    }

    /// A call without a body, answered with `200` and its JSON as `T`.
    pub fn get<T: serde::de::DeserializeOwned>(&self, route: &str) -> T {
        let (status, body) = self.call(route, &[]);
        assert_eq!(status, 200, "{route}: {body}");
        serde_json::from_value(body).unwrap()
    }

    /// A call with a body, answered with `200` and its JSON as `T`.
    pub fn post_ok<T: serde::de::DeserializeOwned>(
        &self,
        route: &str,
        body: &impl serde::Serialize,
    ) -> T {
        let (status, json) = self.post_json(route, body);
        assert_eq!(status, 200, "{route}: {json}");
        serde_json::from_value(json).unwrap()
    }

    pub fn post_json(&self, route: &str, body: &impl serde::Serialize) -> (u16, Value) {
        self.call(route, &serde_json::to_vec(body).unwrap())
    }

    pub fn post_bytes(&self, route: &str, bytes: &[u8]) -> (u16, Value) {
        self.call(route, bytes)
    }
}
