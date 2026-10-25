use std::env;
use std::error::Error;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use mint_core::state::AppState;
use mint_types::Amount;
use psp_mint::config::{self, Config};
use psp_mint::delivery::Deliverer;
use psp_mint::network::{self, Network};
use psp_mint::price::Lazy;
use psp_mint::records::{Records, SharedRecords};
use psp_mint::rpc::Rpc;
use psp_mint::stop::Stop;
use psp_mint::{rotation, seal, server, service, snapshot};
use sp_wire::seal::SecretKey;
use tokio::signal::unix::{Signal, SignalKind, signal};

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
    if let Ok(dir) = env::var("PSP_DATA_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
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

#[derive(Clone)]
struct Settings {
    config: Config,
    /// The network's own folder: keys, notes, records and the checkpoint.
    data_dir: PathBuf,
    /// The base folder, where the settings file is, and the network flag the
    /// process was started with: what a reload (`SIGHUP`) needs to read the
    /// settings again.
    base: PathBuf,
    flag: Option<Network>,
}

impl Settings {
    /// Read and check the settings at the start. `serve` reads them again on
    /// `SIGHUP` (`config::Handle`).
    fn load(flag: Option<Network>) -> Result<(Self, config::Loaded), Box<dyn Error>> {
        let base = data_dir()?;
        let loaded = load_config(&base, flag)?;
        Ok((
            Settings {
                data_dir: loaded.config.network.data_dir(&base),
                config: loaded.config.clone(),
                base,
                flag,
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

    /// [`Settings::verify_network`] when `serve` opens the mint. At the first
    /// start it is exact. When `serve` opens it again (after a snapshot), a bitcoind that cannot be asked right now does not stop it,
    /// because a mint that is down for that is worse than one that scans again
    /// when bitcoind is back: the scan checks the network at every pass and
    /// refuses a wrong one. A bitcoind that answers with another network is
    /// refused either way.
    fn verify_network_at_open(&self, reopening: bool) -> Result<(), Box<dyn Error>> {
        if !reopening {
            return self.verify_network();
        }
        let node = match self.node() {
            Ok(node) => node,
            Err(e) => {
                eprintln!(
                    "psp-mint: cannot reach bitcoind to check its network ({e}); opening anyway, the scan checks it at every pass"
                );
                return Ok(());
            }
        };
        match network::verify(&node, self.config.network) {
            Err(e) if e.starts_with("cannot ask bitcoind") => {
                eprintln!(
                    "psp-mint: {e}; opening anyway, the scan checks the network at every pass"
                );
                Ok(())
            }
            other => Ok(other?),
        }
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
        Some(Deliverer::new(air.server, air.timeout_secs, *key))
    }

    fn records(&self) -> Result<Records, Box<dyn Error>> {
        Ok(Records::open(&self.data_dir.join("settlements.json"))?)
    }

    /// Scan the wallet once and move every payment as far as it can go
    /// (`service::cycle`), after checking that bitcoind is on the selected
    /// network. `quiet` leaves out payments that are already settled.
    fn cycle(
        &self,
        cfg: &Config,
        state: &AppState,
        rot: &server::SharedRotating,
        records: &Mutex<Records>,
        did: &SecretKey,
        quiet: bool,
    ) -> Result<(), Box<dyn Error>> {
        let rpc = Rpc::from_cookie_with_timeout(
            &cfg.wallet_url(),
            Path::new(&cfg.cookie),
            cfg.rpc_timeout_secs,
        )?;
        network::verify(
            &Rpc::from_cookie_with_timeout(
                &cfg.rpc_url,
                Path::new(&cfg.cookie),
                cfg.rpc_timeout_secs,
            )?,
            cfg.network,
        )?;
        let price_config = cfg.price();
        let policy = cfg.policy();
        let mut price = Lazy::new(|| service::btc_usd_cents(price_config));
        let pass = service::Pass {
            data_dir: &self.data_dir,
            wallet: &cfg.wallet,
            min_confirmations: cfg.min_confirmations,
            policy: &policy,
            did,
            quiet,
        };
        service::cycle(&pass, &rpc, state, rot, records, &mut || price.get())
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
    let cycle = settings.cycle(&settings.config, &state, &rot, &records, &did, false);
    // Delivery is tried even when the scan failed: what was issued before is owed.
    service::deliver(&mut deliverer, &records, false);
    service::forfeit(&records, server::now_secs());
    cycle
}

/// How long the local API gets to finish the requests it is answering when the
/// mint stops (for the snapshot or an exit). A request that is still running
/// then is cut off, so that every store can be dropped.
const DRAIN_SECS: u64 = 30;

/// What one run of the mint ended with. It is returned only after every thread
/// has stopped and every store has been dropped, so the files are closed.
enum Outcome {
    /// `SIGTERM` or `SIGINT`: the process ends.
    Exit,
    /// `SIGUSR1`: copy the data folder, then run again.
    Snapshot,
}

/// What ended the wait of one run.
enum Trigger {
    Exit,
    Snapshot,
    /// The local API stopped without being asked to: the process is no good.
    ApiEnded(String),
}

/// The signal streams `serve` listens to. They are made once, before anything
/// is opened, and kept for the life of the process: a signal that arrives
/// between two runs of the mint waits in its stream and is handled by the next,
/// and none ever takes its default action, which would end the process.
struct Signals {
    /// Read the settings again.
    hangup: Signal,
    /// Copy the data folder.
    snapshot: Signal,
    terminate: Signal,
    interrupt: Signal,
}

impl Signals {
    fn new(runtime: &tokio::runtime::Runtime) -> std::io::Result<Self> {
        let _inside = runtime.enter();
        Ok(Signals {
            hangup: signal(SignalKind::hangup())?,
            snapshot: signal(SignalKind::user_defined1())?,
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }
}

/// What lives as long as the process: the runtime that waits for signals, the
/// settings handle, the live ceilings and the local API's listening socket.
struct Process {
    /// Waits for signals, on the main thread. The local API has a runtime of
    /// its own for each run, which is dropped at its end.
    runtime: tokio::runtime::Runtime,
    signals: Signals,
    handle: Arc<config::Handle>,
    live: Arc<server::LiveLimits>,
    /// The local API's socket, bound once and kept open across runs, so that a
    /// connection made while the mint is between two runs (a snapshot) waits in
    /// the kernel's queue and is not refused.
    listener: TcpListener,
}

impl Process {
    /// Whether `SIGTERM` or `SIGINT` has come in. It is looked for after the
    /// mint has been stopped for a snapshot, so that a stop that
    /// came meanwhile is not lost: the mint exits instead of opening again.
    fn stop_requested(&mut self) -> bool {
        // A few milliseconds, so that the runtime has read the signal pipe.
        let wait = Duration::from_millis(5);
        let signals = &mut self.signals;
        self.runtime.block_on(async {
            tokio::select! {
                Ok(_) = tokio::time::timeout(wait, signals.terminate.recv()) => true,
                Ok(_) = tokio::time::timeout(wait, signals.interrupt.recv()) => true,
                else => false,
            }
        })
    }

    /// Open everything, run until a signal asks for something else, then stop
    /// everything: the scan, the AIR listener, the local API and the watch, in
    /// that order, each joined, and drop every store. `reopening`: this is not
    /// the first run of this process and `bitcoind` may be the reason it cannot
    /// be asked for its network now (see `Settings::verify_network_at_open`).
    fn serve_once(
        &mut self,
        settings: &Settings,
        reopening: bool,
    ) -> Result<Outcome, Box<dyn Error>> {
        // AIR is the only way wallets reach this mint, so serve does not start
        // without it.
        settings.config.require_air()?;
        settings.verify_network_at_open(reopening)?;

        let (logic, rot) = settings.open_mint()?;
        let state = server::app_state(logic);
        // One set of records, shared by the scan thread and the AIR listener (the
        // reconciliation requests of kind 15 settle against it). Lock order, everywhere:
        // records, then the mint, then the rotor.
        let records: SharedRecords = Arc::new(Mutex::new(settings.records()?));
        let did = settings.did_key()?;
        let air = settings.config.air_listener(&settings.data_dir);
        let deliverer = settings.deliverer(&did);
        let records_path = settings.data_dir.join("settlements.json");

        // How long without a successful pass before `/health` fails:
        // `scan.health_intervals` intervals plus `scan.health_grace_secs`.
        let health = server::Health::for_network(
            settings.config.health_stale_after_secs(),
            settings.config.network.name(),
        );
        // The burn memos are opened with the rotation (a rollover and a spend over
        // AIR clear them, `/melt` records them); an unreadable file stops serve here.
        let melts: server::SharedMelts = Arc::clone(rot.melts()?);

        // The AIR listener answers wallets' requests, on a thread of its own.
        let air_thread = start_air_listener(
            air,
            did,
            Arc::clone(&state),
            Arc::clone(&rot),
            Arc::clone(&records),
            Arc::clone(&health),
            Arc::clone(&self.live),
        )?;

        // The scan is a plain thread, not a task: the price fetch uses a
        // blocking HTTP client, which must not run on the async runtime.
        let scan_stop = Arc::new(Stop::new());
        let scan_ended = Arc::new(AtomicBool::new(false));
        let scan = thread::Builder::new().name("scan".into()).spawn({
            let settings = settings.clone();
            let state = Arc::clone(&state);
            let health = Arc::clone(&health);
            let records = Arc::clone(&records);
            let rot = Arc::clone(&rot);
            let handle = Arc::clone(&self.handle);
            let stop = Arc::clone(&scan_stop);
            let ended = Arc::clone(&scan_ended);
            let mut deliverer = deliverer;
            move || {
                // Set when the thread ends in any way, a panic included.
                let _ended = Ended(ended);
                println!(
                    "scanning wallet {} every {} s",
                    settings.config.wallet, settings.config.scan_interval_secs
                );
                loop {
                    // The settings in force for this pass; a reload applies from the next.
                    let cfg = handle.current();
                    match settings.cycle(&cfg, &state, &rot, &records, &did, true) {
                        Ok(()) => health.record_scan(),
                        Err(e) => eprintln!("scan failed, will retry: {e}"),
                    }
                    // After every scan, whether or not it worked: records that could not
                    // be written earlier are tried again here, with a backoff of their own.
                    service::deliver(&mut deliverer, &records, true);
                    service::forfeit(&records, server::now_secs());
                    // And the PSP reissued to the SPI's wallet that a spend could not
                    // write to `revenue/` when it was made.
                    if let Ok(mut store) = rot.rollovers.lock()
                        && let Err(e) = store.flush_revenue()
                    {
                        eprintln!("could not write the reissued PSP to revenue/, will retry: {e}");
                    }
                    if stop.wait(Duration::from_secs(handle.current().scan_interval_secs)) {
                        return;
                    }
                }
            }
        })?;

        // A scan thread that ends without being asked to has panicked. A
        // poisoned mint lock means a thread panicked while holding the mint; a
        // poisoned records lock means one panicked while holding the records. In
        // all three cases the process is no good: exit, and let a supervisor restart it.
        let watch_stop = Arc::new(Stop::new());
        let watch = thread::Builder::new().name("watch".into()).spawn({
            let state = Arc::clone(&state);
            let records = Arc::clone(&records);
            let scan_stop = Arc::clone(&scan_stop);
            let stop = Arc::clone(&watch_stop);
            move || {
                loop {
                    let scan_lost = scan_ended.load(Ordering::SeqCst) && !scan_stop.is_fired();
                    let why = server::fatal(&state, scan_lost).or_else(|| {
                        records
                            .is_poisoned()
                            .then_some("the records lock is poisoned")
                    });
                    if let Some(why) = why {
                        eprintln!(
                            "fatal: {why}. Exiting so that a supervisor can restart the process"
                        );
                        std::process::exit(1);
                    }
                    if stop.wait(Duration::from_secs(1)) {
                        return;
                    }
                }
            }
        })?;

        // The local API, on a runtime of its own that is dropped at the end of
        // the run, so that nothing it was doing can keep a store open. It
        // serves on a copy of the listening socket: the original stays open.
        let http = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("psp-http")
            .build()?;
        let router = server::router_live(
            Arc::clone(&state),
            Arc::clone(&health),
            melts,
            Arc::clone(&self.live),
            Arc::clone(&rot),
            records_path,
        );
        let api = self.listener.try_clone()?;
        api.set_nonblocking(true)?;
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let mut api_task = {
            let _inside = http.enter();
            let listener = tokio::net::TcpListener::from_std(api)?;
            let shutdown = Arc::clone(&shutdown);
            http.spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move { shutdown.notified().await })
                    .await
            })
        };
        let mut api_done = false;

        // Wait for something to do. SIGHUP reads the settings again; a bad file
        // changes nothing. The settings with `reload` false in `config::SPECS`
        // wait for a restart: a reload names them and leaves them as they are.
        let Process {
            runtime,
            signals,
            handle,
            live,
            ..
        } = self;
        let trigger = runtime.block_on(async {
            loop {
                tokio::select! {
                    _ = signals.hangup.recv() => {
                        match handle.reload(&settings.base, &config::process_env) {
                            Ok(reloaded) => {
                                let now = handle.current();
                                live.set(now.limits());
                                health.set_stale_after_secs(now.health_stale_after_secs());
                                println!("psp-mint: SIGHUP, settings read again:");
                                for line in &reloaded.lines {
                                    println!("{line}");
                                }
                                if !reloaded.needs_restart.is_empty() {
                                    let keys: Vec<&str> =
                                        reloaded.needs_restart.iter().map(|(key, _)| *key).collect();
                                    println!(
                                        "psp-mint: {} need a restart, which is a brief downtime, so they keep their running values until the mint is restarted",
                                        keys.join(", ")
                                    );
                                }
                            }
                            Err(problems) => {
                                eprintln!("psp-mint: SIGHUP, the settings are not valid, so none changed:");
                                for problem in problems {
                                    eprintln!("  {problem}");
                                }
                            }
                        }
                    }
                    _ = signals.snapshot.recv() => {
                        println!("psp-mint: SIGUSR1, taking a snapshot of the data folder");
                        break Trigger::Snapshot;
                    }
                    _ = signals.terminate.recv() => {
                        println!("psp-mint: SIGTERM, shutting down");
                        break Trigger::Exit;
                    }
                    _ = signals.interrupt.recv() => {
                        println!("psp-mint: SIGINT, shutting down");
                        break Trigger::Exit;
                    }
                    ended = &mut api_task => {
                        api_done = true;
                        break Trigger::ApiEnded(match ended {
                            Ok(Ok(())) => "the local API stopped".to_string(),
                            Ok(Err(e)) => format!("the local API failed: {e}"),
                            Err(e) => format!("the local API task failed: {e}"),
                        });
                    }
                }
            }
        });

        // Stop everything, so that every store is dropped. The order keeps the
        // mint answering for as long as it can: the scan first (wallets and the
        // local API are served meanwhile), then the AIR listener (a request it
        // has not read waits in its slot), then the local API (a connection
        // made now waits in the socket's queue), then the watch.
        scan_stop.fire();
        if scan.join().is_err() {
            eprintln!("psp-mint: the scan thread had panicked");
        }
        air_thread.stop();
        shutdown.notify_one();
        if !api_done {
            // The timeout is made inside the runtime: `tokio::time::timeout`
            // needs the runtime's timer when it is created, and outside it the
            // process panics ("there is no reactor running") on every stop.
            let drained = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(DRAIN_SECS), &mut api_task).await
            });
            if drained.is_err() {
                eprintln!(
                    "psp-mint: the local API still had requests running after {DRAIN_SECS} s; they are cut off"
                );
            }
        }
        // Drops every task and connection the runtime still has.
        http.shutdown_timeout(Duration::from_secs(5));
        watch_stop.fire();
        if watch.join().is_err() {
            eprintln!("psp-mint: the watch thread had panicked");
        }
        // The scan and the AIR listener are joined, the local API is gone and
        // the watch is joined: what is left of the mint's stores, the mint, the
        // records, the rotation, goes at the end of this function, and sled
        // writes everything and lets go of the files then. A task that outlived
        // the 5 s above would still hold them. Whether any does is checked here,
        // because a snapshot copied from files that are still open would not be
        // whole, and the mint cannot open them again either.
        let holding: Vec<&str> = [
            ("the mint", Arc::strong_count(&state)),
            ("the rotation", Arc::strong_count(&rot)),
            ("the records", Arc::strong_count(&records)),
        ]
        .into_iter()
        .filter(|(_, handles)| *handles > 1)
        .map(|(what, _)| what)
        .collect();
        if !holding.is_empty() {
            let why = format!(
                "a task still holds {} after the local API was stopped",
                holding.join(" and ")
            );
            if matches!(trigger, Trigger::Snapshot) {
                return Err(format!(
                    "the snapshot is refused: {why}, so a copy of the data folder would not be whole and it cannot be opened again in this process. Exiting so that a supervisor starts the mint again"
                )
                .into());
            }
            eprintln!(
                "psp-mint: warning: {why}, so the data may not have been flushed before the stop"
            );
        }

        match trigger {
            Trigger::Exit => Ok(Outcome::Exit),
            Trigger::Snapshot => Ok(Outcome::Snapshot),
            Trigger::ApiEnded(why) => Err(why.into()),
        }
    }
}

/// Sets a flag when it is dropped: the scan thread's way of saying that it has
/// ended, by a stop or by a panic.
struct Ended(Arc<AtomicBool>);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Run the AIR listener (the only way wallets reach the mint), the local HTTP
/// API (`/melt`, `/burns`, `/audit`, `/health`) and rescan the wallet every
/// `scan.interval_secs`, in one process. The mint's database can only be opened by one process, so this
/// cannot run at the same time as a one-shot run.
///
/// The data folder can be copied without the local API ever refusing a
/// connection and without a wallet's request being lost (`snapshot.rs`,
/// DESIGN.md "Snapshots"):
///
/// - `SIGHUP`: read the settings again. The live ones apply at once; the ones
///   that need a restart are named and keep their running values.
/// - `SIGUSR1`: a snapshot of the data folder, taken with the stores dropped.
/// - `SIGTERM`, `SIGINT`: stop, with the stores dropped, and exit.
fn serve(settings: Settings) -> Result<(), Box<dyn Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // Listened for from the first moment, so that a signal sent while the mint
    // is starting is kept and does not take its default action, which ends the
    // process.
    let signals = Signals::new(&runtime)?;
    let listener = TcpListener::bind(&settings.config.bind)?;
    println!(
        "psp-mint local api (melt, burns, audit, health) listening on http://{}",
        listener.local_addr()?
    );
    let mut process = Process {
        runtime,
        signals,
        handle: Arc::new(config::Handle::new(settings.config.clone(), settings.flag)),
        live: Arc::new(server::LiveLimits::new(settings.config.limits())),
        listener,
    };
    let mut reopening = false;
    loop {
        // The settings in force: the file's, but for the ones that need a
        // restart, which are the ones this process started with.
        let mut now = settings.clone();
        now.config = (*process.handle.current()).clone();
        let outcome = process.serve_once(&now, reopening)?;
        reopening = true;
        match outcome {
            Outcome::Exit => {
                // After every thread is joined and every store is dropped.
                println!("psp-mint: stopped, the data is closed");
                return Ok(());
            }
            Outcome::Snapshot => {
                let started = std::time::Instant::now();
                match snapshot::take(&now.data_dir, server::now_secs()) {
                    Ok(folder) => println!(
                        "psp-mint: snapshot written to {} in {:.1} s; the mint opens its data again",
                        folder.display(),
                        started.elapsed().as_secs_f64()
                    ),
                    Err(e) => eprintln!(
                        "psp-mint: the snapshot failed, the mint opens its data again and keeps serving: {e}"
                    ),
                }
            }
        }
        if process.stop_requested() {
            println!("psp-mint: a stop came meanwhile, so the mint exits");
            return Ok(());
        }
    }
}

/// The AIR listener's thread, with the way to stop it.
struct AirThread {
    stop: tokio::sync::oneshot::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl AirThread {
    /// Ask the listener to stop and wait for it. It finishes the slot it is
    /// answering first, and leaves the slots it has not read in the AIR server,
    /// where the next listener finds them (it resumes from the stored counter).
    fn stop(self) {
        let AirThread { stop, thread } = self;
        stop.send(()).ok();
        if thread.join().is_err() {
            eprintln!("psp-mint: the AIR listener thread had panicked");
        }
    }
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
    limits: Arc<server::LiveLimits>,
) -> Result<AirThread, Box<dyn Error>> {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let thread = thread::Builder::new()
        .name("air-listener".into())
        .spawn(move || {
            let serve = move |kind, body: &[u8]| {
                // The ceilings and burn rule in force when the request comes.
                let now = limits.get();
                server::air_served_with(&state, &rot, &records, &health, &now, kind, body)
            };
            match psp_mint::air_listener::run_blocking(&air, &did, &serve, stopped) {
                Ok(()) => println!("AIR listener: stopped"),
                Err(e) => {
                    eprintln!(
                        "AIR listener failed: {e}\nExiting so that a supervisor can restart the process"
                    );
                    std::process::exit(1);
                }
            }
        })?;
    println!("AIR listener starting");
    Ok(AirThread { stop, thread })
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

const USAGE: &str = "\
psp-mint [--mainnet | --regtest | --signet | --testnet] [command]

  serve                                    The AIR listener (needs air.server
                                           and air.host), the local HTTP API,
                                           the wallet scan and the delivery of
                                           each purchase's signatures over
                                           AIR, in one process. Signals:
                                           HUP reads the settings again, USR1
                                           copies the data folder, TERM and INT
                                           stop it.
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
  --help                                   Print this text.
  --version                                Print the version of psp-mint and of
                                           the libraries it was built with.

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
    if args.iter().any(|a| a == "--version" || a == "-V") {
        for line in psp_mint::version::lines() {
            println!("{line}");
        }
        return Ok(());
    }
    let flag = network::take_flag(&mut args)?;
    // `encrypt` needs no settings, so it is answered before they are read.
    if args.first().map(String::as_str) == Some("encrypt") {
        return match args.as_slice() {
            [_, key, payload] => encrypt(key, payload),
            _ => Err("encrypt takes a mint public key and a payload, both hex: \
                 psp-mint encrypt <mint pubkey hex> <payload hex>"
                .into()),
        };
    }
    let (settings, loaded) = Settings::load(flag)?;
    if args.first().map(String::as_str) != Some("config") {
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
            "unknown command {other:?}. Use `serve`, `config`, `pubkey`, `encrypt <mint pubkey hex> <payload hex>`, or no command to run once. Add --regtest, --signet or --testnet for a network other than mainnet"
        )
        .into()),
    }
}
