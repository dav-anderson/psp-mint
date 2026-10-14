use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mint_types::Amount;
use psp_mint::price::{self, Aggregate, Config, Lazy};
use psp_mint::records::Records;
use psp_mint::rpc::Rpc;
use psp_mint::settle::{LOCK_CONFIRMATIONS, MIN_CONFIRMATIONS, Rejection, Settlement};
use psp_mint::{mint, scan, settle};

const MAX_DENOMINATION_UNITS: u64 = 1_048_576;

fn setting(key: &str, default: String) -> String {
    env::var(key).unwrap_or(default)
}

fn number(key: &str, default: u64) -> Result<u64, Box<dyn std::error::Error>> {
    match env::var(key) {
        Ok(v) => Ok(v.parse().map_err(|_| format!("{key} must be a whole number"))?),
        Err(_) => Ok(default),
    }
}

/// Fetch the three tickers and combine them. Returns whole cents per BTC, or
/// why no price could be taken this run.
fn btc_usd_cents(config: Config) -> Result<u64, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock: {e}"))?
        .as_secs();
    let lookup = price::combine(&price::fetch_all(), now, config);
    for (source, why) in &lookup.excluded {
        println!("  price: excluded {source}: {why}");
    }
    match lookup.aggregate {
        Aggregate::Price { cents, sources } => {
            println!("  price: {} USD/BTC from {sources} sources", cents_text(cents));
            Ok(cents)
        }
        Aggregate::TooFewSources { usable } => {
            Err(format!("only {usable} usable price source(s), need at least 2"))
        }
        Aggregate::Disagree { low, high } => Err(format!(
            "two usable prices disagree: {} and {} USD/BTC",
            cents_text(low),
            cents_text(high)
        )),
    }
}

fn cents_text(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = env::var("HOME")?;
    let base = setting("PSP_RPC_URL", "http://127.0.0.1:18443".to_string());
    let wallet = setting("PSP_WALLET", "psp".to_string());
    let cookie = setting(
        "PSP_COOKIE",
        format!("{home}/Library/Application Support/Bitcoin/regtest/.cookie"),
    );
    let price_config = Config {
        max_age_secs: number("PSP_PRICE_MAX_AGE_SECS", price::DEFAULT_MAX_AGE_SECS)?,
        max_spread_bps: number("PSP_PRICE_MAX_SPREAD_BPS", price::DEFAULT_MAX_SPREAD_BPS)?,
    };
    let data_dir = PathBuf::from(setting("PSP_DATA_DIR", format!("{home}/.psp-mint")));

    fs::create_dir_all(&data_dir)?;
    let logic = mint::open(&data_dir, Amount::from_units(MAX_DENOMINATION_UNITS))?;
    let mut records = Records::open_with_legacy(
        &data_dir.join("settlements.json"),
        &data_dir.join("processed_txids.json"),
    )?;
    let rpc = Rpc::from_cookie(&format!("{base}/wallet/{wallet}"), Path::new(&cookie))?;

    let mut price = Lazy::new(|| btc_usd_cents(price_config));

    let found = scan::received(&rpc, 1000)?;
    if found.is_empty() {
        println!("no payments received by wallet {wallet}");
    }
    for r in &found {
        println!(
            "{} confirmations={} value_sats={}",
            r.txid, r.confirmations, r.value_sats
        );
        match settle::settle(&logic, &mut records, r, &mut || price.get())? {
            Settlement::Waiting { confirmations } => {
                println!("  waiting: {confirmations} of {LOCK_CONFIRMATIONS} confirmation to lock the price")
            }
            Settlement::Locked {
                confirmations,
                price_cents,
            } => println!(
                "  price locked at {} USD/BTC, waiting: {confirmations} of {MIN_CONFIRMATIONS} confirmations",
                cents_text(price_cents)
            ),
            Settlement::AlreadySettled(status) => println!("  already settled ({status:?})"),
            Settlement::PriceUnavailable(why) => {
                println!("  waiting: no price this run ({why}); will retry next run")
            }
            Settlement::Rejected(Rejection::NoDataOutput) => {
                println!("  rejected: no single data output")
            }
            Settlement::Rejected(why) => println!("  rejected: {why:?}"),
            Settlement::Issued(issue) => {
                println!("  over/under: {} bps", issue.over_under_bps);
                if issue.skipped_units > 0 {
                    println!(
                        "  underpaid by more than 1%: {} units not signed, kept by the mint",
                        issue.skipped_units
                    );
                }
                if issue.credit_cents > 0 {
                    // The JAMJAR message asking the payer's DID for a new request
                    // is not built yet. The credit is only recorded.
                    println!(
                        "  overpaid by 10% or more: credit {} USD recorded (JAMJAR message to the payer not built)",
                        cents_text(issue.credit_cents)
                    );
                }
                for result in issue.results {
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
