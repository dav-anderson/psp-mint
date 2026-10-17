//! Body ceilings on the PSP's HTTP API: a body over a route's ceiling is
//! refused with `413` and a JSON body before it is parsed, and nothing is
//! burned.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mint_server::api::{CheckStateRequest, CheckStateResponse};
use mint_server::state::AppState;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash};
use psp_mint::melts::MeltLog;
use psp_mint::server::{BurnsRequest, Health, Limits};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1};

const MIB: usize = 1024 * 1024;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("psp-mint-limits-{}-{name}", {
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

async fn start(name: &str, limits: Option<Limits>) -> (String, Arc<AppState>) {
    let dir = scratch(name);
    let logic = psp_mint::mint::open(&dir, Amount::from_units(1024)).unwrap();
    let state = psp_mint::server::app_state(logic);
    let melts = Arc::new(Mutex::new(
        MeltLog::open(&dir.join("melt_memos.json")).unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let health = Health::new(100);
    let app = match limits {
        Some(limits) => psp_mint::server::router_with(Arc::clone(&state), health, melts, limits),
        None => psp_mint::server::router(Arc::clone(&state), health, melts),
    };
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

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

/// `json` followed by spaces, so that it is exactly `size` bytes and still the
/// same request.
fn padded(json: &impl serde::Serialize, size: usize) -> Vec<u8> {
    let mut body = serde_json::to_vec(json).unwrap();
    assert!(body.len() <= size);
    body.resize(size, b' ');
    body
}

async fn post(base: &str, path: &str, body: Vec<u8>) -> (u16, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

/// Send only the headers of a request that says it is `length` bytes, and read
/// the reply. A large body the server refuses is never written, so the server
/// closing early does not reset the connection under the test.
async fn declared(base: &str, path: &str, length: usize) -> (u16, serde_json::Value) {
    let addr = base.trim_start_matches("http://").to_string();
    let path = path.to_string();
    let reply = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(&addr).unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        reply
    })
    .await
    .unwrap();
    let status = reply[9..12].parse().unwrap();
    let body = reply.split("\r\n\r\n").nth(1).unwrap_or("");
    (
        status,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

fn spent(state: &AppState, nonce: Nonce) -> bool {
    state.logic.lock().unwrap().is_spent(nonce).unwrap()
}

#[tokio::test]
async fn a_melt_over_the_ceiling_is_refused_before_parsing_and_burns_nothing() {
    let (base, state) = start(
        "melt",
        Some(Limits {
            melt_bytes: 4096,
            ..Limits::default()
        }),
    )
    .await;
    let amount = Amount::from_units(4);
    let (note, keypair) = issued_note(&state, amount);
    let memo = "limits test";
    let request = psp_mint::server::PspMeltRequest {
        melt: sp_wire::psp::MeltRequest {
            inputs: vec![melt_input(note, &keypair, amount, memo)],
            memo: memo.to_string(),
        },
        keysets: vec![psp_mint::rotation::keyset_id(
            state.logic.lock().unwrap().public_keys(),
        )],
    };

    let (status, body) = post(&base, "/melt", padded(&request, 4097)).await;
    assert_eq!(status, 413);
    assert_eq!(body["code"], "request_too_large");
    assert_eq!(body["limit_bytes"], 4096);
    assert_eq!(body["size_bytes"], 4097);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("at most 4096 bytes")
    );
    assert!(!spent(&state, note.nonce));

    // The same request at exactly the ceiling goes through.
    let (status, _) = post(&base, "/melt", padded(&request, 4096)).await;
    assert_eq!(status, 200);
    assert!(spent(&state, note.nonce));
}

/// A body sent in chunks, with no `Content-Length`, is read only as far as the
/// ceiling.
#[tokio::test]
async fn a_chunked_body_over_the_ceiling_is_refused_too() {
    let (base, _state) = start(
        "chunked",
        Some(Limits {
            burns_bytes: 4096,
            ..Limits::default()
        }),
    )
    .await;
    let addr = base.trim_start_matches("http://").to_string();
    let reply = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(&addr).unwrap();
        let chunk = vec![b' '; 3000];
        let mut request = format!(
            "POST /burns HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        for _ in 0..2 {
            request.extend(format!("{:x}\r\n", chunk.len()).as_bytes());
            request.extend(&chunk);
            request.extend(b"\r\n");
        }
        request.extend(b"0\r\n\r\n");
        stream.write_all(&request).unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        reply
    })
    .await
    .unwrap();
    assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
    assert!(reply.contains("\"code\":\"request_too_large\""), "{reply}");
    assert!(reply.contains("\"size_bytes\":null"), "{reply}");
}

#[tokio::test]
async fn by_default_every_route_with_a_body_takes_up_to_one_mib() {
    let (base, state) = start("defaults", None).await;
    let (note, _) = issued_note(&state, Amount::from_units(1));
    let check = CheckStateRequest {
        nonces: vec![note.nonce],
    };
    let burns = BurnsRequest {
        nonces: vec![note.nonce],
    };

    let (status, body) = post(&base, "/check-state", padded(&check, MIB)).await;
    assert_eq!(status, 200, "{body}");
    let parsed: CheckStateResponse = serde_json::from_value(body).unwrap();
    assert_eq!(parsed.spent, vec![false]);
    let (status, body) = declared(&base, "/check-state", MIB + 1).await;
    assert_eq!(
        (status, body["limit_bytes"].as_u64()),
        (413, Some(MIB as u64))
    );

    let (status, _) = post(&base, "/burns", padded(&burns, MIB)).await;
    assert_eq!(status, 200);
    let (status, body) = declared(&base, "/burns", MIB + 1).await;
    assert_eq!(
        (status, body["code"].as_str()),
        (413, Some("request_too_large"))
    );

    let (status, body) = declared(&base, "/melt", MIB + 1).await;
    assert_eq!(
        (status, body["limit_bytes"].as_u64()),
        (413, Some(MIB as u64))
    );
}

/// A ceiling above axum's own 2 MB default is honoured, not cut short by it.
#[tokio::test]
async fn a_ceiling_above_two_megabytes_is_the_one_in_force() {
    let big = 3 * MIB;
    let limits = Limits {
        melt_bytes: big,
        burns_bytes: big,
        rollover_bytes: big,
        rollover_notes: 1000,
        check_state_bytes: big,
    };
    let (base, state) = start("big", Some(limits)).await;
    let (note, _) = issued_note(&state, Amount::from_units(1));
    let check = CheckStateRequest {
        nonces: vec![note.nonce],
    };
    let (status, body) = post(&base, "/check-state", padded(&check, big)).await;
    assert_eq!(status, 200, "{body}");
    let (status, _) = declared(&base, "/check-state", big + 1).await;
    assert_eq!(status, 413);
}

/// A refusal for a reason other than size keeps the usual error body.
#[tokio::test]
async fn under_the_ceiling_a_bad_melt_is_still_a_400_with_an_error() {
    let (base, state) = start("bad", None).await;
    let amount = Amount::from_units(2);
    let (note, keypair) = issued_note(&state, amount);
    let request = psp_mint::server::PspMeltRequest {
        melt: sp_wire::psp::MeltRequest {
            inputs: vec![melt_input(note, &keypair, amount, "signed memo")],
            memo: "a different memo".to_string(),
        },
        keysets: vec![psp_mint::rotation::keyset_id(
            state.logic.lock().unwrap().public_keys(),
        )],
    };
    let (status, body) = post(&base, "/melt", serde_json::to_vec(&request).unwrap()).await;
    assert_eq!(status, 400);
    assert!(body["error"].is_string());
    assert!(!spent(&state, note.nonce));
}
