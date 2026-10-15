//! The PSP's HTTP API, end to end: a real server on a local port, a note issued
//! by the mint and unblinded the way a wallet would, then burned over HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use mint_server::api::{
    AuditResponse, CheckStateRequest, CheckStateResponse, KeysResponse, MeltRequest, MeltResponse,
};
use mint_server::state::AppState;
use psp_mint::server::{Health, fatal};
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("psp-mint-api-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(name: &str) -> (String, Arc<AppState>) {
    let (base, state, _health) = start_with_health(name).await;
    (base, state)
}

async fn start_with_health(name: &str) -> (String, Arc<AppState>, Arc<Health>) {
    let logic = psp_mint::mint::open(&scratch(name), Amount::from_units(1024)).unwrap();
    let state = psp_mint::server::app_state(logic);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let health = Health::new(100);
    let app = psp_mint::server::router(Arc::clone(&state), Arc::clone(&health));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state, health)
}

/// What the mint does when a payment settles, and what the wallet does after.
fn issued_note(state: &AppState, amount: Amount) -> (Note, Keypair) {
    let keypair = Keypair::new(&SECP256K1, &mut OsRng);
    let nonce = Nonce(keypair.public_key());
    let (blind_point, blinding_key) = bdhke::blind_message(&nonce.as_hash_preimage(), None);
    let logic = state.logic.lock().unwrap();
    let outcome = logic
        .issue(&MintOutput {
            amount,
            blind_nonce: BlindNonce(blind_point),
        })
        .unwrap();
    let signature = bdhke::unblind_message(&outcome.0, &blinding_key, &logic.public_keys()[&amount]);
    (Note { nonce, signature }, keypair)
}

fn melt_input(note: Note, keypair: &Keypair, amount: Amount, memo: &str) -> MintInput {
    let unsigned = MintInput {
        amount,
        note,
        spend_signature: secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap(),
    };
    let sighash = melt_sighash(std::slice::from_ref(&unsigned), memo.as_bytes());
    MintInput {
        spend_signature: SECP256K1.sign_schnorr(&sighash, keypair),
        ..unsigned
    }
}

#[tokio::test]
async fn keys_lists_every_denomination() {
    let (base, _state) = start("keys").await;
    let keys: KeysResponse = reqwest::get(format!("{base}/keys"))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    // 1, 2, 4, ... 1024
    assert_eq!(keys.keys.len(), 11);
    assert!(keys.keys.contains_key(&Amount::from_units(1)));
    assert!(keys.keys.contains_key(&Amount::from_units(1024)));
    assert!(!keys.keys.contains_key(&Amount::from_units(3)));
}

#[tokio::test]
async fn a_note_can_be_burned_once_over_http() {
    let (base, state) = start("melt").await;
    let http = reqwest::Client::new();
    let amount = Amount::from_units(4);
    let (note, keypair) = issued_note(&state, amount);
    let memo = "esp-mint exchange test";
    let request = MeltRequest {
        inputs: vec![melt_input(note, &keypair, amount, memo)],
        memo: memo.to_string(),
    };

    let melted: MeltResponse = http
        .post(format!("{base}/melt"))
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(melted.burned, amount);

    let checked: CheckStateResponse = http
        .post(format!("{base}/check-state"))
        .json(&CheckStateRequest {
            nonces: vec![note.nonce],
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(checked.spent, vec![true]);

    let audit: AuditResponse = reqwest::get(format!("{base}/audit"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(audit.issued, amount);
    assert_eq!(audit.redeemed, amount);
    assert_eq!(audit.outstanding, Amount::ZERO);

    // the same request again is a double spend
    let again = http
        .post(format!("{base}/melt"))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_burn_is_bound_to_its_memo() {
    let (base, state) = start("memo").await;
    let http = reqwest::Client::new();
    let amount = Amount::from_units(2);
    let (note, keypair) = issued_note(&state, amount);
    // signed for one memo, presented with another
    let request = MeltRequest {
        inputs: vec![melt_input(note, &keypair, amount, "the signed memo")],
        memo: "a different memo".to_string(),
    };
    let response = http
        .post(format!("{base}/melt"))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let audit: AuditResponse = reqwest::get(format!("{base}/audit"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(audit.redeemed, Amount::ZERO);
}

#[tokio::test]
async fn a_note_signed_by_someone_else_cannot_be_burned() {
    let (base, state) = start("thief").await;
    let amount = Amount::from_units(2);
    let (note, _owner) = issued_note(&state, amount);
    let thief = Keypair::new(&SECP256K1, &mut OsRng);
    let request = MeltRequest {
        inputs: vec![melt_input(note, &thief, amount, "memo")],
        memo: "memo".to_string(),
    };
    let response = reqwest::Client::new()
        .post(format!("{base}/melt"))
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn issuing_and_swapping_are_not_exposed() {
    let (base, _state) = start("closed").await;
    let http = reqwest::Client::new();
    for path in ["/admin/issue", "/swap"] {
        let response = http
            .post(format!("{base}{path}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_api_works_while_the_mint_is_shared() {
    // The scanner holds the same mutex for short stretches. A request that
    // arrives while it is held waits and then succeeds.
    let (base, state) = start("shared").await;
    let held = Arc::clone(&state);
    let holder = std::thread::spawn(move || {
        let _guard = held.logic.lock().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    let response = reqwest::get(format!("{base}/audit")).await.unwrap();
    assert!(response.status().is_success());
    holder.join().unwrap();
}

async fn health_json(base: &str) -> (reqwest::StatusCode, serde_json::Value) {
    let response = reqwest::get(format!("{base}/health")).await.unwrap();
    let code = response.status();
    (code, response.json().await.unwrap())
}

#[tokio::test]
async fn health_reports_the_last_successful_scan() {
    let (base, _state, health) = start_with_health("health").await;

    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::OK);
    assert_eq!(body["status"], "starting");
    assert!(body["last_scan_unix"].is_null());

    health.record_scan();
    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["last_scan_unix"].as_u64(), health.last_scan());
}

#[tokio::test]
async fn health_fails_when_the_last_scan_is_too_old() {
    let (base, _state, health) = start_with_health("health-stale").await;
    health.record_scan_at(psp_mint::server::now_secs() - 500);
    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "scan_stale");
}

#[tokio::test]
async fn health_answers_while_the_mint_is_busy() {
    let (base, state, _health) = start_with_health("health-busy").await;
    let _guard = state.logic.lock().unwrap();
    let (code, _) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::OK);
}

fn poison(state: &Arc<AppState>) {
    let held = Arc::clone(state);
    let result = std::thread::spawn(move || {
        let _guard = held.logic.lock().unwrap();
        panic!("a handler died holding the mint");
    })
    .join();
    assert!(result.is_err());
}

#[tokio::test]
async fn a_poisoned_mint_lock_fails_health_and_is_fatal() {
    let (base, state, health) = start_with_health("poisoned").await;
    health.record_scan();
    assert_eq!(fatal(&state, false), None);

    poison(&state);

    assert_eq!(fatal(&state, false), Some("the mint lock is poisoned"));
    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "mint_lock_poisoned");
}

#[tokio::test]
async fn a_stopped_scan_thread_is_fatal() {
    let (_base, state, _health) = start_with_health("scan-dead").await;
    assert_eq!(fatal(&state, false), None);
    assert_eq!(fatal(&state, true), Some("the scan thread has stopped"));
}

#[tokio::test]
async fn a_panicking_scan_thread_shows_as_finished() {
    // What the watchdog in `serve` relies on: a thread that panics is finished.
    let handle = std::thread::spawn(|| panic!("scan blew up"));
    while !handle.is_finished() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(handle.join().is_err());
}
