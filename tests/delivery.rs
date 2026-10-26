//! The mint's side of delivering an owed record, against a real `air_server`
//! started in this process: a payment reaches its tier through the real
//! settlement code, the deliverer writes its owed record, and the payer's side
//! (the code the wallet runs: `sp_wire::air_request::delivery`) reads it from the
//! address it computes and opens it with the claim key.
//!
//! The AIR client always dials port 5702 (upstream item 12), so these tests need
//! that port free; `local_server::start` says so when it is not. One server
//! serves every test of this binary, and the tests take turns on it.

mod common;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use psp_mint::delivery::{Deliverer, Report, retry_after};
use psp_mint::records::{Records, SharedRecords};
use psp_mint::scan::Received;
use psp_mint::server::Limits;
use psp_mint::settle::{self, Policy, Settlement};
use sp_wire::ServiceDid;
use sp_wire::air_request::addr;
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::delivery::{self as wire, Outcome, Written};
use sp_wire::air_request::local_server;
use sp_wire::seal::{PublicKey, SecretKey};

/// $100,000 per BTC: 100 sats are exactly 1 unit, so 700 sats are 7 units.
const BTC_100K: u64 = 10_000_000;
const NOW: u64 = 1_800_000_000;
/// When the deliverer's passes find a record stored: not the time the record was
/// made, so a window counted from the wrong moment shows.
const LAND: u64 = NOW + 5_000;

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

/// A mint wired as `serve` wires it (see `common::PspMint`), with a DID key for
/// delivering.
struct Env {
    mint: common::PspMint,
    shared: SharedRecords,
    key: SecretKey,
    psp: ServiceDid,
}

fn env(name: &str) -> Env {
    let mint = common::PspMint::open(&format!("delivery-{name}"), NOW, Limits::default());
    let shared = std::sync::Arc::clone(&mint.records);
    let key = SecretKey::new();
    let psp = addr::key_to_did(&key.public_key());
    Env {
        mint,
        shared,
        key,
        psp,
    }
}

impl Env {
    fn records_path(&self) -> PathBuf {
        self.mint.records_path()
    }

    /// The records the deliverer and the settlement share.
    fn records(&self) -> MutexGuard<'_, Records> {
        self.shared.lock().unwrap()
    }

    /// A payment of `sats` to `payer`'s claim key, at its tier.
    fn owe(&mut self, txid: &str, sats: u64, payer: &Payer) -> Settlement {
        let received = Received {
            txid: txid.to_string(),
            confirmations: 3,
            value_sats: sats,
            payload: Some(payer.did.as_bytes().to_vec()),
            block_time: Some(NOW),
        };
        let mut records = self.shared.lock().unwrap();
        settle::settle_ruled(
            &mut records,
            &received,
            &mut || Ok(BTC_100K),
            &Policy::default(),
            NOW,
        )
        .unwrap()
    }

    fn deliverer(&self, server: &ServerRef) -> Deliverer {
        Deliverer::new(server.clone(), 2, self.key)
    }
}

/// The payer's one-off claim key and the DID it stands for.
struct Payer {
    key: SecretKey,
    did: ServiceDid,
}

fn payer() -> Payer {
    let key = SecretKey::new();
    let did = addr::key_to_did(&key.public_key());
    Payer { key, did }
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
fn an_owed_record_is_delivered_and_the_payer_reads_it_with_the_claim_key() {
    let server = server();
    let mut e = env("owed");
    let payer = payer();
    assert!(matches!(e.owe("tx1", 700, &payer), Settlement::Owed(_)));

    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        delivered_report()
    );

    // the payer's side: the address it computes from what it knows
    let address = wire::owed_address(&e.psp, &payer.did, 0);
    let sealed = read(&server, &address).expect("the record is on the server");
    let opened = wire::open_record(&payer.key, &e.psp, 0, &sealed).unwrap();
    let Outcome::Owed(owed) = opened.outcome;
    assert_eq!(
        (owed.txid.as_str(), owed.paid_sats, owed.value_units),
        ("tx1", 700, 7)
    );
    assert_eq!(owed.terms.due_units, 7);
    // the record carries a length, not a date: the wallet counts it from its
    // first read, the mint from the moment the record landed
    assert_eq!(owed.terms.window_secs, 2_592_000);

    // delivered is in the payment's record, and a second pass has nothing to do
    let reopened = Records::open(&e.records_path()).unwrap();
    assert!(reopened.undelivered().is_empty());
    let record = reopened.get("tx1").unwrap();
    // the deadline is counted from the landing (LAND), not from the payment (NOW)
    assert_eq!(record.landed_at, Some(LAND));
    assert_eq!(record.open_until, Some(LAND + 2_592_000));
    let delivery = record.delivery.clone().unwrap();
    assert!(delivery.delivered);
    assert_eq!(
        delivery.sealed.as_deref(),
        Some(hex::encode(&sealed).as_str())
    );
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        Report::default()
    );
}

#[test]
fn two_payments_to_one_claim_key_are_two_records_in_two_cells() {
    let server = server();
    let mut e = env("two");
    let payer = payer();
    e.owe("tx1", 700, &payer);
    e.owe("tx2", 300, &payer);
    let mut deliverer = e.deliverer(&server);
    // one record per claim key per pass, lowest index first
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        delivered_report()
    );
    assert!(read(&server, &wire::owed_address(&e.psp, &payer.did, 1)).is_none());
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND + 60),
        delivered_report()
    );
    // each record's window starts when that record landed
    assert_eq!(
        e.records().get("tx1").unwrap().open_until,
        Some(LAND + 2_592_000)
    );
    assert_eq!(
        e.records().get("tx2").unwrap().open_until,
        Some(LAND + 60 + 2_592_000)
    );
    for (index, units) in [(0, 7), (1, 3)] {
        let address = wire::owed_address(&e.psp, &payer.did, index);
        let sealed = read(&server, &address).expect("the record is on the server");
        let Outcome::Owed(owed) = wire::open_record(&payer.key, &e.psp, index, &sealed)
            .unwrap()
            .outcome;
        assert_eq!(owed.terms.due_units, units);
    }
    // the next cell holds nothing: that is how the wallet knows it has read them all
    assert_eq!(
        read(&server, &wire::owed_address(&e.psp, &payer.did, 2)),
        None
    );
}

#[test]
fn air_down_at_settlement_then_a_later_pass_delivers_exactly_one_record() {
    let server = server();
    let mut e = env("down");
    let payer = payer();
    e.owe("tx1", 700, &payer);
    let address = wire::owed_address(&e.psp, &payer.did, 0);

    // AIR is down: the write fails, and the owed record stands
    let mut deliverer = e.deliverer(&down(&server));
    let t0 = Instant::now();
    assert_eq!(
        deliverer.pass_shared(&e.shared, t0, LAND),
        Report {
            failed: 1,
            ..Report::default()
        }
    );
    assert_eq!(e.records().undelivered().len(), 1);
    assert_eq!(read(&server, &address), None);
    // the sealed record was made and saved before the write was tried
    let saved = e
        .records()
        .get("tx1")
        .unwrap()
        .delivery
        .clone()
        .unwrap()
        .sealed;
    assert!(saved.is_some());

    // inside the wait: left alone, no attempt
    assert_eq!(
        deliverer.pass_shared(&e.shared, t0 + Duration::from_secs(1), LAND),
        Report {
            waiting: 1,
            ..Report::default()
        }
    );
    // after the wait: tried again, still down, and the wait doubles
    let t1 = t0 + retry_after(1) + Duration::from_secs(1);
    assert_eq!(
        deliverer.pass_shared(&e.shared, t1, LAND),
        Report {
            failed: 1,
            ..Report::default()
        }
    );
    assert_eq!(
        deliverer.pass_shared(&e.shared, t1 + retry_after(1), LAND),
        Report {
            waiting: 1,
            ..Report::default()
        }
    );

    // AIR is back, and the mint restarted (new deliverer, records read from disk)
    let records = Mutex::new(Records::open(&e.records_path()).unwrap());
    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass_shared(&records, Instant::now(), LAND),
        delivered_report()
    );
    let sealed = read(&server, &address).expect("delivered");
    // the bytes are the ones saved before the first try: one record, one write
    assert_eq!(Some(hex::encode(&sealed)), saved);
    assert!(wire::open_record(&payer.key, &e.psp, 0, &sealed).is_ok());
    assert_eq!(
        deliverer.pass_shared(&records, Instant::now(), LAND),
        Report::default()
    );
}

#[test]
fn a_write_that_got_through_unacknowledged_is_found_stored_not_written_twice() {
    let server = server();
    let mut e = env("unacked");
    let payer = payer();
    e.owe("tx1", 700, &payer);

    // the first pass makes and saves the record, but AIR is down
    let mut deliverer = e.deliverer(&down(&server));
    deliverer.pass_shared(&e.shared, Instant::now(), LAND);
    let delivery = e.records().get("tx1").unwrap().delivery.clone().unwrap();
    let sealed = hex::decode(delivery.sealed.unwrap()).unwrap();

    // meanwhile a write of those bytes landed, and its answer was lost
    let cell = wire::owed_secret(&e.psp, &payer.did, 0);
    assert_eq!(write(&server, &cell, &sealed), Written::Created);

    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        delivered_report()
    );
    assert!(e.records().undelivered().is_empty());
}

#[test]
fn a_different_record_at_the_address_is_reported_and_never_overwritten() {
    let server = server();
    let mut e = env("different");
    let payer = payer();
    e.owe("tx1", 700, &payer);
    let cell = wire::owed_secret(&e.psp, &payer.did, 0);
    // somebody's bytes are already there
    assert_eq!(
        write(&server, &cell, b"not the mint's record"),
        Written::Created
    );

    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        Report {
            abandoned: 1,
            ..Report::default()
        }
    );
    let delivery = e.records().get("tx1").unwrap().delivery.clone().unwrap();
    assert!(!delivery.delivered);
    assert!(delivery.abandoned.unwrap().contains("different record"));
    // and the time it was given up on, from which retention counts
    assert_eq!(delivery.abandoned_at, Some(LAND));
    // not tried again
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        Report::default()
    );
    let address = wire::owed_address(&e.psp, &payer.did, 0);
    assert_eq!(
        read(&server, &address),
        Some(b"not the mint's record".to_vec())
    );
}

/// An owed record that was given up on is pruned `abandoned_at` + retention (90
/// days) to the second, the scan listing it again settles nothing, and its share
/// of the `/audit` totals is the same before and after. Fails if an abandoned
/// payment is never pruned (the file grows without limit), if the time it counts
/// from is not the time it was given up on, or if the pruned txid is owed again.
#[test]
fn an_abandoned_payment_is_pruned_after_the_retention_and_not_owed_again() {
    const RETENTION: u64 = 7_776_000;
    const HORIZON: u64 = 18_144_000;
    let server = server();
    let mut e = env("abandoned-pruned");
    let payer = payer();
    e.owe("tx1", 700, &payer);
    let cell = wire::owed_secret(&e.psp, &payer.did, 0);
    assert_eq!(
        write(&server, &cell, b"not the mint's record"),
        Written::Created
    );
    let mut deliverer = e.deliverer(&server);
    deliverer.pass_shared(&e.shared, Instant::now(), LAND);
    assert_eq!(
        e.records()
            .get("tx1")
            .unwrap()
            .delivery
            .as_ref()
            .unwrap()
            .abandoned_at,
        Some(LAND)
    );
    let before = e.records().claims().abandoned;
    assert_eq!((before.payments, before.sats, before.units), (1, 700, 7));
    assert_eq!(e.records().claims().unclaimed.payments, 0);

    // 90 days less a second after it was given up on: still there
    let early = e
        .records()
        .prune(LAND + RETENTION - 1, RETENTION, HORIZON)
        .unwrap();
    assert_eq!(early.removed, 0);
    let on_time = e
        .records()
        .prune(LAND + RETENTION, RETENTION, HORIZON)
        .unwrap();
    assert_eq!(on_time.removed, 1);
    assert!(e.records().get("tx1").is_none());
    assert!(e.records().was_pruned("tx1"));
    let after = e.records().claims().abandoned;
    assert_eq!((after.payments, after.sats, after.units), (1, 700, 7));
    assert_eq!(e.records().claims().unclaimed.payments, 0);
    // the scan lists it again: nothing is made
    assert_eq!(e.owe("tx1", 700, &payer), Settlement::Pruned);
    assert!(e.records().get("tx1").is_none());
    // the same claim key's next payment is record 1: cell 0 holds another record
    let next = e.owe("tx2", 700, &payer);
    let Settlement::Owed(owed) = next else {
        panic!("{next:?}")
    };
    assert_eq!(owed.index, 1);
}

/// Fails without the ordering of `Records::next_to_deliver`: with it gone, record
/// 1 is written while record 0 is still failing, and a wallet that reads cells in
/// order and stops at the first empty one would never see it land late.
#[test]
fn a_failing_write_of_index_0_holds_index_1_back_until_0_lands() {
    let server = server();
    let mut e = env("order");
    let payer = payer();
    e.owe("tx1", 700, &payer);
    e.owe("tx2", 300, &payer);
    let first = wire::owed_address(&e.psp, &payer.did, 0);
    let second = wire::owed_address(&e.psp, &payer.did, 1);

    // the write of 0 fails: only 0 is tried
    let mut deliverer = e.deliverer(&down(&server));
    let t0 = Instant::now();
    assert_eq!(
        deliverer.pass_shared(&e.shared, t0, LAND),
        Report {
            failed: 1,
            ..Report::default()
        }
    );
    // the next pass, inside the wait: 0 is waiting, 1 is not written
    assert_eq!(
        deliverer.pass_shared(&e.shared, t0 + Duration::from_secs(1), LAND),
        Report {
            waiting: 1,
            ..Report::default()
        }
    );
    assert_eq!(read(&server, &second), None);
    assert!(
        e.records()
            .get("tx2")
            .unwrap()
            .delivery
            .clone()
            .unwrap()
            .sealed
            .is_none()
    );

    // AIR is back: 0 lands, and 1 is still not written in that pass
    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        delivered_report()
    );
    assert!(read(&server, &first).is_some());
    assert_eq!(read(&server, &second), None);
    // the pass after: 1
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        delivered_report()
    );
    assert!(read(&server, &second).is_some());
    assert!(e.records().undelivered().is_empty());
}

/// Fails if a record given up on holds the later ones back.
#[test]
fn an_abandoned_index_0_does_not_block_index_1() {
    let server = server();
    let mut e = env("abandoned-order");
    let payer = payer();
    e.owe("tx1", 700, &payer);
    e.owe("tx2", 300, &payer);
    let cell = wire::owed_secret(&e.psp, &payer.did, 0);
    assert_eq!(
        write(&server, &cell, b"not the mint's record"),
        Written::Created
    );

    let mut deliverer = e.deliverer(&server);
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        Report {
            abandoned: 1,
            ..Report::default()
        }
    );
    assert_eq!(
        deliverer.pass_shared(&e.shared, Instant::now(), LAND),
        delivered_report()
    );
    let sealed = read(&server, &wire::owed_address(&e.psp, &payer.did, 1)).unwrap();
    assert!(wire::open_record(&payer.key, &e.psp, 1, &sealed).is_ok());
    // the abandoned record never lands, so it never has a window
    let record = e.records().get("tx1").unwrap().clone();
    assert_eq!((record.landed_at, record.open_until), (None, None));
}
