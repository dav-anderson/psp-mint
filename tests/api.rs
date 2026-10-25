//! The PSP's API, end to end: a real server on a local port, a note issued by
//! the mint and unblinded the way a wallet would, then burned over HTTP (the
//! SPI's `/melt`, `/burns`, `/audit`). What the wallet asks (keys, check-state)
//! goes over AIR, through the mint's real listener on a real `air_server` (see
//! `common`).

mod common;

use std::sync::Arc;

use mint_core::state::AppState;
use mint_types::{
    Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash, transaction_sighash,
};
use psp_mint::server::{AuditResponse, Burn, Limits, fatal};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1};
use sp_wire::keys::{CheckStateRequest, CheckStateResponse};
use sp_wire::melt::{MeltRequest, MeltResponse};

use common::{PspMint, burns};

/// The mint's clock: after it, a mint that has issued would be an hour from its
/// first rotation.
const CLOCK: u64 = 1_800_000_000;

/// The melt with every input's keyset named (the mint's one keyset).
fn ks(state: &AppState, melt: MeltRequest) -> psp_mint::server::PspMeltRequest {
    let id = psp_mint::rotation::keyset_id(state.logic.lock().unwrap().public_keys());
    psp_mint::server::PspMeltRequest {
        keysets: vec![id; melt.inputs.len()],
        melt: sp_wire::melt::MeltRequest {
            inputs: melt.inputs,
            memo: melt.memo,
        },
    }
}

/// The mint as `serve` builds it, with its local HTTP API on a port of its own.
async fn start(name: &str) -> (String, PspMint) {
    let mint = PspMint::open(name, CLOCK, Limits::default());
    let base = mint.http().await;
    (base, mint)
}

/// That a melt was refused with `400` and this message, which `mint-core` gives.
async fn assert_refused(response: reqwest::Response, why: &str) {
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"], why, "{body}");
}

const SPENT: &str = "The note is already spent";
const NOT_AUTHORIZED: &str =
    "The spend signature does not authorize this note for this transaction";

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
async fn a_note_can_be_burned_once_over_http() {
    let (base, mint) = start("melt").await;
    let air = mint.air();
    let state = &mint.state;
    let http = reqwest::Client::new();
    let amount = Amount::from_units(4);
    let (note, keypair) = issued_note(state, amount);
    let memo = "esp-mint exchange test";
    let request = ks(
        state,
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

    let checked: CheckStateResponse = air.post_ok(
        "/check-state",
        &CheckStateRequest {
            nonces: vec![note.nonce],
        },
    );
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
    assert_refused(again, SPENT).await;
    // and the refusal changed nothing
    let audit: AuditResponse = reqwest::get(format!("{base}/audit"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(audit.redeemed, amount);
}

#[tokio::test]
async fn a_burn_is_bound_to_its_memo() {
    let (base, mint) = start("memo").await;
    let state = Arc::clone(&mint.state);
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
    assert_refused(response, NOT_AUTHORIZED).await;
    let audit: AuditResponse = reqwest::get(format!("{base}/audit"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(audit.redeemed, Amount::ZERO);
    assert_eq!(
        burns(&base, vec![note.nonce]).await,
        vec![Burn {
            spent: false,
            memo: None
        }]
    );
}

#[tokio::test]
async fn a_burn_keeps_its_memo_and_burns_reports_it() {
    let (base, mint) = start("burn-memo").await;
    let state = Arc::clone(&mint.state);
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
    let (base, mint) = start("burn-failed").await;
    let state = Arc::clone(&mint.state);
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
    assert_refused(response, NOT_AUTHORIZED).await;
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
    let (base, mint) = start("burn-relabel").await;
    let state = Arc::clone(&mint.state);
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
        if expect_ok {
            assert!(response.status().is_success(), "{memo}");
        } else {
            assert_refused(response, SPENT).await;
        }
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
    // A rollover burns its inputs without a melt memo, so this is the state of a
    // note that was spent some way other than through /melt.
    let (base, mint) = start("burn-unrecorded").await;
    let state = Arc::clone(&mint.state);
    let amount = Amount::from_units(2);
    let (note, keypair) = issued_note(&state, amount);
    let keyset = psp_mint::rotation::keyset_id(state.logic.lock().unwrap().public_keys());
    let blinded = Keypair::new(SECP256K1, &mut OsRng);
    let (blind_point, _) =
        bdhke::blind_message(&Nonce(blinded.public_key()).as_hash_preimage(), None);
    let outputs = vec![MintOutput {
        amount,
        blind_nonce: BlindNonce(blind_point),
    }];
    let unsigned = MintInput {
        amount,
        note,
        spend_signature: secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap(),
    };
    let sighash = transaction_sighash(std::slice::from_ref(&unsigned), &outputs);
    let input = MintInput {
        spend_signature: SECP256K1.sign_schnorr(&sighash, &keypair),
        ..unsigned
    };
    {
        let air = mint.air();
        let (status, body) = air.post_json(
            "/rollover",
            &psp_mint::rotation::RolloverRequest {
                input_keysets: vec![keyset.clone()],
                inputs: vec![input],
                keyset,
                outputs,
            },
        );
        assert_eq!(status, 200, "{body}");
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
    let (base, mint) = start("burn-many").await;
    let state = Arc::clone(&mint.state);
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

/// Only `/melt`, `/burns`, `/audit` and `/health` are HTTP routes, on the router
/// `serve` builds. Issuing and swapping are not mounted, and the calls wallets
/// make are answered over AIR only.
#[tokio::test]
async fn only_melt_burns_audit_and_health_are_http_routes() {
    let (base, _mint) = start("routes").await;
    let http = reqwest::Client::new();
    for path in [
        "/admin/issue",
        "/swap",
        "/keys",
        "/keysets",
        "/rotation",
        "/rollover",
        "/check-state",
        "/spend",
        "/reconcile",
    ] {
        for method in [reqwest::Method::GET, reqwest::Method::POST] {
            let response = http
                .request(method.clone(), format!("{base}{path}"))
                .json(&serde_json::json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND,
                "{method} {path}"
            );
        }
    }
    // the four that are routes answer, though not always with a success: a
    // missing body is refused, a wrong method is not allowed, and neither is a 404
    for (method, path) in [
        (reqwest::Method::GET, "/audit"),
        (reqwest::Method::GET, "/health"),
        (reqwest::Method::POST, "/melt"),
        (reqwest::Method::POST, "/burns"),
        (reqwest::Method::GET, "/melt"),
        (reqwest::Method::GET, "/burns"),
        (reqwest::Method::POST, "/audit"),
        (reqwest::Method::POST, "/health"),
    ] {
        let response = http
            .request(method.clone(), format!("{base}{path}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND,
            "{method} {path}"
        );
    }
    let allowed = |response: &reqwest::Response| response.status().is_success();
    assert!(allowed(
        &reqwest::get(format!("{base}/audit")).await.unwrap()
    ));
    assert!(allowed(
        &reqwest::get(format!("{base}/health")).await.unwrap()
    ));
    for (method, path) in [
        (reqwest::Method::GET, "/melt"),
        (reqwest::Method::POST, "/audit"),
    ] {
        let response = http
            .request(method.clone(), format!("{base}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}"
        );
    }
}

async fn health_json(base: &str) -> (reqwest::StatusCode, serde_json::Value) {
    let response = reqwest::get(format!("{base}/health")).await.unwrap();
    let code = response.status();
    (code, response.json().await.unwrap())
}

#[tokio::test]
async fn health_reports_the_last_successful_scan() {
    let (base, mint) = start("health").await;
    let health = &mint.health;

    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::OK);
    assert_eq!(body["status"], "starting");
    assert!(body["last_scan_unix"].is_null());
    assert_eq!(body["wire_version"], sp_wire::WIRE_VERSION);
    assert_eq!(body["network"], common::NETWORK);
    // What `spi upgrade` checks after a restart: the build that answers.
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    health.record_scan();
    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["last_scan_unix"].as_u64(), health.last_scan());
}

#[tokio::test]
async fn health_fails_when_the_last_scan_is_too_old() {
    let (base, mint) = start("health-stale").await;
    mint.health
        .record_scan_at(psp_mint::server::now_secs() - 500);
    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "scan_stale");
    assert_eq!(body["stale_after_secs"], 100);
}

#[tokio::test]
async fn a_new_health_allowance_applies_to_the_next_health_request() {
    // A reload of scan.health_intervals or scan.health_grace_secs changes this
    // allowance in the running mint (psp-mint's `SIGHUP`).
    let (base, mint) = start("health-allowance").await;
    mint.health
        .record_scan_at(psp_mint::server::now_secs() - 500);
    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["stale_after_secs"], 100);

    mint.health.set_stale_after_secs(1_000);
    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["stale_after_secs"], 1_000);
}

#[tokio::test]
async fn health_answers_while_the_mint_is_busy() {
    let (base, mint) = start("health-busy").await;
    // The mint lock is held on another thread for the whole request: a guard
    // kept in the async test itself would be held across the await.
    let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let busy = Arc::clone(&mint.state);
    let holder = std::thread::spawn(move || {
        let _guard = busy.logic.lock().unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
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
    assert!(state.logic.is_poisoned());
}

#[tokio::test]
async fn a_poisoned_mint_lock_fails_health_and_is_fatal() {
    let (base, mint) = start("poisoned").await;
    mint.health.record_scan();
    assert_eq!(fatal(&mint.state, false), None);

    poison(&mint.state);

    assert_eq!(fatal(&mint.state, false), Some("the mint lock is poisoned"));
    let (code, body) = health_json(&base).await;
    assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "mint_lock_poisoned");
}

#[tokio::test]
async fn a_stopped_scan_thread_is_fatal() {
    let (_base, mint) = start("scan-dead").await;
    assert_eq!(fatal(&mint.state, false), None);
    assert_eq!(
        fatal(&mint.state, true),
        Some("the scan thread has stopped")
    );
}
