//! The PSP's HTTP API, end to end: a real server on a local port, a note issued
//! by the mint and unblinded the way a wallet would, then burned over HTTP.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mint_server::api::{
    AuditResponse, CheckStateRequest, CheckStateResponse, KeysResponse, MeltRequest, MeltResponse,
};
use mint_server::state::AppState;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash};
use psp_mint::melts::MeltLog;
use psp_mint::server::{Burn, BurnsRequest, BurnsResponse, Health, fatal};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("psp-mint-api-{}-{name}", {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The melt with every input's keyset named (the mint's one keyset).
fn ks(state: &AppState, melt: MeltRequest) -> psp_mint::server::PspMeltRequest {
    let id = psp_mint::rotation::keyset_id(state.logic.lock().unwrap().public_keys());
    psp_mint::server::PspMeltRequest {
        keysets: vec![id; melt.inputs.len()],
        melt: sp_wire::psp::MeltRequest {
            inputs: melt.inputs,
            memo: melt.memo,
        },
    }
}

async fn start(name: &str) -> (String, Arc<AppState>) {
    let (base, state, _health) = start_with_health(name).await;
    (base, state)
}

async fn start_with_health(name: &str) -> (String, Arc<AppState>, Arc<Health>) {
    let dir = scratch(name);
    let logic = psp_mint::mint::open(&dir, Amount::from_units(1024)).unwrap();
    let state = psp_mint::server::app_state(logic);
    let melts = Arc::new(Mutex::new(
        MeltLog::open(&dir.join("melt_memos.json")).unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let health = Health::new(100);
    let app = psp_mint::server::router(Arc::clone(&state), Arc::clone(&health), melts);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state, health)
}

/// What the mint does when a payment settles, and what the wallet does after.
fn issued_note(state: &AppState, amount: Amount) -> (Note, Keypair) {
    let keypair = Keypair::new(SECP256K1, &mut OsRng);
    let nonce = Nonce(keypair.public_key());
    let (blind_point, blinding_key) = bdhke::blind_message(&nonce.as_hash_preimage(), None);
    let logic = state.logic.lock().unwrap();
    let outcome = logic
        .issue(&MintOutput {
            amount,
            blind_nonce: BlindNonce(blind_point),
        })
        .unwrap();
    let signature =
        bdhke::unblind_message(&outcome.0, &blinding_key, &logic.public_keys()[&amount]);
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
    let request = ks(
        &state,
        MeltRequest {
            inputs: vec![melt_input(note, &keypair, amount, memo)],
            memo: memo.to_string(),
        },
    );

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
    let request = ks(
        &state,
        MeltRequest {
            inputs: vec![melt_input(note, &keypair, amount, "the signed memo")],
            memo: "a different memo".to_string(),
        },
    );
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

async fn burns(base: &str, nonces: Vec<Nonce>) -> Vec<Burn> {
    let response: BurnsResponse = reqwest::Client::new()
        .post(format!("{base}/burns"))
        .json(&BurnsRequest { nonces })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    response.burns
}

#[tokio::test]
async fn a_burn_keeps_its_memo_and_burns_reports_it() {
    let (base, state) = start("burn-memo").await;
    let http = reqwest::Client::new();
    let amount = Amount::from_units(4);
    let (spent_note, spent_key) = issued_note(&state, amount);
    let (fresh_note, _fresh_key) = issued_note(&state, amount);
    let memo = "the exchange memo";
    http.post(format!("{base}/melt"))
        .json(&ks(
            &state,
            MeltRequest {
                inputs: vec![melt_input(spent_note, &spent_key, amount, memo)],
                memo: memo.to_string(),
            },
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let reported = burns(&base, vec![spent_note.nonce, fresh_note.nonce]).await;
    assert_eq!(
        reported,
        vec![
            Burn {
                spent: true,
                memo: Some(memo.to_string())
            },
            Burn {
                spent: false,
                memo: None
            },
        ]
    );
}

#[tokio::test]
async fn a_burn_that_fails_leaves_the_note_unspent_and_without_a_memo() {
    let (base, state) = start("burn-failed").await;
    let amount = Amount::from_units(2);
    let (note, _owner) = issued_note(&state, amount);
    let thief = Keypair::new(SECP256K1, &mut OsRng);
    let response = reqwest::Client::new()
        .post(format!("{base}/melt"))
        .json(&ks(
            &state,
            MeltRequest {
                inputs: vec![melt_input(note, &thief, amount, "thief memo")],
                memo: "thief memo".to_string(),
            },
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        burns(&base, vec![note.nonce]).await,
        vec![Burn {
            spent: false,
            memo: None
        }]
    );
}

#[tokio::test]
async fn a_second_burn_does_not_change_the_memo_on_record() {
    // A holder tries to burn a note under a second memo after the first burn.
    // The second is a double spend, and the memo on record stays the first.
    let (base, state) = start("burn-relabel").await;
    let http = reqwest::Client::new();
    let amount = Amount::from_units(2);
    let (note, keypair) = issued_note(&state, amount);
    for (memo, expect_ok) in [("first", true), ("second", false)] {
        let response = http
            .post(format!("{base}/melt"))
            .json(&ks(
                &state,
                MeltRequest {
                    inputs: vec![melt_input(note, &keypair, amount, memo)],
                    memo: memo.to_string(),
                },
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().is_success(), expect_ok, "{memo}");
    }
    assert_eq!(
        burns(&base, vec![note.nonce]).await,
        vec![Burn {
            spent: true,
            memo: Some("first".to_string())
        }]
    );
}

#[tokio::test]
async fn a_note_spent_with_no_memo_on_record_reports_spent_and_no_memo() {
    // Spent some other way than through /melt, such as before the log existed.
    let (base, state) = start("burn-unrecorded").await;
    let amount = Amount::from_units(2);
    let (note, keypair) = issued_note(&state, amount);
    {
        let logic = state.logic.lock().unwrap();
        logic
            .melt(&[melt_input(note, &keypair, amount, "direct")], b"direct")
            .unwrap();
    }
    assert_eq!(
        burns(&base, vec![note.nonce]).await,
        vec![Burn {
            spent: true,
            memo: None
        }]
    );
}

#[tokio::test]
async fn every_note_in_one_burn_is_recorded_under_the_one_memo() {
    let (base, state) = start("burn-many").await;
    let amount = Amount::from_units(2);
    let (a, ka) = issued_note(&state, amount);
    let (b, kb) = issued_note(&state, amount);
    let memo = "two notes";
    // both inputs sign over the whole input list, so sign the pair together
    let unsigned = |note: Note| MintInput {
        amount,
        note,
        spend_signature: secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap(),
    };
    let mut inputs = vec![unsigned(a), unsigned(b)];
    let sighash = melt_sighash(&inputs, memo.as_bytes());
    inputs[0].spend_signature = SECP256K1.sign_schnorr(&sighash, &ka);
    inputs[1].spend_signature = SECP256K1.sign_schnorr(&sighash, &kb);
    reqwest::Client::new()
        .post(format!("{base}/melt"))
        .json(&ks(
            &state,
            MeltRequest {
                inputs,
                memo: memo.to_string(),
            },
        ))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let reported = burns(&base, vec![a.nonce, b.nonce]).await;
    assert!(
        reported
            .iter()
            .all(|r| r.spent && r.memo.as_deref() == Some(memo))
    );
}

#[tokio::test]
async fn one_spent_note_among_several_leaves_the_others_unburned() {
    // mint-core's melt checks every input before it burns any; this holds the
    // PSP to that: a melt of three notes, one already spent, burns nothing.
    let (base, state) = start("burn-partial").await;
    let http = reqwest::Client::new();
    let amount = Amount::from_units(2);
    let (a, ka) = issued_note(&state, amount);
    let (b, kb) = issued_note(&state, amount);
    let (c, kc) = issued_note(&state, amount);
    let first = http
        .post(format!("{base}/melt"))
        .json(&ks(
            &state,
            MeltRequest {
                inputs: vec![melt_input(a, &ka, amount, "first")],
                memo: "first".to_string(),
            },
        ))
        .send()
        .await
        .unwrap();
    assert!(first.status().is_success());
    let memo = "all three";
    let unsigned = |note: Note| MintInput {
        amount,
        note,
        spend_signature: secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap(),
    };
    let mut inputs = vec![unsigned(b), unsigned(a), unsigned(c)];
    let sighash = melt_sighash(&inputs, memo.as_bytes());
    inputs[0].spend_signature = SECP256K1.sign_schnorr(&sighash, &kb);
    inputs[1].spend_signature = SECP256K1.sign_schnorr(&sighash, &ka);
    inputs[2].spend_signature = SECP256K1.sign_schnorr(&sighash, &kc);
    let all = http
        .post(format!("{base}/melt"))
        .json(&ks(
            &state,
            MeltRequest {
                inputs,
                memo: memo.to_string(),
            },
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(all.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        burns(&base, vec![a.nonce, b.nonce, c.nonce]).await,
        vec![
            Burn {
                spent: true,
                memo: Some("first".to_string())
            },
            Burn {
                spent: false,
                memo: None
            },
            Burn {
                spent: false,
                memo: None
            },
        ],
        "only the first, separate burn happened"
    );
}

#[tokio::test]
async fn a_note_signed_by_someone_else_cannot_be_burned() {
    let (base, state) = start("thief").await;
    let amount = Amount::from_units(2);
    let (note, _owner) = issued_note(&state, amount);
    let thief = Keypair::new(SECP256K1, &mut OsRng);
    let request = ks(
        &state,
        MeltRequest {
            inputs: vec![melt_input(note, &thief, amount, "memo")],
            memo: "memo".to_string(),
        },
    );
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
    assert_eq!(body["wire_version"], sp_wire::WIRE_VERSION);

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
    // The mint lock is held on another thread for the whole request: a guard
    // kept in the async test itself would be held across the await.
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let busy = Arc::clone(&state);
    let holder = std::thread::spawn(move || {
        let _guard = busy.logic.lock().unwrap();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    held_rx.recv().unwrap();
    let (code, _) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::OK);
    release_tx.send(()).unwrap();
    holder.join().unwrap();
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
