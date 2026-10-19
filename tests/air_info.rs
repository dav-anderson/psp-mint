//! The AIR listener against a real `air_server` started in this process: a
//! wallet's mint-info request is answered, signed by the mint DID key, and
//! checked by the same code the wallet runs (`sp_wire::air_request::ask`).
//!
//! The AIR client always dials port 5702 (upstream item 12), so these tests
//! need that port free; `local_server::start` says so when it is not. One server
//! serves every test of this binary, and each test has its own mint key, so no
//! two tests ever touch the same cell.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use psp_mint::air_listener::{self, NEXT_FILE, Settings};
use psp_mint::server::Health;
use sp_wire::air_request::addr::{self, mailbox_secret, reply_secret, seal_to, shared_wallet};
use sp_wire::air_request::ask::{AskConfig, AskError, InFlight, Outcome, ask_mint_info};
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::envelope::{
    STATUS_ANSWERED, WireRequest, build_reply, build_reply_naming,
};
use sp_wire::air_request::{KIND_MINT_INFO, MintInfo, VerifyError, local_server, verify_receipt};
use sp_wire::seal::SecretKey;
use sp_wire::{ServiceDid, WIRE_VERSION};

const NETWORK: &str = "regtest";

fn server() -> ServerRef {
    local_server::start().expect("a local AIR server (is port 5702 free?)")
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

fn start_listener(server: &ServerRef, key: &SecretKey, dir: &PathBuf, info: Info) -> Listener {
    let settings = Settings {
        server: server.clone(),
        held_reads: 16,
        timeout_secs: 10,
        state_dir: dir.clone(),
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
            runtime.block_on(air_listener::run(&settings, &key, &*info, async move {
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
        mint: did.clone(),
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

fn wait_for_next(dir: &PathBuf, want: u32) {
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
    thread::sleep(Duration::from_secs(4));
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

/// What a bad reply is checked against: a fresh request that is already
/// "answered" with `plain` (sealed to the request key), asked through the
/// wallet's flow.
fn ask_with_planted_reply(
    server: &ServerRef,
    did: &ServiceDid,
    network: &str,
    make: impl FnOnce(&InFlight) -> Vec<u8>,
) -> Result<Outcome, AskError> {
    let flight = InFlight::new();
    let plain = make(&flight);
    let sealed = seal_to(&flight.r.public_key(), &plain).unwrap();
    assert!(matches!(
        put(server, &reply_cell(did, &flight), sealed),
        client::Put::Created(_)
    ));
    let mut cfg = config(server, did, 3, 1);
    cfg.network = network.to_string();
    let mut flight = flight;
    ask(&cfg, &mut flight)
}

fn info_body(network: &str) -> Vec<u8> {
    serde_json::to_vec(&MintInfo {
        status: "ok".into(),
        wire_version: WIRE_VERSION,
        network: network.into(),
    })
    .unwrap()
}

#[test]
fn a_tampered_or_forged_reply_is_rejected() {
    let server = server();
    let (key, did) = mint_key();
    let (other, _) = mint_key();
    let now = addr::now_secs();

    // The control: an honest reply, built here, is accepted.
    let honest = |f: &InFlight| {
        build_reply(
            &key,
            &f.request(),
            STATUS_ANSWERED,
            now,
            &info_body(NETWORK),
        )
    };
    let out = ask_with_planted_reply(&server, &did, NETWORK, honest).unwrap();
    assert_eq!(out.info.status, "ok");
    assert_eq!(out.attempts_used, 0);

    // One byte of the reply changed after it was signed.
    let tampered = |f: &InFlight| {
        let mut plain = honest(f);
        plain[100] ^= 1;
        plain
    };
    assert!(matches!(
        ask_with_planted_reply(&server, &did, NETWORK, tampered),
        Err(AskError::Verify(VerifyError::BadSignature))
    ));

    // Signed by another key, naming that key's DID.
    let other_signer = |f: &InFlight| {
        build_reply(
            &other,
            &f.request(),
            STATUS_ANSWERED,
            now,
            &info_body(NETWORK),
        )
    };
    assert!(matches!(
        ask_with_planted_reply(&server, &did, NETWORK, other_signer),
        Err(AskError::Verify(VerifyError::WrongMint))
    ));

    // Signed by another key, naming the right DID.
    let forged = |f: &InFlight| {
        build_reply_naming(
            &other,
            did.as_bytes(),
            &f.request(),
            STATUS_ANSWERED,
            now,
            &info_body(NETWORK),
        )
    };
    assert!(matches!(
        ask_with_planted_reply(&server, &did, NETWORK, forged),
        Err(AskError::Verify(VerifyError::BadSignature))
    ));

    // A genuine reply to a different request.
    let other_request = |_: &InFlight| {
        let request = WireRequest::mint_info(
            addr::random_id(),
            addr::key_to_bytes(&SecretKey::new().public_key()),
        );
        build_reply(&key, &request, STATUS_ANSWERED, now, &info_body(NETWORK))
    };
    assert!(matches!(
        ask_with_planted_reply(&server, &did, NETWORK, other_request),
        Err(AskError::Verify(VerifyError::WrongRequestId))
    ));

    // A reply signed long ago (a replay).
    let old = |f: &InFlight| {
        build_reply(
            &key,
            &f.request(),
            STATUS_ANSWERED,
            now - 10_000,
            &info_body(NETWORK),
        )
    };
    assert!(matches!(
        ask_with_planted_reply(&server, &did, NETWORK, old),
        Err(AskError::Verify(VerifyError::Stale { .. }))
    ));

    // The mint is on another network than the wallet.
    assert!(matches!(
        ask_with_planted_reply(&server, &did, "mainnet", honest),
        Err(AskError::Verify(VerifyError::Network { .. }))
    ));

    // The reply cell holds bytes that do not open with the request key.
    let flight = InFlight::new();
    assert!(matches!(
        put(&server, &reply_cell(&did, &flight), addr::random_bytes(200)),
        client::Put::Created(_)
    ));
    let mut flight = flight;
    assert!(matches!(
        ask(&config(&server, &did, 3, 1), &mut flight),
        Err(AskError::NotOpenable(_))
    ));
}

#[test]
fn a_wallet_on_another_network_rejects_a_genuine_reply() {
    let server = server();
    let (key, did) = mint_key();
    let dir = scratch("network");
    let listener = start_listener(&server, &key, &dir, fixed_info("ok"));
    let mut cfg = config(&server, &did, 10, 3);
    cfg.network = "mainnet".into();
    match ask(&cfg, &mut InFlight::new()) {
        Err(AskError::Verify(VerifyError::Network { theirs, ours })) => {
            assert_eq!(theirs, NETWORK);
            assert_eq!(ours, "mainnet");
        }
        other => panic!("expected a network mismatch, got {other:?}"),
    }
    listener.stop().unwrap();
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
