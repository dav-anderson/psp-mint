//! The AIR plumbing the integration tests share: a real `air_server` in this
//! process (`sp_wire::air_request::local_server`), the PSP mint's own listener on
//! it, and a wallet-side `call` that sends one request the way the wallet does
//! (`sp_wire::air_request::ask`) and hands back the status and JSON the mint's
//! outcome has.
//!
//! Wallets reach the PSP mint over AIR only (keys, keysets, rotation, rollover,
//! check-state, spend and reconcile), so the tests of those calls call the AIR
//! listener, not an HTTP route. A reply is read as an HTTP answer would be: an
//! answered request is `200` and its JSON; a signed refusal is
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
//!
//! [`PspMint`] is the mint as `psp-mint serve` builds it (`Settings::open_mint`
//! and `serve`): a `Rotor` and its `Rollovers` behind a `Rotating`, the settlement
//! records shared with the listener, the burn memos, and a `Health` with a
//! network. Every integration test that needs a mint builds it here, so none of
//! them tests a shape production does not run.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use mint_server::state::AppState;
use psp_mint::air_listener::{self, Serve, Settings};
use psp_mint::melts::MeltLog;
use psp_mint::records::{Records, SharedRecords};
use psp_mint::rotation::{Rollovers, Rotor};
use psp_mint::server::{self, Health, Limits, Rotating, SharedMelts, SharedRotating};
use serde_json::Value;
use sp_wire::ServiceDid;
use sp_wire::WIRE_VERSION;
use sp_wire::air_request::addr;
use sp_wire::air_request::ask::{AskConfig, AskError, InFlight, Verified, ask};
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::{
    KIND_CHECK_STATE, KIND_KEYS, KIND_KEYSETS, KIND_MINT_INFO, KIND_PSP_RECONCILE, KIND_PSP_SPEND,
    KIND_ROLLOVER, KIND_ROTATION, STATUS_UNAVAILABLE, VerifyError, local_server,
};
use sp_wire::seal::SecretKey as DidKey;

/// The network every test mint is on (`bitcoin.network`), as a mint's `Health`
/// and the wallet side of the AIR agree on it.
pub const NETWORK: &str = "regtest";

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
                // Resolves when the test drops the listener, or if the sender
                // is gone; either way the listener is to stop.
                stopped.await.ok();
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
            // An error means the listener has already ended; its end is read
            // below.
            stop.send(()).ok();
        }
        if let Some(thread) = self.thread.take() {
            let ended = thread.join();
            // A test that is already failing keeps its own message.
            if !thread::panicking() {
                match ended {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => panic!("the AIR listener ended with an error: {e}"),
                    Err(_) => panic!("the AIR listener thread panicked"),
                }
            }
        }
    }
}

/// One PSP mint on the AIR: its DID, its listener and the wallet side to call it.
pub struct Air {
    pub did: ServiceDid,
    cfg: AskConfig,
    /// The same, with a short wait and one attempt, for a request that is not
    /// meant to be answered (`ask_unanswered`).
    quick: AskConfig,
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
        "/spend" => KIND_PSP_SPEND,
        "/reconcile" => KIND_PSP_RECONCILE,
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
    /// under a new mint key and a slot counter of its own, as `psp-mint serve`
    /// runs it: it settles reconciliation requests (kind 15) against `records`,
    /// which the scan side shares, and answers mint info from `health`. The turn
    /// is held until this drops.
    pub fn start(
        state: &Arc<AppState>,
        rot: &SharedRotating,
        records: &SharedRecords,
        health: &Arc<Health>,
        limits: Limits,
    ) -> Air {
        let server = server();
        let key = DidKey::new();
        let did = addr::key_to_did(&key.public_key());
        let state = Arc::clone(state);
        let rot = Arc::clone(rot);
        let records = Arc::clone(records);
        let health = Arc::clone(health);
        let serve: Arc<Serve> = Arc::new(move |kind: u8, body: &[u8]| {
            server::air_served_with(&state, &rot, &records, &health, &limits, kind, body)
        });
        let listener = start_serving(&server, &key, scratch(), serve);
        let config = |reply_wait_secs, attempts| AskConfig {
            server: server.server.clone(),
            mint: did.clone(),
            wire_version: WIRE_VERSION,
            network: NETWORK.to_string(),
            reply_wait_secs,
            attempts,
            max_skew_secs: 300,
            timeout_secs: 10,
        };
        let cfg = config(30, 3);
        let quick = config(3, 1);
        Air {
            did,
            cfg,
            quick,
            _listener: listener,
            server,
        }
    }

    /// Send one request of `kind` with `body` and return the verified reply.
    pub fn ask(&self, kind: u8, body: &[u8]) -> Result<Verified, AskError> {
        self.ask_with(&self.cfg, kind, body)
    }

    /// A request the mint is not meant to answer (a refused reconciliation):
    /// waits a few seconds for a reply once and returns what the asker meets, an
    /// error. A reply, if there were one, would be returned as `Ok`.
    pub fn ask_unanswered(&self, kind: u8, body: &[u8]) -> Result<Verified, AskError> {
        self.ask_with(&self.quick, kind, body)
    }

    fn ask_with(&self, cfg: &AskConfig, kind: u8, body: &[u8]) -> Result<Verified, AskError> {
        async fn go(cfg: &AskConfig, flight: &mut InFlight) -> Result<Verified, AskError> {
            let client = client::connect(&cfg.server).await?;
            ask(&client, cfg, flight, &mut |_| Ok(())).await
        }
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

/// The text of a `rollovers.json` with the fields in `set` put over the real
/// file: every other field is what the mint writes (the real `Rollovers`,
/// serialised), so a crash-state file made here has the current shape whenever
/// the struct changes. A field `set` names that the struct does not have fails.
pub fn rollovers_file(set: Value) -> String {
    let mut file = serde_json::to_value(Rollovers::default()).unwrap();
    let fields = file.as_object_mut().unwrap();
    for (name, value) in set.as_object().expect("the fields to set are an object") {
        assert!(fields.contains_key(name), "Rollovers has no field `{name}`");
        fields.insert(name.clone(), value.clone());
    }
    file.to_string()
}

/// A PSP mint wired as `psp-mint serve` wires it.
pub struct PspMint {
    pub dir: PathBuf,
    pub state: Arc<AppState>,
    pub rot: SharedRotating,
    pub records: SharedRecords,
    pub melts: SharedMelts,
    pub health: Arc<Health>,
    pub limits: Limits,
}

/// A folder of its own for one test mint, empty.
pub fn mint_dir(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "psp-mint-it-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

impl PspMint {
    /// A mint in a folder of its own that rotates every hour from an hour after
    /// the first issuance (`rotation.activation_hours = 1`, `period_hours = 1`),
    /// on a fixed clock.
    pub fn open(name: &str, clock: u64, limits: Limits) -> PspMint {
        PspMint::open_in(&mint_dir(name), 1, 1, clock, limits)
    }

    /// As [`PspMint::open`], in `dir`, with these rotation settings (0 turns
    /// rotation off, as the settings do). Opens the way `Settings::open_mint`
    /// does: `Rotor::open_at`, `Rollovers::open`, `Rotating::new`, `app_state`;
    /// the records and the burn memos are the ones `serve` opens.
    pub fn open_in(
        dir: &Path,
        activation_hours: u64,
        period_hours: u64,
        clock: u64,
        limits: Limits,
    ) -> PspMint {
        let (rotor, logic) = Rotor::open_at(
            dir,
            mint_types::Amount::from_units(1024),
            activation_hours,
            period_hours,
            clock,
        )
        .unwrap();
        let rot: SharedRotating = Rotating::new(rotor, Rollovers::open(dir).unwrap());
        let records: SharedRecords = Arc::new(Mutex::new(
            Records::open(&dir.join("settlements.json")).unwrap(),
        ));
        PspMint {
            dir: dir.to_path_buf(),
            state: server::app_state(logic),
            rot,
            records,
            melts: Arc::new(Mutex::new(
                MeltLog::open(&dir.join("melt_memos.json")).unwrap(),
            )),
            health: Health::for_network(100, NETWORK),
            limits,
        }
    }

    /// `settlements.json`, the file `/audit` reads.
    pub fn records_path(&self) -> PathBuf {
        self.dir.join("settlements.json")
    }

    /// The local HTTP API, as `serve` builds it.
    pub fn router(&self) -> axum::Router {
        server::router_audited(
            Arc::clone(&self.state),
            Arc::clone(&self.health),
            Arc::clone(&self.melts),
            self.limits,
            Arc::clone(&self.rot),
            self.records_path(),
        )
    }

    /// The local HTTP API on a port of its own; the base URL.
    pub async fn http(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = self.router();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// The mint's AIR listener, with the wallet side to call it. Holds the turn
    /// on the shared AIR server until it drops.
    pub fn air(&self) -> Air {
        Air::start(
            &self.state,
            &self.rot,
            &self.records,
            &self.health,
            self.limits,
        )
    }
}
