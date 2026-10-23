//! The `psp-mint` command line, run as a process: what each command prints, the
//! exit status, and what it leaves on disk. Every run gets an empty environment
//! (`env_clear`) with its own `HOME` and `PSP_DATA_DIR`, so nothing of the
//! machine's settings, keys or records is read or written, and a deadline, so a
//! command that hangs fails the test instead of the run.
//!
//! Not covered, because they need a running bitcoind: a scan that finds payments
//! (`run`, `serve`'s scan thread; their pass is tested through the library in
//! `tests/cycle.rs`), `serve` past its start-up checks (it needs bitcoind on the
//! right network, and the AIR server on port 5702 for its listener), and the
//! network check against a real node's `getblockchaininfo`
//! (`network::verify` is tested with a stand-in node).

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use psp_mint::config::SPECS;
use psp_mint::seal;

/// The generator point of secp256k1: a valid compressed public key to seal to.
const SOME_KEY: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

const DEADLINE: Duration = Duration::from_secs(60);

/// A folder of its own for one run: the data directory and a home.
struct Sandbox {
    root: PathBuf,
    data: PathBuf,
    home: PathBuf,
}

fn sandbox(name: &str) -> Sandbox {
    static N: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "psp-mint-cli-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::remove_dir_all(&root).ok();
    let (data, home) = (root.join("data"), root.join("home"));
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&home).unwrap();
    Sandbox { root, data, home }
}

/// What a run printed and how it ended.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Sandbox {
    fn run(&self, args: &[&str]) -> Ran {
        self.run_with(args, &[])
    }

    /// Run `psp-mint args` with only `HOME`, `PSP_DATA_DIR` and `env` set.
    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Ran {
        let out = self.root.join("stdout.txt");
        let err = self.root.join("stderr.txt");
        let mut command = Command::new(env!("CARGO_BIN_EXE_psp-mint"));
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PSP_DATA_DIR", &self.data)
            .stdin(Stdio::null())
            .stdout(fs::File::create(&out).unwrap())
            .stderr(fs::File::create(&err).unwrap());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("the psp-mint binary starts");
        let until = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= until {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "`psp-mint {}` did not finish in {DEADLINE:?}",
                    args.join(" ")
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Ran {
            code: status.code(),
            stdout: fs::read_to_string(out).unwrap(),
            stderr: fs::read_to_string(err).unwrap(),
        }
    }

    /// The network's folder for `name` under the data directory.
    fn network_dir(&self, name: &str) -> PathBuf {
        self.data.join(name)
    }

    /// Every file and folder under the data directory, relative to it.
    fn written(&self) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                out.push(path.strip_prefix(base).unwrap().display().to_string());
                if path.is_dir() {
                    walk(&path, base, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.data, &self.data, &mut out);
        out.sort();
        out
    }
}

/// The error line a failed run ends with.
fn last_line(text: &str) -> &str {
    text.trim_end().lines().last().unwrap_or("")
}

// ---- help and unknown input ----

#[test]
fn help_prints_the_usage_and_nothing_else_and_names_no_removed_command() {
    let s = sandbox("help");
    let long = s.run(&["--help"]);
    let short = s.run(&["-h"]);
    assert_eq!((long.code, short.code), (Some(0), Some(0)));
    assert_eq!(long.stdout, short.stdout);
    assert_eq!((long.stderr.as_str(), short.stderr.as_str()), ("", ""));
    assert!(
        long.stdout
            .starts_with("psp-mint [--mainnet | --regtest | --signet | --testnet] [command]\n"),
        "{}",
        long.stdout
    );
    for command in ["serve", "run", "config", "pubkey", "encrypt"] {
        assert!(
            long.stdout.contains(&format!("\n  {command} ")),
            "`{command}` is not described in\n{}",
            long.stdout
        );
    }
    assert!(!long.stdout.contains("migrate"), "{}", long.stdout);
    // `--help` wins wherever it is, and nothing is read or written
    let after = s.run(&["serve", "--help"]);
    assert_eq!(
        (after.code, after.stdout.as_str()),
        (Some(0), long.stdout.as_str())
    );
    assert!(s.written().is_empty());
}

#[test]
fn an_unknown_command_is_refused_and_the_removed_migrate_is_unknown() {
    let s = sandbox("unknown");
    for word in ["frobnicate", "migrate"] {
        let ran = s.run(&[word]);
        assert_eq!(ran.code, Some(1), "{word}");
        assert_eq!(ran.stdout, "", "{word}");
        assert_eq!(
            last_line(&ran.stderr),
            format!(
                "error: unknown command \"{word}\". Use `serve`, `config`, `pubkey`, `encrypt <mint pubkey hex> <payload hex>`, or no command to run once. Add --regtest, --signet or --testnet for a network other than mainnet"
            ),
            "{word}"
        );
    }
    // an unknown flag is an unknown command, not a network
    let ran = s.run(&["--regnet"]);
    assert_eq!(ran.code, Some(1));
    assert!(
        last_line(&ran.stderr).starts_with("error: unknown command \"--regnet\"."),
        "{}",
        ran.stderr
    );
    assert!(s.written().is_empty());
}

#[test]
fn two_different_network_flags_are_refused_before_anything_is_read() {
    let s = sandbox("two-flags");
    let ran = s.run(&["--regtest", "--signet", "config"]);
    assert_eq!(ran.code, Some(1));
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        "error: both --regtest and --signet were given: choose one network\n"
    );
    // the same flag twice is one network
    let twice = s.run(&["--regtest", "--regtest", "config"]);
    assert_eq!(twice.code, Some(0), "{}", twice.stderr);
    assert!(s.written().is_empty());
}

// ---- encrypt ----

#[test]
fn encrypt_seals_to_the_key_and_the_mint_opens_it() {
    let s = sandbox("encrypt");
    let pubkey = s.run(&["pubkey"]);
    assert_eq!(pubkey.code, Some(0), "{}", pubkey.stderr);
    let key = pubkey.stdout.trim().to_string();

    let sealed = s.run(&["encrypt", &key, "deadbeef"]);
    assert_eq!(sealed.code, Some(0), "{}", sealed.stderr);
    // nothing but the answer: no settings were read, so no start-up line either
    assert_eq!(sealed.stderr, "");
    let hex_text = sealed.stdout.trim_end();
    assert_eq!(sealed.stdout, format!("{hex_text}\n"));
    let bytes = hex::decode(hex_text).expect("the answer is hex");
    // 4 bytes and the 80 bytes of sealing overhead
    assert_eq!(bytes.len(), 4 + 80);
    let mint = seal::load_or_generate(&s.network_dir("main").join(seal::KEY_FILENAME)).unwrap();
    assert_eq!(
        seal::decrypt(&mint, &bytes).unwrap(),
        vec![0xde, 0xad, 0xbe, 0xef]
    );

    // sealing twice gives different bytes
    let again = s.run(&["encrypt", &key, "deadbeef"]);
    assert_ne!(again.stdout, sealed.stdout);
}

#[test]
fn encrypt_refuses_a_bad_key_a_bad_payload_and_prints_nothing() {
    let s = sandbox("encrypt-bad");
    // not hex, and hex of the wrong length (a 32-byte x-only key)
    for key in ["zz", &SOME_KEY[2..]] {
        let ran = s.run(&["encrypt", key, "00"]);
        assert_eq!(ran.code, Some(1), "{key}");
        assert_eq!(ran.stdout, "", "{key}");
        assert!(
            ran.stderr.starts_with("error: not a public key:") && ran.stderr.ends_with('\n'),
            "{key}: {}",
            ran.stderr
        );
        assert_eq!(ran.stderr.lines().count(), 1, "{key}: {}", ran.stderr);
    }
    // a payload that is not hex (an odd number of digits is not hex either)
    for payload in ["xyz", "abc"] {
        let ran = s.run(&["encrypt", SOME_KEY, payload]);
        assert_eq!(ran.code, Some(1), "{payload}");
        assert_eq!(ran.stdout, "", "{payload}");
        assert_eq!(ran.stderr, "error: the payload must be hex\n", "{payload}");
    }
    assert!(s.written().is_empty());
}

#[test]
fn encrypt_with_the_wrong_number_of_arguments_says_what_it_takes() {
    let s = sandbox("encrypt-args");
    for args in [
        vec!["encrypt"],
        vec!["encrypt", SOME_KEY],
        vec!["encrypt", SOME_KEY, "00", "00"],
    ] {
        let ran = s.run(&args);
        assert_eq!(ran.code, Some(1), "{args:?}");
        assert_eq!(ran.stdout, "", "{args:?}");
        assert_eq!(
            ran.stderr.trim_end(),
            "error: encrypt takes a mint public key and a payload, both hex: \
             psp-mint encrypt <mint pubkey hex> <payload hex>",
            "{args:?}"
        );
        // nothing was read or written: the settings are not loaded
        assert!(s.written().is_empty(), "{args:?}");
    }
}

// ---- pubkey ----

#[test]
fn pubkey_makes_the_key_once_and_prints_the_same_key_every_time() {
    let s = sandbox("pubkey");
    let first = s.run(&["pubkey"]);
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let key = first.stdout.trim_end();
    assert_eq!(first.stdout, format!("{key}\n"));
    assert_eq!(key.len(), 66);
    assert!(key.starts_with("02") || key.starts_with("03"), "{key}");
    assert!(key.bytes().all(|b| b.is_ascii_hexdigit()), "{key}");
    // the start-up line is on stderr, so `$(psp-mint pubkey)` is just the key
    assert_eq!(
        first.stderr,
        format!(
            "psp-mint on main: data in {}\n",
            s.network_dir("main").display()
        )
    );
    assert_eq!(s.written(), ["main", "main/mint_did_key.json"]);

    let second = s.run(&["pubkey"]);
    assert_eq!(second.stdout, first.stdout);
}

#[test]
fn pubkey_with_a_damaged_key_file_fails_and_leaves_the_file_alone() {
    let s = sandbox("pubkey-damaged");
    fs::create_dir_all(s.network_dir("main")).unwrap();
    let file = s.network_dir("main").join("mint_did_key.json");
    fs::write(&file, "not a key").unwrap();

    let ran = s.run(&["pubkey"]);
    assert_eq!(ran.code, Some(1));
    assert_eq!(ran.stdout, "");
    assert!(
        last_line(&ran.stderr)
            .starts_with(&format!("error: {}: not a DID key file:", file.display())),
        "{}",
        ran.stderr
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "not a key");
    assert_eq!(s.written(), ["main", "main/mint_did_key.json"]);
}

// ---- config ----

#[test]
fn config_prints_every_setting_and_where_it_came_from_and_writes_nothing() {
    let s = sandbox("config");
    let ran = s.run_with(&["--regtest", "config"], &[("PSP_BIND", "127.0.0.1:4000")]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(ran.stderr, "", "`config` prints no start-up line");
    assert!(
        ran.stdout
            .starts_with("psp-mint settings: no config file, so defaults and the environment:\n"),
        "{}",
        ran.stdout
    );
    let lines: Vec<&str> = ran.stdout.lines().collect();
    // one line for each documented setting, in the order of the table
    for spec in SPECS {
        let prefix = format!("  {} = ", spec.key);
        assert_eq!(
            lines.iter().filter(|l| l.starts_with(&prefix)).count(),
            1,
            "{} is not printed once in\n{}",
            spec.key,
            ran.stdout
        );
    }
    assert!(
        lines.contains(&"  server.bind = 127.0.0.1:4000 host:port (env PSP_BIND)"),
        "{}",
        ran.stdout
    );
    let network = lines
        .iter()
        .find(|l| l.starts_with("  bitcoin.network = "))
        .unwrap();
    assert!(
        network.starts_with("  bitcoin.network = regtest ")
            && network.ends_with("(command-line flag)"),
        "{network}"
    );
    let wallet = lines
        .iter()
        .find(|l| l.starts_with("  bitcoin.wallet = "))
        .unwrap();
    assert!(
        wallet.starts_with("  bitcoin.wallet = psp-regtest "),
        "{wallet}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("rotation: activation ")),
        "{}",
        ran.stdout
    );
    assert!(s.written().is_empty());
}

#[test]
fn config_refuses_range_settings_all_at_once() {
    let s = sandbox("config-range");
    let ran = s.run_with(
        &["config"],
        &[
            ("PSP_SCAN_INTERVAL_SECS", "0"),
            ("PSP_RECONCILIATION_WINDOW_DAYS", "0"),
            ("PSP_RECONCILIATION_ROUND_UP_FROM", "11"),
        ],
    );
    assert_eq!(ran.code, Some(1));
    assert_eq!(ran.stdout, "");
    assert_eq!(
        ran.stderr,
        "error: the settings are not valid:\n\
         \x20 scan.interval_secs = 0 is out of range: it must be from 1 to 86400 (seconds)\n\
         \x20 settle.reconciliation_window_days = 0 is out of range: it must be from 1 to 365 (days)\n\
         \x20 reconciliation.round_up_from = 11 is out of range: it must be from 1 to 10 (cents; 10 never rounds up)\n"
    );
    assert!(s.written().is_empty());
}

#[test]
fn config_refuses_a_fee_over_the_most_a_fee_may_be() {
    let s = sandbox("config-fee");
    // 2^62 is the most; one more is refused, and 2^62 itself is not
    let over = s.run_with(
        &["config"],
        &[("PSP_RECONCILIATION_FEE", "4611686018427387905")],
    );
    assert_eq!(over.code, Some(1));
    assert_eq!(
        over.stderr,
        "error: the settings are not valid:\n\
         \x20 reconciliation.fee = 4611686018427387905 is out of range: it must be from 0 to 4611686018427387904 (whole PSP units; 0 takes no fee)\n"
    );
    let at = s.run_with(
        &["config"],
        &[("PSP_RECONCILIATION_FEE", "4611686018427387904")],
    );
    assert_eq!(at.code, Some(0), "{}", at.stderr);
    assert!(
        at.stdout.contains(
            "  reconciliation.fee = 4611686018427387904 PSP units (env PSP_RECONCILIATION_FEE)"
        ),
        "{}",
        at.stdout
    );
}

#[test]
fn config_names_the_variable_of_every_bad_value() {
    let s = sandbox("config-env");
    let ran = s.run_with(
        &["config"],
        &[("PSP_SCAN_INTERVAL_SECS", "often"), ("PSP_NETWORK", "moon")],
    );
    assert_eq!(ran.code, Some(1));
    assert_eq!(
        ran.stderr,
        "error: the settings are not valid:\n\
         \x20 environment variable PSP_NETWORK: `moon` is not a network: use main, regtest, signet or testnet\n\
         \x20 environment variable PSP_SCAN_INTERVAL_SECS: scan.interval_secs = `often` is not a whole number (no sign, no decimals)\n"
    );
    assert!(s.written().is_empty());
}

#[test]
fn config_says_air_enabled_is_gone_from_the_file_and_from_the_environment() {
    let s = sandbox("config-air-enabled");
    let file = s.data.join("psp-mint.toml");
    fs::write(&file, "[air]\nenabled = true\n").unwrap();
    let from_file = s.run(&["config"]);
    assert_eq!(from_file.code, Some(1));
    let line = last_line(&from_file.stderr);
    assert!(
        line.starts_with(&format!(
            "  {}: air.enabled is gone: the AIR listener always runs",
            file.display()
        )) && line.ends_with("(PSP_AIR_SERVER, PSP_AIR_HOST)"),
        "{}",
        from_file.stderr
    );
    fs::remove_file(&file).unwrap();
    let from_env = s.run_with(&["config"], &[("PSP_AIR_ENABLED", "true")]);
    assert_eq!(from_env.code, Some(1));
    assert!(
        last_line(&from_env.stderr)
            .starts_with("  environment variable PSP_AIR_ENABLED: air.enabled is gone:"),
        "{}",
        from_env.stderr
    );
}

// ---- serve ----

#[test]
fn serve_without_the_air_settings_says_which_are_missing_before_anything_is_opened() {
    let s = sandbox("serve-air");
    let neither = s.run(&["serve"]);
    assert_eq!(neither.code, Some(1));
    // the settings were described, then the refusal; nothing was bound, opened
    // or made
    assert!(
        neither.stdout.starts_with("psp-mint settings: "),
        "{}",
        neither.stdout
    );
    let line = last_line(&neither.stderr);
    assert!(
        line.starts_with(
            "error: serve needs the AIR settings, because AIR is the only way wallets reach this mint, and air.server (PSP_AIR_SERVER) and air.host (PSP_AIR_HOST) are not set."
        ),
        "{line}"
    );
    assert!(s.written().is_empty());

    let host_only = s.run_with(&["serve"], &[("PSP_AIR_HOST", "127.0.0.1")]);
    assert_eq!(host_only.code, Some(1));
    assert!(
        last_line(&host_only.stderr).contains("and air.server (PSP_AIR_SERVER) is not set."),
        "{}",
        host_only.stderr
    );
    assert!(!last_line(&host_only.stderr).contains("air.host (PSP_AIR_HOST)"));
    assert!(s.written().is_empty());
}

// ---- run, with bitcoind unreachable ----

/// A loopback port nothing listens on.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[test]
fn run_with_bitcoind_unreachable_fails_clearly_and_writes_nothing() {
    let s = sandbox("run-down");
    let cookie = s.root.join("cookie");
    fs::write(&cookie, "__cookie__:secret").unwrap();
    let url = format!("http://127.0.0.1:{}", closed_port());
    let env = [
        ("PSP_RPC_URL", url.as_str()),
        ("PSP_COOKIE", cookie.to_str().unwrap()),
        ("PSP_RPC_TIMEOUT_SECS", "5"),
    ];
    // no command is `run`; both end the same way
    for args in [vec!["--regtest"], vec!["--regtest", "run"]] {
        let ran = s.run_with(&args, &env);
        assert_eq!(ran.code, Some(1), "{args:?}");
        assert!(
            ran.stdout.starts_with("psp-mint settings: "),
            "{args:?}: {}",
            ran.stdout
        );
        let line = last_line(&ran.stderr);
        assert!(
            line.starts_with(
                "error: cannot ask bitcoind which network it is on: rpc request failed:"
            ),
            "{args:?}: {}",
            ran.stderr
        );
        assert!(
            ran.stderr.starts_with(&format!(
                "psp-mint on regtest: data in {}\n",
                s.network_dir("regtest").display()
            )),
            "{args:?}: {}",
            ran.stderr
        );
        // no key, records, checkpoint or keys were made
        assert!(s.written().is_empty(), "{args:?}: {:?}", s.written());
    }
}

#[test]
fn run_without_the_rpc_cookie_says_so_and_writes_nothing() {
    let s = sandbox("run-cookie");
    let missing = s.root.join("no-cookie");
    let ran = s.run_with(&["--regtest"], &[("PSP_COOKIE", missing.to_str().unwrap())]);
    assert_eq!(ran.code, Some(1));
    assert!(
        last_line(&ran.stderr).starts_with(&format!(
            "error: cannot read rpc cookie: {}:",
            missing.display()
        )),
        "{}",
        ran.stderr
    );
    assert!(s.written().is_empty());
}
