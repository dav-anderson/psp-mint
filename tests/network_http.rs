//! The network a mint runs on is in `/health` and `/audit`, so a wallet or the
//! ESP mint can refuse a mint on the wrong one. On the router `serve` builds.

mod common;

use common::PspMint;
use psp_mint::server::Limits;

async fn json(url: String) -> serde_json::Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn health_and_audit_name_the_network() {
    let mint = PspMint::open("named", 1_800_000_000, Limits::default());
    let base = mint.http().await;
    let health = json(format!("{base}/health")).await;
    assert_eq!(health["network"], "regtest");
    assert_eq!(health["status"], "starting");
    let audit = json(format!("{base}/audit")).await;
    assert_eq!(audit["network"], "regtest");
    assert_eq!(audit["issued"], 0);
    assert_eq!(audit["outstanding"], 0);
}
