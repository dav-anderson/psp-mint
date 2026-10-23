//! Body ceilings: a body over a route's ceiling is refused with the `413` JSON
//! body (`request_too_large`) before it is parsed, and nothing is burned.
//!
//! `/melt` and `/burns` are the SPI's, over HTTP, where the refusal is a `413`.
//! The wallet's check-state is over AIR, through the mint's real listener on a
//! real `air_server` (see `common`), where the same body comes back as a signed
//! refusal. The ceiling is tried over AIR at 4 KiB; the 1 MiB default is
//! tried by requests answered under it and refused over it.

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;

use mint_server::api::{CheckStateRequest, CheckStateResponse};
use mint_server::state::AppState;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash};
use psp_mint::server::{BurnsRequest, Limits};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1};

const MIB: usize = 1024 * 1024;

const T: u64 = 1_800_000_000;

/// A mint wired as `serve` wires it, with its HTTP server for `/melt` and
/// `/burns`; the AIR listener is `mint.air()`.
async fn start(name: &str, limits: Option<Limits>) -> (String, common::PspMint) {
    let mint = common::PspMint::open(name, T, limits.unwrap_or_default());
    let base = mint.http().await;
    (base, mint)
}

/// What the server sent before it closed. A server that refuses a large body and
/// closes with part of it unread may reset the connection; the reply was sent
/// first, so a reset after some bytes is not a failure, and the tests check
/// what was received. Any other error, or a reset with nothing received, is.
fn read_reply(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    match stream.read_to_end(&mut bytes) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::ConnectionReset && !bytes.is_empty() => {}
        Err(e) => panic!("the reply could not be read: {e}"),
    }
    String::from_utf8(bytes).unwrap()
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
        let mut stream = TcpStream::connect(&addr).unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        read_reply(&mut stream)
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
    let (base, m) = start(
        "melt",
        Some(Limits {
            melt_bytes: 4096,
            ..Limits::default()
        }),
    )
    .await;
    let amount = Amount::from_units(4);
    let (note, keypair) = issued_note(&m.state, amount);
    let memo = "limits test";
    let request = psp_mint::server::PspMeltRequest {
        melt: sp_wire::psp::MeltRequest {
            inputs: vec![melt_input(note, &keypair, amount, memo)],
            memo: memo.to_string(),
        },
        keysets: vec![psp_mint::rotation::keyset_id(
            m.state.logic.lock().unwrap().public_keys(),
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
    assert!(!spent(&m.state, note.nonce));

    // The same request at exactly the ceiling goes through.
    let (status, _) = post(&base, "/melt", padded(&request, 4096)).await;
    assert_eq!(status, 200);
    assert!(spent(&m.state, note.nonce));
}

/// A body sent in chunks, with no `Content-Length`, is read only as far as the
/// ceiling.
#[tokio::test]
async fn a_chunked_body_over_the_ceiling_is_refused_too() {
    let (base, _m) = start(
        "chunked",
        Some(Limits {
            burns_bytes: 4096,
            ..Limits::default()
        }),
    )
    .await;
    let addr = base.trim_start_matches("http://").to_string();
    let reply = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(&addr).unwrap();
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
        read_reply(&mut stream)
    })
    .await
    .unwrap();
    assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
    assert!(reply.contains("\"code\":\"request_too_large\""), "{reply}");
    assert!(reply.contains("\"size_bytes\":null"), "{reply}");
}

#[tokio::test]
async fn by_default_every_route_with_a_body_takes_up_to_one_mib() {
    let (base, m) = start("defaults", None).await;
    let (note, _) = issued_note(&m.state, Amount::from_units(1));
    let check = CheckStateRequest {
        nonces: vec![note.nonce],
    };
    let burns = BurnsRequest {
        nonces: vec![note.nonce],
    };

    // The wallet's check-state is over AIR, and answered under the defaults.
    {
        let air = m.air();
        let parsed: CheckStateResponse = air.post_ok("/check-state", &check);
        assert_eq!(parsed.spent, vec![false]);
    }

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

/// The check-state ceiling, over AIR: a body over it is refused with the same
/// JSON as the HTTP routes', before it is parsed; one at it is answered.
#[tokio::test]
async fn a_check_state_over_the_ceiling_is_refused_before_parsing() {
    let (_base, m) = start(
        "check-state",
        Some(Limits {
            check_state_bytes: 4096,
            ..Limits::default()
        }),
    )
    .await;
    let air = m.air();
    let (note, _) = issued_note(&m.state, Amount::from_units(1));
    let check = CheckStateRequest {
        nonces: vec![note.nonce],
    };

    let (status, body) = air.post_bytes("/check-state", &padded(&check, 4097));
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "request_too_large");
    assert_eq!(body["limit_bytes"], 4096);
    assert_eq!(body["size_bytes"], 4097);

    let (status, body) = air.post_bytes("/check-state", &padded(&check, 4096));
    assert_eq!(status, 200, "{body}");
    let parsed: CheckStateResponse = serde_json::from_value(body).unwrap();
    assert_eq!(parsed.spent, vec![false]);
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
    let (base, m) = start("big", Some(limits)).await;
    let (note, _) = issued_note(&m.state, Amount::from_units(1));
    let burns = BurnsRequest {
        nonces: vec![note.nonce],
    };
    let (status, body) = post(&base, "/burns", padded(&burns, big)).await;
    assert_eq!(status, 200, "{body}");
    let (status, _) = declared(&base, "/burns", big + 1).await;
    assert_eq!(status, 413);
}
