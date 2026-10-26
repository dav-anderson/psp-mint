//! The AIR listener against a real `air_server` started in this process: a
//! wallet's mint-info, keys, keysets, rotation, rollover and check-state requests
//! are answered, signed by the mint DID key, and checked by the same code the
//! wallet runs (`sp_wire::air_request::ask`). The calls run against a real mint
//! and are compared with what the mint serves when it is called in this process;
//! the rollover is the first that changes the mint's state, so it is also
//! resent, refused and run at its full size (1000 notes).
//!
//! The AIR client always dials port 5702 (upstream item 12), so these tests
//! need that port free; `local_server::start` says so when it is not. One server
//! serves every test of this binary, and each test has its own mint key, so no
//! two tests ever touch the same cell.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use mint_core::state::AppState;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, transaction_sighash};
use psp_mint::air_listener::{self, NEXT_FILE, Serve, Settings};
use psp_mint::rotation::{RolloverRequest, RolloverResponse, RotationResponse};
use psp_mint::server::{Health, KeysetsResponse, Limits, SharedRotating};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1};
use sp_wire::air_request::addr::{self, mailbox_secret, reply_secret, seal_to, shared_wallet};
use sp_wire::air_request::ask::{AskConfig, AskError, InFlight, Outcome, Verified, ask_mint_info};
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::envelope::{STATUS_ANSWERED, WireRequest};
use sp_wire::air_request::{
    KIND_CHECK_STATE, KIND_KEYS, KIND_KEYSETS, KIND_MINT_INFO, KIND_ROLLOVER, KIND_ROTATION,
    MintInfo, STATUS_REFUSED, STATUS_UNAVAILABLE, Served, VerifyError, local_server,
    verify_receipt,
};
use sp_wire::keys::CheckStateRequest;
use sp_wire::seal::SecretKey;
use sp_wire::{ServiceDid, WIRE_VERSION};

const NETWORK: &str = "regtest";

/// One test at a time on the shared AIR server. It has a single storage worker,
/// so tests running in parallel starve each other past their timeouts.
static AIR_TURN: Mutex<()> = Mutex::new(());

/// The shared server plus this test's turn on it; the turn ends when this drops,
/// whether the test passed, failed or panicked.
struct Server {
    server: ServerRef,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl std::ops::Deref for Server {
    type Target = ServerRef;

    fn deref(&self) -> &ServerRef {
        &self.server
    }
}

fn server() -> Server {
    let turn = AIR_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let server = local_server::start().expect("a local AIR server (is port 5702 free?)");
    Server {
        server,
        _turn: turn,
    }
}

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "psp-mint-air-info-{}-{name}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A mint's DID key and the DID it stands for.
fn mint_key() -> (SecretKey, ServiceDid) {
    let key = SecretKey::new();
    let did = addr::key_to_did(&key.public_key());
    (key, did)
}

type Info = Arc<dyn Fn() -> MintInfo + Send + Sync>;

fn fixed_info(status: &'static str) -> Info {
    Arc::new(move || MintInfo {
        status: status.to_string(),
        wire_version: WIRE_VERSION,
        network: NETWORK.to_string(),
    })
}

/// The listener on a thread of its own, as `serve` runs it. Dropping it stops it.
struct Listener {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

fn start_listener(server: &ServerRef, key: &SecretKey, dir: &Path, info: Info) -> Listener {
    start_serving(
        server,
        key,
        dir,
        Arc::new(air_listener::mint_info_only(move || info())),
    )
}

fn start_serving(server: &ServerRef, key: &SecretKey, dir: &Path, serve: Arc<Serve>) -> Listener {
    let settings = Settings {
        server: server.clone(),
        held_reads: 16,
        timeout_secs: 10,
        state_dir: dir.to_path_buf(),
    };
    let key = *key;
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

impl Listener {
    /// Stop it and say how it ended: `Ok` for a stop, `Err` for a failure or a panic.
    fn stop(mut self) -> Result<(), String> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match self.thread.take().unwrap().join() {
            Ok(result) => result,
            Err(_) => Err("the listener thread panicked".into()),
        }
    }

    /// Wait for the thread to end by itself (a panic or a failure).
    fn ended(mut self) -> Result<(), String> {
        let thread = self.thread.take().unwrap();
        match thread.join() {
            Ok(result) => result,
            Err(_) => Err("the listener thread panicked".into()),
        }
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

fn config(server: &ServerRef, did: &ServiceDid, wait: u64, attempts: u32) -> AskConfig {
    AskConfig {
        server: server.clone(),
        mint: *did,
        wire_version: WIRE_VERSION,
        network: NETWORK.to_string(),
        reply_wait_secs: wait,
        attempts,
        max_skew_secs: 300,
        timeout_secs: 10,
    }
}

/// The wallet's flow, blocking, as the wallet's library runs it. Every state
/// that `save` saw is pushed on `saved`.
fn ask_recording(
    cfg: &AskConfig,
    flight: &mut InFlight,
    saved: &mut Vec<Vec<u32>>,
) -> Result<Outcome, AskError> {
    async fn go(
        cfg: &AskConfig,
        flight: &mut InFlight,
        saved: &mut Vec<Vec<u32>>,
    ) -> Result<Outcome, AskError> {
        let client = client::connect(&cfg.server).await?;
        ask_mint_info(&client, cfg, flight, &mut |f| {
            saved.push(f.slots.clone());
            Ok(())
        })
        .await
    }
    block_on_new_runtime(move || go(cfg, flight, saved))
        .map_err(AskError::from)
        .and_then(|r| r)
}

fn ask(cfg: &AskConfig, flight: &mut InFlight) -> Result<Outcome, AskError> {
    ask_recording(cfg, flight, &mut Vec::new())
}

/// `Create` bytes in a cell, as anyone may in a mailbox slot.
fn put(server: &ServerRef, key: &SecretKey, bytes: Vec<u8>) -> client::Put {
    block_on_new_runtime(move || async move {
        let client = client::connect(server).await.unwrap();
        client::create(&client, key, bytes, 10).await.unwrap()
    })
    .unwrap()
}

/// What a cell holds now, if anything.
fn peek(server: &ServerRef, key: &SecretKey) -> Option<Vec<u8>> {
    block_on_new_runtime(move || async move {
        let client = client::connect(server).await.unwrap();
        client::read_once(&client, key.public_key(), 10)
            .await
            .unwrap()
    })
    .unwrap()
}

fn reply_cell(did: &ServiceDid, flight: &InFlight) -> SecretKey {
    let shared = shared_wallet(&flight.r, &addr::did_to_key(did).unwrap());
    reply_secret(&shared, &flight.request_id, 0)
}

/// Wait, for at most 20 seconds, until the cell holds something.
fn wait_for_cell(server: &ServerRef, key: &SecretKey) {
    let until = Instant::now() + Duration::from_secs(20);
    while peek(server, key).is_none() {
        assert!(Instant::now() < until, "the cell was not written");
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_next(dir: &Path, want: u32) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let now = air_listener::read_next(dir).unwrap();
        if now == want {
            return;
        }
        assert!(
            Instant::now() < until,
            "the counter is {now}, expected {want}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_wallet_gets_the_mints_signed_info_over_air() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("happy");
    let listener = start_listener(&server, &key, &dir, fixed_info("ok"));

    let mut flight = InFlight::new();
    let mut saved = Vec::new();
    let out = ask_recording(&config(&server, &did, 10, 3), &mut flight, &mut saved).unwrap();

    assert_eq!(out.info.status, "ok");
    assert_eq!(out.info.wire_version, WIRE_VERSION);
    assert_eq!(out.info.network, NETWORK);
    assert_eq!(out.slots, vec![0]);
    assert_eq!(out.attempts_used, 1);
    assert_eq!(out.request_id, flight.request_id);
    // The request is kept before anything is written, and again after the write.
    assert_eq!(saved, vec![vec![], vec![0]]);
    // The receipt verifies from the DID alone.
    let receipt = verify_receipt(&did, &out.receipt).unwrap();
    assert_eq!(receipt.request_id, flight.request_id);
    // The counter moved past the answered slot.
    wait_for_next(&dir, 1);
    listener.stop().unwrap();
}

#[test]
fn the_reply_reports_what_health_reports_now() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("health");
    let health = Health::for_network(100, NETWORK);
    let info_health = Arc::clone(&health);
    let listener = start_listener(
        &server,
        &key,
        &dir,
        Arc::new(move || info_health.mint_info(false, psp_mint::server::now_secs())),
    );
    let cfg = config(&server, &did, 10, 3);

    let first = ask(&cfg, &mut InFlight::new()).unwrap();
    assert_eq!(first.info.status, "starting");

    health.record_scan_at(psp_mint::server::now_secs());
    let second = ask(&cfg, &mut InFlight::new()).unwrap();
    assert_eq!(second.info.status, "ok");
    assert_eq!(second.slots, vec![1]);

    health.record_scan_at(psp_mint::server::now_secs() - 1_000);
    let third = ask(&cfg, &mut InFlight::new()).unwrap();
    assert_eq!(third.info.status, "scan_stale");
    listener.stop().unwrap();
}

#[test]
fn garbage_in_the_slots_is_skipped_and_the_next_request_is_still_answered() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("garbage");
    let mint_pub = addr::did_to_key(&did).unwrap();

    // Slot 0: bytes that do not open. Slot 1: opens, is not a request.
    // Slot 2: a request of a kind this mint does not know. Slot 3: a request
    // whose reply key is not a key.
    let junk = addr::random_bytes(149);
    let not_a_request = seal_to(&mint_pub, b"hello").unwrap();
    let r_public = addr::key_to_bytes(&SecretKey::new().public_key());
    let unknown_kind = WireRequest {
        kind: 9,
        request_id: addr::random_id(),
        reply_key: r_public,
        body: vec![],
    };
    let bad_key = WireRequest {
        kind: KIND_MINT_INFO,
        request_id: addr::random_id(),
        reply_key: [0u8; 33],
        body: vec![],
    };
    for (slot, bytes) in [
        junk,
        not_a_request,
        seal_to(&mint_pub, &unknown_kind.encode()).unwrap(),
        seal_to(&mint_pub, &bad_key.encode()).unwrap(),
    ]
    .into_iter()
    .enumerate()
    {
        let created = put(&server, &mailbox_secret(&did, slot as u32), bytes);
        assert!(matches!(created, client::Put::Created(_)));
    }

    let listener = start_listener(&server, &key, &dir, fixed_info("ok"));
    let out = ask(&config(&server, &did, 10, 3), &mut InFlight::new()).unwrap();
    assert_eq!(out.slots, vec![4], "the wallet moves past the taken slots");
    assert_eq!(out.info.status, "ok");
    wait_for_next(&dir, 5);
    // Nothing was answered for the junk: the unknown kind's reply cell is empty.
    let shared = addr::shared_mint(&key, &addr::bytes_to_key(&r_public).unwrap());
    assert!(peek(&server, &reply_secret(&shared, &unknown_kind.request_id, 0)).is_none());
    listener.stop().unwrap();
}

#[test]
fn a_restarted_mint_resumes_from_its_counter_and_a_late_reply_is_recovered() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("restart");
    let cfg = config(&server, &did, 10, 3);

    let first = start_listener(&server, &key, &dir, fixed_info("ok"));
    let out = ask(&cfg, &mut InFlight::new()).unwrap();
    assert_eq!(out.slots, vec![0]);
    wait_for_next(&dir, 1);
    first.stop().unwrap();

    // The mint is down: the wallet writes its request and gives up.
    let mut flight = InFlight::new();
    let down = config(&server, &did, 2, 1);
    match ask(&down, &mut flight) {
        Err(AskError::Unreachable { attempts: 1, .. }) => {}
        other => panic!("expected the mint to be unreachable, got {other:?}"),
    }
    assert_eq!(
        flight.slots,
        vec![1],
        "the wallet kept the request it wrote"
    );

    // The mint starts again, reads its counter (1), and answers the waiting request.
    let second = start_listener(&server, &key, &dir, fixed_info("ok"));
    wait_for_next(&dir, 2);

    // The wallet asks again with the same request: the reply is already there,
    // so nothing is written (attempts_used 0) and the slots are unchanged.
    let out = ask(&cfg, &mut flight).unwrap();
    assert_eq!(out.attempts_used, 0);
    assert_eq!(out.slots, vec![1]);
    assert_eq!(out.info.status, "ok");
    second.stop().unwrap();
}

#[test]
fn a_mint_that_dies_before_replying_is_retried_and_answers_once() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("dies");

    // The first listener dies in the middle of its first answer, the way a crash
    // would: after it opened the request, before the reply was written.
    let armed = Arc::new(AtomicBool::new(true));
    let dying: Info = {
        let armed = Arc::clone(&armed);
        Arc::new(move || {
            if armed.swap(false, Ordering::SeqCst) {
                panic!("simulated crash of the mint");
            }
            MintInfo {
                status: "ok".into(),
                wire_version: WIRE_VERSION,
                network: NETWORK.into(),
            }
        })
    };
    let first = start_listener(&server, &key, &dir, dying);

    // The wallet: tries of 3 s, three of them.
    let wallet = {
        let cfg = config(&server, &did, 3, 3);
        thread::spawn(move || {
            let mut flight = InFlight::new();
            let out = ask(&cfg, &mut flight);
            (flight, out)
        })
    };

    assert!(
        first.ended().is_err(),
        "the first listener should have died"
    );
    assert_eq!(
        air_listener::read_next(&dir).unwrap(),
        0,
        "it died before finishing slot 0"
    );

    // The mint comes back after the wallet's second try has been written. It
    // starts from its counter (0): slot 0 is answered, slot 1 finds the reply
    // cell taken.
    wait_for_cell(&server, &mailbox_secret(&did, 1));
    let second = start_listener(&server, &key, &dir, fixed_info("ok"));

    let (flight, out) = wallet.join().unwrap();
    let out = out.unwrap();
    assert_eq!(out.attempts_used, 2, "answered during the second try");
    assert_eq!(out.slots, vec![0, 1]);
    assert_eq!(flight.slots, vec![0, 1]);

    wait_for_next(&dir, 2);
    // One reply only: the cell still holds the reply the wallet verified, not
    // the second answer the restarted mint made for slot 1.
    let cell = reply_cell(&did, &flight);
    let sealed = peek(&server, &cell).expect("the reply cell is filled");
    assert_eq!(addr::open(&flight.r, &sealed).unwrap(), out.receipt);
    second.stop().unwrap();
}

#[test]
fn a_damaged_counter_ends_the_listener_with_a_clear_error() {
    let server = server();
    let (key, _) = mint_key();
    let dir = scratch("damaged");
    std::fs::write(dir.join(NEXT_FILE), "not a number").unwrap();
    let listener = start_listener(&server, &key, &dir, fixed_info("ok"));
    let error = listener.ended().unwrap_err();
    assert!(error.contains(NEXT_FILE), "{error}");
}

#[test]
fn the_listener_without_a_counter_probes_from_zero_and_skips_answered_slots() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("rebuild");
    let cfg = config(&server, &did, 10, 3);

    let first = start_listener(&server, &key, &dir, fixed_info("ok"));
    ask(&cfg, &mut InFlight::new()).unwrap();
    ask(&cfg, &mut InFlight::new()).unwrap();
    wait_for_next(&dir, 2);
    first.stop().unwrap();

    // The counter is lost: the mint starts at slot 0 again. The two answered
    // slots find their reply cells taken and are passed over.
    std::fs::remove_file(dir.join(NEXT_FILE)).unwrap();
    let second = start_listener(&server, &key, &dir, fixed_info("ok"));
    wait_for_next(&dir, 2);
    let out = ask(&cfg, &mut InFlight::new()).unwrap();
    assert_eq!(out.slots, vec![2]);
    second.stop().unwrap();
}

// ---------------------------------------------------------------- keys, keysets, rotation

/// A real mint (a rotating one, with a manual clock, so the rotation answer is
/// the same from one call to the next), and the listener's `serve` for it.
struct RealMint {
    state: Arc<AppState>,
    rot: SharedRotating,
    /// The records and the mint's DID, as the listener's `serve` holds them.
    desk: psp_mint::server::ClaimDesk,
    health: Arc<Health>,
    limits: Limits,
}

fn real_mint(dir: &Path) -> RealMint {
    real_mint_with(dir, Limits::default())
}

fn real_mint_with(dir: &Path, limits: Limits) -> RealMint {
    // Wired as `serve` wires it: see `common::PspMint`.
    let mint = common::PspMint::open_in(dir, 1, 1, 1_800_000_000, limits);
    RealMint {
        state: mint.state,
        rot: mint.rot,
        desk: psp_mint::server::ClaimDesk {
            records: mint.records,
            psp: mint.did,
        },
        health: mint.health,
        limits,
    }
}

impl RealMint {
    fn serve(&self) -> Arc<Serve> {
        let (state, rot, desk, health, limits) = (
            Arc::clone(&self.state),
            Arc::clone(&self.rot),
            self.desk.clone(),
            Arc::clone(&self.health),
            self.limits,
        );
        Arc::new(move |kind, body: &[u8]| {
            psp_mint::server::air_served_with(&state, &rot, &desk, &health, &limits, kind, body)
        })
    }

    /// What the mint answers to `kind` with `body` when it is called in this
    /// process, as the listener calls it.
    fn served(&self, kind: u8, body: &[u8]) -> Option<Served> {
        psp_mint::server::air_served_with(
            &self.state,
            &self.rot,
            &self.desk,
            &self.health,
            &self.limits,
            kind,
            body,
        )
    }

    /// The body of an answered call made in this process.
    fn served_body(&self, kind: u8, body: &[u8]) -> Vec<u8> {
        match self.served(kind, body) {
            Some(Served::Body(bytes)) => bytes,
            Some(Served::Refused(body)) => {
                panic!(
                    "expected an answer, got a refusal: {}",
                    String::from_utf8_lossy(&body)
                )
            }
            Some(Served::Unavailable) => panic!("expected an answer, got 'unavailable'"),
            Some(Served::Ignored(why)) => panic!("expected an answer, got no reply: {why}"),
            None => panic!("expected an answer, the kind is not served"),
        }
    }

    /// The same request handed straight to the mint is answered with `want`.
    fn assert_served(&self, kind: u8, body: &[u8], want: &[u8]) {
        assert_eq!(self.served_body(kind, body).as_slice(), want);
    }
}

/// The wallet's flow for any kind, blocking.
fn ask_any(cfg: &AskConfig, flight: &mut InFlight) -> Result<Verified, AskError> {
    async fn go(cfg: &AskConfig, flight: &mut InFlight) -> Result<Verified, AskError> {
        let client = client::connect(&cfg.server).await?;
        sp_wire::air_request::ask::ask(&client, cfg, flight, &mut |_| Ok(())).await
    }
    block_on_new_runtime(move || go(cfg, flight))
        .map_err(AskError::from)
        .and_then(|r| r)
}

#[test]
fn keys_keysets_and_rotation_are_answered_with_what_the_mint_serves() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("keys-real");
    let mint = real_mint(&dir.join("mint"));
    let listener = start_serving(&server, &key, &dir, mint.serve());
    let cfg = config(&server, &did, 10, 3);

    for (kind, name) in [
        (KIND_KEYS, "keys"),
        (KIND_KEYSETS, "keysets"),
        (KIND_ROTATION, "rotation"),
    ] {
        let mut flight = InFlight::of(kind);
        let v = ask_any(&cfg, &mut flight).unwrap();
        mint.assert_served(kind, b"", v.body());
        assert!(!v.body().is_empty(), "{name}");
        assert_eq!(v.reply.status, 0);
        // The receipt verifies from the DID alone.
        verify_receipt(&did, &v.receipt).unwrap();
    }

    // The bodies are the types the wallet reads.
    let keys = ask_any(&cfg, &mut InFlight::of(KIND_KEYS)).unwrap();
    let keys: serde_json::Value = keys.json().unwrap();
    assert!(!keys["keys"].as_object().unwrap().is_empty());
    let keysets: KeysetsResponse = ask_any(&cfg, &mut InFlight::of(KIND_KEYSETS))
        .unwrap()
        .json()
        .unwrap();
    assert!(keysets.keysets.contains_key(&keysets.current));
    let window: RotationResponse = ask_any(&cfg, &mut InFlight::of(KIND_ROTATION))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(window.current.id, keysets.current);

    // Mint info on the same listener, from the same serve.
    let info = ask(&cfg, &mut InFlight::new()).unwrap();
    assert_eq!(info.info.status, "starting");
    assert_eq!(info.info.network, NETWORK);

    // A kind this mint does not serve is not answered (the listener skips it).
    assert!(
        mint.served(9, &[]).is_none(),
        "unknown kinds are not served"
    );
    listener.stop().unwrap();
}
#[test]
fn a_mint_that_cannot_answer_says_so_in_a_signed_reply() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("keys-poisoned");
    let mint = real_mint(&dir.join("mint"));
    let listener = start_serving(&server, &key, &dir, mint.serve());
    let cfg = config(&server, &did, 10, 3);

    // A thread panics while it holds the mint: the lock is poisoned.
    let state = Arc::clone(&mint.state);
    let joined = thread::spawn(move || {
        let _held = state.logic.lock().unwrap();
        panic!("poisoning the mint lock on purpose");
    })
    .join();
    assert!(joined.is_err(), "the holder must have panicked");
    assert!(mint.state.logic.is_poisoned());

    // Every call that reaches the mint: keys, keysets and rotation take no body;
    // rollover and check-state are requests that parse and would reach the mint.
    let calls: [(u8, &[u8]); 5] = [
        (KIND_KEYS, &[]),
        (KIND_KEYSETS, &[]),
        (KIND_ROTATION, &[]),
        (KIND_ROLLOVER, &br#"{"inputs":[],"outputs":[]}"#[..]),
        (KIND_CHECK_STATE, &br#"{"nonces":[]}"#[..]),
    ];
    for (kind, body) in calls {
        let mut flight = if body.is_empty() {
            InFlight::of(kind)
        } else {
            InFlight::with_body(kind, body.to_vec())
        };
        match ask_any(&cfg, &mut flight) {
            Err(AskError::Verify(VerifyError::Refused(s))) => assert_eq!(s, STATUS_UNAVAILABLE),
            other => panic!("kind {kind}: expected a signed refusal, got {other:?}"),
        }
        // It came at once, from the first try, and was signed: the reply cell
        // holds a receipt that verifies from the DID.
        let sealed = peek(&server, &reply_cell(&did, &flight)).expect("a reply");
        let plain = addr::open(&flight.r, &sealed).unwrap();
        let receipt = verify_receipt(&did, &plain).unwrap();
        assert_eq!(receipt.status, STATUS_UNAVAILABLE, "kind {kind}");
        assert!(receipt.body.is_empty(), "kind {kind}");
    }
    // Mint info still says what is wrong.
    let info = ask(&cfg, &mut InFlight::new()).unwrap();
    assert_eq!(info.info.status, "mint_lock_poisoned");
    listener.stop().unwrap();
}

// ---------------------------------------------------------------- rollover and check-state

const T: u64 = 1_800_000_000;
const H: u64 = 3600;

fn zero_sig() -> secp256k1::schnorr::Signature {
    secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap()
}

impl RealMint {
    fn set_clock(&self, t: u64) {
        self.rot
            .rotor
            .lock()
            .unwrap()
            .clock
            .store(t, Ordering::SeqCst);
    }

    /// A note of `amount` of the current keyset, with the key that spends it.
    fn issue(&self, amount: Amount) -> (Note, Keypair) {
        let kp = Keypair::new(SECP256K1, &mut OsRng);
        let nonce = Nonce(kp.public_key());
        let (b, r) = bdhke::blind_message(&nonce.as_hash_preimage(), None);
        let logic = self.state.logic.lock().unwrap();
        let out = logic
            .issue(&MintOutput {
                amount,
                blind_nonce: BlindNonce(b),
            })
            .unwrap();
        let signature = bdhke::unblind_message(&out.0, &r, &logic.public_keys()[&amount]);
        (Note { nonce, signature }, kp)
    }

    /// `n` notes of one unit, then one rotation later: the notes are of the
    /// previous keyset. Returns the notes, the previous and the current keyset.
    fn rotated(&self, n: usize) -> (Vec<(Note, Keypair)>, String, String) {
        let notes: Vec<_> = (0..n).map(|_| self.issue(Amount::from_units(1))).collect();
        // The first rotation request records the first issue, which starts the clock.
        let started: RotationResponse =
            serde_json::from_slice(&self.served_body(KIND_ROTATION, b"")).unwrap();
        self.set_clock(T + H);
        let keysets: KeysetsResponse =
            serde_json::from_slice(&self.served_body(KIND_KEYSETS, b"")).unwrap();
        assert_eq!(
            keysets.previous.as_deref(),
            Some(started.current.id.as_str()),
            "one period on, the set the notes were issued under is the previous one"
        );
        assert_ne!(keysets.current, started.current.id);
        (notes, keysets.previous.unwrap(), keysets.current)
    }
}

/// A rollover of all of `notes` (one unit each) into as many fresh outputs.
fn rollover_of(notes: &[(Note, Keypair)], input_keyset: &str, keyset: &str) -> RolloverRequest {
    let one = Amount::from_units(1);
    let outputs: Vec<MintOutput> = notes
        .iter()
        .map(|_| {
            let kp = Keypair::new(SECP256K1, &mut OsRng);
            let (b, _) = bdhke::blind_message(&Nonce(kp.public_key()).as_hash_preimage(), None);
            MintOutput {
                amount: one,
                blind_nonce: BlindNonce(b),
            }
        })
        .collect();
    let unsigned: Vec<MintInput> = notes
        .iter()
        .map(|(n, _)| MintInput {
            amount: one,
            note: *n,
            spend_signature: zero_sig(),
        })
        .collect();
    let h = transaction_sighash(&unsigned, &outputs);
    let inputs = unsigned
        .into_iter()
        .zip(notes)
        .map(|(i, (_, k))| MintInput {
            spend_signature: SECP256K1.sign_schnorr(&h, k),
            ..i
        })
        .collect();
    RolloverRequest {
        input_keysets: vec![input_keyset.to_string(); notes.len()],
        inputs,
        keyset: keyset.to_string(),
        outputs,
    }
}

fn check_of(notes: &[(Note, Keypair)]) -> Vec<u8> {
    serde_json::to_vec(&CheckStateRequest {
        nonces: notes.iter().map(|(n, _)| n.nonce).collect(),
    })
    .unwrap()
}

fn json_bytes(value: &impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

/// Ask with a body, blocking, with the wallet's own flow.
fn ask_body(cfg: &AskConfig, kind: u8, body: &[u8]) -> Result<Verified, AskError> {
    ask_any(cfg, &mut InFlight::with_body(kind, body.to_vec()))
}

fn spent_of(v: &Verified) -> Vec<bool> {
    #[derive(serde::Deserialize)]
    struct Spent {
        spent: Vec<bool>,
    }
    v.json::<Spent>().unwrap().spent
}

#[test]
fn rollover_and_check_state_are_answered_and_the_rollover_is_applied_once() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("roll-real");
    let mint = real_mint(&dir.join("mint"));
    let listener = start_serving(&server, &key, &dir, mint.serve());
    let cfg = config(&server, &did, 10, 3);

    let (notes, prev, current) = mint.rotated(3);
    let check = check_of(&notes);

    // Before: nothing is spent. The AIR body is what the mint serves.
    let before = ask_body(&cfg, KIND_CHECK_STATE, &check).unwrap();
    assert_eq!(spent_of(&before), vec![false; 3]);
    mint.assert_served(KIND_CHECK_STATE, &check, before.body());
    assert_eq!(before.reply.status, STATUS_ANSWERED);
    verify_receipt(&did, &before.receipt).unwrap();

    // The rollover over AIR changes the mint's state.
    let request = rollover_of(&notes, &prev, &current);
    let body = json_bytes(&request);
    let over_air = ask_body(&cfg, KIND_ROLLOVER, &body).unwrap();
    assert_eq!(over_air.reply.status, STATUS_ANSWERED);
    let answer: RolloverResponse = over_air.json().unwrap();
    assert_eq!(answer.keyset, current);
    assert_eq!(answer.outcomes.len(), 3);
    verify_receipt(&did, &over_air.receipt).unwrap();

    // The same request handed straight to the mint is answered from the stored
    // answer: the same bytes. (It is also the proof that the AIR call was
    // applied once and stored by request hash.)
    mint.assert_served(KIND_ROLLOVER, &body, over_air.body());

    // After: the notes are spent, over AIR and as the mint serves it.
    let after = ask_body(&cfg, KIND_CHECK_STATE, &check).unwrap();
    assert_eq!(spent_of(&after), vec![true; 3]);
    mint.assert_served(KIND_CHECK_STATE, &check, after.body());
    listener.stop().unwrap();
}

#[test]
fn a_resent_rollover_is_answered_again_and_not_applied_twice() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("roll-resend");
    let mint = real_mint(&dir.join("mint"));
    let listener = start_serving(&server, &key, &dir, mint.serve());
    let cfg = config(&server, &did, 10, 3);
    let mint_pub = addr::did_to_key(&did).unwrap();

    let (notes, prev, current) = mint.rotated(2);
    let request = rollover_of(&notes, &prev, &current);
    let body = json_bytes(&request);

    let mut flight = InFlight::with_body(KIND_ROLLOVER, body.clone());
    let first = ask_any(&cfg, &mut flight).unwrap();
    assert_eq!(first.slots, vec![0]);
    wait_for_next(&dir, 1);
    let cell = reply_cell(&did, &flight);
    let stored = peek(&server, &cell).expect("the reply is in its cell");

    // 1. The wallet's retry after a lost reply: it reads the cell first, finds
    //    the reply, and writes no request at all.
    let again = ask_any(&cfg, &mut flight).unwrap();
    assert_eq!(again.attempts_used, 0);
    assert_eq!(again.body(), first.body());
    assert_eq!(flight.slots, vec![0], "nothing was written");

    // 2. The same request (same id, same key) reaches the mint a second time,
    //    in another slot: a copied cell, or a retry that raced the first reply.
    //    The mint serves it again, the rollover is the stored answer and not a
    //    second rollover, and the write-once cell keeps the first reply.
    let sealed = seal_to(&mint_pub, &flight.request().encode()).unwrap();
    assert!(matches!(
        put(&server, &mailbox_secret(&did, 1), sealed),
        client::Put::Created(_)
    ));
    wait_for_next(&dir, 2);
    assert_eq!(
        peek(&server, &cell).unwrap(),
        stored,
        "the first reply stays"
    );

    // 3. A new request id for the same notes and the same body (the wallet
    //    started over after a stale reply): the same answer, from the store.
    let fresh = ask_body(&cfg, KIND_ROLLOVER, &body).unwrap();
    assert_ne!(fresh.request_id, first.request_id);
    assert_eq!(
        fresh.body(),
        first.body(),
        "the stored answer, not a second rollover"
    );

    // The notes were spent once: the check says so, and a different rollover of
    // the same notes (new outputs) is refused as spent.
    let check = ask_body(&cfg, KIND_CHECK_STATE, &check_of(&notes)).unwrap();
    assert_eq!(spent_of(&check), vec![true; 2]);
    let other = json_bytes(&rollover_of(&notes, &prev, &current));
    let refused = ask_body(&cfg, KIND_ROLLOVER, &other).unwrap();
    assert!(refused.refused());
    let why: serde_json::Value = refused.json().unwrap();
    assert_eq!(why["code"], sp_wire::codes::NOTE_SPENT_ELSEWHERE);
    assert_eq!(why["spent"], serde_json::json!([0]));
    listener.stop().unwrap();
}

#[test]
fn refusals_are_signed_replies_with_the_mints_text() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("roll-refused");
    let limits = Limits {
        rollover_bytes: 4096,
        rollover_notes: 1,
        check_state_bytes: 4096,
        ..Limits::default()
    };
    let mint = real_mint_with(&dir.join("mint"), limits);
    let listener = start_serving(&server, &key, &dir, mint.serve());
    let cfg = config(&server, &did, 10, 3);
    let (notes, prev, current) = mint.rotated(2);

    // What AIR says to `body`, and what the mint serves: the same JSON, and the
    // AIR reply is signed and a receipt like any other.
    let same_as_served = |kind: u8, path: &str, body: &[u8], code: &str| -> serde_json::Value {
        let v = ask_body(&cfg, kind, body).unwrap();
        assert!(v.refused(), "{path}: expected a refusal");
        assert_eq!(v.reply.status, STATUS_REFUSED);
        let receipt = verify_receipt(&did, &v.receipt).unwrap();
        assert_eq!(receipt.status, STATUS_REFUSED);
        match mint.served(kind, body) {
            Some(Served::Refused(served)) => {
                assert_eq!(
                    v.body(),
                    &served[..],
                    "{path}: the refusal is the served body"
                )
            }
            _ => panic!("{path}: the mint served no refusal"),
        }
        let why: serde_json::Value = v.json().unwrap();
        assert_eq!(why["code"], code, "{why}");
        why
    };

    // More notes than the cap: a 400 `refused`, nothing is spent.
    let two = json_bytes(&rollover_of(&notes, &prev, &current));
    let why = same_as_served(KIND_ROLLOVER, "/rollover", &two, "refused");
    assert!(
        why["error"].as_str().unwrap().contains("1 to 1 notes"),
        "{why}"
    );

    // An unknown keyset: the keyset's own code.
    let one = &notes[..1];
    let nope = json_bytes(&rollover_of(one, &prev, "no-such-keyset"));
    same_as_served(
        KIND_ROLLOVER,
        "/rollover",
        &nope,
        sp_wire::codes::UNKNOWN_KEYSET,
    );

    // Over the body ceiling: the 413 text, with the sizes, before it is parsed.
    let big = vec![b' '; 4097];
    for (kind, path) in [
        (KIND_ROLLOVER, "/rollover"),
        (KIND_CHECK_STATE, "/check-state"),
    ] {
        let why = same_as_served(kind, path, &big, sp_wire::codes::REQUEST_TOO_LARGE);
        assert_eq!(why["size_bytes"], 4097);
        assert_eq!(why["limit_bytes"], 4096);
    }
    // At the ceiling it is read (and refused for being empty JSON, not for size).
    let at = vec![b' '; 4096];
    let v = ask_body(&cfg, KIND_CHECK_STATE, &at).unwrap();
    assert_eq!(
        v.json::<serde_json::Value>().unwrap()["code"],
        sp_wire::codes::MALFORMED_REQUEST
    );

    // Not a request at all: a signed refusal, `malformed_request`.
    for kind in [KIND_ROLLOVER, KIND_CHECK_STATE] {
        let v = ask_body(&cfg, kind, b"this is not json").unwrap();
        assert!(v.refused());
        let why: serde_json::Value = v.json().unwrap();
        assert_eq!(why["code"], sp_wire::codes::MALFORMED_REQUEST, "{why}");
    }

    // None of that spent anything.
    let v = ask_body(&cfg, KIND_CHECK_STATE, &check_of(&notes)).unwrap();
    assert_eq!(spent_of(&v), vec![false; 2]);
    // And one note is within the cap.
    let ok = ask_body(
        &cfg,
        KIND_ROLLOVER,
        &json_bytes(&rollover_of(one, &prev, &current)),
    )
    .unwrap();
    assert!(!ok.refused());
    listener.stop().unwrap();
}

/// The full size of a wallet's rollover: 1000 notes, the most one request may
/// carry. The request is about half a megabyte (the body length is 4 bytes, and
/// the ceiling is the mint's setting), the AIR server takes it as one record,
/// and the answer comes back whole.
#[test]
fn a_full_rollover_of_a_thousand_notes_goes_over_air() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("roll-1000");
    let mint = real_mint(&dir.join("mint"));
    let listener = start_serving(&server, &key, &dir, mint.serve());
    let cfg = config(&server, &did, 60, 1);

    let n = sp_wire::psp::MAX_ROLLOVER_NOTES;
    let (notes, prev, current) = mint.rotated(n);
    let body = json_bytes(&rollover_of(&notes, &prev, &current));
    assert!(
        body.len() > 300_000 && body.len() < Limits::default().rollover_bytes,
        "{} bytes: under the 1 MiB ceiling, and a real size",
        body.len()
    );

    let v = ask_body(&cfg, KIND_ROLLOVER, &body).unwrap();
    assert!(!v.refused(), "{}", String::from_utf8_lossy(v.body()));
    let answer: RolloverResponse = v.json().unwrap();
    assert_eq!(answer.keyset, current);
    assert_eq!(answer.outcomes.len(), n);
    assert_eq!(answer.keys.len(), n);
    verify_receipt(&did, &v.receipt).unwrap();

    // The same bytes handed straight to the mint: the stored answer, equal byte
    // for byte.
    mint.assert_served(KIND_ROLLOVER, &body, v.body());

    // And all 1000 inputs are spent: the check-state request is full size too.
    let check = check_of(&notes);
    let c = ask_body(&cfg, KIND_CHECK_STATE, &check).unwrap();
    assert_eq!(spent_of(&c), vec![true; n]);
    listener.stop().unwrap();
}
