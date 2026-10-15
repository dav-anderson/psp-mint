use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use mint_server::state::AppState;
use mint_types::Amount;
use psp_mint::price::{self, Config, Lazy};
use psp_mint::records::Records;
use psp_mint::rpc::Rpc;
use psp_mint::settle::Settlement;
use psp_mint::{mint, scan, server, service};

const MAX_DENOMINATION_UNITS: u64 = 1_048_576;

fn setting(key: &str, default: String) -> String {
    env::var(key).unwrap_or(default)
}

fn number(key: &str, default: u64) -> Result<u64, Box<dyn Error>> {
    match env::var(key) {
        Ok(v) => Ok(v
            .parse()
            .map_err(|_| format!("{key} must be a whole number"))?),
        Err(_) => Ok(default),
    }
}

struct Settings {
    rpc_url: String,
    cookie: PathBuf,
    data_dir: PathBuf,
    wallet: String,
    price: Config,
}

impl Settings {
    fn from_env() -> Result<Self, Box<dyn Error>> {
        let home = env::var("HOME")?;
        let base = setting("PSP_RPC_URL", "http://127.0.0.1:18443".to_string());
        let wallet = setting("PSP_WALLET", "psp".to_string());
        Ok(Settings {
            rpc_url: format!("{base}/wallet/{wallet}"),
            cookie: PathBuf::from(setting(
                "PSP_COOKIE",
                format!("{home}/Library/Application Support/Bitcoin/regtest/.cookie"),
            )),
            data_dir: PathBuf::from(setting("PSP_DATA_DIR", format!("{home}/.psp-mint"))),
            wallet,
            price: Config {
                max_age_secs: number("PSP_PRICE_MAX_AGE_SECS", price::DEFAULT_MAX_AGE_SECS)?,
                max_spread_bps: number("PSP_PRICE_MAX_SPREAD_BPS", price::DEFAULT_MAX_SPREAD_BPS)?,
            },
        })
    }

    fn records(&self) -> Result<Records, Box<dyn Error>> {
        Ok(Records::open_with_legacy(
            &self.data_dir.join("settlements.json"),
            &self.data_dir.join("processed_txids.json"),
        )?)
    }

    /// Scan the wallet once and move every payment as far as it can go.
    /// `quiet` leaves out payments that are already settled.
    fn cycle(
        &self,
        state: &AppState,
        records: &mut Records,
        quiet: bool,
    ) -> Result<(), Box<dyn Error>> {
        let rpc = Rpc::from_cookie(&self.rpc_url, Path::new(&self.cookie))?;
        let found = scan::received(&rpc, 1000)?;
        if found.is_empty() && !quiet {
            println!("no payments received by wallet {}", self.wallet);
        }
        let mut price = Lazy::new(|| service::btc_usd_cents(self.price));
        service::run_once(
            &state.logic,
            records,
            &found,
            &mut || price.get(),
            &mut |received, settlement| {
                if quiet && matches!(settlement, Settlement::AlreadySettled(_)) {
                    return;
                }
                for line in service::describe(received, settlement) {
                    println!("{line}");
                }
            },
        )?;
        Ok(())
    }
}

/// Scan the wallet once, settle what is ready, and exit.
fn run_once(settings: &Settings) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(&settings.data_dir)?;
    let logic = mint::open(&settings.data_dir, Amount::from_units(MAX_DENOMINATION_UNITS))?;
    let state = server::app_state(logic);
    let mut records = settings.records()?;
    settings.cycle(&state, &mut records, false)
}

/// Run the HTTP API and rescan the wallet every `PSP_SCAN_INTERVAL_SECS`, in one
/// process. The mint's database can only be opened by one process, so this
/// cannot run at the same time as a one-shot run.
fn serve(settings: Settings) -> Result<(), Box<dyn Error>> {
    let bind = setting("PSP_BIND", "127.0.0.1:3000".to_string());
    let interval = Duration::from_secs(number("PSP_SCAN_INTERVAL_SECS", 120)?.max(1));

    fs::create_dir_all(&settings.data_dir)?;
    let logic = mint::open(&settings.data_dir, Amount::from_units(MAX_DENOMINATION_UNITS))?;
    let state = server::app_state(logic);
    let mut records = settings.records()?;

    // How long without a successful pass before `/health` fails: three
    // intervals, plus a minute for a slow price fetch.
    let health = server::Health::new(interval.as_secs().saturating_mul(3).saturating_add(60));

    // A plain thread, not a task: the price fetch uses a blocking HTTP client,
    // which must not run on the async runtime.
    let scan_state = Arc::clone(&state);
    let scan_health = Arc::clone(&health);
    let scan = thread::spawn(move || {
        println!(
            "scanning wallet {} every {} s",
            settings.wallet,
            interval.as_secs()
        );
        loop {
            match settings.cycle(&scan_state, &mut records, true) {
                Ok(()) => scan_health.record_scan(),
                Err(e) => eprintln!("scan failed, will retry: {e}"),
            }
            thread::sleep(interval);
        }
    });

    // The scan loop never returns, so a finished scan thread means it panicked.
    // A poisoned mint lock means a thread panicked while holding the mint. In
    // both cases the process is no good: exit, and let a supervisor restart it.
    let watch_state = Arc::clone(&state);
    thread::spawn(move || {
        loop {
            if let Some(why) = server::fatal(&watch_state, scan.is_finished()) {
                eprintln!("fatal: {why}. Exiting so that a supervisor can restart the process");
                std::process::exit(1);
            }
            thread::sleep(Duration::from_secs(1));
        }
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind).await?;
        println!("psp-mint api listening on http://{bind}");
        axum::serve(listener, server::router(state, health))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                println!("shutting down");
            })
            .await?;
        Ok::<(), Box<dyn Error>>(())
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let settings = Settings::from_env()?;
    match env::args().nth(1).as_deref() {
        None | Some("run") => run_once(&settings),
        Some("serve") => serve(settings),
        Some(other) => Err(format!("unknown command {other:?}. Use `serve`, or no command to run once").into()),
    }
}
