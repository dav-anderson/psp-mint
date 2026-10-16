//! The network a mint runs on is in `/health` and `/audit`, so a wallet or the
//! ESP mint can refuse a mint on the wrong one.

use std::sync::{Arc, Mutex};

use mint_types::Amount;
use psp_mint::melts::MeltLog;
use psp_mint::server::Health;

async fn start(name: &str, health: Arc<Health>) -> String {
    let dir = std::env::temp_dir().join(format!("psp-mint-nethttp-{}-{name}", {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }));
    let _ = std::fs::remove_dir_all(&dir);
    let logic = psp_mint::mint::open(&dir, Amount::from_units(1024)).unwrap();
    let state = psp_mint::server::app_state(logic);
    let melts = Arc::new(Mutex::new(
        MeltLog::open(&dir.join("melt_memos.json")).unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = psp_mint::server::router(state, health, melts);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn json(url: String) -> serde_json::Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn health_and_audit_name_the_network() {
    let base = start("named", Health::for_network(100, "regtest")).await;
    let health = json(format!("{base}/health")).await;
    assert_eq!(health["network"], "regtest");
    assert_eq!(health["status"], "starting");
    let audit = json(format!("{base}/audit")).await;
    assert_eq!(audit["network"], "regtest");
    assert_eq!(audit["issued"], 0);
    assert_eq!(audit["outstanding"], 0);
}

#[tokio::test]
async fn without_a_network_the_replies_are_as_before() {
    let base = start("plain", Health::new(100)).await;
    assert!(
        json(format!("{base}/health"))
            .await
            .get("network")
            .is_none()
    );
    let audit = json(format!("{base}/audit")).await;
    assert!(audit.get("network").is_none());
    assert_eq!(audit["redeemed"], 0);
}
