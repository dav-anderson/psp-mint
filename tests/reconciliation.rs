//! `/audit` shows the payments held open and the payments forfeited apart from
//! the issued, redeemed and outstanding totals, once `serve` gives the router the
//! settlement records (`router_audited`). Over a real server on a local port.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mint_types::Amount;
use psp_mint::melts::MeltLog;
use psp_mint::records::{Record, Records, Status};
use psp_mint::rotation::{Rollovers, Rotor};
use psp_mint::server::{Health, Limits, Rotating, SharedRotating};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("psp-mint-recon-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A held payment of `sats`, open until `until`.
fn held(sats: u64, until: u64) -> Record {
    let mut r = Record::locked(1_000_000, false);
    r.status = Status::PaidUnsettled;
    r.value_sats = sats;
    r.open_until = Some(until);
    r
}

fn records(dir: &PathBuf) -> Records {
    let mut records = Records::open(&dir.join("settlements.json")).unwrap();
    records.put("a", held(300, 100)).unwrap();
    records.put("b", held(700, 100)).unwrap();
    records.put("c", held(900, 5_000)).unwrap();
    // a payment that was never open is not in the report
    records.put("d", Record::locked(1, false)).unwrap();
    records.forfeit_expired(100).unwrap();
    records
}

async fn serve(
    dir: &PathBuf,
    rotating: bool,
    records_path: Option<PathBuf>,
) -> (String, tokio::task::JoinHandle<()>) {
    let (state, rot): (_, Option<SharedRotating>) = if rotating {
        let (rotor, logic) =
            Rotor::open_at(dir, Amount::from_units(1024), 1, 1, 1_800_000_000).unwrap();
        let rot = Rotating::new(rotor, Rollovers::open(dir).unwrap());
        (psp_mint::server::app_state(logic), Some(rot))
    } else {
        let logic = psp_mint::mint::open(dir, Amount::from_units(1024)).unwrap();
        (psp_mint::server::app_state(logic), None)
    };
    let melts = Arc::new(Mutex::new(
        MeltLog::open(&dir.join("melt_memos.json")).unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = psp_mint::server::router_audited(
        state,
        Health::new(100),
        melts,
        Limits::default(),
        rot,
        records_path,
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), task)
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
async fn audit_shows_forfeited_and_open_payments_apart_without_rotation() {
    let dir = scratch("plain");
    let _records = records(&dir);
    let (base, task) = serve(&dir, false, Some(dir.join("settlements.json"))).await;
    check(&audit(&base).await);
    task.abort();
}

#[tokio::test]
async fn audit_shows_forfeited_and_open_payments_apart_with_rotation() {
    let dir = scratch("rotating");
    let _records = records(&dir);
    let (base, task) = serve(&dir, true, Some(dir.join("settlements.json"))).await;
    check(&audit(&base).await);
    task.abort();
}

#[tokio::test]
async fn audit_without_the_records_is_the_totals_alone() {
    let dir = scratch("none");
    let (base, task) = serve(&dir, false, None).await;
    let report = audit(&base).await;
    assert!(report.get("reconciliation").is_none(), "{report}");
    assert!(report.get("issued").is_some());
    task.abort();
}

#[tokio::test]
async fn audit_is_current_as_of_the_last_write() {
    let dir = scratch("current");
    let mut records = records(&dir);
    let (base, task) = serve(&dir, false, Some(dir.join("settlements.json"))).await;
    check(&audit(&base).await);
    // the open one closes
    records.forfeit_expired(5_000).unwrap();
    let report = audit(&base).await;
    assert_eq!(report["reconciliation"]["forfeited"]["payments"], 3);
    assert_eq!(report["reconciliation"]["forfeited"]["sats"], 1_900);
    assert_eq!(report["reconciliation"]["open"]["payments"], 0);
    task.abort();
}
