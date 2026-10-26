//! `/audit` shows the claimed, unclaimed, expired and abandoned payments apart from the
//! issued, redeemed and outstanding totals, as `serve` builds the router (`router_live`,
//! with the mint's settlement records). Over a real server on a local port.

mod common;

use psp_mint::records::{Claimed, Delivery, Record, Records, Status};
use psp_mint::server::Limits;
use sp_wire::air_request::delivery::{
    ClaimTerms, EXPIRY_NOTICE, Outcome, Owed, SEND_PURCHASE_CLAIM,
};
use sp_wire::psp::{CLAIM_WINDOW_SECS, ClaimResponse, KeysetsResponse};

/// The claim window, 30 days, written out.
const WINDOW: u64 = 2_592_000;

/// An owed payment of `sats` and `units`, with `kept` cents left over, whose
/// record was stored on AIR at `landed`: open for `WINDOW` from then.
fn owed(sats: u64, units: u64, kept: u64, landed: u64) -> Record {
    let mut r = Record::locked(10_000_000, false, sats);
    r.status = Status::Owed;
    r.value_units = units;
    r.due_units = units;
    r.kept_cents = kept;
    r.landed_at = Some(landed);
    r.open_until = Some(landed + WINDOW);
    r
}

fn claimed(mut r: Record) -> Record {
    r.read_prepaid_units = 2;
    r.write_prepaid_units = 1;
    r.claimed = Some(Claimed {
        hash: "00".repeat(32),
        response: ClaimResponse {
            keyset: "k".into(),
            signatures: vec![],
            keysets: KeysetsResponse {
                current: "k".into(),
                previous: None,
                keysets: Default::default(),
            },
            issued_units: r.due_units,
            read_prepaid_units: 2,
            write_prepaid_units: 1,
        },
    });
    r
}

fn fill(records: &mut Records) {
    let mut a = claimed(owed(700, 7, 6, 100));
    a.claimed_at = Some(200);
    records.put("a", a).unwrap();
    records.put("b", owed(300, 3, 0, 100)).unwrap();
    records.put("c", owed(900, 9, 4, 5_000)).unwrap();
    // kept with nothing owed, and a payment still waiting for its tier
    let mut kept = Record::locked(10_000_000, false, 50);
    kept.status = Status::Kept;
    records.put("d", kept).unwrap();
    records.put("e", Record::locked(1, false, 5)).unwrap();
    records.expire_unclaimed(100 + WINDOW).unwrap();
}

async fn audit(base: &str) -> serde_json::Value {
    reqwest::get(format!("{base}/audit"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn audit_is_current_as_of_the_last_write() {
    let m = common::PspMint::open("audit-claims", 1_800_000_000, Limits::default());
    fill(&mut m.records.lock().unwrap());
    let base = m.http().await;
    let report = audit(&base).await;
    // the totals are still there, and the claims are apart
    assert_eq!(report["issued"], "0");
    assert!(report.get("outstanding").is_some(), "{report}");
    let claims = &report["claims"];
    assert_eq!(
        claims["claimed"],
        serde_json::json!({"payments": 1, "sats": 700, "units": 7})
    );
    assert_eq!(
        claims["expired"],
        serde_json::json!({"payments": 1, "sats": 300, "units": 3})
    );
    assert_eq!(
        claims["unclaimed"],
        serde_json::json!({"payments": 1, "sats": 900, "units": 9})
    );
    assert_eq!(claims["kept"]["payments"], 1, "{report}");
    assert_eq!(claims["subunit_kept_cents"], 10);
    assert_eq!(claims["read_prepaid_units"], 2);
    assert_eq!(claims["write_prepaid_units"], 1);
    // the open one closes
    m.records
        .lock()
        .unwrap()
        .expire_unclaimed(5_000 + WINDOW)
        .unwrap();
    let report = audit(&base).await;
    assert_eq!(report["claims"]["expired"]["payments"], 2);
    assert_eq!(report["claims"]["expired"]["sats"], 1_200);
    assert_eq!(report["claims"]["unclaimed"]["payments"], 0);
}

/// Fails if pruning a claimed or expired payment takes it out of the totals:
/// without the saved aggregate the three figures below shrink.
#[tokio::test]
async fn audit_totals_do_not_shrink_when_claimed_and_expired_payments_are_pruned() {
    const RETENTION: u64 = 7_776_000;
    const HORIZON: u64 = 18_144_000;
    let m = common::PspMint::open("audit-pruned", 1_800_000_000, Limits::default());
    fill(&mut m.records.lock().unwrap());
    let base = m.http().await;
    let before = audit(&base).await["claims"].clone();
    assert_eq!(
        before["claimed"],
        serde_json::json!({"payments": 1, "sats": 700, "units": 7})
    );
    // a (claimed at 200) and b (expired at 100 + WINDOW) are 90 days old; c is open
    let now = 100 + WINDOW + RETENTION;
    let pruned = m
        .records
        .lock()
        .unwrap()
        .prune(now, RETENTION, HORIZON)
        .unwrap();
    assert_eq!(pruned.removed, 2);
    assert_eq!(m.records.lock().unwrap().len(), 3);
    let after = audit(&base).await["claims"].clone();
    assert_eq!(after, before);
    assert_eq!(
        after["expired"],
        serde_json::json!({"payments": 1, "sats": 300, "units": 3})
    );
    assert_eq!(after["subunit_kept_cents"], 10);
    // and the file a restart reads gives the same figures
    let reopened = Records::open(&m.records_path()).unwrap();
    assert_eq!(serde_json::to_value(reopened.claims()).unwrap(), after);
}

/// An owed payment of 50,000 sats and 400 units whose owed record the mint gave
/// up on at `at`: the cell of its claim key holds another record, so the record
/// can never be written and the payer cannot claim.
fn abandoned(at: u64) -> Record {
    let mut r = Record::locked(10_000_000, false, 50_000);
    r.status = Status::Owed;
    r.value_units = 400;
    r.due_units = 400;
    r.delivery = Some(Delivery {
        claim_did: "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5".into(),
        index: 0,
        outcome: Outcome::Owed(Owed {
            txid: "aa".repeat(32),
            paid_sats: 50_000,
            value_units: 400,
            terms: ClaimTerms {
                send: SEND_PURCHASE_CLAIM.to_string(),
                kind: 15,
                due_units: 400,
                read_prepaid_units: 0,
                write_prepaid_units: 0,
                window_secs: CLAIM_WINDOW_SECS,
                max_attempts: 2,
                on_expiry: EXPIRY_NOTICE.to_string(),
            },
        }),
        sealed: None,
        delivered: false,
        abandoned: Some("the cell holds another record".into()),
        abandoned_at: Some(at),
    });
    r
}

/// An abandoned payment is counted under `abandoned`, with its sats and units,
/// and not under `unclaimed`; the same figures stand after retention removes it.
/// Fails if `Claims::add` counts it as unclaimed, or if the pruned aggregate puts
/// its share anywhere else.
#[tokio::test]
async fn an_abandoned_payment_is_counted_apart_from_unclaimed_and_stays_so_after_pruning() {
    const RETENTION: u64 = 7_776_000;
    const HORIZON: u64 = 18_144_000;
    let m = common::PspMint::open("audit-abandoned", 1_800_000_000, Limits::default());
    m.records
        .lock()
        .unwrap()
        .put("gave-up", abandoned(1_000))
        .unwrap();
    let base = m.http().await;
    let none = serde_json::json!({"payments": 0, "sats": 0, "units": 0});
    let one = serde_json::json!({"payments": 1, "sats": 50_000, "units": 400});
    let report = audit(&base).await;
    assert_eq!(report["claims"]["abandoned"], one, "{report}");
    assert_eq!(report["claims"]["unclaimed"], none, "{report}");
    // 90 days = 7,776,000 s after it was given up on, it is removed from the file
    let pruned = m
        .records
        .lock()
        .unwrap()
        .prune(1_000 + RETENTION, RETENTION, HORIZON)
        .unwrap();
    assert_eq!(pruned.removed, 1);
    assert_eq!(m.records.lock().unwrap().len(), 0);
    let report = audit(&base).await;
    assert_eq!(report["claims"]["abandoned"], one, "{report}");
    assert_eq!(report["claims"]["unclaimed"], none, "{report}");
}
