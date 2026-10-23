//! A snapshot of a mint that has issued and burned notes restores to a mint whose
//! `/audit` is the same, and that remembers which notes are spent.
//!
//! This is what `SIGUSR1` does in `serve` (`main.rs`), minus the signal: the
//! mint's stores are dropped, `snapshot::take` copies the folder, and a mint is
//! opened on the copy. `tests/serve_process.rs` does it on the real process.

mod common;

use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash};
use psp_mint::server::{Limits, PspMeltRequest};
use psp_mint::snapshot;
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1};
use serde_json::Value;

use common::PspMint;

const CLOCK: u64 = 1_800_000_000;

fn issued_note(mint: &PspMint, amount: Amount) -> (Note, Keypair) {
    let keypair = Keypair::new(SECP256K1, &mut OsRng);
    let nonce = Nonce(keypair.public_key());
    let (blind_point, blinding_key) = bdhke::blind_message(&nonce.as_hash_preimage(), None);
    let logic = mint.state.logic.lock().unwrap();
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

fn melt_request(mint: &PspMint, note: Note, keypair: &Keypair, amount: Amount) -> PspMeltRequest {
    let memo = "snapshot test";
    let unsigned = MintInput {
        amount,
        note,
        spend_signature: secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap(),
    };
    let sighash = melt_sighash(std::slice::from_ref(&unsigned), memo.as_bytes());
    let input = MintInput {
        spend_signature: SECP256K1.sign_schnorr(&sighash, keypair),
        ..unsigned
    };
    let id = psp_mint::rotation::keyset_id(mint.state.logic.lock().unwrap().public_keys());
    PspMeltRequest {
        keysets: vec![id],
        melt: sp_wire::melt::MeltRequest {
            inputs: vec![input],
            memo: memo.to_string(),
        },
    }
}

/// Serve the mint's local API on a runtime of its own for the length of `ask`,
/// and drop the runtime (and with it every task and every handle on the mint)
/// before returning.
fn with_api<T>(mint: &PspMint, ask: impl FnOnce(&str) -> T) -> T {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let router = mint.router();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    runtime.spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    let answer = ask(&base);
    drop(runtime);
    answer
}

fn audit(base: &str) -> Value {
    reqwest::blocking::get(format!("{base}/audit"))
        .unwrap()
        .json()
        .unwrap()
}

#[test]
fn a_snapshot_restores_to_a_mint_with_the_same_audit_and_the_same_spent_notes() {
    let mint = PspMint::open("snapshot-restore", CLOCK, Limits::default());
    let dir = mint.dir.clone();
    let four = Amount::from_units(4);
    let two = Amount::from_units(2);
    let (spent, spent_key) = issued_note(&mint, four);
    let (_kept, _) = issued_note(&mint, two);
    let melt = melt_request(&mint, spent, &spent_key, four);

    let before = with_api(&mint, |base| {
        let client = reqwest::blocking::Client::new();
        let burned = client
            .post(format!("{base}/melt"))
            .json(&melt)
            .send()
            .unwrap();
        assert!(burned.status().is_success(), "{:?}", burned.text());
        audit(base)
    });
    assert_eq!(before["issued"], serde_json::json!(Amount::from_units(6)));
    assert_eq!(before["redeemed"], serde_json::json!(four));

    // What `serve` does: every store dropped, then the copy.
    drop(mint);
    let taken = snapshot::take(&dir, 1_700_000_000).unwrap();
    assert_eq!(taken, dir.join("snapshots").join("20231114T221320Z"));

    // A mint opened on the copy: the same audit, and the burned note still burned.
    let restored = PspMint::open_in(&taken, 1, 1, CLOCK, Limits::default());
    let after = with_api(&restored, |base| {
        let client = reqwest::blocking::Client::new();
        let again = client
            .post(format!("{base}/melt"))
            .json(&melt)
            .send()
            .unwrap();
        assert_eq!(again.status(), reqwest::StatusCode::BAD_REQUEST);
        let body: Value = again.json().unwrap();
        assert_eq!(body["error"], "The note is already spent", "{body}");
        audit(base)
    });
    assert_eq!(before, after);
}
