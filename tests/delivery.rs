//! The mint's side of delivering a purchase, against a real `air_server` started
//! in this process: a payment is settled with the real settlement code, the
//! deliverer writes its record, and the payer's side (the code the wallet runs:
//! `sp_wire::air_request::delivery`) reads it from the address it computes and
//! opens it with the purchase's one-off key.
//!
//! The AIR client always dials port 5702 (upstream item 12), so these tests need
//! that port free; `local_server::start` says so when it is not. One server
//! serves every test of this binary, and the tests take turns on it.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mint_core::MintLogic;
use mint_types::{Amount, BlindNonce, MintOutput};
use psp_mint::delivery::{Deliverer, Report, retry_after};
use psp_mint::records::Records;
use psp_mint::scan::Received;
use psp_mint::settle::{self, Issue, Settlement};
use rand::rngs::OsRng;
use secp256k1::SECP256K1;
use sp_wire::ServiceDid;
use sp_wire::air_request::addr;
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::delivery::{self as wire, Outcome, Written};
use sp_wire::air_request::local_server;
use sp_wire::payload::{Payload, encode};
use sp_wire::seal::{PublicKey, SecretKey};

/// $10,000 per BTC: a unit is $0.10, so 5,000 sats is exactly 5 units.
const BTC_10K: u64 = 1_000_000;

/// One test at a time on the shared AIR server (it has a single storage worker).
static AIR_TURN: Mutex<()> = Mutex::new(());

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

/// An AIR server that nothing answers: the right name, a host nobody is at (an
/// address reserved for documentation), so a connection fails or times out.
fn down(server: &ServerRef) -> ServerRef {
    ServerRef {
        name: server.name.clone(),
        host: "192.0.2.1".to_string(),
    }
}

/// A mint with its DID key and records, in a folder of its own.
struct Env {
    dir: PathBuf,
    logic: MintLogic,
    records: Records,
    key: SecretKey,
    psp: ServiceDid,
}

fn env(name: &str) -> Env {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "psp-mint-delivery-{}-{name}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let logic = psp_mint::mint::open(&dir, Amount::from_units(1024)).unwrap();
    let records = Records::open(&dir.join("settlements.json")).unwrap();
    let key = SecretKey::new();
    let psp = addr::key_to_did(&key.public_key());
    Env {
        dir,
        logic,
        records,
        key,
        psp,
    }
}

impl Env {
    fn records_path(&self) -> PathBuf {
        self.dir.join("settlements.json")
    }

    /// Settle a payment of `sats` carrying `payload`, at 6 confirmations.
    fn settle(&mut self, txid: &str, sats: u64, payload: &[u8]) -> Settlement {
        let received = Received {
            txid: txid.to_string(),
            confirmations: 6,
            value_sats: sats,
            payload: Some(payload.to_vec()),
        };
        settle::settle(&self.logic, &mut self.records, &received, &mut || {
            Ok(BTC_10K)
        })
        .unwrap()
    }

    fn deliverer(&self, server: &ServerRef) -> Deliverer {
        Deliverer::new(server.clone(), 2, self.key.clone())
    }
}

/// The payer's one-off key for a purchase, and the DID it stands for.
struct Reply {
    key: SecretKey,
    did: ServiceDid,
}

fn reply() -> Reply {
    let key = SecretKey::new();
    let did = addr::key_to_did(&key.public_key());
    Reply { key, did }
}

fn point() -> secp256k1::PublicKey {
    let secret = secp256k1::SecretKey::new(&mut OsRng);
    secp256k1::PublicKey::from_secret_key(SECP256K1, &secret)
}

fn outputs(units: &[u64]) -> Vec<(u64, secp256k1::PublicKey)> {
    units.iter().map(|&u| (u, point())).collect()
}

/// The unsealed payload bytes the wallet would build.
fn payload(did: &[u8; 33], outputs: &[(u64, secp256k1::PublicKey)]) -> Vec<u8> {
    encode(&Payload {
        did: *did,
        outputs: outputs
            .iter()
            .map(|&(units, key)| MintOutput {
                amount: Amount::from_units(units),
                blind_nonce: BlindNonce(key),
            })
            .collect(),
        keyset: None,
    })
    .unwrap()
}

fn issued(settlement: Settlement) -> Issue {
    match settlement {
        Settlement::Issued(issue) => issue,
        other => panic!("{other:?}"),
    }
}

/// What the payer does: read the cell it computes, once.
fn read(server: &ServerRef, address: &PublicKey) -> Option<Vec<u8>> {
    block_on_new_runtime(|| async {
        let c = client::connect(server).await.unwrap();
        wire::read_record(&c, address, 10, 0).await.unwrap()
    })
    .unwrap()
}

fn write(server: &ServerRef, cell: &SecretKey, sealed: &[u8]) -> Written {
    block_on_new_runtime(|| async {
        let c = client::connect(server).await.unwrap();
        wire::write_record(&c, cell, sealed, 10).await.unwrap()
    })
    .unwrap()
}

fn delivered_report() -> Report {
    Report {
        delivered: 1,
        ..Report::default()
    }
}

#[test]
fn a_settled_purchase_is_delivered_and_the_payer_reads_it_with_no_paste() {
    let server = server();
    let mut e = env("settled");
    let payer = reply();
    let bytes = payload(payer.did.as_bytes(), &outputs(&[4, 1]));
    let issue = issued(e.settle("tx1", 5_000, &bytes));

    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        delivered_report()
    );

    // the payer's side: the address it computes from what it knows
    let purchase = wire::purchase_id(&bytes);
    let address = wire::delivery_address(&e.psp, &payer.did, &purchase);
    let sealed = read(&server, &address).expect("the record is on the server");
    let opened = wire::open_record(&payer.key, &e.psp, &purchase, &sealed).unwrap();
    let Outcome::Signed(signed) = opened.outcome else {
        panic!("an issued purchase delivers its signatures");
    };
    assert_eq!(signed.keyset, issue.keyset);
    assert_eq!(signed.signatures.len(), 2);
    for ((nonce, result), pair) in issue
        .blind_nonces
        .iter()
        .zip(&issue.results)
        .zip(&signed.signatures)
    {
        assert_eq!(pair.blind_nonce, nonce.0.to_string());
        assert_eq!(pair.signature, result.as_ref().unwrap().0.to_string());
    }
    // the keysets to unblind with come in the record
    assert_eq!(signed.keysets.current, issue.keyset);
    assert!(signed.keysets.keysets.contains_key(&issue.keyset));

    // delivered is in the purchase's record, and a second pass has nothing to do
    let reopened = Records::open(&e.records_path()).unwrap();
    assert!(reopened.undelivered().is_empty());
    let delivery = reopened.get("tx1").unwrap().delivery.clone().unwrap();
    assert!(delivery.delivered);
    assert_eq!(
        delivery.sealed.as_deref(),
        Some(hex::encode(&sealed).as_str())
    );
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        Report::default()
    );
}

#[test]
fn a_rejection_is_delivered_with_its_reason() {
    let server = server();
    let mut e = env("rejected");
    let payer = reply();
    let first = outputs(&[4, 1]);
    issued(e.settle("tx1", 5_000, &payload(payer.did.as_bytes(), &first)));

    // a second purchase that reuses a blind nonce the mint has signed
    let second = reply();
    let reused = payload(second.did.as_bytes(), &[(2, point()), first[1]]);
    assert!(matches!(
        e.settle("tx2", 5_000, &reused),
        Settlement::Rejected(settle::Rejection::BlindNonceAlreadyUsed)
    ));

    let mut deliverer = e.deliverer(&server);
    // both records: the first purchase's signatures and the second's rejection
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        Report {
            delivered: 2,
            ..Report::default()
        }
    );
    let purchase = wire::purchase_id(&reused);
    let address = wire::delivery_address(&e.psp, &second.did, &purchase);
    let sealed = read(&server, &address).expect("the rejection is on the server");
    let opened = wire::open_record(&second.key, &e.psp, &purchase, &sealed).unwrap();
    let Outcome::Refused(refused) = opened.outcome else {
        panic!("a rejected purchase delivers its reason");
    };
    assert!(
        refused.reason.contains("already signed"),
        "{}",
        refused.reason
    );
    // it says what was paid and that the purchase stays open for the window
    assert_eq!((refused.paid_sats, refused.paid_cents), (5_000, 50));
    let rec = e.records.get("tx2").unwrap();
    assert_eq!(rec.open_until, Some(refused.open_until));
    assert!(refused.open_until > opened.issued_at);
    assert_eq!(rec.forfeited_at, None);
}

#[test]
fn a_held_payment_is_delivered_with_what_was_paid_and_forfeited_when_its_window_closes() {
    let server = server();
    let mut e = env("held");
    let payer = reply();
    // 1,000 sats is 1 unit against the 5 asked: short beyond the target, so
    // nothing is signed and the payment is held
    let bytes = payload(payer.did.as_bytes(), &outputs(&[4, 1]));
    assert!(matches!(
        e.settle("tx1", 1_000, &bytes),
        Settlement::PaidUnsettled(_)
    ));

    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        delivered_report()
    );
    let purchase = wire::purchase_id(&bytes);
    let address = wire::delivery_address(&e.psp, &payer.did, &purchase);
    let sealed = read(&server, &address).expect("the held record is on the server");
    let opened = wire::open_record(&payer.key, &e.psp, &purchase, &sealed).unwrap();
    let Outcome::Held(held) = opened.outcome else {
        panic!("a held payment delivers a held record");
    };
    assert_eq!((held.paid_sats, held.paid_cents), (1_000, 10));
    assert_eq!(held.requested_units, 5);
    assert!(held.over_under_bps < 0);
    assert!(held.open_until > opened.issued_at);

    // open inside the window, and shown as such
    let until = held.open_until;
    assert!(e.records.forfeit_expired(until - 1).unwrap().is_empty());
    let open = e.records.reconciliation();
    assert_eq!((open.open.payments, open.open.sats), (1, 1_000));
    assert_eq!(open.forfeited.payments, 0);

    // the window closes unsettled: forfeited, the mint keeps the BTC, and it is
    // shown apart
    assert_eq!(e.records.forfeit_expired(until).unwrap(), vec!["tx1"]);
    let after = Records::open(&e.records_path()).unwrap().reconciliation();
    assert_eq!(after.open.payments, 0);
    assert_eq!((after.forfeited.payments, after.forfeited.sats), (1, 1_000));
    // the record the payer was sent is still there to read
    assert!(read(&server, &address).is_some());
}

#[test]
fn a_payment_that_cannot_be_read_has_nothing_to_deliver() {
    let server = server();
    let mut e = env("unreadable");
    let received = Received {
        txid: "tx1".to_string(),
        confirmations: 6,
        value_sats: 5_000,
        payload: Some(vec![0]),
    };
    settle::settle(&e.logic, &mut e.records, &received, &mut || Ok(BTC_10K)).unwrap();
    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        Report::default()
    );
}

#[test]
fn air_down_at_settlement_then_a_later_pass_delivers_exactly_one_record() {
    let server = server();
    let mut e = env("down");
    let payer = reply();
    let bytes = payload(payer.did.as_bytes(), &outputs(&[4, 1]));
    issued(e.settle("tx1", 5_000, &bytes));
    let purchase = wire::purchase_id(&bytes);
    let address = wire::delivery_address(&e.psp, &payer.did, &purchase);

    // AIR is down: the write fails, and issuance stands
    let mut deliverer = e.deliverer(&down(&server));
    let t0 = Instant::now();
    assert_eq!(
        deliverer.pass(&mut e.records, t0),
        Report {
            failed: 1,
            ..Report::default()
        }
    );
    assert_eq!(e.records.undelivered().len(), 1);
    assert_eq!(read(&server, &address), None);
    // the sealed record was made and saved before the write was tried
    let saved = e
        .records
        .get("tx1")
        .unwrap()
        .delivery
        .clone()
        .unwrap()
        .sealed;
    assert!(saved.is_some());

    // inside the wait: left alone, no attempt
    assert_eq!(
        deliverer.pass(&mut e.records, t0 + Duration::from_secs(1)),
        Report {
            waiting: 1,
            ..Report::default()
        }
    );
    // after the wait: tried again, still down, and the wait doubles
    let t1 = t0 + retry_after(1) + Duration::from_secs(1);
    assert_eq!(
        deliverer.pass(&mut e.records, t1),
        Report {
            failed: 1,
            ..Report::default()
        }
    );
    assert_eq!(
        deliverer.pass(&mut e.records, t1 + retry_after(1)),
        Report {
            waiting: 1,
            ..Report::default()
        }
    );

    // AIR is back, and the mint restarted (new deliverer, records read from disk)
    let mut records = Records::open(&e.records_path()).unwrap();
    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass(&mut records, Instant::now()),
        delivered_report()
    );
    let sealed = read(&server, &address).expect("delivered");
    // the bytes are the ones saved before the first try: one record, one write
    assert_eq!(Some(hex::encode(&sealed)), saved);
    assert!(
        wire::open_record(&payer.key, &e.psp, &purchase, &sealed).is_ok(),
        "the payer can open it"
    );
    assert_eq!(
        deliverer.pass(&mut records, Instant::now()),
        Report::default()
    );
    assert!(
        Records::open(&e.records_path())
            .unwrap()
            .undelivered()
            .is_empty()
    );
}

#[test]
fn a_write_that_got_through_unacknowledged_is_found_stored_not_written_twice() {
    let server = server();
    let mut e = env("unacked");
    let payer = reply();
    let bytes = payload(payer.did.as_bytes(), &outputs(&[2]));
    issued(e.settle("tx1", 2_000, &bytes));
    let purchase = wire::purchase_id(&bytes);

    // the first pass makes and saves the record, but AIR is down
    let mut deliverer = e.deliverer(&down(&server));
    deliverer.pass(&mut e.records, Instant::now());
    let delivery = e.records.get("tx1").unwrap().delivery.clone().unwrap();
    let sealed = hex::decode(delivery.sealed.unwrap()).unwrap();

    // meanwhile a write of those bytes landed, and its answer was lost
    let cell = wire::delivery_secret(&e.psp, &payer.did, &purchase);
    assert_eq!(write(&server, &cell, &sealed), Written::Created);

    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        delivered_report()
    );
    assert!(e.records.undelivered().is_empty());
}

#[test]
fn a_different_record_at_the_address_is_reported_and_never_overwritten() {
    let server = server();
    let mut e = env("different");
    let payer = reply();
    let bytes = payload(payer.did.as_bytes(), &outputs(&[2]));
    issued(e.settle("tx1", 2_000, &bytes));
    let purchase = wire::purchase_id(&bytes);
    let cell = wire::delivery_secret(&e.psp, &payer.did, &purchase);
    // somebody's bytes are already there
    assert_eq!(
        write(&server, &cell, b"not the mint's record"),
        Written::Created
    );

    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        Report {
            abandoned: 1,
            ..Report::default()
        }
    );
    let delivery = e.records.get("tx1").unwrap().delivery.clone().unwrap();
    assert!(!delivery.delivered);
    assert!(delivery.abandoned.unwrap().contains("different record"));
    // not tried again
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        Report::default()
    );
    let address = wire::delivery_address(&e.psp, &payer.did, &purchase);
    assert_eq!(
        read(&server, &address),
        Some(b"not the mint's record".to_vec())
    );
}

#[test]
fn a_reply_did_that_is_not_a_key_is_given_up_on_not_retried_for_ever() {
    let server = server();
    let mut e = env("not-a-key");
    let bytes = payload(&[7u8; 33], &outputs(&[2]));
    issued(e.settle("tx1", 2_000, &bytes));
    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass(&mut e.records, Instant::now()),
        Report {
            abandoned: 1,
            ..Report::default()
        }
    );
    assert!(e.records.undelivered().is_empty());
    assert!(
        e.records
            .get("tx1")
            .unwrap()
            .delivery
            .as_ref()
            .unwrap()
            .abandoned
            .is_some()
    );
}

#[test]
fn a_settlements_file_from_before_delivery_still_reads_and_owes_nothing() {
    let server = server();
    let e = env("old-state");
    std::fs::write(
        e.records_path(),
        r#"{"aa": {"locked_price_cents": 1000000, "issued": true, "over_under_bps": 0,
            "status": "issued", "late": false, "skipped_units": 0, "credit_cents": 0,
            "reason": null, "payer_did": "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            "requested_units": 5, "value_sats": 5000},
          "bb": {"locked_price_cents": 1000000, "issued": false, "over_under_bps": null,
            "status": "rejected", "late": false, "skipped_units": 0, "credit_cents": 0,
            "reason": "NoDataOutput"}}"#,
    )
    .unwrap();
    let mut records = Records::open(&e.records_path()).unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.undelivered().is_empty());
    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass(&mut records, Instant::now()),
        Report::default()
    );
}
