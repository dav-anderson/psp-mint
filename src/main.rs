use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mint_server::state::AppState;
use mint_types::Amount;
use psp_mint::config::{self, Config};
use psp_mint::delivery::Deliverer;
use psp_mint::melts::MeltLog;
use psp_mint::network::{self, Network};
use psp_mint::price::Lazy;
use psp_mint::records::{Records, SharedRecords};
use psp_mint::rpc::Rpc;
use psp_mint::settle::Settlement;
use psp_mint::{rotation, scan, seal, server, service};
use sp_wire::seal::SecretKey;

/// Load the settings, or say everything that is wrong with them.
fn load_config(data_dir: &Path, flag: Option<Network>) -> Result<config::Loaded, Box<dyn Error>> {
    config::load_with(data_dir, &config::process_env, flag).map_err(|problems| {
        let mut text = String::from("the settings are not valid:");
        for problem in problems {
            text.push_str("\n  ");
            text.push_str(&problem);
        }
        text.into()
    })
}

/// The base data directory: `PSP_DATA_DIR`, or `~/.psp-mint`. The settings
/// file is here; each network's state is in its own folder under it
/// (`main/`, `regtest/`, `signet/`, `testnet/`). Not a setting, because the
/// settings file is in it.
fn data_dir() -> Result<PathBuf, Box<dyn Error>> {
    if let Ok(dir) = env::var("PSP_DATA_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = env::var("HOME")?;
    Ok(PathBuf::from(format!("{home}/.psp-mint")))
}

/// The settings in use and where they came from.
fn describe(loaded: &config::Loaded) -> String {
    let mut out = match &loaded.file {
        Some(file) => format!(
            "psp-mint settings from {}, and the environment:\n",
            file.display()
        ),
        None => "psp-mint settings: no config file, so defaults and the environment:\n".to_string(),
    };
    for line in loaded.describe() {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

struct Settings {
    config: Config,
    /// The network's own folder: keys, notes, records and the checkpoint.
    data_dir: PathBuf,
}

impl Settings {
    /// Read and check the settings once. A change needs a restart.
    fn load(flag: Option<Network>) -> Result<(Self, config::Loaded), Box<dyn Error>> {
        let base = data_dir()?;
        let loaded = load_config(&base, flag)?;
        Ok((
            Settings {
                data_dir: loaded.config.network.data_dir(&base),
                config: loaded.config.clone(),
            },
            loaded,
        ))
    }

    /// bitcoind, without the wallet path.
    fn node(&self) -> Result<Rpc, Box<dyn Error>> {
        let cfg = &self.config;
        Ok(Rpc::from_cookie_with_timeout(
            &cfg.rpc_url,
            Path::new(&cfg.cookie),
            cfg.rpc_timeout_secs,
        )?)
    }

    /// Refuse unless bitcoind is on the selected network.
    fn verify_network(&self) -> Result<(), Box<dyn Error>> {
        network::verify(&self.node()?, self.config.network)?;
        Ok(())
    }

    /// The mint's current key set (shared) and the rotation with the previous one.
    fn open_mint(&self) -> Result<(mint_core::MintLogic, server::SharedRotating), Box<dyn Error>> {
        fs::create_dir_all(&self.data_dir)?;
        let (rotor, logic) = rotation::Rotor::open(
            &self.data_dir,
            Amount::from_units(self.config.max_denomination_units),
            self.config.rotation_activation_hours,
            self.config.rotation_period_hours,
        )?;
        for line in rotor.describe() {
            println!("{line}");
        }
        let rollovers = rotation::Rollovers::open(&self.data_dir)?;
        Ok((logic, server::Rotating::new(rotor, rollovers)))
    }

    /// The mint's DID key, made on first use. Payloads are sealed to its public
    /// half.
    fn did_key(&self) -> Result<SecretKey, Box<dyn Error>> {
        fs::create_dir_all(&self.data_dir)?;
        seal::load_or_generate(&self.data_dir.join(seal::KEY_FILENAME))
    }

    /// The writer of delivery records, when `air.server` and `air.host` are set
    /// (they need no setting of their own: it is the AIR server the listener
    /// uses). `serve` requires them; a one-shot `run` can go without.
    fn deliverer(&self, key: &SecretKey) -> Option<Deliverer> {
        let cfg = &self.config;
        if cfg.air_server.is_empty() || cfg.air_host.is_empty() {
            return None;
        }
        let air = cfg.air_listener(&self.data_dir);
        Some(Deliverer::new(air.server, air.timeout_secs, key.clone()))
    }

    /// Write the records of every purchase that is owed one and has none stored.
    /// Never fails a scan: what could not be written is printed and tried again
    /// on the next pass.
    fn deliver(&self, deliverer: &mut Option<Deliverer>, records: &Mutex<Records>, quiet: bool) {
        match deliverer {
            Some(deliverer) => {
                // The records are locked to plan and to apply, not while the
                // records are written to the AIR server.
                deliverer.pass_shared(records, Instant::now());
            }
            None => {
                let Ok(guard) = service::lock_records(records) else {
                    eprintln!("delivery: the settlement records lock is poisoned; skipped");
                    return;
                };
                let waiting = guard.undelivered().len();
                drop(guard);
                if waiting > 0 && !quiet {
                    println!(
                        "{waiting} purchase(s) are waiting for their signatures to be delivered over \
                         AIR; air.server and air.host are not set, so they stay in the records"
                    );
                }
            }
        }
    }

    /// Forfeit every payment whose reconciliation window has closed with the
    /// payment still open: the mint keeps the BTC and owes nothing, and `/audit`
    /// shows it apart. Never fails a scan. A poisoned records lock is logged and
    /// skipped (the watcher exits the process).
    fn forfeit(&self, records: &Mutex<Records>) {
        let Ok(mut guard) = service::lock_records(records) else {
            eprintln!("forfeit check skipped: the settlement records lock is poisoned");
            return;
        };
        match guard.forfeit_expired(server::now_secs()) {
            Ok(gone) => {
                for txid in gone {
                    println!(
                        "forfeited {txid}: its reconciliation window closed with the payment \
                         unsettled; the mint keeps the BTC"
                    );
                }
            }
            Err(e) => eprintln!("forfeit check failed, will retry: {e}"),
        }
    }

    fn records(&self) -> Result<Records, Box<dyn Error>> {
        Ok(Records::open_with_legacy(
            &self.data_dir.join("settlements.json"),
            &self.data_dir.join("processed_txids.json"),
        )?)
    }

    /// Scan the wallet once and move every payment as far as it can go.
    /// `quiet` leaves out payments that are already settled.
    ///
    /// Payloads are opened with the mint's DID key first. A payment whose data
    /// does not open is not for this mint and is ignored, not rejected.
    fn cycle(
        &self,
        state: &AppState,
        rot: &server::SharedRotating,
        records: &Mutex<Records>,
        did: &SecretKey,
        quiet: bool,
    ) -> Result<(), Box<dyn Error>> {
        let cfg = &self.config;
        let rpc = Rpc::from_cookie_with_timeout(
            &cfg.wallet_url(),
            Path::new(&cfg.cookie),
            cfg.rpc_timeout_secs,
        )?;
        network::verify(&self.node()?, cfg.network)?;
        let checkpoint = scan::Checkpoint::load(&self.data_dir)?;
        // The records are locked only to read what is unsettled, not while the
        // wallet is scanned.
        let unsettled = service::lock_records(records)?.unsettled();
        let since = scan::since(&rpc, checkpoint.as_ref(), cfg.min_confirmations, &unsettled)?;
        let (found, ignored) = seal::open_payloads(did, since.received);
        if !quiet {
            if found.is_empty() && ignored == 0 {
                println!("no payments received by wallet {}", cfg.wallet);
            }
            if ignored > 0 {
                println!("ignored {ignored} payment(s) whose data is not encrypted to this mint");
            }
        }
        // Rotate first, so a settlement signs with the current key set.
        {
            let mut logic = state
                .logic
                .lock()
                .map_err(|_| "the mint lock is poisoned")?;
            rot.rotor
                .lock()
                .map_err(|_| "the rotor lock is poisoned")?
                .refresh(&mut logic)?;
        }
        let price_config = cfg.price();
        let policy = cfg.policy();
        let terms = policy.terms;
        let mut price = Lazy::new(|| service::btc_usd_cents(price_config));
        service::run_once_shared(
            &state.logic,
            Some(&**rot),
            records,
            &found,
            &mut || price.get(),
            &mut |received, settlement| {
                if quiet && matches!(settlement, Settlement::AlreadySettled(_)) {
                    return;
                }
                for line in service::describe_with(received, settlement, &terms) {
                    println!("{line}");
                }
            },
            &policy,
        )?;
        // Only after every payment was processed: a failed pass reads the same
        // blocks again next time.
        since.next.save(&self.data_dir)?;
        Ok(())
    }
}

/// Scan the wallet once, settle what is ready, and exit.
fn run_once(settings: &Settings) -> Result<(), Box<dyn Error>> {
    settings.verify_network()?;
    let (logic, rot) = settings.open_mint()?;
    let state = server::app_state(logic);
    let records = Mutex::new(settings.records()?);
    let did = settings.did_key()?;
    let mut deliverer = settings.deliverer(&did);
    let cycle = settings.cycle(&state, &rot, &records, &did, false);
    // Delivery is tried even when the scan failed: what was issued before is owed.
    settings.deliver(&mut deliverer, &records, false);
    settings.forfeit(&records);
    cycle
}

/// Run the AIR listener (the only way wallets reach the mint), the local HTTP
/// API (`/melt`, `/burns`, `/audit`, `/health`) and rescan the wallet every
/// `scan.interval_secs`, in one process. The mint's database can only be opened by one process, so this
/// cannot run at the same time as a one-shot run.
fn serve(settings: Settings) -> Result<(), Box<dyn Error>> {
    let bind = settings.config.bind.clone();
    let limits = settings.config.limits();
    let interval = Duration::from_secs(settings.config.scan_interval_secs);
    // AIR is the only way wallets reach this mint, so serve does not start
    // without it.
    settings.config.require_air()?;
    settings.verify_network()?;

    let (logic, rot) = settings.open_mint()?;
    let state = server::app_state(logic);
    // One set of records, shared by the scan thread and the AIR listener (the
    // reconciliation requests of kind 15 settle against it). Lock order, everywhere:
    // records, then the mint, then the rotor.
    let records: SharedRecords = Arc::new(Mutex::new(settings.records()?));
    let did = settings.did_key()?;
    let scan_rot = Arc::clone(&rot);
    let air_rot = Arc::clone(&rot);
    // The AIR listener answers wallets' requests, on a thread of its own.
    // Settings are read before `settings` moves into the scan thread.
    let air = settings.config.air_listener(&settings.data_dir);
    let mut deliverer = settings.deliverer(&did);
    let records_path = settings.data_dir.join("settlements.json");

    // How long without a successful pass before `/health` fails:
    // `scan.health_intervals` intervals plus `scan.health_grace_secs`.
    let health = server::Health::for_network(
        settings.config.health_stale_after_secs(),
        settings.config.network.name(),
    );
    let melts: server::SharedMelts = Arc::new(Mutex::new(MeltLog::open(
        &settings.data_dir.join("melt_memos.json"),
    )?));
    start_air_listener(
        air,
        did,
        Arc::clone(&state),
        air_rot,
        Arc::clone(&records),
        Arc::clone(&health),
        limits,
    )?;

    // A plain thread, not a task: the price fetch uses a blocking HTTP client,
    // which must not run on the async runtime.
    let scan_state = Arc::clone(&state);
    let scan_health = Arc::clone(&health);
    let scan_records = Arc::clone(&records);
    let watch_records = Arc::clone(&records);
    let scan = thread::spawn(move || {
        println!(
            "scanning wallet {} every {} s",
            settings.config.wallet,
            interval.as_secs()
        );
        loop {
            match settings.cycle(&scan_state, &scan_rot, &scan_records, &did, true) {
                Ok(()) => scan_health.record_scan(),
                Err(e) => eprintln!("scan failed, will retry: {e}"),
            }
            // After every scan, whether or not it worked: records that could not
            // be written earlier are tried again here, with a backoff of their own.
            settings.deliver(&mut deliverer, &scan_records, true);
            settings.forfeit(&scan_records);
            thread::sleep(interval);
        }
    });

    // The scan loop never returns, so a finished scan thread means it panicked.
    // A poisoned mint lock means a thread panicked while holding the mint; a
    // poisoned records lock means one panicked while holding the records. In
    // all three cases the process is no good: exit, and let a supervisor restart it.
    let watch_state = Arc::clone(&state);
    thread::spawn(move || {
        loop {
            let why = server::fatal(&watch_state, scan.is_finished()).or_else(|| {
                watch_records
                    .is_poisoned()
                    .then_some("the records lock is poisoned")
            });
            if let Some(why) = why {
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
        println!("psp-mint local api (melt, burns, audit, health) listening on http://{bind}");
        axum::serve(
            listener,
            server::router_audited(state, health, melts, limits, Some(rot), Some(records_path)),
        )
        .with_graceful_shutdown(async {
            shutdown_signal().await;
            println!("shutting down");
        })
        .await?;
        Ok::<(), Box<dyn Error>>(())
    })
}

/// Start the AIR listener on its own thread. It answers wallets' requests
/// (`server::air_served_with`, which settles kind 15 against the shared
/// records). If it fails (the AIR client gave up) it says so and
/// the process exits with status 1, so that the service manager starts it again
/// and the listener resumes from the stored slot counter: a mint that no wallet
/// can reach is not running.
fn start_air_listener(
    air: psp_mint::air_listener::Settings,
    did: SecretKey,
    state: Arc<AppState>,
    rot: server::SharedRotating,
    records: SharedRecords,
    health: Arc<server::Health>,
    limits: server::Limits,
) -> Result<(), Box<dyn Error>> {
    thread::Builder::new()
        .name("air-listener".into())
        .spawn(move || {
            let serve = move |kind, body: &[u8]| {
                server::air_served_with(&state, &rot, Some(&records), &health, &limits, kind, body)
            };
            match psp_mint::air_listener::run_blocking(&air, &did, &serve) {
                Ok(()) => eprintln!("AIR listener: stopped"),
                Err(e) => {
                    eprintln!(
                        "AIR listener failed: {e}\nExiting so that a supervisor can restart the process"
                    );
                    std::process::exit(1);
                }
            }
        })?;
    println!("AIR listener starting");
    Ok(())
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM, which is what `launchd` and
/// `systemd` send to stop a service. The server then stops taking new
/// connections, lets the requests it is answering finish, and returns, and the
/// process exits with status 0.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).ok();
    let terminate = async {
        match term.as_mut() {
            Some(t) => {
                t.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate => {}
    }
}

/// Print the mint's DID public key, the one wallets seal payloads to.
fn pubkey(settings: &Settings) -> Result<(), Box<dyn Error>> {
    println!("{}", seal::public_key_hex(&settings.did_key()?));
    Ok(())
}

/// Seal a payload to a mint's DID public key and print the result as hex, for
/// the OP_RETURN. This stands in for a wallet until there is one.
fn encrypt(mint_key: &str, payload_hex: &str) -> Result<(), Box<dyn Error>> {
    let key = seal::parse_public_key(mint_key)?;
    let payload = hex::decode(payload_hex.trim()).map_err(|_| "the payload must be hex")?;
    println!("{}", hex::encode(seal::encrypt(&key, &payload)?));
    Ok(())
}

/// Move the state of a data directory from before networks had their own
/// folders into `regtest/`. Moves, never deletes.
fn migrate() -> Result<(), Box<dyn Error>> {
    let base = data_dir()?;
    let moved = network::migrate_to_regtest(&base)?;
    println!(
        "moved {} into {}",
        moved.join(", "),
        Network::Regtest.data_dir(&base).display()
    );
    println!("nothing was deleted. Start the mint with --regtest to use them");
    Ok(())
}

const USAGE: &str = "\
psp-mint [--mainnet | --regtest | --signet | --testnet] [command]

  serve                                    The AIR listener (needs air.server
                                           and air.host), the local HTTP API,
                                           the wallet scan and the delivery of
                                           each purchase's signatures over
                                           AIR, in one process.
  run                                      The default. Scan the wallet once,
                                           settle what it can, deliver what is
                                           owed (when air.server and air.host
                                           are set), and exit. For testing.
  config                                   Print the settings in use and where
                                           each came from.
  pubkey                                   Print the mint's DID public key (66
                                           hex characters), making the key file
                                           on first use.
  encrypt <mint pubkey hex> <payload hex>  Seal a payload to a public key and
                                           print it as hex, for testing.
  migrate                                  Move data that sits directly in
                                           ~/.psp-mint (from before the
                                           per-network folders) into
                                           ~/.psp-mint/regtest.
  --help                                   Print this text.

The network is --mainnet unless another is given. Settings: psp-mint.toml in
PSP_DATA_DIR (default ~/.psp-mint) or the file PSP_CONFIG names, then PSP_...
variables.";

fn main() {
    // An error is printed as plain text, not as the debug form of the value
    // (`Error: "..."`), which is what returning it from `main` does.
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let flag = network::take_flag(&mut args)?;
    if let [cmd, key, payload] = args.as_slice() {
        if cmd == "encrypt" {
            return encrypt(key, payload);
        }
    }
    if args.first().map(String::as_str) == Some("migrate") {
        return migrate();
    }
    let (settings, loaded) = Settings::load(flag)?;
    if args.first().map(String::as_str) != Some("config") {
        network::refuse_legacy(&data_dir()?)?;
        // On stderr, so that `psp-mint pubkey` (and `encrypt`) print only their
        // answer on stdout and `MINT_KEY=$(psp-mint pubkey)` captures just the key.
        eprintln!(
            "psp-mint on {}: data in {}",
            settings.config.network,
            settings.data_dir.display()
        );
    }
    match args.first().map(String::as_str) {
        None | Some("run") => {
            print!("{}", describe(&loaded));
            run_once(&settings)
        }
        Some("serve") => {
            print!("{}", describe(&loaded));
            serve(settings)
        }
        Some("pubkey") => pubkey(&settings),
        Some("config") => {
            print!("{}", describe(&loaded));
            for line in rotation::describe_offline(
                &settings.data_dir,
                settings.config.rotation_activation_hours,
                settings.config.rotation_period_hours,
            ) {
                println!("{line}");
            }
            Ok(())
        }
        Some(other) => Err(format!(
            "unknown command {other:?}. Use `serve`, `config`, `pubkey`, `migrate`, `encrypt <mint pubkey hex> <payload hex>`, or no command to run once. Add --regtest, --signet or --testnet for a network other than mainnet"
        )
        .into()),
    }
}
