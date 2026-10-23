//! `/audit` shows the payments held open and the payments forfeited apart from
//! the issued, redeemed and outstanding totals, as `serve` builds the router
//! (`router_audited`, with the mint's settlement records). Over a real server on
//! a local port. Every figure of the report, with fees, is checked in
//! `tests/reconcile.rs`.

mod common;

use psp_mint::records::{Record, Records, Status};
use psp_mint::server::Limits;

/// A held payment of `sats`, open until `until`.
fn held(sats: u64, until: u64) -> Record {
    let mut r = Record::locked(1_000_000, false);
    r.status = Status::PaidUnsettled;
    r.value_sats = sats;
    r.open_until = Some(until);
    r
}

fn fill(records: &mut Records) {
    records.put("a", held(300, 100)).unwrap();
    records.put("b", held(700, 100)).unwrap();
    records.put("c", held(900, 5_000)).unwrap();
    // a payment that was never open is not in the report
    records.put("d", Record::locked(1, false)).unwrap();
    records.forfeit_expired(100).unwrap();
}

async fn audit(base: &str) -> serde_json::Value {
    reqwest::get(format!("{base}/audit"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn check(report: &serde_json::Value) {
    // the totals are still there, and the held and forfeited payments are apart
    assert_eq!(report["issued"], 0);
    assert!(report.get("outstanding").is_some(), "{report}");
    let recon = &report["reconciliation"];
    assert_eq!(recon["forfeited"]["payments"], 2, "{report}");
    assert_eq!(recon["forfeited"]["sats"], 1_000, "{report}");
    assert_eq!(recon["forfeited"]["credit_cents"], 0, "{report}");
    assert_eq!(recon["open"]["payments"], 1, "{report}");
    assert_eq!(recon["open"]["sats"], 900, "{report}");
}

#[tokio::test]
async fn audit_is_current_as_of_the_last_write() {
    let m = common::PspMint::open("recon-current", 1_800_000_000, Limits::default());
    fill(&mut m.records.lock().unwrap());
    let base = m.http().await;
    check(&audit(&base).await);
    // the open one closes
    m.records.lock().unwrap().forfeit_expired(5_000).unwrap();
    let report = audit(&base).await;
    assert_eq!(report["reconciliation"]["forfeited"]["payments"], 3);
    assert_eq!(report["reconciliation"]["forfeited"]["sats"], 1_900);
    assert_eq!(report["reconciliation"]["open"]["payments"], 0);
}
