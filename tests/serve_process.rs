//! The real `psp-mint serve` process: how it stops and the live snapshot.
//!
//! Each test runs `psp-mint serve` as a process, with a stand-in bitcoind (a
//! small HTTP server that answers `getblockchaininfo` and refuses every other
//! call, so the scan fails and is retried, which `serve` does not mind) and the
//! real `air_server` of the other tests (`local_server`, port 5702), and sends
//! it the signals an operator sends.
//!
//! What they check:
//!
//! - `SIGTERM` while the mint serves: the process ends with status 0 and says it
//!   stopped (before the fix it panicked on every stop, "there is no reactor
//!   running", and ended with status 101).
//! - `SIGUSR1`: a snapshot appears while a wallet keeps asking and a poller asks
//!   the local API for `/health` on a fresh connection every few milliseconds;
//!   the mint keeps serving, every request a wallet wrote is answered exactly
//!   once, and a second mint started on the snapshot's contents reports the same
//!   `/audit`.
//!
//! The AIR client always dials port 5702, so these tests need it free, and they
//! take turns on the one AIR server (`common::server`).

mod common;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sp_wire::ServiceDid;
use sp_wire::WIRE_VERSION;
use sp_wire::air_request::addr;
use sp_wire::air_request::ask::{AskConfig, AskError, InFlight, Outcome, ask_mint_info};
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};

const WAIT: Duration = Duration::from_secs(60);

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "psp-mint-serve-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Wait until `check` gives `Some`, or fail with `what` after [`WAIT`].
fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + WAIT;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < until, "gave up waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// A stand-in bitcoind: `getblockchaininfo` says regtest, every other call is
/// an error. It runs for the rest of the test process.
fn stand_in_bitcoind() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || answer_rpc(stream));
        }
    });
    port
}

fn answer_rpc(mut stream: TcpStream) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
        }
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..head_end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < head_end + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
        }
    }
    let call: Value =
        serde_json::from_slice(&bytes[head_end..head_end + length]).unwrap_or_default();
    let body = if call["method"] == "getblockchaininfo" {
        json!({"result": {"chain": "regtest", "blocks": 0}, "error": null, "id": "psp-mint"})
    } else {
        json!({"result": null, "error": {"code": -32601, "message": "Method not found"}, "id": "psp-mint"})
    }
    .to_string();
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(reply.as_bytes()).ok();
}

/// A mint's place on disk and its settings.
struct Setup {
    base: PathBuf,
    rpc_port: u16,
    server: ServerRef,
    did: ServiceDid,
}

impl Setup {
    fn new(name: &str, server: &ServerRef) -> Setup {
        let base = scratch(name);
        let rpc_port = stand_in_bitcoind();
        fs::write(base.join("cookie"), "user:password").unwrap();
        let mut setup = Setup {
            base,
            rpc_port,
            server: server.clone(),
            did: addr::key_to_did(&sp_wire::seal::SecretKey::new().public_key()),
        };
        setup.write_settings(free_port());
        // The mint makes its DID key on first use; ask for its public half.
        let out = setup
            .command(&["--regtest", "pubkey"])
            .output()
            .expect("run psp-mint pubkey");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let hex = String::from_utf8(out.stdout).unwrap();
        setup.did = addr::key_to_did(&sp_wire::seal::parse_public_key(hex.trim()).unwrap());
        setup
    }

    /// The settings file.
    fn write_settings(&mut self, bind_port: u16) {
        let text = format!(
            "[server]\nbind = \"127.0.0.1:{bind_port}\"\n[bitcoin]\nrpc_url = \"http://127.0.0.1:{}\"\ncookie = \"{}\"\n[air]\nserver = \"{}\"\nhost = \"{}\"\n",
            self.rpc_port,
            self.base.join("cookie").display(),
            self.server.name,
            self.server.host,
        );
        fs::write(self.base.join("psp-mint.toml"), text).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_psp-mint"));
        command
            .args(args)
            .env_clear()
            .env("PSP_DATA_DIR", &self.base)
            .env("HOME", &self.base)
            .stdin(Stdio::null());
        command
    }

    fn bind_port(&self) -> u16 {
        let text = fs::read_to_string(self.base.join("psp-mint.toml")).unwrap();
        let line = text.lines().find(|l| l.starts_with("bind")).unwrap();
        line.rsplit(':')
            .next()
            .unwrap()
            .trim_end_matches('"')
            .parse()
            .unwrap()
    }
}

/// A running `psp-mint serve`.
struct Mint {
    child: Child,
    log: PathBuf,
    health: String,
}

impl Mint {
    fn start(setup: &Setup, name: &str) -> Mint {
        let log = setup.base.with_file_name(format!(
            "{}-{name}.log",
            setup.base.file_name().unwrap().to_string_lossy()
        ));
        let out = File::create(&log).unwrap();
        let err = out.try_clone().unwrap();
        let child = setup
            .command(&["--regtest", "serve"])
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("start psp-mint serve");
        let mint = Mint {
            child,
            log,
            health: format!("http://127.0.0.1:{}/health", setup.bind_port()),
        };
        wait_for("the mint to answer /health", || mint.health().map(|_| ()));
        mint
    }

    /// The code and body of `/health`, or `None` if it did not answer.
    fn health(&self) -> Option<(u16, Value)> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(0)
            .build()
            .unwrap();
        let response = client.get(&self.health).send().ok()?;
        let code = response.status().as_u16();
        Some((code, response.json().ok()?))
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(self.child.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{name} failed");
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// `SIGTERM`, and the status the process ends with.
    fn stop(mut self) -> ExitStatus {
        self.signal("TERM");
        wait_for("the mint to exit", || self.child.try_wait().unwrap())
    }
}

impl Drop for Mint {
    fn drop(&mut self) {
        // A test that failed must not leave a mint holding the AIR mailbox.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn ask_config(server: &ServerRef, did: &ServiceDid) -> AskConfig {
    AskConfig {
        server: server.clone(),
        mint: *did,
        wire_version: WIRE_VERSION,
        network: common::NETWORK.to_string(),
        // Long enough that no try is repeated while the mint is stopped for the
        // snapshot, so a request is written once.
        reply_wait_secs: 60,
        attempts: 1,
        max_skew_secs: 300,
        timeout_secs: 10,
    }
}

/// One mint-info request, the way the wallet runs it.
fn ask_info(cfg: &AskConfig) -> Result<Outcome, AskError> {
    async fn go(cfg: &AskConfig, flight: &mut InFlight) -> Result<Outcome, AskError> {
        let client = client::connect(&cfg.server).await?;
        ask_mint_info(&client, cfg, flight, &mut |_| Ok(())).await
    }
    let mut flight = InFlight::new();
    block_on_new_runtime(|| go(cfg, &mut flight))
        .map_err(AskError::from)
        .and_then(|r| r)
}

/// A wallet that keeps asking, one request after another.
struct Asker {
    stop: Arc<AtomicBool>,
    answered: Arc<AtomicU64>,
    thread: JoinHandle<(Vec<String>, Vec<String>)>,
}

impl Asker {
    fn start(server: &ServerRef, did: &ServiceDid) -> Asker {
        let cfg = ask_config(server, did);
        let stop = Arc::new(AtomicBool::new(false));
        let answered = Arc::new(AtomicU64::new(0));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            let answered = Arc::clone(&answered);
            move || {
                let (mut ids, mut failures) = (Vec::new(), Vec::new());
                while !stop.load(Ordering::SeqCst) {
                    match ask_info(&cfg) {
                        Ok(out) => {
                            ids.push(hex::encode(out.request_id));
                            answered.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => failures.push(e.to_string()),
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                (ids, failures)
            }
        });
        Asker {
            stop,
            answered,
            thread,
        }
    }

    fn answered(&self) -> u64 {
        self.answered.load(Ordering::SeqCst)
    }

    /// Wait for `more` further answers.
    fn wait_for_more(&self, more: u64) {
        let goal = self.answered() + more;
        wait_for("the wallet's requests to be answered", || {
            (self.answered() >= goal).then_some(())
        });
    }

    /// Stop asking; the ids of the requests answered and the failures.
    fn finish(self) -> (Vec<String>, Vec<String>) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().unwrap()
    }
}

/// What a [`Poller`] saw.
struct Polled {
    /// Requests that failed twice in a row: a connection the mint refused or lost.
    errors: Vec<String>,
    /// Requests that failed once and worked at once after (see [`Poller`]).
    retried: u64,
    longest: Duration,
    polls: u64,
}

/// Polls `/health` on a fresh connection every few milliseconds and keeps what
/// went wrong and the longest wait.
///
/// One failure is allowed to be tried again at once, and is counted: a
/// connection that the local API had accepted but whose request it had not read
/// yet is closed when it stops, which no server can avoid (a connection not yet
/// accepted waits in the socket's queue and is never closed). A client of the
/// local API that retries, as the SPI and esp-mint do, never notices it.
struct Poller {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Polled>,
}

impl Poller {
    fn start(url: String) -> Poller {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .pool_max_idle_per_host(0)
                    .build()
                    .unwrap();
                let mut seen = Polled {
                    errors: Vec::new(),
                    retried: 0,
                    longest: Duration::ZERO,
                    polls: 0,
                };
                while !stop.load(Ordering::SeqCst) {
                    let began = Instant::now();
                    match client.get(&url).send() {
                        Ok(_) => seen.polls += 1,
                        Err(_) => match client.get(&url).send() {
                            Ok(_) => seen.retried += 1,
                            Err(e) => seen.errors.push(e.to_string()),
                        },
                    }
                    seen.longest = seen.longest.max(began.elapsed());
                    thread::sleep(Duration::from_millis(5));
                }
                seen
            }
        });
        Poller { stop, thread }
    }

    fn finish(self) -> Polled {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().unwrap()
    }
}

/// How many times `needle` is in `text`.
fn count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

// One test at a time on the AIR server and on its port, which `common::server`
// makes sure of; this keeps the tests' own mints from overlapping as well.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

#[test]
fn sigterm_while_serving_ends_the_process_with_status_0_and_says_it_stopped() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let server = common::server();
    let setup = Setup::new("sigterm", &server);
    let mut mint = Mint::start(&setup, "sigterm");
    assert!(mint.running());

    let log_path = mint.log.clone();
    let status = mint.stop();
    let log = fs::read_to_string(&log_path).unwrap();
    // The exit status is the check: a panic in the stop path ends the process
    // with 101, after the line that says the stop began.
    assert_eq!(status.code(), Some(0), "{log}");
    assert_eq!(count(&log, "psp-mint: SIGTERM, shutting down"), 1, "{log}");
    assert_eq!(
        count(&log, "psp-mint: stopped, the data is closed"),
        1,
        "{log}"
    );
    assert_eq!(count(&log, "panicked"), 0, "{log}");
}

/// The files under `dir` as (path relative to `dir`, bytes), sorted.
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let name = path.strip_prefix(root).unwrap().to_path_buf();
                out.push((name, fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn a_snapshot_is_taken_while_serving_and_starts_a_mint_with_the_same_audit() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let server = common::server();
    let setup = Setup::new("snapshot", &server);
    let mut mint = Mint::start(&setup, "snapshot");
    let asker = Asker::start(&server, &setup.did);
    let poller = Poller::start(mint.health.clone());
    asker.wait_for_more(2);

    mint.signal("USR1");
    let network = setup.base.join("regtest");
    let latest = network.join("snapshots").join("LATEST");
    let name = wait_for("snapshots/LATEST", || {
        fs::read_to_string(&latest)
            .ok()
            .map(|n| n.trim().to_string())
    });
    wait_for("the mint to open its data again", || {
        mint.log()
            .contains("the mint opens its data again")
            .then_some(())
    });
    assert!(mint.running());
    asker.wait_for_more(2);
    let (ids, failures) = asker.finish();
    let polled = poller.finish();
    assert!(failures.is_empty(), "{failures:?}");
    assert!(polled.errors.is_empty(), "{:?}", polled.errors);
    assert!(polled.retried <= 1, "{}", polled.retried);
    println!(
        "snapshot: the local API's longest wait was {:?}",
        polled.longest
    );
    let log = mint.log();
    assert_eq!(count(&log, "already answered"), 0, "{log}");
    for id in &ids {
        assert_eq!(count(&log, &format!("answered request {id}")), 1, "{id}");
    }
    let audit_before: Value =
        reqwest::blocking::get(format!("http://127.0.0.1:{}/audit", setup.bind_port()))
            .unwrap()
            .json()
            .unwrap();
    assert!(mint.stop().success());

    // The mint's own files are in the snapshot, as they were.
    let snapshot = network.join("snapshots").join(&name);
    assert!(snapshot.is_dir());
    assert_eq!(
        fs::read(snapshot.join("mint_did_key.json")).unwrap(),
        fs::read(network.join("mint_did_key.json")).unwrap(),
        "mint_did_key.json"
    );

    // A second mint on the snapshot's contents, in a base folder of its own and
    // on another port.
    let mut restored = Setup {
        base: scratch("snapshot-restored"),
        rpc_port: setup.rpc_port,
        server: setup.server.clone(),
        did: setup.did,
    };
    restored.write_settings(free_port());
    fs::write(restored.base.join("cookie"), "user:password").unwrap();
    // The settings name the cookie in the first base folder; that is fine.
    fs::create_dir_all(restored.base.join("regtest")).unwrap();
    for (path, bytes) in tree(&snapshot) {
        let to = restored.base.join("regtest").join(path);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::write(to, bytes).unwrap();
    }
    let second = Mint::start(&restored, "snapshot-restored");
    let audit_after: Value =
        reqwest::blocking::get(format!("http://127.0.0.1:{}/audit", restored.bind_port()))
            .unwrap()
            .json()
            .unwrap();
    assert_eq!(audit_before, audit_after);
    assert!(second.stop().success());
}
