//! The PSP mint's settings: one file, `psp-mint.toml`, with environment
//! overrides.
//!
//! Nothing that an operator might want to tune is a constant in the code. Every
//! address, path, timeout, rate, threshold and limit is a setting listed in
//! `SPECS` (which is also what the README's table and the startup log are made
//! from), with its default, its unit and what it affects.
//!
//! Where the values come from, strongest first:
//!
//! 1. an environment variable (the `env` column of `SPECS`);
//! 2. the file `psp-mint.toml` in the data directory, or the file `PSP_CONFIG`
//!    names (which must exist);
//! 3. the default.
//!
//! The file is TOML in sections (`[settle]`, `min_confirmations = 6`). A key the
//! mint does not know is an error, so a misspelt setting never silently does
//! nothing. So is a value of the wrong type or out of range: the mint lists
//! every problem and does not start.
//!
//! The settings are read once, at startup. There is no reload: a change takes
//! effect at the next start. `PSP_DATA_DIR` is not a setting, because the file
//! lives in it.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sp_wire::storage::StoragePolicy;

use crate::price;
use crate::rpc;
use crate::settle::{self, Terms};

/// The name of the file looked for in the data directory.
pub const FILE_NAME: &str = "psp-mint.toml";

/// The shortest PSP rotation period the mint starts with (rotation on).
/// BTC->PSP claims need time to settle (about an hour for 6 confirmations,
/// longer in slow blocks) before a rotation; a shorter period would let the
/// claim hold run into the next rotation. A hard limit, for PSP only.
pub const MIN_PSP_ROTATION_PERIOD_HOURS: u64 = 24;
/// The environment variable that names a config file elsewhere.
pub const FILE_ENV: &str = "PSP_CONFIG";
/// The default ceiling on one request body (1 MiB).
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// The smallest ceiling accepted: a request with one note has to fit.
pub const MIN_REQUEST_BYTES: u64 = 4096;
/// The default largest denomination, in PSP units.
pub const DEFAULT_MAX_DENOMINATION_UNITS: u64 = 16_777_216;
/// The default time between scans in `serve`.
pub const DEFAULT_SCAN_INTERVAL_SECS: u64 = 120;
/// The default number of scan intervals without a successful scan before
/// `/health` fails.
pub const DEFAULT_HEALTH_INTERVALS: u64 = 3;
/// The default extra time, on top of the intervals, before `/health` fails
/// (room for a slow price fetch).
pub const DEFAULT_HEALTH_GRACE_SECS: u64 = 60;
/// The cookie path under `$HOME`.
pub use crate::network::Network;

/// Why `scan.max_transactions` is refused now.
pub const RETIRED_MAX_TRANSACTIONS: &str = "scan.max_transactions is retired: the scan now reads every block since its checkpoint (listsinceblock), so there is no window to size. Remove it";

/// Why `settle.underpay_tolerance_bps` and `settle.credit_threshold_bps` are refused now.
pub const RETIRED_TOLERANCES: &str = "is retired: one setting, settle.payment_reconciliation_target (a fraction; 0.10 is 10%), now covers a payment that is short and one that is over. Remove it";

/// Why `air.enabled` is refused now.
pub const GONE_AIR_ENABLED: &str = "air.enabled is gone: the AIR listener always runs, because AIR is the only way wallets reach this mint (there is no HTTP API for them). Remove air.enabled; serve needs air.server and air.host (PSP_AIR_SERVER, PSP_AIR_HOST)";

/// One setting, as documented.
pub struct Spec {
    /// `section.name`, as in the file.
    pub key: &'static str,
    /// The environment variable that overrides it.
    pub env: &'static str,
    /// The default, as written in the file.
    pub default: &'static str,
    pub units: &'static str,
    pub what: &'static str,
}

pub const SPECS: &[Spec] = &[
    Spec {
        key: "server.bind",
        env: "PSP_BIND",
        default: "127.0.0.1:3000",
        units: "host:port",
        what: "The address of the HTTP API that is left (`serve` only): `/melt` and `/burns` (the ESP mint's calls, inside the SPI), `/audit` (the operator's report) and `/health` (read by the SPI and the health monitor). Must be a loopback address: wallets reach this mint over AIR only, never over HTTP.",
    },
    Spec {
        key: "server.melt_max_request_bytes",
        env: "PSP_MELT_MAX_BYTES",
        default: "1048576",
        units: "bytes",
        what: "The ceiling on one `POST /melt` body (1 MiB). A larger body is refused with 413 before it is parsed: nothing is burned.",
    },
    Spec {
        key: "server.rollover_max_request_bytes",
        env: "PSP_ROLLOVER_MAX_BYTES",
        default: "1048576",
        units: "bytes",
        what: "The ceiling on one rollover request (1 MiB), which comes over AIR. A larger body is refused with 413 before it is parsed: nothing is burned.",
    },
    Spec {
        key: "rollover.max_items",
        env: "PSP_ROLLOVER_MAX_ITEMS",
        default: "1000",
        units: "notes",
        what: "The most notes in one rollover request. More is refused 400 before anything is burned.",
    },
    Spec {
        key: "server.burns_max_request_bytes",
        env: "PSP_BURNS_MAX_BYTES",
        default: "1048576",
        units: "bytes",
        what: "The ceiling on one `POST /burns` body (1 MiB). A larger body is refused with 413 before it is parsed.",
    },
    Spec {
        key: "server.check_state_max_request_bytes",
        env: "PSP_CHECK_STATE_MAX_BYTES",
        default: "1048576",
        units: "bytes",
        what: "The ceiling on one check-state request (1 MiB), which comes over AIR. A larger body is refused with 413 before it is parsed.",
    },
    Spec {
        key: "bitcoin.network",
        env: "PSP_NETWORK",
        default: "main",
        units: "network",
        what: "main, regtest, signet or testnet. `--regtest`, `--signet` or `--testnet` on the command line overrides it. Picks the defaults below and the data folder.",
    },
    Spec {
        key: "bitcoin.rpc_url",
        env: "PSP_RPC_URL",
        default: "http://127.0.0.1:8332",
        units: "URL",
        what: "Bitcoin Core's RPC address, without the wallet path. Default per network: port 8332 main, 18443 regtest, 38332 signet, 18332 testnet.",
    },
    Spec {
        key: "bitcoin.wallet",
        env: "PSP_WALLET",
        default: "psp-main",
        units: "name",
        what: "The Bitcoin Core wallet that receives payments. Default per network: psp-main, psp-regtest, psp-signet, psp-testnet.",
    },
    Spec {
        key: "bitcoin.cookie",
        env: "PSP_COOKIE",
        default: "$HOME/Library/Application Support/Bitcoin/.cookie (macOS), $HOME/.bitcoin/.cookie (Linux)",
        units: "path",
        what: "Bitcoin Core's RPC cookie file. Default per network: bitcoind's own (`.cookie`, `regtest/.cookie`, `signet/.cookie`, `testnet3/.cookie` under its data directory).",
    },
    Spec {
        key: "bitcoin.rpc_timeout_secs",
        env: "PSP_RPC_TIMEOUT_SECS",
        default: "30",
        units: "seconds",
        what: "How long one RPC call may take before the scan fails and is retried.",
    },
    Spec {
        key: "scan.interval_secs",
        env: "PSP_SCAN_INTERVAL_SECS",
        default: "120",
        units: "seconds",
        what: "Time between wallet scans (`serve` only).",
    },
    Spec {
        key: "scan.health_intervals",
        env: "PSP_HEALTH_INTERVALS",
        default: "3",
        units: "scan intervals",
        what: "`/health` fails after this many scan intervals plus `scan.health_grace_secs` without a successful scan (`serve` only).",
    },
    Spec {
        key: "scan.health_grace_secs",
        env: "PSP_HEALTH_GRACE_SECS",
        default: "60",
        units: "seconds",
        what: "Added to `scan.health_intervals` intervals before `/health` fails, for a slow price fetch.",
    },
    Spec {
        key: "price.max_age_secs",
        env: "PSP_PRICE_MAX_AGE_SECS",
        default: "120",
        units: "seconds",
        what: "A quote whose time is further than this from now is excluded.",
    },
    Spec {
        key: "price.max_spread_bps",
        env: "PSP_PRICE_MAX_SPREAD_BPS",
        default: "200",
        units: "basis points",
        what: "With two usable quotes, wait if they differ by more than this of the lower one (2%).",
    },
    Spec {
        key: "price.timeout_secs",
        env: "PSP_PRICE_TIMEOUT_SECS",
        default: "10",
        units: "seconds",
        what: "How long one ticker request may take.",
    },
    Spec {
        key: "settle.lock_confirmations",
        env: "PSP_LOCK_CONFIRMATIONS",
        default: "1",
        units: "confirmations",
        what: "A payment's price is locked when it is first seen with this many confirmations.",
    },
    Spec {
        key: "settle.min_confirmations",
        env: "PSP_MIN_CONFIRMATIONS",
        default: "6",
        units: "confirmations",
        what: "A payment is settled, at its locked price, once it has this many confirmations.",
    },
    Spec {
        key: "settle.price_tolerance",
        env: "PSP_PRICE_TOLERANCE",
        default: "0.01",
        units: "fraction, 0 to 1 (0.01 is 1%)",
        what: "How far a payment may miss the requested amount, over or under, and still settle in full as if it were exact: every requested note is signed, and nothing is credited, dropped or kept. It covers the BTC price moving between the price lock (settle.lock_confirmations) and settlement (settle.min_confirmations), which no payer should be penalised or credited for. Widen it in a volatile market. It must not be more than settle.payment_reconciliation_target (the mint refuses to start otherwise), because that setting handles the payments outside it. See the README.",
    },
    Spec {
        key: "settle.payment_reconciliation_target",
        env: "PSP_PAYMENT_RECONCILIATION_TARGET",
        default: "0.10",
        units: "fraction, 0 to 1 (0.10 is 10%)",
        what: "For a payment outside settle.price_tolerance, which is genuinely wrong: how far it may miss the requested amount before the mint stops absorbing the difference and reconciles it. Over by no more than this: the mint keeps the excess; over by more: the excess is recorded as credit for the payer's DID. Short: the smallest requested outputs are dropped until the signed total is no more than was paid; if that is within this of the amount paid, those notes are signed and the mint keeps the remainder; otherwise nothing is signed and the payment is held as paid but unsettled. Tighter is fairer to payers but sends more payments to credit or reconciliation; looser settles more automatically but keeps or forgoes more value. See the README.",
    },
    Spec {
        key: "settle.reconciliation_window_days",
        env: "PSP_RECONCILIATION_WINDOW_DAYS",
        default: "14",
        units: "days",
        what: "How long a credit, a held payment (paid but unsettled) or a rejected purchase stays open, counted from when the mint settles the payment. The payer is told over AIR, and the window is in the record. When it closes with the payment still open, the mint forfeits it: it keeps the BTC, owes no credit, and /audit shows it apart. The payer's wallet deletes the purchase's key at the same time. Nothing settles an open payment yet, so today every one is forfeited when its window closes. 1 to 365.",
    },
    Spec {
        key: "mint.max_denomination_units",
        env: "PSP_MAX_DENOMINATION_UNITS",
        default: "16777216",
        units: "PSP units",
        what: "The largest denomination (powers of two from 1). Only used when the mint's keys are first made; an existing `mint_keys.json` keeps its denominations.",
    },
    Spec {
        key: "rotation.activation_hours",
        env: "PSP_ROTATION_ACTIVATION_HOURS",
        default: "8760",
        units: "hours",
        what: "Hours from the mint's first issuance (recorded once in first_issue.json) to its first key rotation. 0 = never rotate (one key set, one spent list).",
    },
    Spec {
        key: "rotation.period_hours",
        env: "PSP_ROTATION_PERIOD_HOURS",
        default: "87600",
        units: "hours",
        what: "Hours between key rotations (10 years). Notes of the previous key set stay good until the next rotation; then that set and its spent list are deleted. 0 = never rotate. When rotation is on it must be at least 24: BTC claims need time to settle before a rotation (the mint refuses to start otherwise).",
    },
    Spec {
        key: "storage.mint_state",
        env: "PSP_STORAGE_MINT_STATE",
        default: "",
        units: "service DIDs (66 hex characters)",
        what: "Which AIR services keep this mint's state (keys, spent lists, records), as an ordered list of service IDs: index 0 is the primary and the rest are secondaries, in the order they would be promoted. A service ID is the service's AIR DID, a 33-byte compressed key in hex (66 characters, starting 02 or 03); the old 32-byte x-only form is refused. In the environment the IDs are separated by commas; in the file it is an array of strings or one such string. One entry: that service holds the state and the mint keeps a local backup copy. Empty (the default): local only. At most 8, no duplicates. Setting only for now: nothing reads or writes the AIR yet (see DESIGN.md in sandbox-mint, \"AIR storage requirements\").",
    },
    Spec {
        key: "air.server",
        env: "PSP_AIR_SERVER",
        default: "",
        units: "AIR server name (66 hex characters)",
        what: "The AIR server the listener holds reads on and writes replies to: its identity, a public key in AIR's compressed hex form (66 characters). The listener checks the server's signed challenge against it. Required by serve: the AIR listener always runs, because AIR is the only way wallets reach this mint. Until the AIR resolves a DID to a server (upstream item 4), this is configuration.",
    },
    Spec {
        key: "air.host",
        env: "PSP_AIR_HOST",
        default: "",
        units: "host or IP, no port",
        what: "Where that AIR server is: a host name or an IP (127.0.0.1 for the SPI's own AIR server). No port: the AIR client always uses 5702 (ws). Required by serve, with air.server.",
    },
    Spec {
        key: "air.held_reads",
        env: "PSP_AIR_HELD_READS",
        default: "16",
        units: "reads",
        what: "How many mailbox slots the listener holds a read on, ahead of the next unfinished one. More lets more wallets be answered at once without waiting for a slot to be reached; each held read costs the AIR server about 2 KB. The AIR server closes the whole session at its 257th held read, so the most accepted is 200.",
    },
    Spec {
        key: "air.timeout_secs",
        env: "PSP_AIR_TIMEOUT_SECS",
        default: "10",
        units: "seconds",
        what: "How long the AIR server may take to answer one Create or Read before the listener counts the try as failed (a reply is tried three times), and how long a connection may take when the mint writes a delivery record.",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub bind: String,
    pub network: Network,
    pub melt_max_request_bytes: usize,
    pub burns_max_request_bytes: usize,
    pub rollover_max_request_bytes: usize,
    pub rollover_max_items: usize,
    pub check_state_max_request_bytes: usize,
    pub rpc_url: String,
    pub wallet: String,
    pub cookie: String,
    pub rpc_timeout_secs: u64,
    pub scan_interval_secs: u64,
    pub health_intervals: u64,
    pub health_grace_secs: u64,
    pub price_max_age_secs: u64,
    pub price_max_spread_bps: u64,
    pub price_timeout_secs: u64,
    pub lock_confirmations: u32,
    pub min_confirmations: u32,
    /// `settle.price_tolerance` in basis points (100 is 0.01).
    pub price_tolerance_bps: u64,
    /// `settle.payment_reconciliation_target` in basis points (1000 is 0.10).
    pub payment_reconciliation_target_bps: u64,
    /// `settle.reconciliation_window_days`.
    pub reconciliation_window_days: u64,
    pub max_denomination_units: u64,
    pub rotation_activation_hours: u64,
    pub rotation_period_hours: u64,
    /// Which AIR services keep this mint's state (setting only for now).
    pub storage_mint_state: StoragePolicy,
    /// The AIR server's name (its public key, 66 hex characters).
    pub air_server: String,
    /// The AIR server's host or IP, no port.
    pub air_host: String,
    pub air_held_reads: u32,
    pub air_timeout_secs: u64,
}

impl Config {
    /// The rotation settings in seconds, with the first issue time.
    pub fn rotation(&self, first_issue: Option<u64>) -> crate::rotation::Rotation {
        crate::rotation::Rotation {
            first_issue,
            activation_secs: self.rotation_activation_hours.saturating_mul(3600),
            period_secs: self.rotation_period_hours.saturating_mul(3600),
        }
    }

    /// Every setting at its default. `home` is where the default cookie path
    /// starts.
    pub fn defaults(home: &str) -> Self {
        Config {
            bind: "127.0.0.1:3000".into(),
            network: Network::Main,
            melt_max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            burns_max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            rollover_max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            rollover_max_items: sp_wire::psp::MAX_ROLLOVER_NOTES,
            check_state_max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            rpc_url: Network::Main.default_rpc_url(),
            wallet: Network::Main.default_wallet(),
            cookie: Network::Main.default_cookie(home),
            rpc_timeout_secs: rpc::DEFAULT_TIMEOUT_SECS,
            scan_interval_secs: DEFAULT_SCAN_INTERVAL_SECS,
            health_intervals: DEFAULT_HEALTH_INTERVALS,
            health_grace_secs: DEFAULT_HEALTH_GRACE_SECS,
            price_max_age_secs: price::DEFAULT_MAX_AGE_SECS,
            price_max_spread_bps: price::DEFAULT_MAX_SPREAD_BPS,
            price_timeout_secs: price::DEFAULT_TIMEOUT_SECS,
            lock_confirmations: settle::LOCK_CONFIRMATIONS,
            min_confirmations: settle::MIN_CONFIRMATIONS,
            price_tolerance_bps: settle::PRICE_TOLERANCE_BPS as u64,
            payment_reconciliation_target_bps: settle::PAYMENT_RECONCILIATION_TARGET_BPS as u64,
            reconciliation_window_days: settle::RECONCILIATION_WINDOW_SECS / 86_400,
            max_denomination_units: DEFAULT_MAX_DENOMINATION_UNITS,
            rotation_activation_hours: 8760,
            rotation_period_hours: 87600,
            storage_mint_state: StoragePolicy::default(),
            air_server: String::new(),
            air_host: String::new(),
            air_held_reads: 16,
            air_timeout_secs: 10,
        }
    }
}

/// Why `host` is not a host or IP for `air.host`, if it is not: it is a host name
/// or an IP with no port, no scheme and no path. (An IPv6 address has colons, so
/// it is told from a `host:port` by parsing it.)
pub fn air_host_problem(host: &str) -> Option<&'static str> {
    sp_wire::air_request::host_problem(host)
}

/// A fraction written as text ("0.10", "0.1", "1", "0.0525"), as basis points
/// (1000, 1000, 10000, 525). At most four digits after the point, so the value
/// is exact; no sign, no exponent. Range is checked by `validate`.
pub fn fraction_to_bps(key: &str, value: &str) -> Result<u64, String> {
    let bad = || {
        format!(
            "{key} = `{value}` is not a fraction like 0.10 (digits only, at most four after the point, no sign)"
        )
    };
    let text = value.trim();
    let (int, frac) = text.split_once('.').unwrap_or((text, ""));
    if int.is_empty()
        || frac.len() > 4
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    let int: u64 = int.parse().map_err(|_| bad())?;
    let mut frac_bps: u64 = if frac.is_empty() {
        0
    } else {
        frac.parse().map_err(|_| bad())?
    };
    for _ in frac.len()..4 {
        frac_bps *= 10;
    }
    int.checked_mul(10_000)
        .and_then(|n| n.checked_add(frac_bps))
        .ok_or_else(bad)
}

/// Basis points as a fraction with at least two decimals: 1000 is "0.10", 525 is
/// "0.0525", 10000 is "1.00".
pub fn bps_to_fraction(bps: u64) -> String {
    let (whole, rest) = (bps / 10_000, bps % 10_000);
    let mut frac = format!("{rest:04}");
    while frac.len() > 2 && frac.ends_with('0') {
        frac.pop();
    }
    format!("{whole}.{frac}")
}

fn whole<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{key} = `{value}` is not a whole number (no sign, no decimals)"))
}

impl Config {
    /// The current value of a setting, as it is written in the file.
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "server.bind" => self.bind.clone(),
            "server.melt_max_request_bytes" => self.melt_max_request_bytes.to_string(),
            "server.burns_max_request_bytes" => self.burns_max_request_bytes.to_string(),
            "server.rollover_max_request_bytes" => self.rollover_max_request_bytes.to_string(),
            "rollover.max_items" => self.rollover_max_items.to_string(),
            "server.check_state_max_request_bytes" => {
                self.check_state_max_request_bytes.to_string()
            }
            "bitcoin.network" => self.network.name().to_string(),
            "bitcoin.rpc_url" => self.rpc_url.clone(),
            "bitcoin.wallet" => self.wallet.clone(),
            "bitcoin.cookie" => self.cookie.clone(),
            "bitcoin.rpc_timeout_secs" => self.rpc_timeout_secs.to_string(),
            "scan.interval_secs" => self.scan_interval_secs.to_string(),
            "scan.health_intervals" => self.health_intervals.to_string(),
            "scan.health_grace_secs" => self.health_grace_secs.to_string(),
            "price.max_age_secs" => self.price_max_age_secs.to_string(),
            "price.max_spread_bps" => self.price_max_spread_bps.to_string(),
            "price.timeout_secs" => self.price_timeout_secs.to_string(),
            "settle.lock_confirmations" => self.lock_confirmations.to_string(),
            "settle.min_confirmations" => self.min_confirmations.to_string(),
            "settle.price_tolerance" => bps_to_fraction(self.price_tolerance_bps),
            "settle.payment_reconciliation_target" => {
                bps_to_fraction(self.payment_reconciliation_target_bps)
            }
            "settle.reconciliation_window_days" => self.reconciliation_window_days.to_string(),
            "mint.max_denomination_units" => self.max_denomination_units.to_string(),
            "rotation.activation_hours" => self.rotation_activation_hours.to_string(),
            "rotation.period_hours" => self.rotation_period_hours.to_string(),
            "storage.mint_state" => self.storage_mint_state.to_config_string(),
            "air.server" => self.air_server.clone(),
            "air.host" => self.air_host.clone(),
            "air.held_reads" => self.air_held_reads.to_string(),
            "air.timeout_secs" => self.air_timeout_secs.to_string(),
            _ => return None,
        })
    }

    /// Set one setting from text. Only checks that the text is the right kind of
    /// value; `validate` checks the ranges.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        match key {
            "server.bind" => self.bind = value.trim().to_string(),
            "server.melt_max_request_bytes" => self.melt_max_request_bytes = whole(key, value)?,
            "server.burns_max_request_bytes" => self.burns_max_request_bytes = whole(key, value)?,
            "server.rollover_max_request_bytes" => {
                self.rollover_max_request_bytes = whole(key, value)?
            }
            "rollover.max_items" => self.rollover_max_items = whole(key, value)?,
            "server.check_state_max_request_bytes" => {
                self.check_state_max_request_bytes = whole(key, value)?
            }
            "bitcoin.network" => self.network = Network::parse(value)?,
            "bitcoin.rpc_url" => self.rpc_url = value.trim().to_string(),
            "bitcoin.wallet" => self.wallet = value.trim().to_string(),
            "bitcoin.cookie" => self.cookie = value.trim().to_string(),
            "bitcoin.rpc_timeout_secs" => self.rpc_timeout_secs = whole(key, value)?,
            "scan.interval_secs" => self.scan_interval_secs = whole(key, value)?,
            "scan.max_transactions" => return Err(RETIRED_MAX_TRANSACTIONS.to_string()),
            "scan.health_intervals" => self.health_intervals = whole(key, value)?,
            "scan.health_grace_secs" => self.health_grace_secs = whole(key, value)?,
            "price.max_age_secs" => self.price_max_age_secs = whole(key, value)?,
            "price.max_spread_bps" => self.price_max_spread_bps = whole(key, value)?,
            "price.timeout_secs" => self.price_timeout_secs = whole(key, value)?,
            "settle.lock_confirmations" => self.lock_confirmations = whole(key, value)?,
            "settle.min_confirmations" => self.min_confirmations = whole(key, value)?,
            "settle.price_tolerance" => self.price_tolerance_bps = fraction_to_bps(key, value)?,
            "settle.payment_reconciliation_target" => {
                self.payment_reconciliation_target_bps = fraction_to_bps(key, value)?
            }
            "settle.underpay_tolerance_bps" | "settle.credit_threshold_bps" => {
                return Err(format!("{key} {RETIRED_TOLERANCES}"));
            }
            "settle.reconciliation_window_days" => {
                self.reconciliation_window_days = whole(key, value)?
            }
            "mint.max_denomination_units" => self.max_denomination_units = whole(key, value)?,
            "rotation.activation_hours" => self.rotation_activation_hours = whole(key, value)?,
            "rotation.period_hours" => self.rotation_period_hours = whole(key, value)?,
            "storage.mint_state" => {
                self.storage_mint_state =
                    StoragePolicy::parse(value).map_err(|e| format!("{key} = `{value}`: {e}"))?
            }
            "air.server" => self.air_server = value.trim().to_string(),
            "air.host" => self.air_host = value.trim().to_string(),
            "air.held_reads" => self.air_held_reads = whole(key, value)?,
            "air.timeout_secs" => self.air_timeout_secs = whole(key, value)?,
            other => return Err(format!("unknown setting `{other}`")),
        }
        Ok(())
    }

    /// Check every value and say everything that is wrong, not just the first.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        let mut range = |key: &str, value: u64, low: u64, high: u64, why: &str| {
            if value < low || value > high {
                problems.push(format!(
                    "{key} = {value} is out of range: it must be from {low} to {high} ({why})"
                ));
            }
        };
        for (key, value) in [
            ("server.melt_max_request_bytes", self.melt_max_request_bytes),
            (
                "server.burns_max_request_bytes",
                self.burns_max_request_bytes,
            ),
            (
                "server.rollover_max_request_bytes",
                self.rollover_max_request_bytes,
            ),
            (
                "server.check_state_max_request_bytes",
                self.check_state_max_request_bytes,
            ),
        ] {
            range(
                key,
                value as u64,
                MIN_REQUEST_BYTES,
                1 << 30,
                "bytes; a request with one note has to fit, and 1 GiB is the most the mint will hold in memory",
            );
        }
        range(
            "rollover.max_items",
            self.rollover_max_items as u64,
            1,
            100_000,
            "notes",
        );
        range(
            "bitcoin.rpc_timeout_secs",
            self.rpc_timeout_secs,
            1,
            600,
            "seconds",
        );
        range(
            "scan.interval_secs",
            self.scan_interval_secs,
            1,
            86_400,
            "seconds",
        );
        range(
            "scan.health_intervals",
            self.health_intervals,
            1,
            100,
            "scan intervals",
        );
        range(
            "scan.health_grace_secs",
            self.health_grace_secs,
            0,
            86_400,
            "seconds",
        );
        range(
            "price.max_age_secs",
            self.price_max_age_secs,
            1,
            3600,
            "seconds",
        );
        range(
            "price.max_spread_bps",
            self.price_max_spread_bps,
            0,
            10_000,
            "basis points; 10000 is 100%",
        );
        range(
            "price.timeout_secs",
            self.price_timeout_secs,
            1,
            600,
            "seconds",
        );
        range(
            "settle.lock_confirmations",
            u64::from(self.lock_confirmations),
            1,
            1000,
            "confirmations; a price is never locked on an unconfirmed payment",
        );
        range(
            "settle.min_confirmations",
            u64::from(self.min_confirmations),
            u64::from(self.lock_confirmations.max(1)),
            1000,
            "confirmations; at least settle.lock_confirmations, so the price is locked first",
        );
        range(
            "settle.reconciliation_window_days",
            self.reconciliation_window_days,
            1,
            365,
            "days",
        );
        range(
            "mint.max_denomination_units",
            self.max_denomination_units,
            1,
            1 << 62,
            "PSP units",
        );
        range(
            "rotation.activation_hours",
            self.rotation_activation_hours,
            0,
            10_000_000,
            "hours; 0 turns rotation off",
        );
        range(
            "rotation.period_hours",
            self.rotation_period_hours,
            0,
            10_000_000,
            "hours; 0 turns rotation off",
        );
        range(
            "air.held_reads",
            u64::from(self.air_held_reads),
            1,
            200,
            "reads; the AIR server closes the session at its 257th held read",
        );
        range("air.timeout_secs", self.air_timeout_secs, 1, 600, "seconds");
        if self.payment_reconciliation_target_bps > 10_000 {
            problems.push(format!(
                "settle.payment_reconciliation_target = {} is out of range: it must be a fraction from 0 to 1 (0.10 is 10%; 0 reconciles every payment that is not exact)",
                bps_to_fraction(self.payment_reconciliation_target_bps)
            ));
        }
        if self.price_tolerance_bps > 10_000 {
            problems.push(format!(
                "settle.price_tolerance = {} is out of range: it must be a fraction from 0 to 1 (0.01 is 1%; 0 treats only an exact payment as exact)",
                bps_to_fraction(self.price_tolerance_bps)
            ));
        } else if self.price_tolerance_bps > self.payment_reconciliation_target_bps {
            problems.push(format!(
                "settle.price_tolerance = {} is more than settle.payment_reconciliation_target = {}: the tolerance settles a payment as if exact, so it cannot be wider than the target that handles the payments outside it. Lower the tolerance or raise the target",
                bps_to_fraction(self.price_tolerance_bps),
                bps_to_fraction(self.payment_reconciliation_target_bps)
            ));
        }
        if self.rotation_activation_hours > 0
            && self.rotation_period_hours > 0
            && self.rotation_period_hours < MIN_PSP_ROTATION_PERIOD_HOURS
        {
            problems.push(format!(
                "rotation.period_hours = {} is too short: the PSP rotation period must be at least {MIN_PSP_ROTATION_PERIOD_HOURS} hours, because BTC claims need time to settle before a rotation (or 0 to turn rotation off)",
                self.rotation_period_hours
            ));
        }
        if self.bind.parse::<std::net::SocketAddr>().is_err() {
            problems.push(format!(
                "server.bind = `{}` is not an address like 127.0.0.1:3000",
                self.bind
            ));
        }
        if !(self.rpc_url.starts_with("http://") || self.rpc_url.starts_with("https://")) {
            problems.push(format!(
                "bitcoin.rpc_url = `{}` must start with http:// or https://",
                self.rpc_url
            ));
        }
        if self.wallet.is_empty() || self.wallet.contains('/') {
            problems.push(format!(
                "bitcoin.wallet = `{}` must be a wallet name (not empty, no `/`)",
                self.wallet
            ));
        }
        if self.cookie.is_empty() {
            problems.push("bitcoin.cookie must be a path".to_string());
        }
        match self.bind.parse::<std::net::SocketAddr>() {
            Ok(addr) if !addr.ip().is_loopback() => problems.push(format!(
                "server.bind = `{}` is not a loopback address. Wallets reach this mint over AIR only; what is left on this address (/melt, /burns, /audit and /health) is for the SPI and the operator on this machine",
                self.bind
            )),
            _ => {}
        }
        // What is set is checked when the settings load; that both are set is
        // checked by `serve` (`require_air`), so the other commands do not need
        // them.
        if !self.air_server.is_empty() {
            if let Err(e) = sp_wire::seal::parse_public_key(&self.air_server) {
                problems.push(format!(
                    "air.server = `{}` is not an AIR server name (a public key of 66 hex characters): {e}",
                    self.air_server
                ));
            }
        }
        if !self.air_host.is_empty() {
            if let Some(why) = air_host_problem(&self.air_host) {
                problems.push(format!("air.host = `{}` {why}", self.air_host));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// The RPC address of the wallet.
    /// How long `serve` may go without a successful scan before `/health`
    /// fails: `scan.health_intervals` intervals plus `scan.health_grace_secs`
    /// (420 s with the defaults).
    pub fn health_stale_after_secs(&self) -> u64 {
        self.scan_interval_secs
            .saturating_mul(self.health_intervals)
            .saturating_add(self.health_grace_secs)
    }

    pub fn wallet_url(&self) -> String {
        format!(
            "{}/wallet/{}",
            self.rpc_url.trim_end_matches('/'),
            self.wallet
        )
    }

    pub fn price(&self) -> price::Config {
        price::Config {
            max_age_secs: self.price_max_age_secs,
            max_spread_bps: self.price_max_spread_bps,
            timeout_secs: self.price_timeout_secs,
        }
    }

    pub fn terms(&self) -> Terms {
        Terms {
            lock_confirmations: self.lock_confirmations,
            min_confirmations: self.min_confirmations,
            price_tolerance_bps: u128::from(self.price_tolerance_bps),
            reconciliation_target_bps: u128::from(self.payment_reconciliation_target_bps),
            reconciliation_window_secs: self.reconciliation_window_days.saturating_mul(86_400),
        }
    }

    /// What `serve` needs: the AIR server and its host, because the AIR
    /// listener always runs (AIR is the only way wallets reach this mint). The
    /// error names every one that is not set. The other commands do not need
    /// them.
    pub fn require_air(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        if self.air_server.is_empty() {
            missing.push("air.server (PSP_AIR_SERVER)");
        }
        if self.air_host.is_empty() {
            missing.push("air.host (PSP_AIR_HOST)");
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(format!(
            "serve needs the AIR settings, because AIR is the only way wallets reach this mint, and {} {} not set. air.server is the AIR server's name (a public key of 66 hex characters) and air.host where to reach it (a host name or IP, no port)",
            missing.join(" and "),
            if missing.len() == 1 { "is" } else { "are" }
        ))
    }

    /// The AIR listener's settings (call [`Config::require_air`] first).
    /// `state_dir` is the network's data folder.
    pub fn air_listener(&self, state_dir: &Path) -> crate::air_listener::Settings {
        crate::air_listener::Settings {
            server: sp_wire::air_request::client::ServerRef {
                name: self.air_server.clone(),
                host: self.air_host.clone(),
            },
            held_reads: self.air_held_reads,
            timeout_secs: self.air_timeout_secs,
            state_dir: state_dir.to_path_buf(),
        }
    }

    pub fn limits(&self) -> crate::server::Limits {
        crate::server::Limits {
            melt_bytes: self.melt_max_request_bytes,
            burns_bytes: self.burns_max_request_bytes,
            rollover_bytes: self.rollover_max_request_bytes,
            rollover_notes: self.rollover_max_items,
            check_state_bytes: self.check_state_max_request_bytes,
        }
    }
}

/// Where a value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Default,
    /// `--regtest`, `--signet`, `--testnet` or `--mainnet`.
    Flag,
    File,
    Env(&'static str),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Default => write!(f, "default"),
            Source::Flag => write!(f, "command-line flag"),
            Source::File => write!(f, "file"),
            Source::Env(name) => write!(f, "env {name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub config: Config,
    pub sources: BTreeMap<&'static str, Source>,
    /// The file that was read, if there was one.
    pub file: Option<PathBuf>,
}

impl Loaded {
    /// One line for each setting, for the startup log.
    pub fn describe(&self) -> Vec<String> {
        SPECS
            .iter()
            .map(|spec| {
                let value = self.config.get(spec.key).unwrap_or_default();
                let source = self
                    .sources
                    .get(spec.key)
                    .cloned()
                    .unwrap_or(Source::Default);
                format!("  {} = {} {} ({source})", spec.key, value, spec.units)
            })
            .collect()
    }
}

/// Where the file is: `PSP_CONFIG` if set (and then it must exist), otherwise
/// `psp-mint.toml` in the data directory (and then it may be missing).
pub fn file_path(data_dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> (PathBuf, bool) {
    match env(FILE_ENV) {
        Some(path) if !path.is_empty() => (PathBuf::from(path), true),
        _ => (data_dir.join(FILE_NAME), false),
    }
}

/// Read the file text into `(key, value)` pairs, rejecting anything that is not
/// a known `[section]` of plain values.
fn parse_file(text: &str) -> Result<Vec<(String, String)>, Vec<String>> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| vec![format!("not valid TOML: {}", e.message())])?;
    let mut pairs = Vec::new();
    let mut problems = Vec::new();
    for (section, body) in &table {
        let Some(body) = body.as_table() else {
            problems.push(format!(
                "`{section}` must be a [section] of settings, not a single value"
            ));
            continue;
        };
        for (name, value) in body {
            let key = format!("{section}.{name}");
            if key == "scan.max_transactions" {
                problems.push(RETIRED_MAX_TRANSACTIONS.to_string());
                continue;
            }
            if key == "settle.underpay_tolerance_bps" || key == "settle.credit_threshold_bps" {
                problems.push(format!("{key} {RETIRED_TOLERANCES}"));
                continue;
            }
            if key == "air.enabled" {
                problems.push(GONE_AIR_ENABLED.to_string());
                continue;
            }
            if !SPECS.iter().any(|s| s.key == key) {
                problems.push(format!("unknown setting `{key}`"));
                continue;
            }
            match value {
                toml::Value::Integer(n) => pairs.push((key, n.to_string())),
                // A decimal is only meaningful for the two fractions
                // (`0.01`, `0.10`); everywhere else it is a type error, not a
                // number to be truncated.
                toml::Value::Float(f)
                    if key == "settle.payment_reconciliation_target"
                        || key == "settle.price_tolerance" =>
                {
                    pairs.push((key, f.to_string()))
                }
                toml::Value::Float(f) => {
                    problems.push(format!("{key} must be a whole number, not the decimal {f}"))
                }
                toml::Value::String(s) => pairs.push((key, s.clone())),
                toml::Value::Array(items) if key.starts_with("storage.") => {
                    let ids: Vec<&str> = items.iter().filter_map(|v| v.as_str()).collect();
                    if ids.len() == items.len() {
                        pairs.push((key, ids.join(",")));
                    } else {
                        problems.push(format!(
                            "{key} must be a list of strings (service IDs), not {items:?}"
                        ));
                    }
                }
                _ => problems.push(format!(
                    "{key} must be a number or a string, not {}",
                    value.type_str()
                )),
            }
        }
    }
    if problems.is_empty() {
        Ok(pairs)
    } else {
        Err(problems)
    }
}

/// Load the settings: defaults, then the file, then the environment, then check.
/// `env` looks up an environment variable (`HOME` too, for the default cookie
/// path).
pub fn load(data_dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Loaded, Vec<String>> {
    load_with(data_dir, env, None)
}

/// As [`load`], with the network flag from the command line, which beats the
/// file and the environment. Then `bitcoin.rpc_url`, `bitcoin.wallet` and
/// `bitcoin.cookie`, unless set in the file or the environment, take the
/// selected network's defaults.
pub fn load_with(
    data_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    flag: Option<Network>,
) -> Result<Loaded, Vec<String>> {
    let (path, must_exist) = file_path(data_dir, env);
    let home = env("HOME").unwrap_or_default();
    let mut config = Config::defaults(&home);
    let mut sources: BTreeMap<&'static str, Source> = BTreeMap::new();
    let mut problems = Vec::new();
    let mut file = None;

    match fs::read_to_string(&path) {
        Ok(text) => {
            file = Some(path.clone());
            match parse_file(&text) {
                Ok(pairs) => {
                    for (key, value) in pairs {
                        match config.set(&key, &value) {
                            Ok(()) => {
                                if let Some(spec) = SPECS.iter().find(|s| s.key == key) {
                                    sources.insert(spec.key, Source::File);
                                }
                            }
                            Err(e) => problems.push(format!("{}: {e}", path.display())),
                        }
                    }
                }
                Err(list) => {
                    problems.extend(list.into_iter().map(|p| format!("{}: {p}", path.display())))
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && !must_exist => {}
        Err(e) => problems.push(format!("cannot read config file {}: {e}", path.display())),
    }

    for spec in SPECS {
        if let Some(value) = env(spec.env) {
            if value.is_empty() {
                continue;
            }
            match config.set(spec.key, &value) {
                Ok(()) => {
                    sources.insert(spec.key, Source::Env(spec.env));
                }
                Err(e) => problems.push(format!("environment variable {}: {e}", spec.env)),
            }
        }
    }

    for (var, key) in [
        (
            "PSP_UNDERPAY_TOLERANCE_BPS",
            "settle.underpay_tolerance_bps",
        ),
        ("PSP_CREDIT_THRESHOLD_BPS", "settle.credit_threshold_bps"),
    ] {
        if env(var).is_some_and(|v| !v.is_empty()) {
            problems.push(format!(
                "environment variable {var}: {key} {RETIRED_TOLERANCES}"
            ));
        }
    }
    if env("PSP_AIR_ENABLED").is_some_and(|v| !v.is_empty()) {
        problems.push(format!(
            "environment variable PSP_AIR_ENABLED: {GONE_AIR_ENABLED}"
        ));
    }
    if env("PSP_SCAN_MAX_TRANSACTIONS").is_some_and(|v| !v.is_empty()) {
        problems.push(format!(
            "environment variable PSP_SCAN_MAX_TRANSACTIONS: {RETIRED_MAX_TRANSACTIONS}"
        ));
    }
    if let Some(network) = flag {
        config.network = network;
        sources.insert("bitcoin.network", Source::Flag);
    }
    let network = config.network;
    if !sources.contains_key("bitcoin.rpc_url") {
        config.rpc_url = network.default_rpc_url();
    }
    if !sources.contains_key("bitcoin.wallet") {
        config.wallet = network.default_wallet();
    }
    if !sources.contains_key("bitcoin.cookie") {
        config.cookie = network.default_cookie(&home);
    }

    if problems.is_empty() {
        if let Err(list) = config.validate() {
            problems.extend(list);
        }
    }
    if problems.is_empty() {
        Ok(Loaded {
            config,
            sources,
            file,
        })
    } else {
        Err(problems)
    }
}

/// The real environment.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psp-mint-cfg-{}-{name}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_storage_policy_comes_from_the_file_and_the_environment() {
        const A: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        const B: &str = "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";
        let dir = scratch("storage");
        let first = load(&dir, &env_of(&[])).unwrap();
        assert!(first.config.storage_mint_state.is_local_only());
        assert_eq!(first.config.get("storage.mint_state").as_deref(), Some(""));

        fs::write(
            dir.join(FILE_NAME),
            format!("[storage]\nmint_state = [\"{A}\", \"{B}\"]\n"),
        )
        .unwrap();
        let l = load(&dir, &env_of(&[])).unwrap();
        let p = &l.config.storage_mint_state;
        assert_eq!(p.primary().unwrap().to_hex(), A);
        assert_eq!(p.secondaries().len(), 1);
        assert_eq!(l.sources["storage.mint_state"], Source::File);
        assert_eq!(
            l.config.get("storage.mint_state").unwrap(),
            format!("{A},{B}")
        );

        // The environment wins over the file. One entry is valid.
        let l = load(&dir, &env_of(&[("PSP_STORAGE_MINT_STATE", A)])).unwrap();
        assert_eq!(l.config.storage_mint_state.to_config_string(), A);
        assert!(l.config.storage_mint_state.secondaries().is_empty());
        assert_eq!(
            l.sources["storage.mint_state"],
            Source::Env("PSP_STORAGE_MINT_STATE")
        );

        // One string in the file works too, and so does an empty list.
        fs::write(
            dir.join(FILE_NAME),
            format!("[storage]\nmint_state = \"{B}, {A}\"\n"),
        )
        .unwrap();
        let l = load(&dir, &env_of(&[])).unwrap();
        assert_eq!(l.config.storage_mint_state.primary().unwrap().to_hex(), B);
        fs::write(dir.join(FILE_NAME), "[storage]\nmint_state = []\n").unwrap();
        let l = load(&dir, &env_of(&[])).unwrap();
        assert!(l.config.storage_mint_state.is_local_only());

        // Every kind of mistake is refused, naming the variable or the setting.
        let twice = format!("{A},{A}");
        let p = load(&dir, &env_of(&[("PSP_STORAGE_MINT_STATE", twice.as_str())])).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("PSP_STORAGE_MINT_STATE") && x.contains("listed twice")),
            "{p:?}"
        );
        let gap = format!("{A},,{B}");
        let p = load(&dir, &env_of(&[("PSP_STORAGE_MINT_STATE", gap.as_str())])).unwrap_err();
        assert!(p.iter().any(|x| x.contains("entry 2")), "{p:?}");
        let p = load(&dir, &env_of(&[("PSP_STORAGE_MINT_STATE", "not-a-key")])).unwrap_err();
        assert!(p.iter().any(|x| x.contains("must be hex")), "{p:?}");
        // The 32-byte x-only form of wire version 2 is refused, and says why.
        let old_form = &A[2..];
        let p = load(&dir, &env_of(&[("PSP_STORAGE_MINT_STATE", old_form)])).unwrap_err();
        assert!(p.iter().any(|x| x.contains("x-only")), "{p:?}");
        fs::write(dir.join(FILE_NAME), "[storage]\nmint_state = [1]\n").unwrap();
        let p = load(&dir, &env_of(&[])).unwrap_err();
        assert!(p.iter().any(|x| x.contains("list of strings")), "{p:?}");
        fs::write(dir.join(FILE_NAME), "[storage]\nwallet = \"x\"\n").unwrap();
        let p = load(&dir, &env_of(&[])).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("unknown setting `storage.wallet`")),
            "{p:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn a_psp_rotation_period_under_24_hours_refuses_to_start() {
        let dir = scratch("floor");
        fs::write(dir.join(FILE_NAME), "[rotation]\nperiod_hours = 12\n").unwrap();
        let p = load(&dir, &env_of(&[])).unwrap_err();
        assert!(
            p.iter().any(|e| e.contains("rotation.period_hours = 12")
                && e.contains("at least 24 hours")
                && e.contains("BTC claims need time to settle")),
            "{p:?}"
        );
        fs::write(dir.join(FILE_NAME), "").unwrap();
        let p = load(&dir, &env_of(&[("PSP_ROTATION_PERIOD_HOURS", "23")])).unwrap_err();
        assert!(p.iter().any(|e| e.contains("at least 24 hours")), "{p:?}");
        // 24 is fine; 0 (off) is fine; rotation off makes the period moot.
        assert!(load(&dir, &env_of(&[("PSP_ROTATION_PERIOD_HOURS", "24")])).is_ok());
        assert!(load(&dir, &env_of(&[("PSP_ROTATION_PERIOD_HOURS", "0")])).is_ok());
        assert!(
            load(
                &dir,
                &env_of(&[
                    ("PSP_ROTATION_PERIOD_HOURS", "1"),
                    ("PSP_ROTATION_ACTIVATION_HOURS", "0")
                ])
            )
            .is_ok()
        );
    }

    #[test]
    fn with_no_file_and_no_environment_every_setting_is_its_documented_default() {
        let dir = scratch("defaults");
        let loaded = load(&dir, &env_of(&[("HOME", "/home/x")])).unwrap();
        assert_eq!(loaded.config, Config::defaults("/home/x"));
        assert!(loaded.file.is_none());
        for spec in SPECS {
            let mut documented = spec.default.replace("$HOME", "/home/x");
            // A default given per OS: "A (macOS), B (Linux)".
            if let Some((mac, rest)) = documented.clone().split_once(" (macOS), ") {
                let linux = rest.trim_end_matches(" (Linux)").to_string();
                documented = if std::env::consts::OS == "macos" {
                    mac.to_string()
                } else {
                    linux
                };
            }
            assert_eq!(
                loaded.config.get(spec.key).as_deref(),
                Some(documented.as_str()),
                "{}",
                spec.key
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_defaults_are_the_values_the_mint_used_before_it_had_a_config() {
        // The network is mainnet by default now, with mainnet's node settings.
        let c = Config::defaults("/h");
        assert_eq!(c.network, Network::Main);
        assert_eq!(c.bind, "127.0.0.1:3000");
        assert_eq!(c.wallet_url(), "http://127.0.0.1:8332/wallet/psp-main");
        assert_eq!(c.cookie, "/h/Library/Application Support/Bitcoin/.cookie");
        assert_eq!(c.scan_interval_secs, 120);
        assert_eq!(c.rpc_timeout_secs, 30);
        assert_eq!(c.terms(), Terms::default());
        assert_eq!(c.price(), price::Config::default());
        assert_eq!(c.payment_reconciliation_target_bps, 1000);
        assert_eq!(
            c.get("settle.payment_reconciliation_target").unwrap(),
            "0.10"
        );
        assert_eq!(c.price_tolerance_bps, 100);
        assert_eq!(c.get("settle.price_tolerance").unwrap(), "0.01");
        assert_eq!(c.max_denomination_units, 16_777_216);
        for bytes in [
            c.melt_max_request_bytes,
            c.burns_max_request_bytes,
            c.rollover_max_request_bytes,
            c.check_state_max_request_bytes,
        ] {
            assert_eq!(bytes, 1024 * 1024);
        }
        assert_eq!(c.rollover_max_items, 1000);
    }

    #[test]
    fn the_air_listener_always_runs_and_serve_needs_a_server_and_a_host() {
        const SERVER: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let dir = scratch("air");
        // The settings load without them (the other commands do not need them),
        // and serve names both.
        let bare = load(&dir, &env_of(&[])).unwrap();
        assert_eq!(bare.config.air_held_reads, 16);
        assert_eq!(bare.config.air_timeout_secs, 10);
        let e = bare.config.require_air().unwrap_err();
        assert!(
            e.contains("PSP_AIR_SERVER") && e.contains("PSP_AIR_HOST"),
            "{e}"
        );
        assert!(e.contains("are not set"), "{e}");
        // One set: only the other is named.
        let host = load(&dir, &env_of(&[("PSP_AIR_HOST", "127.0.0.1")])).unwrap();
        let e = host.config.require_air().unwrap_err();
        assert!(
            e.contains("PSP_AIR_SERVER") && !e.contains("PSP_AIR_HOST"),
            "{e}"
        );
        assert!(e.contains("is not set"), "{e}");

        // Complete, from the file.
        fs::write(
            dir.join(FILE_NAME),
            format!("[air]\nserver = \"{SERVER}\"\nhost = \"127.0.0.1\"\nheld_reads = 8\n"),
        )
        .unwrap();
        let on = load(&dir, &env_of(&[])).unwrap();
        assert_eq!(on.config.require_air(), Ok(()));
        let s = on.config.air_listener(&dir);
        assert_eq!(s.server.name, SERVER);
        assert_eq!(s.server.host, "127.0.0.1");
        assert_eq!(s.held_reads, 8);
        assert_eq!(s.timeout_secs, 10);
        assert_eq!(s.state_dir, dir);
        assert_eq!(on.sources["air.server"], Source::File);

        // The environment wins over the file.
        let env = load(&dir, &env_of(&[("PSP_AIR_HOST", "air.example.org")])).unwrap();
        assert_eq!(env.config.air_listener(&dir).server.host, "air.example.org");
        assert_eq!(env.sources["air.host"], Source::Env("PSP_AIR_HOST"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn air_enabled_is_gone_and_says_so_from_the_file_and_the_environment() {
        let dir = scratch("air-enabled-gone");
        for text in ["[air]\nenabled = true\n", "[air]\nenabled = false\n"] {
            fs::write(dir.join(FILE_NAME), text).unwrap();
            let p = load(&dir, &env_of(&[])).unwrap_err();
            assert!(
                p.iter()
                    .any(|x| x.contains("air.enabled is gone") && x.contains("PSP_AIR_SERVER")),
                "{p:?}"
            );
        }
        fs::remove_file(dir.join(FILE_NAME)).unwrap();
        let p = load(&dir, &env_of(&[("PSP_AIR_ENABLED", "true")])).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("PSP_AIR_ENABLED") && x.contains("is gone")),
            "{p:?}"
        );
        // Empty is unset, as for every variable.
        assert!(load(&dir, &env_of(&[("PSP_AIR_ENABLED", "")])).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn server_bind_must_be_a_loopback_address() {
        let dir = scratch("bind-loopback");
        for ok in ["127.0.0.1:3000", "127.0.0.2:3000", "[::1]:3000"] {
            assert!(load(&dir, &env_of(&[("PSP_BIND", ok)])).is_ok(), "{ok}");
        }
        for bad in ["0.0.0.0:3000", "192.168.1.5:3000", "[::]:3000"] {
            let p = load(&dir, &env_of(&[("PSP_BIND", bad)])).unwrap_err();
            assert!(
                p.iter().any(|x| x.starts_with("server.bind")
                    && x.contains("not a loopback address")
                    && x.contains("AIR")),
                "{bad}: {p:?}"
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_air_mistake_is_refused_naming_the_setting() {
        const SERVER: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let dir = scratch("air-bad");
        let on = |extra: &[(&'static str, &'static str)]| {
            let mut pairs = vec![("PSP_AIR_SERVER", SERVER), ("PSP_AIR_HOST", "127.0.0.1")];
            pairs.extend_from_slice(extra);
            load(&dir, &env_of(&pairs))
        };
        assert!(on(&[]).is_ok());
        for (extra, word) in [
            (("PSP_AIR_SERVER", "zz"), "air.server"),
            // 64 characters: an x-only key is not a name
            (
                (
                    "PSP_AIR_SERVER",
                    "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
                ),
                "air.server",
            ),
            (("PSP_AIR_HOST", "127.0.0.1:5702"), "no port"),
            (("PSP_AIR_HOST", "ws://127.0.0.1"), "not a URL"),
            (("PSP_AIR_HELD_READS", "0"), "air.held_reads"),
            (("PSP_AIR_HELD_READS", "201"), "air.held_reads"),
            (("PSP_AIR_TIMEOUT_SECS", "0"), "air.timeout_secs"),
        ] {
            let p = on(&[extra]).unwrap_err();
            assert!(
                p.iter().any(|x| x.contains(word)),
                "{extra:?}: expected `{word}` in {p:?}"
            );
        }
        // An IPv6 address is a host, not a host and a port.
        assert!(air_host_problem("::1").is_none());
        assert!(air_host_problem("[::1]").is_none());
        assert!(air_host_problem("air.example.org").is_none());
        // A host that is wrong is refused even for a command that does not need it.
        assert!(load(&dir, &env_of(&[("PSP_AIR_HOST", "127.0.0.1:5702")])).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_spec_can_be_read_and_set_and_has_its_own_env_var() {
        let mut c = Config::defaults("/h");
        let mut envs = std::collections::HashSet::new();
        for spec in SPECS {
            let value = c.get(spec.key).unwrap();
            c.set(spec.key, &value).unwrap();
            assert!(spec.env.starts_with("PSP_"));
            assert!(envs.insert(spec.env), "{} used twice", spec.env);
        }
        assert_eq!(c, Config::defaults("/h"));
    }

    #[test]
    fn the_file_overrides_the_default_and_the_environment_overrides_the_file() {
        let dir = scratch("layers");
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\nmin_confirmations = 3\npayment_reconciliation_target = 0.05\n[bitcoin]\nwallet = \"shop\"\n",
        )
        .unwrap();
        let loaded = load(
            &dir,
            &env_of(&[("PSP_PAYMENT_RECONCILIATION_TARGET", "0.2")]),
        )
        .unwrap();
        assert_eq!(loaded.config.min_confirmations, 3);
        assert_eq!(loaded.config.payment_reconciliation_target_bps, 2000);
        assert_eq!(loaded.config.wallet, "shop");
        assert_eq!(loaded.sources["settle.min_confirmations"], Source::File);
        assert_eq!(
            loaded.sources["settle.payment_reconciliation_target"],
            Source::Env("PSP_PAYMENT_RECONCILIATION_TARGET")
        );
        assert_eq!(loaded.file.as_deref(), Some(dir.join(FILE_NAME).as_path()));
        let lines = loaded.describe();
        assert_eq!(lines.len(), SPECS.len());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("settle.min_confirmations = 3") && l.contains("(file)"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("(env PSP_PAYMENT_RECONCILIATION_TARGET)"))
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_config_file_named_by_the_environment_must_exist() {
        let dir = scratch("named");
        let named = dir.join("elsewhere.toml");
        let env = env_of(&[("PSP_CONFIG", named.to_str().unwrap())]);
        let problems = load(&dir, &env).unwrap_err();
        assert!(problems[0].contains("cannot read config file"));
        fs::write(&named, "[scan]\ninterval_secs = 7\n").unwrap();
        assert_eq!(load(&dir, &env).unwrap().config.scan_interval_secs, 7);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_keys_bad_types_and_bad_toml_are_all_reported() {
        let dir = scratch("bad");
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\nmin_confirmation = 3\nlock_confirmations = true\n[scan]\ninterval_secs = 1.5\nloose = 1\n",
        )
        .unwrap();
        let problems = load(&dir, &env_of(&[])).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unknown setting `settle.min_confirmation`"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("settle.lock_confirmations must be"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("scan.interval_secs must be"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unknown setting `scan.loose`"))
        );
        fs::write(dir.join(FILE_NAME), "[settle\n").unwrap();
        assert!(load(&dir, &env_of(&[])).unwrap_err()[0].contains("not valid TOML"));
        fs::write(dir.join(FILE_NAME), "wallet = \"x\"\n").unwrap();
        assert!(load(&dir, &env_of(&[])).unwrap_err()[0].contains("must be a [section]"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bad_environment_value_is_reported_with_its_variable() {
        let dir = scratch("badenv");
        let problems = load(&dir, &env_of(&[("PSP_SCAN_INTERVAL_SECS", "-5")])).unwrap_err();
        assert!(problems[0].contains("environment variable PSP_SCAN_INTERVAL_SECS"));
        // empty means unset
        assert!(load(&dir, &env_of(&[("PSP_SCAN_INTERVAL_SECS", "")])).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn out_of_range_values_are_all_refused_at_once() {
        let mut c = Config::defaults("/h");
        c.melt_max_request_bytes = 100;
        c.burns_max_request_bytes = 2 << 30;
        c.rollover_max_request_bytes = 100;
        c.rollover_max_items = 0;
        c.scan_interval_secs = 0;
        c.lock_confirmations = 0;
        c.payment_reconciliation_target_bps = 10_001;
        c.price_max_spread_bps = 10_001;
        c.bind = "localhost".into();
        c.rpc_url = "127.0.0.1:18443".into();
        c.wallet = "".into();
        let problems = c.validate().unwrap_err();
        for key in [
            "server.melt_max_request_bytes",
            "server.burns_max_request_bytes",
            "server.rollover_max_request_bytes",
            "rollover.max_items",
            "scan.interval_secs",
            "settle.lock_confirmations",
            "settle.payment_reconciliation_target",
            "price.max_spread_bps",
            "server.bind",
            "bitcoin.rpc_url",
            "bitcoin.wallet",
        ] {
            assert!(
                problems.iter().any(|p| p.starts_with(key)),
                "{key}: {problems:?}"
            );
        }
    }

    #[test]
    fn settlement_needs_at_least_the_lock_confirmations() {
        let mut c = Config::defaults("/h");
        c.lock_confirmations = 3;
        c.min_confirmations = 2;
        let problems = c.validate().unwrap_err();
        assert!(problems[0].starts_with("settle.min_confirmations"));
        c.min_confirmations = 3;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn the_settings_reach_the_parts_that_use_them() {
        let mut c = Config::defaults("/h");
        c.lock_confirmations = 2;
        c.min_confirmations = 10;
        c.payment_reconciliation_target_bps = 2500;
        c.price_timeout_secs = 4;
        c.melt_max_request_bytes = 5000;
        c.rollover_max_request_bytes = 6000;
        c.rollover_max_items = 7;
        c.rpc_url = "http://node:8332/".into();
        assert_eq!(
            c.terms(),
            Terms {
                lock_confirmations: 2,
                min_confirmations: 10,
                price_tolerance_bps: 100,
                reconciliation_target_bps: 2500,
                reconciliation_window_secs: 14 * 86_400,
            }
        );
        assert_eq!(c.price().timeout_secs, 4);
        assert_eq!(c.limits().melt_bytes, 5000);
        assert_eq!(c.limits().rollover_bytes, 6000);
        assert_eq!(c.limits().rollover_notes, 7);
        assert_eq!(c.wallet_url(), "http://node:8332/wallet/psp-main");
    }

    #[test]
    fn fractions_are_read_exactly_and_written_with_at_least_two_decimals() {
        let k = "settle.payment_reconciliation_target";
        for (text, bps) in [
            ("0.10", 1000),
            ("0.1", 1000),
            (" 0.05 ", 500),
            ("0.0525", 525),
            ("0.0001", 1),
            ("0", 0),
            ("1", 10_000),
            ("1.0", 10_000),
            ("2", 20_000),
        ] {
            assert_eq!(fraction_to_bps(k, text), Ok(bps), "{text}");
        }
        for bad in [
            "",
            ".5",
            "-0.1",
            "+0.1",
            "0.12345",
            "1e-1",
            "ten percent",
            "0,1",
            "10%",
        ] {
            assert!(fraction_to_bps(k, bad).is_err(), "{bad}");
        }
        assert!(fraction_to_bps(k, "99999999999999999999").is_err());
        for (bps, text) in [
            (1000, "0.10"),
            (500, "0.05"),
            (525, "0.0525"),
            (0, "0.00"),
            (10_000, "1.00"),
            (1, "0.0001"),
        ] {
            assert_eq!(bps_to_fraction(bps), text);
            assert_eq!(fraction_to_bps(k, text), Ok(bps));
        }
    }

    #[test]
    fn the_target_cannot_be_more_than_one() {
        let mut c = Config::defaults("/h");
        c.set("settle.payment_reconciliation_target", "1").unwrap();
        assert!(c.validate().is_ok());
        c.set("settle.payment_reconciliation_target", "1.0001")
            .unwrap();
        let problems = c.validate().unwrap_err();
        assert!(
            problems[0].starts_with("settle.payment_reconciliation_target = 1.0001"),
            "{problems:?}"
        );
    }

    #[test]
    fn the_tolerance_is_read_from_the_file_and_the_environment() {
        let dir = scratch("tolerance");
        fs::write(dir.join(FILE_NAME), "[settle]\nprice_tolerance = 0.02\n").unwrap();
        let loaded = load(&dir, &env_of(&[])).unwrap();
        assert_eq!(loaded.config.price_tolerance_bps, 200);
        assert_eq!(loaded.config.terms().price_tolerance_bps, 200);
        assert_eq!(loaded.sources["settle.price_tolerance"], Source::File);
        let loaded = load(&dir, &env_of(&[("PSP_PRICE_TOLERANCE", "0.005")])).unwrap();
        assert_eq!(loaded.config.price_tolerance_bps, 50);
        assert_eq!(
            loaded.sources["settle.price_tolerance"],
            Source::Env("PSP_PRICE_TOLERANCE")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tolerance_must_be_a_fraction_from_zero_to_one() {
        let mut c = Config::defaults("/h");
        c.payment_reconciliation_target_bps = 10_000;
        c.set("settle.price_tolerance", "0").unwrap();
        assert!(c.validate().is_ok());
        c.set("settle.price_tolerance", "1").unwrap();
        assert!(c.validate().is_ok());
        c.set("settle.price_tolerance", "1.0001").unwrap();
        let problems = c.validate().unwrap_err();
        assert!(
            problems[0].starts_with("settle.price_tolerance = 1.0001"),
            "{problems:?}"
        );
        assert!(c.set("settle.price_tolerance", "-0.01").is_err());
        assert!(c.set("settle.price_tolerance", "1%").is_err());
    }

    #[test]
    fn a_tolerance_wider_than_the_target_is_refused_at_startup() {
        let mut c = Config::defaults("/h");
        // equal is allowed: the target then handles nothing the tolerance did not
        c.set("settle.price_tolerance", "0.10").unwrap();
        assert!(c.validate().is_ok());
        c.set("settle.price_tolerance", "0.1001").unwrap();
        let problems = c.validate().unwrap_err();
        assert!(
            problems[0].starts_with("settle.price_tolerance = 0.1001 is more than"),
            "{problems:?}"
        );
        // lowering the target below the default tolerance is refused the same way
        let mut c = Config::defaults("/h");
        c.set("settle.payment_reconciliation_target", "0.005")
            .unwrap();
        assert!(c.validate().is_err());
        // and through the file, where startup sees it
        let dir = scratch("tolerance-over-target");
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\nprice_tolerance = 0.05\npayment_reconciliation_target = 0.02\n",
        )
        .unwrap();
        let problems = load(&dir, &env_of(&[])).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("settle.price_tolerance = 0.05 is more than")),
            "{problems:?}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_float_in_the_file_is_read_as_the_fraction_it_looks_like() {
        let dir = scratch("float");
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\npayment_reconciliation_target = 0.0525\n",
        )
        .unwrap();
        let loaded = load(&dir, &env_of(&[])).unwrap();
        assert_eq!(loaded.config.payment_reconciliation_target_bps, 525);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_two_retired_tolerances_are_refused_in_the_file_and_the_environment() {
        let dir = scratch("retired-tolerances");
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\ncredit_threshold_bps = 500\nunderpay_tolerance_bps = 100\n",
        )
        .unwrap();
        let problems = load(&dir, &env_of(&[("PSP_CREDIT_THRESHOLD_BPS", "700")])).unwrap_err();
        for needle in [
            "settle.credit_threshold_bps is retired",
            "settle.underpay_tolerance_bps is retired",
            "PSP_CREDIT_THRESHOLD_BPS: settle.credit_threshold_bps is retired",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "{needle}: {problems:?}"
            );
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;

    #[test]
    fn the_health_timeout_defaults_to_three_intervals_plus_a_minute() {
        let mut c = Config::defaults("/h");
        assert_eq!(c.health_stale_after_secs(), 3 * 120 + 60);
        c.set("scan.interval_secs", "10").unwrap();
        assert_eq!(c.health_stale_after_secs(), 90);
        c.set("scan.health_intervals", "5").unwrap();
        c.set("scan.health_grace_secs", "0").unwrap();
        assert_eq!(c.health_stale_after_secs(), 50);
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.get("scan.health_intervals").as_deref(), Some("5"));
    }

    #[test]
    fn health_settings_out_of_range_are_refused() {
        let mut c = Config::defaults("/h");
        c.health_intervals = 0;
        c.health_grace_secs = 86_401;
        let problems = c.validate().unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("scan.health_intervals"));
        assert!(problems[1].contains("scan.health_grace_secs"));
        assert!(
            SPECS
                .iter()
                .any(|s| s.key == "scan.health_intervals" && s.env == "PSP_HEALTH_INTERVALS")
        );
        assert!(
            SPECS
                .iter()
                .any(|s| s.key == "scan.health_grace_secs" && s.env == "PSP_HEALTH_GRACE_SECS")
        );
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: std::collections::BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("psp-mint-cfgnet-{}-{name}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn each_network_has_its_own_node_defaults_and_explicit_values_still_win() {
        let d = dir("defaults");
        let env = env_of(&[("HOME", "/h")]);
        let main = load_with(&d, &env, None).unwrap().config;
        assert_eq!(
            (main.network, main.rpc_url.as_str(), main.wallet.as_str()),
            (Network::Main, "http://127.0.0.1:8332", "psp-main")
        );
        let reg = load_with(&d, &env, Some(Network::Regtest)).unwrap();
        assert_eq!(reg.config.rpc_url, "http://127.0.0.1:18443");
        assert_eq!(reg.config.wallet, "psp-regtest");
        assert_eq!(
            reg.config.cookie,
            "/h/Library/Application Support/Bitcoin/regtest/.cookie"
        );
        assert_eq!(reg.sources.get("bitcoin.network"), Some(&Source::Flag));
        let sig = load_with(&d, &env, Some(Network::Signet)).unwrap().config;
        assert_eq!(sig.rpc_url, "http://127.0.0.1:38332");
        let test = load_with(&d, &env, Some(Network::Testnet)).unwrap().config;
        assert_eq!(
            (test.rpc_url.as_str(), test.wallet.as_str()),
            ("http://127.0.0.1:18332", "psp-testnet")
        );
        // explicit values beat the network's defaults
        let env2 = env_of(&[
            ("HOME", "/h"),
            ("PSP_WALLET", "psp"),
            ("PSP_RPC_URL", "http://10.0.0.2:9999"),
        ]);
        let c = load_with(&d, &env2, Some(Network::Regtest)).unwrap().config;
        assert_eq!(
            (c.wallet.as_str(), c.rpc_url.as_str()),
            ("psp", "http://10.0.0.2:9999")
        );
        assert_eq!(
            c.cookie,
            "/h/Library/Application Support/Bitcoin/regtest/.cookie"
        );
        // the flag beats the environment and the file
        fs::write(d.join(FILE_NAME), "[bitcoin]\nnetwork = \"signet\"\n").unwrap();
        assert_eq!(
            load_with(&d, &env, None).unwrap().config.network,
            Network::Signet
        );
        let env3 = env_of(&[("HOME", "/h"), ("PSP_NETWORK", "testnet")]);
        assert_eq!(
            load_with(&d, &env3, None).unwrap().config.network,
            Network::Testnet
        );
        assert_eq!(
            load_with(&d, &env3, Some(Network::Regtest))
                .unwrap()
                .config
                .network,
            Network::Regtest
        );
        let bad = env_of(&[("HOME", "/h"), ("PSP_NETWORK", "moon")]);
        assert!(load_with(&d, &bad, None).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_retired_scan_window_is_refused_with_the_reason() {
        let d = dir("retired");
        fs::write(d.join(FILE_NAME), "[scan]\nmax_transactions = 1000\n").unwrap();
        let e = load_with(&d, &env_of(&[("HOME", "/h")]), None).unwrap_err();
        assert!(e.iter().any(|p| p.contains("retired")), "{e:?}");
        fs::remove_file(d.join(FILE_NAME)).unwrap();
        let e = load_with(
            &d,
            &env_of(&[("HOME", "/h"), ("PSP_SCAN_MAX_TRANSACTIONS", "5")]),
            None,
        )
        .unwrap_err();
        assert!(e.iter().any(|p| p.contains("retired")), "{e:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_reconciliation_window_is_fourteen_days_by_default_and_set_in_days() {
        let mut c = Config::defaults("/h");
        assert_eq!(c.reconciliation_window_days, 14);
        assert_eq!(c.get("settle.reconciliation_window_days").unwrap(), "14");
        assert_eq!(c.terms().reconciliation_window_secs, 14 * 86_400);
        c.set("settle.reconciliation_window_days", "30").unwrap();
        assert_eq!(c.terms().reconciliation_window_secs, 30 * 86_400);
        assert!(c.validate().is_ok());
        // a decimal or a sign is not a number of days
        assert!(c.set("settle.reconciliation_window_days", "1.5").is_err());
        assert!(c.set("settle.reconciliation_window_days", "-1").is_err());
        for bad in ["0", "366"] {
            c.set("settle.reconciliation_window_days", bad).unwrap();
            let problems = c.validate().unwrap_err();
            assert!(
                problems
                    .iter()
                    .any(|p| p.starts_with("settle.reconciliation_window_days")),
                "{bad}: {problems:?}"
            );
        }
    }

    #[test]
    fn the_reconciliation_window_can_come_from_the_file_and_the_environment() {
        let d = dir("window");
        fs::write(
            d.join(FILE_NAME),
            "[settle]\nreconciliation_window_days = 3\n",
        )
        .unwrap();
        let loaded = load_with(&d, &env_of(&[("HOME", "/h")]), None).unwrap();
        assert_eq!(loaded.config.reconciliation_window_days, 3);
        let loaded = load_with(
            &d,
            &env_of(&[("HOME", "/h"), ("PSP_RECONCILIATION_WINDOW_DAYS", "21")]),
            None,
        )
        .unwrap();
        assert_eq!(loaded.config.reconciliation_window_days, 21);
        fs::remove_dir_all(&d).unwrap();
    }
}
