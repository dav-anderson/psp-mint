use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use mint_types::Amount;
use psp_mint::processed::Processed;
use psp_mint::rpc::Rpc;
use psp_mint::settle::{MIN_CONFIRMATIONS, Settlement};
use psp_mint::{mint, scan, settle};

const MAX_DENOMINATION_UNITS: u64 = 1_048_576;

fn setting(key: &str, default: String) -> String {
    env::var(key).unwrap_or(default)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = env::var("HOME")?;
    let base = setting("PSP_RPC_URL", "http://127.0.0.1:18443".to_string());
    let wallet = setting("PSP_WALLET", "psp".to_string());
    let cookie = setting(
        "PSP_COOKIE",
        format!("{home}/Library/Application Support/Bitcoin/regtest/.cookie"),
    );
    let data_dir = PathBuf::from(setting("PSP_DATA_DIR", format!("{home}/.psp-mint")));

    fs::create_dir_all(&data_dir)?;
    let logic = mint::open(&data_dir, Amount::from_units(MAX_DENOMINATION_UNITS))?;
    let mut processed = Processed::open(&data_dir.join("processed_txids.json"))?;
    let rpc = Rpc::from_cookie(&format!("{base}/wallet/{wallet}"), Path::new(&cookie))?;

    let found = scan::received(&rpc, 1000)?;
    if found.is_empty() {
        println!("no payments received by wallet {wallet}");
    }
    for r in &found {
        println!(
            "{} confirmations={} value_sats={}",
            r.txid, r.confirmations, r.value_sats
        );
        match settle::settle(&logic, &mut processed, r)? {
            Settlement::Waiting { confirmations } => {
                println!("  waiting: {confirmations} of {MIN_CONFIRMATIONS} confirmations")
            }
            Settlement::AlreadyProcessed => println!("  already processed"),
            Settlement::NoPayload => println!("  skipped: no single data output"),
            Settlement::Rejected(why) => println!("  rejected: {why:?}"),
            Settlement::Issued(results) => {
                for result in results {
                    match result {
                        Ok(signature) => println!("  signature {}", signature.0),
                        Err(e) => println!("  not issued: {e}"),
                    }
                }
            }
        }
    }
    Ok(())
}
