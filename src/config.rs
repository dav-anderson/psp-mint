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
//! The file is TOML in sections (`[settle]`, `base_confirmations = 3`). A key the
//! mint does not know is an error, so a misspelt setting never silently does
//! nothing. So is a value of the wrong type or out of range: the mint lists
//! every problem and does not start.
//!
//! Reloading: send the running mint `SIGHUP`. It reads the file and the
//! environment again, checks the result, and swaps it in all at once, or keeps the
//! old values and says why ([`Handle`]). Every setting in `SPECS` has a `reload`
//! flag: `true` is a setting that "reloads live" (the fees, the limits, the
//! settlement rules, the price rules, the scan interval, the health allowance,
//! the burn settings and the RPC address, cookie and timeout); `false` is a
//! setting that "needs a restart" (the listen address, the network, the bitcoin
//! wallet, the keys and rotation, the AIR server). It is read once, when the
//! process opens the mint; a reload names a change to it and leaves the running
//! value alone until the mint is restarted, which is a brief downtime (see the
//! README). `PSP_DATA_DIR` is not a setting, because the file lives in it.

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

/// The shortest PSP rotation period the mint starts with (rotation on): the
/// claim window, in hours (720: 30 days). A claim must name the keyset that is
/// current when it arrives, and a payer has the whole window to claim, so the
/// keyset a payer claims with has to outlive the window. A hard limit, for PSP
/// only. It is tied to `sp_wire::psp::CLAIM_WINDOW_SECS`.
pub const MIN_PSP_ROTATION_PERIOD_HOURS: u64 = sp_wire::psp::CLAIM_WINDOW_SECS / 3600;
/// The environment variable that names a config file elsewhere.
pub const FILE_ENV: &str = "PSP_CONFIG";
/// The default ceiling on one request body (1 MiB).
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024;

// ---- the caps ----------------------------------------------------------------
//
// Every size ceiling has a hard cap (and a floor), checked when the settings
// load, so that no setting can reach a value that breaks the mint's own routes.
// The fees have no cap of their own: see `MAX_FEE_UNITS`. The numbers and the
// reasoning are in DESIGN.md, "Caps on sizes and fees"; the sums are checked at
// compile time below.

/// The most any request ceiling may be: 15 MiB, the largest message AIR carries
/// (16 MiB) less 1 MiB kept for the envelope, its seal and AIR's framing. A higher
/// ceiling would let the mint take a request that no wallet can send. (The two
/// loopback routes, `/melt` and `/burns`, are called by esp-mint, whose
/// `exchange.max_items` cap of 25,000 makes the largest melt about 9.2 MB.)
pub const MAX_REQUEST_BYTES_CAP: u64 = 15 * 1024 * 1024;
/// The least any request ceiling may be: 16 KiB. The widest one-note request is
/// about 8.1 KB: a spend with one note, a 256-byte memo and 64 change outputs
/// (a rollover with 64 outputs is 7.9 KB), each field at its widest in JSON.
pub const MIN_REQUEST_BYTES: u64 = 16 * 1024;
/// The least `rollover.max_items` may be: 64, the most outputs one amount can
/// need (a `u64` splits into at most 64 powers of two), so that a one-note spend
/// always fits. The largest change one note can need is 63 outputs, for a note of
/// 2^63, the largest denomination every keyset has.
pub const MIN_ITEMS: u64 = 64;
/// The most `rollover.max_items` may be: 25,000. A note and an output in JSON
/// are 482 bytes together, so a request of that many is 12.05 MB, which fits
/// [`MAX_REQUEST_BYTES_CAP`]; a higher count could not.
pub const MAX_ITEMS_CAP: u64 = 25_000;
const _: () = {
    // a request of the most items fits the largest ceiling (482 bytes a note and
    // output pair in JSON, measured)
    assert!(MAX_ITEMS_CAP * 482 < MAX_REQUEST_BYTES_CAP);
    // the widest one-note request is 8,090 bytes; the smallest ceiling is twice that
    assert!(MIN_REQUEST_BYTES >= 2 * 8_090);
    // a note of the largest denomination (2^63) has change of at most 63 outputs
    assert!(MIN_ITEMS >= 63);
};
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

/// The settings psp-mint 0.8.0 removed with the tolerance, reconciliation target,
/// credit and forfeit design, each with why and what to use instead. A removed
/// key in `psp-mint.toml`, or its environment variable, is an error that stops the
/// mint (it never silently does something else), and `psp-mint config` says the
/// same. The renamed ones are not read under their old names.
pub const REMOVED: &[(&str, &str, &str)] = &[
    (
        "settle.min_confirmations",
        "PSP_MIN_CONFIRMATIONS",
        "a payment now waits for the confirmation tier its locked USD value picks: set settle.base_confirmations and settle.confirmation_tiers",
    ),
    (
        "settle.price_tolerance",
        "PSP_PRICE_TOLERANCE",
        "there is no tolerance: a payment is owed whole units at its locked price, and the remainder under one unit is kept",
    ),
    (
        "settle.payment_reconciliation_target",
        "PSP_PAYMENT_RECONCILIATION_TARGET",
        "there is no reconciliation target: a payment is owed whole units at its locked price, and the remainder under one unit is kept",
    ),
    (
        "settle.reconciliation_window_days",
        "PSP_RECONCILIATION_WINDOW_DAYS",
        "the claim window is fixed at 30 days (sp_wire::psp::CLAIM_WINDOW_SECS), not a setting",
    ),
    (
        "reconciliation.fee",
        "PSP_RECONCILIATION_FEE",
        "there is no reconciliation and no fee on it; the prepaid costs are claim.write_fee and delivery.read_fee",
    ),
    (
        "reconciliation.max_attempts",
        "PSP_RECONCILIATION_MAX_ATTEMPTS",
        "it is now claim.max_attempts",
    ),
    (
        "reconciliation.write_fee",
        "PSP_RECONCILIATION_WRITE_FEE",
        "it is now claim.write_fee",
    ),
    (
        "reconciliation.round_up_from",
        "PSP_RECONCILIATION_ROUND_UP_FROM",
        "there are no credits to round: units round down and the remainder is kept",
    ),
    (
        "settle.underpay_tolerance_bps",
        "PSP_UNDERPAY_TOLERANCE_BPS",
        "it was retired earlier and the tolerance design is gone",
    ),
    (
        "settle.credit_threshold_bps",
        "PSP_CREDIT_THRESHOLD_BPS",
        "it was retired earlier and the credit design is gone",
    ),
    (
        "mint.max_denomination_units",
        "PSP_MAX_DENOMINATION_UNITS",
        "every keyset has one key per power of two, 1 to 2^63 (64 keys), as fsp-mint's: the notes no longer ride in the payment, so there is nothing to cap",
    ),
];

/// Why a key (as in the file) was removed, if it was.
fn removed_key(key: &str) -> Option<String> {
    REMOVED
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(k, _, why)| format!("{k} was removed in psp-mint 0.8.0: {why}. Remove it"))
}

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
    /// Whether a running mint picks up a change on `SIGHUP` as it is ("reloads
    /// live"). A setting with `false` ("needs a restart") is read once, when the
    /// process opens the mint: `SIGHUP` names a change to it and the running
    /// value stays until the mint is restarted (see the README).
    pub reload: bool,
    pub what: &'static str,
}

/// The most a fee setting may be: 2^62 PSP units, far above anything real. It is
/// not a limit on what a mint should charge (there is none, see DESIGN.md, "Caps
/// on sizes and fees"); it keeps the sums of fees far from `u64::MAX`.
pub const MAX_FEE_UNITS: u64 = 1 << 62;

pub const SPECS: &[Spec] = &[
    Spec {
        key: "server.bind",
        env: "PSP_BIND",
        default: "127.0.0.1:3000",
        units: "host:port",
        reload: false,
        what: "The address of the HTTP API that is left (`serve` only): `/melt` and `/burns` (the ESP mint's calls, inside the SPI), `/audit` (the operator's report) and `/health` (read by the SPI and the health monitor). Must be a loopback address: wallets reach this mint over AIR only, never over HTTP.",
    },
    Spec {
        key: "server.melt_max_request_bytes",
        env: "PSP_MELT_MAX_BYTES",
        default: "1048576",
        units: "bytes",
        reload: true,
        what: "The ceiling on one `POST /melt` body (1 MiB). A larger body is refused with 413 before it is parsed: nothing is burned. 16384 to 15728640 (15 MiB: the most one AIR message carries).",
    },
    Spec {
        key: "server.rollover_max_request_bytes",
        env: "PSP_ROLLOVER_MAX_BYTES",
        default: "1048576",
        units: "bytes",
        reload: true,
        what: "The ceiling on one rollover request or one spend request (1 MiB), which come over AIR. A larger body is refused with 413 before it is parsed: nothing is burned. 16384 to 15728640 (15 MiB: the most one AIR message carries).",
    },
    Spec {
        key: "rollover.max_items",
        env: "PSP_ROLLOVER_MAX_ITEMS",
        default: "1000",
        units: "notes",
        reload: true,
        what: "The most notes in one rollover request, and the most notes in and the most change outputs out of one spend request. More is refused 400 before anything is burned. 64 to 25000: at least the 64 outputs one amount can need, at most what fits one AIR message.",
    },
    Spec {
        key: "server.burns_max_request_bytes",
        env: "PSP_BURNS_MAX_BYTES",
        default: "1048576",
        units: "bytes",
        reload: true,
        what: "The ceiling on one `POST /burns` body (1 MiB). A larger body is refused with 413 before it is parsed. 16384 to 15728640 (15 MiB: the most one AIR message carries).",
    },
    Spec {
        key: "server.check_state_max_request_bytes",
        env: "PSP_CHECK_STATE_MAX_BYTES",
        default: "1048576",
        units: "bytes",
        reload: true,
        what: "The ceiling on one check-state request (1 MiB), which comes over AIR. A larger body is refused with 413 before it is parsed. 16384 to 15728640 (15 MiB: the most one AIR message carries).",
    },
    Spec {
        key: "bitcoin.network",
        env: "PSP_NETWORK",
        default: "main",
        units: "network",
        reload: false,
        what: "main, regtest, signet or testnet. `--regtest`, `--signet` or `--testnet` on the command line overrides it. Picks the defaults below and the data folder.",
    },
    Spec {
        key: "bitcoin.rpc_url",
        env: "PSP_RPC_URL",
        default: "http://127.0.0.1:8332",
        units: "URL",
        reload: true,
        what: "Bitcoin Core's RPC address, without the wallet path. Default per network: port 8332 main, 18443 regtest, 38332 signet, 18332 testnet.",
    },
    Spec {
        key: "bitcoin.wallet",
        env: "PSP_WALLET",
        default: "psp-main",
        units: "name",
        reload: false,
        what: "The Bitcoin Core wallet that receives payments. Default per network: psp-main, psp-regtest, psp-signet, psp-testnet.",
    },
    Spec {
        key: "bitcoin.cookie",
        env: "PSP_COOKIE",
        default: "$HOME/Library/Application Support/Bitcoin/.cookie (macOS), $HOME/.bitcoin/.cookie (Linux)",
        units: "path",
        reload: true,
        what: "Bitcoin Core's RPC cookie file. Default per network: bitcoind's own (`.cookie`, `regtest/.cookie`, `signet/.cookie`, `testnet3/.cookie` under its data directory).",
    },
    Spec {
        key: "bitcoin.rpc_timeout_secs",
        env: "PSP_RPC_TIMEOUT_SECS",
        default: "30",
        units: "seconds",
        reload: true,
        what: "How long one RPC call may take before the scan fails and is retried.",
    },
    Spec {
        key: "scan.interval_secs",
        env: "PSP_SCAN_INTERVAL_SECS",
        default: "120",
        units: "seconds",
        reload: true,
        what: "Time between wallet scans (`serve` only).",
    },
    Spec {
        key: "scan.health_intervals",
        env: "PSP_HEALTH_INTERVALS",
        default: "3",
        units: "scan intervals",
        reload: true,
        what: "`/health` fails after this many scan intervals plus `scan.health_grace_secs` without a successful scan (`serve` only).",
    },
    Spec {
        key: "scan.health_grace_secs",
        env: "PSP_HEALTH_GRACE_SECS",
        default: "60",
        units: "seconds",
        reload: true,
        what: "Added to `scan.health_intervals` intervals before `/health` fails, for a slow price fetch.",
    },
    Spec {
        key: "price.max_age_secs",
        env: "PSP_PRICE_MAX_AGE_SECS",
        default: "120",
        units: "seconds",
        reload: true,
        what: "A quote whose time is further than this from now is excluded.",
    },
    Spec {
        key: "price.max_spread_bps",
        env: "PSP_PRICE_MAX_SPREAD_BPS",
        default: "200",
        units: "basis points",
        reload: true,
        what: "With two usable quotes, wait if they differ by more than this of the lower one (2%).",
    },
    Spec {
        key: "price.timeout_secs",
        env: "PSP_PRICE_TIMEOUT_SECS",
        default: "10",
        units: "seconds",
        reload: true,
        what: "How long one ticker request may take.",
    },
    Spec {
        key: "settle.lock_confirmations",
        env: "PSP_LOCK_CONFIRMATIONS",
        default: "1",
        units: "confirmations",
        reload: true,
        what: "A payment's price is locked when it is first seen with this many confirmations.",
    },
    Spec {
        key: "settle.base_confirmations",
        env: "PSP_BASE_CONFIRMATIONS",
        default: "3",
        units: "confirmations",
        reload: true,
        what: "The confirmations a payment worth less than the first tier of settle.confirmation_tiers must reach before the mint writes its owed record. It is at least settle.lock_confirmations (the mint refuses to start otherwise).",
    },
    Spec {
        key: "settle.confirmation_tiers",
        env: "PSP_CONFIRMATION_TIERS",
        default: "10000:4,50000:5,100000:6",
        units: "whole USD:confirmations, comma-separated",
        reload: true,
        what: "The deeper confirmations a larger payment waits for. Each entry is a whole-dollar threshold and the confirmations for a payment worth at least that much at its locked price, so the default waits for 4 from $10,000, 5 from $50,000 and 6 from $100,000, and settle.base_confirmations below the first. The thresholds ascend, the confirmations never go down (and never below settle.base_confirmations or settle.lock_confirmations), and none is over 1000; the mint refuses to start otherwise. The tier is chosen once, when the price is locked, and stored with the payment, so a change does not touch payments already locked. An empty value means no tiers. The scan trails the chain tip by the highest of them.",
    },
    Spec {
        key: "settle.retention_days",
        env: "PSP_RETENTION_DAYS",
        default: "90",
        units: "days; 0 keeps every record",
        reload: true,
        what: "How long settlements.json keeps a payment after it is finished. The file is rewritten whole on every change, so by default it must not grow without limit: once a claimed payment's claimed time, an expired payment's expired time, a kept payment's kept time (kept with nothing owed) or an abandoned payment's given-up time (its owed record could never be written) is this many days old, the payment is removed from the file (once per scan, at the same step as expiry). Payments that are locked, open, landed and unclaimed, not stored on AIR yet, or being claimed are never removed. What the mint still needs of a removed payment is kept in a few dozen bytes: its txid and block time (so the scan can never owe it again), its place in its claim key's count of owed records (so an index is never reused) and its share of the /audit totals (so they never shrink); settle.prune_horizon_days says how long the first two are kept. A claim resent after its payment was removed is ignored as an unknown record. 0 = keep everything: nothing is ever removed, no marker is ever made, and settle.prune_horizon_days has nothing to cap; windows still close on time and expired payments stay in the file. Otherwise 30 to 3650 days; 90 by default.",
    },
    Spec {
        key: "settle.prune_horizon_days",
        env: "PSP_PRUNE_HORIZON_DAYS",
        default: "210",
        units: "days; 0 keeps every marker",
        reload: true,
        what: "How long the mint remembers a payment it removed from settlements.json (see settle.retention_days), by the payment's block time. At each prune pass a cutoff moves forward to the larger of itself and now less this many days, and never back. A removed payment's txid, and a claim key's count of removed owed records, are dropped once their payment is older than the cutoff, and a payment with no record whose block time is before the cutoff is ignored by the scan (logged once per pass), so settlements.json is bounded by the payments of the last horizon, the live ones and a fixed-size total. Raising this setting does not bring back a marker that was already dropped. The cost: a payment first seen after the cutoff has passed it (the scan blind to it for longer than the horizon, or a rescan of very old blocks) is not settled. Where the markers go the owed index of an old claim key may be reused, which is harmless: a payer keeps the one-time claim key only until its first read plus 30 days. With settle.retention_days above 0 it must be at least 2 x settle.retention_days + 30 days (the mint refuses to start, or a reload that breaks it changes nothing, and the message names both keys), and at most 10000. 0 = never drop a marker and never move the cutoff. With settle.retention_days = 0 nothing is removed, so the horizon has nothing to cap and any value up to 10000 is accepted. 210 by default (2 x 90 + 30).",
    },
    Spec {
        key: "claim.max_attempts",
        env: "PSP_CLAIM_MAX_ATTEMPTS",
        default: "2",
        units: "attempts",
        reload: true,
        what: "How many claims refused for their content an owed record accepts before it ignores every further one and is left to expire when its window closes. An attempt is a claim signed by the right claim key, for an open record, refused for its content (the wrong total, a denomination the mint does not have, a repeated or already signed blind nonce), with a new hash; a bad signature, an unknown record (or one not stored on AIR yet), a claim that names a keyset that is not the current one (the previous, a retired or an unknown keyset), a claim after the window, a claim after the accepted one and an identical resend are not counted. A refused claim gets no reply. Fixed for a payment when its owed record is made. 1 to 1000; 2 by default.",
    },
    Spec {
        key: "claim.write_fee",
        env: "PSP_CLAIM_WRITE_FEE",
        default: "0",
        units: "PSP units",
        reload: true,
        what: "The cost of the one kind 15 write, which the SPI prepays on the wallet's behalf (bound to the claim key and capped by claim.max_attempts). It is taken from the units the payment is owed, once per payment: the SPI absorbs the cost of a second attempt. The owed record and the answer to the claim show it as write_prepaid_units, and /audit adds it up for the claimed payments. Fixed for a payment when its price is locked. 0 or more whole PSP units; 0 (the default) takes nothing. A payment whose costs are as much as it buys is kept with nothing owed. Only the amount is recorded today: the prepaid AIR write itself is not built (it waits on AIR).",
    },
    Spec {
        key: "delivery.read_fee",
        env: "PSP_DELIVERY_READ_FEE",
        default: "0",
        units: "PSP units",
        reload: true,
        what: "The read cost the PSP prepays for each record a payer must read: the owed record, and the answer to the claim. Both come off the units the payment is owed (twice this fee, once per payment). The owed record and the answer show it as read_prepaid_units, and /audit adds it up for the claimed payments. It is meant to be very small. Fixed for a payment when its price is locked. 0 or more whole PSP units; 0 (the default) takes nothing. A payment whose costs are as much as it buys is kept with nothing owed. Only the amounts are recorded today: the prepaid AIR read itself is not built (it waits on AIR).",
    },
    Spec {
        key: "burn.enabled",
        env: "PSP_BURN_ENABLED",
        default: "false",
        units: "true or false",
        reload: true,
        what: "Whether the PSP the mint is paid (the `pay` of a spend, kind 14) may be burned. False (the default): none is burned, and the mint reissues every paid unit as PSP notes in a file in `revenue/` under the data folder, for the SPI's own ecash-wallet to take in (`ecash-wallet receive-psp`). True: the mint reissues paid PSP only while the SPI's wallet holds less than `burn.threshold_psp`, and burns the rest. With no usable wallet balance figure (`burn.balance_file`) nothing is burned. Only paid PSP is ever burned: the PSP the ESP mint burns for an exchange, and the PSP bought with bitcoin, are not touched by this setting.",
    },
    Spec {
        key: "burn.threshold_psp",
        env: "PSP_BURN_THRESHOLD_PSP",
        default: "0",
        units: "PSP units",
        reload: true,
        what: "With `burn.enabled` true: the PSP the SPI's wallet is kept at. Measured against what the wallet holds now: the balance the SPI last published in `burn.balance_file`, plus the PSP the mint has reissued since (`reissued` in `/audit`). Paid PSP is reissued only up to the room left under it (threshold less what the wallet holds); the rest is burned, all of it once the wallet holds the threshold or more, and reissuing starts again when the wallet spends. 0 burns every paid unit whenever a figure is usable. Ignored while `burn.enabled` is false.",
    },
    Spec {
        key: "burn.balance_file",
        env: "PSP_BURN_BALANCE_FILE",
        default: "",
        units: "path",
        reload: true,
        what: "Where the SPI publishes its wallet's PSP balance for `burn.threshold_psp`: a JSON file `{\"psp\": N, \"reissued\": N, \"at\": N}` that `spi` writes (the balance, the mint's own `reissued` total read right after it, and the time in seconds). Empty (the default): `wallet_balance.json` in the network's data folder. Read at each spend, and only while `burn.enabled` is true. A file that is missing, unreadable, not that shape, or older than `burn.balance_max_age_secs` gives no figure, and then every paid unit is reissued: the mint never burns on a guess.",
    },
    Spec {
        key: "burn.balance_max_age_secs",
        env: "PSP_BURN_BALANCE_MAX_AGE_SECS",
        default: "86400",
        units: "seconds, 1 to 604,800",
        reload: true,
        what: "How old the balance figure in `burn.balance_file` may be before the mint ignores it (and reissues every paid unit). A figure dated more than a minute ahead of the clock is ignored too. 1 to 604,800 (a week). Ignored while `burn.enabled` is false.",
    },
    Spec {
        key: "rotation.activation_hours",
        env: "PSP_ROTATION_ACTIVATION_HOURS",
        default: "8760",
        units: "hours",
        reload: false,
        what: "Hours from the mint's first issuance (recorded once in first_issue.json) to its first key rotation. 0 = never rotate (one key set, one spent list).",
    },
    Spec {
        key: "rotation.period_hours",
        env: "PSP_ROTATION_PERIOD_HOURS",
        default: "87600",
        units: "hours",
        reload: false,
        what: "Hours between key rotations (10 years). Notes of the previous key set stay good until the next rotation; then that set and its spent list are deleted. 0 = never rotate. When rotation is on it must be at least 720 hours (30 days, the claim window): a claim must name the current keyset, so the keyset a payer claims with has to outlive the claim window (the mint refuses to start otherwise).",
    },
    Spec {
        key: "storage.mint_state",
        env: "PSP_STORAGE_MINT_STATE",
        default: "",
        units: "service DIDs (66 hex characters)",
        reload: false,
        what: "Which AIR services keep this mint's state (keys, spent lists, records), as an ordered list of service IDs: index 0 is the primary and the rest are secondaries, in the order they would be promoted. A service ID is the service's AIR DID, a 33-byte compressed key in hex (66 characters, starting 02 or 03); the old 32-byte x-only form is refused. In the environment the IDs are separated by commas; in the file it is an array of strings or one such string. One entry: that service holds the state and the mint keeps a local backup copy. Empty (the default): local only. At most 8, no duplicates. Setting only for now: nothing reads or writes the AIR yet (see DESIGN.md in sandbox-mint, \"AIR storage requirements\").",
    },
    Spec {
        key: "air.server",
        env: "PSP_AIR_SERVER",
        default: "",
        units: "AIR server name (66 hex characters)",
        reload: false,
        what: "The AIR server the listener holds reads on and writes replies to: its identity, a public key in AIR's compressed hex form (66 characters). The listener checks the server's signed challenge against it. Required by serve: the AIR listener always runs, because AIR is the only way wallets reach this mint. Until the AIR resolves a DID to a server (upstream item 4), this is configuration.",
    },
    Spec {
        key: "air.host",
        env: "PSP_AIR_HOST",
        default: "",
        units: "host or IP, no port",
        reload: false,
        what: "Where that AIR server is: a host name or an IP (127.0.0.1 for the SPI's own AIR server). No port: the AIR client always uses 5702 (ws). Required by serve, with air.server.",
    },
    Spec {
        key: "air.held_reads",
        env: "PSP_AIR_HELD_READS",
        default: "16",
        units: "reads",
        reload: false,
        what: "How many mailbox slots the listener holds a read on, ahead of the next unfinished one. More lets more wallets be answered at once without waiting for a slot to be reached; each held read costs the AIR server about 2 KB. The AIR server closes the whole session at its 257th held read, so the most accepted is 200.",
    },
    Spec {
        key: "air.timeout_secs",
        env: "PSP_AIR_TIMEOUT_SECS",
        default: "10",
        units: "seconds",
        reload: false,
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
    /// `settle.base_confirmations`.
    pub base_confirmations: u32,
    /// `settle.confirmation_tiers`: (whole USD, confirmations), ascending.
    pub confirmation_tiers: Vec<(u64, u32)>,
    /// `settle.retention_days`.
    pub settle_retention_days: u64,
    /// `settle.prune_horizon_days`.
    pub settle_prune_horizon_days: u64,
    /// `claim.max_attempts`.
    pub claim_max_attempts: u32,
    /// `claim.write_fee`, in PSP units.
    pub claim_write_fee: u64,
    /// `delivery.read_fee`, in PSP units.
    pub delivery_read_fee: u64,
    /// `burn.enabled`.
    pub burn_enabled: bool,
    /// `burn.threshold_psp`, in PSP units.
    pub burn_threshold_psp: u64,
    /// `burn.balance_file`; empty: the default place.
    pub burn_balance_file: String,
    /// `burn.balance_max_age_secs`.
    pub burn_balance_max_age_secs: u64,
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
            base_confirmations: settle::BASE_CONFIRMATIONS,
            confirmation_tiers: settle::DEFAULT_TIERS.to_vec(),
            settle_retention_days: settle::DEFAULT_RETENTION_DAYS,
            settle_prune_horizon_days: settle::DEFAULT_PRUNE_HORIZON_DAYS,
            claim_max_attempts: settle::DEFAULT_MAX_ATTEMPTS,
            claim_write_fee: 0,
            delivery_read_fee: 0,
            burn_enabled: false,
            burn_threshold_psp: 0,
            burn_balance_file: String::new(),
            burn_balance_max_age_secs: crate::balance::DEFAULT_MAX_AGE_SECS,
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

/// The largest tier threshold: $1 trillion, far above anything real; it keeps the
/// threshold in cents far from `u64::MAX`.
pub const MAX_TIER_USD: u64 = 1_000_000_000_000;

/// `settle.confirmation_tiers` from text: `10000:4,50000:5`, whole dollars and
/// confirmations, spaces allowed around the entries. Only the form is checked
/// here; `validate` checks the order and the ranges. An empty text is no tiers.
pub fn parse_tiers(key: &str, value: &str) -> Result<Vec<(u64, u32)>, String> {
    let text = value.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    text.split(',')
        .map(|entry| {
            let bad = || {
                format!(
                    "{key} = `{value}`: `{}` is not USD:confirmations (whole numbers, like 10000:4)",
                    entry.trim()
                )
            };
            let (usd, confirmations) = entry.split_once(':').ok_or_else(bad)?;
            let digits = |t: &str| !t.trim().is_empty() && t.trim().bytes().all(|b| b.is_ascii_digit());
            if !digits(usd) || !digits(confirmations) {
                return Err(bad());
            }
            Ok((
                usd.trim().parse().map_err(|_| bad())?,
                confirmations.trim().parse().map_err(|_| bad())?,
            ))
        })
        .collect()
}

/// The tiers as `parse_tiers` reads them.
pub fn tiers_text(tiers: &[(u64, u32)]) -> String {
    tiers
        .iter()
        .map(|(usd, confirmations)| format!("{usd}:{confirmations}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn whole<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{key} = `{value}` is not a whole number (no sign, no decimals)"))
}

/// `true` or `false`, and nothing else.
fn truth(key: &str, value: &str) -> Result<bool, String> {
    match value.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("{key} = `{value}` must be true or false")),
    }
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
            "settle.base_confirmations" => self.base_confirmations.to_string(),
            "settle.confirmation_tiers" => tiers_text(&self.confirmation_tiers),
            "settle.retention_days" => self.settle_retention_days.to_string(),
            "settle.prune_horizon_days" => self.settle_prune_horizon_days.to_string(),
            "claim.max_attempts" => self.claim_max_attempts.to_string(),
            "claim.write_fee" => self.claim_write_fee.to_string(),
            "delivery.read_fee" => self.delivery_read_fee.to_string(),
            "burn.enabled" => self.burn_enabled.to_string(),
            "burn.threshold_psp" => self.burn_threshold_psp.to_string(),
            "burn.balance_file" => self.burn_balance_file.clone(),
            "burn.balance_max_age_secs" => self.burn_balance_max_age_secs.to_string(),
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
            "settle.base_confirmations" => self.base_confirmations = whole(key, value)?,
            "settle.confirmation_tiers" => self.confirmation_tiers = parse_tiers(key, value)?,
            "settle.retention_days" => self.settle_retention_days = whole(key, value)?,
            "settle.prune_horizon_days" => self.settle_prune_horizon_days = whole(key, value)?,
            "claim.max_attempts" => self.claim_max_attempts = whole(key, value)?,
            "claim.write_fee" => self.claim_write_fee = whole(key, value)?,
            "delivery.read_fee" => self.delivery_read_fee = whole(key, value)?,
            "burn.enabled" => self.burn_enabled = truth(key, value)?,
            "burn.threshold_psp" => self.burn_threshold_psp = whole(key, value)?,
            "burn.balance_file" => self.burn_balance_file = value.trim().to_string(),
            "burn.balance_max_age_secs" => self.burn_balance_max_age_secs = whole(key, value)?,
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
            other => {
                return Err(
                    removed_key(other).unwrap_or_else(|| format!("unknown setting `{other}`"))
                );
            }
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
                MAX_REQUEST_BYTES_CAP,
                "bytes; a one-note request with 64 outputs has to fit, and 15 MiB is the most one AIR message carries",
            );
        }
        range(
            "rollover.max_items",
            self.rollover_max_items as u64,
            MIN_ITEMS,
            MAX_ITEMS_CAP,
            "notes; at least the 64 outputs one amount can need, and at most what fits one AIR message",
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
            "settle.base_confirmations",
            u64::from(self.base_confirmations),
            u64::from(self.lock_confirmations.max(1)),
            u64::from(settle::MAX_CONFIRMATIONS),
            "confirmations; at least settle.lock_confirmations, so the price is locked first",
        );
        for (key, value) in [
            ("claim.write_fee", self.claim_write_fee),
            ("delivery.read_fee", self.delivery_read_fee),
        ] {
            range(
                key,
                value,
                0,
                MAX_FEE_UNITS,
                "whole PSP units; 0 takes no fee",
            );
        }
        range(
            "claim.max_attempts",
            u64::from(self.claim_max_attempts),
            1,
            1000,
            "attempts; at least one claim must be allowed",
        );
        range(
            "burn.threshold_psp",
            self.burn_threshold_psp,
            0,
            MAX_FEE_UNITS,
            "whole PSP units; 0 burns every paid unit",
        );
        range(
            "burn.balance_max_age_secs",
            self.burn_balance_max_age_secs,
            1,
            604_800,
            "seconds",
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
        problems.extend(self.tier_problems());
        problems.extend(self.prune_problems());
        if self.rotation_activation_hours > 0
            && self.rotation_period_hours > 0
            && self.rotation_period_hours < MIN_PSP_ROTATION_PERIOD_HOURS
        {
            problems.push(format!(
                "rotation.period_hours = {} is too short: the PSP rotation period must be at least {MIN_PSP_ROTATION_PERIOD_HOURS} hours (30 days, the claim window), so that the keyset a payer claims with outlives the claim window (or 0 to turn rotation off)",
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
        if !self.air_server.is_empty()
            && let Err(e) = sp_wire::seal::parse_public_key(&self.air_server)
        {
            problems.push(format!(
                "air.server = `{}` is not an AIR server name (a public key of 66 hex characters): {e}",
                self.air_server
            ));
        }
        if !self.air_host.is_empty()
            && let Some(why) = air_host_problem(&self.air_host)
        {
            problems.push(format!("air.host = `{}` {why}", self.air_host));
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

    /// The prepaid costs and the limit of a claim (`claim.*` and
    /// `delivery.read_fee`).
    pub fn fees(&self) -> settle::ClaimFees {
        settle::ClaimFees {
            max_attempts: self.claim_max_attempts,
            write_fee_units: self.claim_write_fee,
            read_fee_units: self.delivery_read_fee,
        }
    }

    /// How long a finished payment is kept in the records, in seconds
    /// (`settle.retention_days`). 0 keeps every record: nothing is pruned.
    pub fn retention_secs(&self) -> u64 {
        self.settle_retention_days.saturating_mul(86_400)
    }

    /// How long the mint remembers a pruned payment, in seconds
    /// (`settle.prune_horizon_days`). 0 keeps every marker: the cutoff never
    /// moves.
    pub fn horizon_secs(&self) -> u64 {
        self.settle_prune_horizon_days.saturating_mul(86_400)
    }

    /// What is wrong with `settle.retention_days` and `settle.prune_horizon_days`,
    /// which are checked together: the horizon may not be shorter than twice the
    /// retention plus the 30-day claim window, unless it is 0 (keep every marker)
    /// or the retention is 0 (nothing is pruned, so the horizon has nothing to
    /// cap).
    fn prune_problems(&self) -> Vec<String> {
        let (retention, horizon) = (self.settle_retention_days, self.settle_prune_horizon_days);
        let mut problems = Vec::new();
        if retention != 0
            && !(settle::MIN_RETENTION_DAYS..=settle::MAX_RETENTION_DAYS).contains(&retention)
        {
            problems.push(format!(
                "settle.retention_days = {retention} is out of range: it must be 0 (keep every record), or from {} to {} (days)",
                settle::MIN_RETENTION_DAYS,
                settle::MAX_RETENTION_DAYS
            ));
        }
        if horizon > settle::MAX_PRUNE_HORIZON_DAYS {
            problems.push(format!(
                "settle.prune_horizon_days = {horizon} is out of range: it must be 0 (keep every marker), or at most {} (days)",
                settle::MAX_PRUNE_HORIZON_DAYS
            ));
        } else if retention != 0
            && horizon != 0
            && horizon < settle::prune_horizon_floor_days(retention)
        {
            problems.push(format!(
                "settle.prune_horizon_days = {horizon} is too short for settle.retention_days = {retention}: the horizon must be 0 (keep every marker) or at least 2 x settle.retention_days + 30 = {} days",
                settle::prune_horizon_floor_days(retention)
            ));
        }
        problems
    }

    /// The settlement rules and the claim fees together.
    pub fn policy(&self) -> settle::Policy {
        settle::Policy {
            terms: self.terms(),
            fees: self.fees(),
        }
    }

    pub fn terms(&self) -> Terms {
        Terms {
            lock_confirmations: self.lock_confirmations,
            base_confirmations: self.base_confirmations,
            tiers: self
                .confirmation_tiers
                .iter()
                .map(|&(usd, confirmations)| settle::Tier {
                    min_cents: usd.saturating_mul(100),
                    confirmations,
                })
                .collect(),
        }
    }

    /// What is wrong with the tiers: one line for each, so the operator sees
    /// everything at once.
    fn tier_problems(&self) -> Vec<String> {
        let key = "settle.confirmation_tiers";
        let mut problems = Vec::new();
        let mut previous: Option<(u64, u32)> = None;
        for &(usd, confirmations) in &self.confirmation_tiers {
            let entry = format!("{usd}:{confirmations}");
            if usd == 0 || usd > MAX_TIER_USD {
                problems.push(format!(
                    "{key} entry {entry}: the threshold must be from 1 to {MAX_TIER_USD} whole USD"
                ));
            }
            if confirmations < self.lock_confirmations || confirmations > settle::MAX_CONFIRMATIONS
            {
                problems.push(format!(
                    "{key} entry {entry}: the confirmations must be from settle.lock_confirmations ({}) to {}",
                    self.lock_confirmations,
                    settle::MAX_CONFIRMATIONS
                ));
            }
            if let Some((last_usd, last_confirmations)) = previous {
                if usd <= last_usd {
                    problems.push(format!(
                        "{key} entry {entry}: the thresholds must ascend, and {usd} is not above {last_usd}"
                    ));
                }
                if confirmations < last_confirmations {
                    problems.push(format!(
                        "{key} entry {entry}: a larger payment cannot wait for fewer confirmations than {last_confirmations}"
                    ));
                }
            } else if confirmations < self.base_confirmations {
                problems.push(format!(
                    "{key} entry {entry}: a tier cannot wait for fewer confirmations than settle.base_confirmations ({})",
                    self.base_confirmations
                ));
            }
            previous = Some((usd, confirmations));
        }
        problems
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
            burn: self.burn(),
            balance: crate::balance::Source {
                file: crate::balance::intern(&self.burn_balance_file),
                max_age_secs: self.burn_balance_max_age_secs,
            },
        }
    }

    /// What a spend does with the PSP it is paid (`burn.*`).
    pub fn burn(&self) -> crate::rotation::BurnPolicy {
        crate::rotation::BurnPolicy {
            enabled: self.burn_enabled,
            threshold_psp: self.burn_threshold_psp,
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
            if let Some(why) = removed_key(&key) {
                problems.push(why);
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
                toml::Value::Float(f) => {
                    problems.push(format!("{key} must be a whole number, not the decimal {f}"))
                }
                toml::Value::String(s) => pairs.push((key, s.clone())),
                // A boolean is a choice only for the burn switch; anywhere else it is a type
                // error, as a number is.
                toml::Value::Boolean(b) if key.starts_with("burn.") => {
                    pairs.push((key, b.to_string()))
                }
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

    for (key, var, why) in REMOVED {
        if env(var).is_some_and(|v| !v.is_empty()) {
            problems.push(format!(
                "environment variable {var}: {key} was removed in psp-mint 0.8.0: {why}. Remove it"
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

    if problems.is_empty()
        && let Err(list) = config.validate()
    {
        problems.extend(list);
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

impl Config {
    /// The settings that need a restart (`Spec::reload` is false), put back to
    /// what the running mint has. Returns each key the file changes with the
    /// value the file gives it: they wait for a restart.
    fn keep_restart_only(
        &mut self,
        running: &Config,
    ) -> Result<Vec<(&'static str, String)>, Vec<String>> {
        let mut ignored = Vec::new();
        let mut problems = Vec::new();
        for spec in SPECS.iter().filter(|s| !s.reload) {
            let (new, old) = (self.get(spec.key), running.get(spec.key));
            if new == old {
                continue;
            }
            ignored.push((spec.key, new.unwrap_or_default()));
            if let Some(old) = old
                && let Err(e) = self.set(spec.key, &old)
            {
                problems.push(e);
            }
        }
        if problems.is_empty() {
            Ok(ignored)
        } else {
            Err(problems)
        }
    }
}

/// What a reload did: the lines to log, and the settings that need a restart
/// and that the file changes, each with the value the file gives it. They are
/// not applied: the running mint keeps its values until it is restarted.
#[derive(Debug)]
pub struct Reloaded {
    pub lines: Vec<String>,
    pub needs_restart: Vec<(&'static str, String)>,
}

/// The running mint's settings, swapped all at once on a reload (`SIGHUP`).
///
/// A reload reads the file and the environment again and checks the result like
/// a start does. If anything is wrong it changes nothing and says what. If not,
/// every setting with `reload` in `SPECS` takes its new value at once, for the
/// next request or the next scan; the others (`reload: false`: the address, the
/// bitcoin wallet and network, the keys, the rotation, the AIR server) keep the
/// value the process is running with, and the reload returns which of them the
/// file changes, so that the caller can say they wait for a restart. A request
/// already running finishes with the values it started with.
pub struct Handle {
    inner: std::sync::RwLock<std::sync::Arc<Config>>,
    /// The network flag the process was started with, which beats the file.
    flag: Option<Network>,
}

impl Handle {
    pub fn new(config: Config, flag: Option<Network>) -> Self {
        Handle {
            inner: std::sync::RwLock::new(std::sync::Arc::new(config)),
            flag,
        }
    }

    /// The settings now in force. A request or a scan takes this once, at the
    /// start, and uses it throughout.
    pub fn current(&self) -> std::sync::Arc<Config> {
        match self.inner.read() {
            Ok(guard) => std::sync::Arc::clone(&guard),
            // A writer only swaps a pointer, so a poisoned lock still holds a
            // whole config.
            Err(poisoned) => std::sync::Arc::clone(&poisoned.into_inner()),
        }
    }

    fn replace(&self, config: Config) {
        let next = std::sync::Arc::new(config);
        match self.inner.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// Read the file and the environment again. On success the new settings are
    /// in force at once, except the ones that need a restart, which keep their
    /// running value until the mint is restarted. On any problem nothing
    /// changes. Returns the lines to log and the restart-only settings the file
    /// changes.
    pub fn reload(
        &self,
        data_dir: &Path,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Reloaded, Vec<String>> {
        let loaded = load_with(data_dir, env, self.flag)?;
        let running = self.current();
        let mut next = loaded.config;
        let ignored = next.keep_restart_only(&running)?;
        let mut lines = Vec::new();
        for spec in SPECS {
            let before = running.get(spec.key).unwrap_or_default();
            let after = next.get(spec.key).unwrap_or_default();
            if before != after {
                lines.push(format!(
                    "  {} changed from {before} to {after} {}",
                    spec.key, spec.units
                ));
            }
        }
        for (key, new) in &ignored {
            lines.push(format!(
                "  {key} is changed in the file from {} to {new}: it needs a restart (a brief downtime) and waits for one",
                running.get(key).unwrap_or_default()
            ));
        }
        if lines.is_empty() {
            lines.push("  no setting changed".to_string());
        }
        self.replace(next);
        Ok(Reloaded {
            lines,
            needs_restart: ignored,
        })
    }
}

/// The expected value for the operating system the tests run on: bitcoind's
/// default data folder is not the same on macOS and Linux, and neither is
/// assumed.
#[cfg(test)]
fn for_this_os<'a>(macos: &'a str, linux: &'a str) -> &'a str {
    if cfg!(target_os = "macos") {
        macos
    } else {
        linux
    }
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

    /// Fails if the floor stays 24 hours, or is not tied to the 30-day claim
    /// window (`CLAIM_WINDOW_SECS`).
    #[test]
    fn a_psp_rotation_period_under_the_claim_window_refuses_to_start() {
        let dir = scratch("floor");
        assert_eq!(MIN_PSP_ROTATION_PERIOD_HOURS, 720);
        assert_eq!(sp_wire::psp::CLAIM_WINDOW_SECS, 720 * 3600);
        fs::write(dir.join(FILE_NAME), "[rotation]\nperiod_hours = 719\n").unwrap();
        let p = load(&dir, &env_of(&[])).unwrap_err();
        let why = "rotation.period_hours = 719 is too short: the PSP rotation period must be at \
                   least 720 hours (30 days, the claim window), so that the keyset a payer \
                   claims with outlives the claim window (or 0 to turn rotation off)";
        assert!(p.iter().any(|e| e == why), "{p:?}");
        fs::write(dir.join(FILE_NAME), "").unwrap();
        let p = load(&dir, &env_of(&[("PSP_ROTATION_PERIOD_HOURS", "719")])).unwrap_err();
        assert!(p.iter().any(|e| e == why), "{p:?}");
        // 24 was the old floor
        let p = load(&dir, &env_of(&[("PSP_ROTATION_PERIOD_HOURS", "24")])).unwrap_err();
        assert!(p.iter().any(|e| e.contains("at least 720 hours")), "{p:?}");
        // 720 is fine; 0 (off) is fine; rotation off makes the period moot.
        assert!(load(&dir, &env_of(&[("PSP_ROTATION_PERIOD_HOURS", "720")])).is_ok());
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

    /// Fails if the default, either bound, the environment name or the file key of
    /// `settle.retention_days` is wrong, or if 0 (keep every record) is refused.
    #[test]
    fn the_retention_is_90_days_by_default_0_or_30_to_3650() {
        let dir = scratch("retention");
        let c = load(&dir, &env_of(&[])).unwrap().config;
        let handle = Handle::new(c.clone(), None);
        assert_eq!(c.settle_retention_days, 90);
        assert_eq!(c.retention_secs(), 7_776_000);
        // the horizon at its top, so that only the retention is under test
        for (days, ok) in [
            ("0", true),
            ("29", false),
            ("30", true),
            ("3650", true),
            ("3651", false),
        ] {
            let got = load(
                &dir,
                &env_of(&[
                    ("PSP_RETENTION_DAYS", days),
                    ("PSP_PRUNE_HORIZON_DAYS", "10000"),
                ]),
            );
            assert_eq!(got.is_ok(), ok, "{days}: {got:?}");
        }
        let p = load(
            &dir,
            &env_of(&[
                ("PSP_RETENTION_DAYS", "29"),
                ("PSP_PRUNE_HORIZON_DAYS", "10000"),
            ]),
        )
        .unwrap_err();
        assert!(
            p.iter().any(|e| e
                == "settle.retention_days = 29 is out of range: it must be 0 (keep every record), or from 30 to 3650 (days)"),
            "{p:?}"
        );
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\nretention_days = 365\nprune_horizon_days = 760\n",
        )
        .unwrap();
        let loaded = load(&dir, &env_of(&[])).unwrap();
        assert_eq!(loaded.config.settle_retention_days, 365);
        assert_eq!(loaded.sources["settle.retention_days"], Source::File);
        // a reload applies it from the next pass
        let reloaded = handle.reload(&dir, &env_of(&[])).unwrap();
        assert_eq!(handle.current().settle_retention_days, 365);
        assert!(reloaded.needs_restart.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The horizon is 210 days by default, and with a retention above 0 it may not
    /// be under 2 x retention + 30 days, in the file, in the environment and on a
    /// reload, which then changes nothing. Fails if the floor is not enforced
    /// together with the retention (a retention raised alone leaves a horizon too
    /// short), or if the message does not name both settings.
    #[test]
    fn the_prune_horizon_is_210_days_and_may_not_be_under_the_floor_for_the_retention() {
        let dir = scratch("horizon");
        let c = load(&dir, &env_of(&[])).unwrap().config;
        assert_eq!(c.settle_prune_horizon_days, 210);
        assert_eq!(c.horizon_secs(), 18_144_000);
        let short = "settle.prune_horizon_days = 209 is too short for settle.retention_days = 90: \
                     the horizon must be 0 (keep every marker) or at least 2 x \
                     settle.retention_days + 30 = 210 days";
        // 2 x 90 + 29 is refused, in the environment and in the file
        let p = load(&dir, &env_of(&[("PSP_PRUNE_HORIZON_DAYS", "209")])).unwrap_err();
        assert!(p.iter().any(|e| e == short), "{p:?}");
        fs::write(dir.join(FILE_NAME), "[settle]\nprune_horizon_days = 209\n").unwrap();
        let p = load(&dir, &env_of(&[])).unwrap_err();
        assert!(p.iter().any(|e| e == short), "{p:?}");
        // 2 x 90 + 30 is accepted
        fs::write(dir.join(FILE_NAME), "[settle]\nprune_horizon_days = 210\n").unwrap();
        assert!(load(&dir, &env_of(&[])).is_ok());
        // the ceiling
        for (days, ok) in [("10000", true), ("10001", false)] {
            let got = load(&dir, &env_of(&[("PSP_PRUNE_HORIZON_DAYS", days)]));
            assert_eq!(got.is_ok(), ok, "{days}: {got:?}");
        }
        // 3650 days of retention needs 7330, which fits under the ceiling
        let top = [
            ("PSP_RETENTION_DAYS", "3650"),
            ("PSP_PRUNE_HORIZON_DAYS", "7330"),
        ];
        assert!(load(&dir, &env_of(&top)).is_ok());
        let p = load(
            &dir,
            &env_of(&[
                ("PSP_RETENTION_DAYS", "3650"),
                ("PSP_PRUNE_HORIZON_DAYS", "7329"),
            ]),
        )
        .unwrap_err();
        assert!(
            p.iter().any(|e| e.contains("= 7329 is too short")
                && e.contains("settle.retention_days = 3650")
                && e.ends_with("= 7330 days")),
            "{p:?}"
        );

        // A reload that raises the retention past the horizon is refused whole.
        fs::write(dir.join(FILE_NAME), "").unwrap();
        let handle = Handle::new(c, None);
        fs::write(dir.join(FILE_NAME), "[settle]\nretention_days = 100\n").unwrap();
        let problems = handle.reload(&dir, &env_of(&[])).unwrap_err();
        assert!(
            problems.iter().any(|e| e
                == "settle.prune_horizon_days = 210 is too short for settle.retention_days = 100: \
                    the horizon must be 0 (keep every marker) or at least 2 x \
                    settle.retention_days + 30 = 230 days"),
            "{problems:?}"
        );
        assert_eq!(handle.current().settle_retention_days, 90);
        assert_eq!(handle.current().settle_prune_horizon_days, 210);
        // together they are accepted, live
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\nretention_days = 100\nprune_horizon_days = 230\n",
        )
        .unwrap();
        let reloaded = handle.reload(&dir, &env_of(&[])).unwrap();
        assert!(reloaded.needs_restart.is_empty());
        assert_eq!(handle.current().settle_retention_days, 100);
        assert_eq!(handle.current().horizon_secs(), 19_872_000);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 0 keeps everything: a retention of 0 prunes no record (so any horizon is
    /// accepted, there being nothing to cap), and a horizon of 0 keeps every
    /// marker (so no floor applies). Fails if either 0 is refused, if a nonzero
    /// retention is checked against the floor when the horizon is 0, or if the
    /// horizon above 0 is not checked once the retention is back above 0.
    #[test]
    fn a_retention_or_a_horizon_of_0_means_keep_everything_and_reloads_live() {
        let dir = scratch("keep-all");
        // retention 0 with any horizon: 1 day, the default, the ceiling; not above
        for (days, ok) in [
            ("0", true),
            ("1", true),
            ("209", true),
            ("10000", true),
            ("10001", false),
        ] {
            let got = load(
                &dir,
                &env_of(&[
                    ("PSP_RETENTION_DAYS", "0"),
                    ("PSP_PRUNE_HORIZON_DAYS", days),
                ]),
            );
            assert_eq!(got.is_ok(), ok, "{days}: {got:?}");
        }
        // horizon 0 with any retention
        for days in ["30", "90", "3650"] {
            let got = load(
                &dir,
                &env_of(&[
                    ("PSP_RETENTION_DAYS", days),
                    ("PSP_PRUNE_HORIZON_DAYS", "0"),
                ]),
            );
            assert!(got.is_ok(), "{days}: {got:?}");
        }
        // a retention above 0 with a horizon under the floor is still refused
        let p = load(
            &dir,
            &env_of(&[
                ("PSP_RETENTION_DAYS", "365"),
                ("PSP_PRUNE_HORIZON_DAYS", "759"),
            ]),
        )
        .unwrap_err();
        assert!(
            p.iter().any(|e| e.contains("= 759 is too short")
                && e.contains("settle.retention_days = 365")
                && e.ends_with("= 760 days")),
            "{p:?}"
        );

        // Live: into 0 and out of it, for each setting.
        let handle = Handle::new(load(&dir, &env_of(&[])).unwrap().config, None);
        fs::write(dir.join(FILE_NAME), "[settle]\nretention_days = 0\n").unwrap();
        let reloaded = handle.reload(&dir, &env_of(&[])).unwrap();
        assert!(reloaded.needs_restart.is_empty());
        assert_eq!(handle.current().retention_secs(), 0);
        assert_eq!(handle.current().horizon_secs(), 18_144_000);
        // while the retention is 0 a retention above 0 still has to fit the horizon
        fs::write(dir.join(FILE_NAME), "[settle]\nretention_days = 365\n").unwrap();
        assert!(handle.reload(&dir, &env_of(&[])).is_err());
        assert_eq!(handle.current().retention_secs(), 0);
        // out of 0, and the horizon into 0
        fs::write(dir.join(FILE_NAME), "[settle]\nprune_horizon_days = 0\n").unwrap();
        let reloaded = handle.reload(&dir, &env_of(&[])).unwrap();
        assert!(reloaded.needs_restart.is_empty());
        assert_eq!(handle.current().retention_secs(), 7_776_000);
        assert_eq!(handle.current().horizon_secs(), 0);
        // and out of it: the defaults
        fs::remove_file(dir.join(FILE_NAME)).unwrap();
        handle.reload(&dir, &env_of(&[])).unwrap();
        assert_eq!(handle.current().retention_secs(), 7_776_000);
        assert_eq!(handle.current().horizon_secs(), 18_144_000);
        fs::remove_dir_all(&dir).unwrap();
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

        // The values the typed fields hold by default, written out, so the table
        // above is not the only thing checked against itself. The network is
        // mainnet by default, with mainnet's node settings.
        let c = Config::defaults("/h");
        assert_eq!(c.network, Network::Main);
        assert_eq!(c.bind, "127.0.0.1:3000");
        assert_eq!(c.wallet_url(), "http://127.0.0.1:8332/wallet/psp-main");
        assert_eq!(
            c.cookie,
            for_this_os(
                "/h/Library/Application Support/Bitcoin/.cookie",
                "/h/.bitcoin/.cookie"
            )
        );
        assert_eq!(c.scan_interval_secs, 120);
        assert_eq!(c.rpc_timeout_secs, 30);
        assert_eq!(c.terms(), Terms::default());
        assert_eq!(c.price(), price::Config::default());
        assert_eq!(c.base_confirmations, 3);
        assert_eq!(c.get("settle.base_confirmations").unwrap(), "3");
        assert_eq!(
            c.get("settle.confirmation_tiers").unwrap(),
            "10000:4,50000:5,100000:6"
        );
        for bytes in [
            c.melt_max_request_bytes,
            c.burns_max_request_bytes,
            c.rollover_max_request_bytes,
            c.check_state_max_request_bytes,
        ] {
            assert_eq!(bytes, 1024 * 1024);
        }
        assert_eq!(c.rollover_max_items, 1000);
        fs::remove_dir_all(&dir).unwrap();
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
        let problems = load(&dir, &env_of(&[("PSP_AIR_HOST", "127.0.0.1:5702")])).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("air.host = `127.0.0.1:5702`") && p.contains("no port")),
            "{problems:?}"
        );
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
            "[settle]\nbase_confirmations = 2\nconfirmation_tiers = \"20000:3\"\n[bitcoin]\nwallet = \"shop\"\n[claim]\nwrite_fee = 3\n",
        )
        .unwrap();
        let loaded = load(
            &dir,
            &env_of(&[
                ("PSP_CLAIM_MAX_ATTEMPTS", "5"),
                ("PSP_CONFIRMATION_TIERS", "20000:3, 60000:6"),
            ]),
        )
        .unwrap();
        assert_eq!(loaded.config.base_confirmations, 2);
        assert_eq!(loaded.config.wallet, "shop");
        // a key of another section from the file, and two more from the environment
        assert_eq!(loaded.config.claim_write_fee, 3);
        assert_eq!(loaded.sources["claim.write_fee"], Source::File);
        assert_eq!(loaded.config.claim_max_attempts, 5);
        assert_eq!(
            loaded.sources["claim.max_attempts"],
            Source::Env("PSP_CLAIM_MAX_ATTEMPTS")
        );
        assert_eq!(
            loaded.config.confirmation_tiers,
            vec![(20_000, 3), (60_000, 6)]
        );
        assert_eq!(
            loaded.sources["settle.confirmation_tiers"],
            Source::Env("PSP_CONFIRMATION_TIERS")
        );
        assert_eq!(loaded.sources["settle.base_confirmations"], Source::File);
        assert_eq!(loaded.file.as_deref(), Some(dir.join(FILE_NAME).as_path()));
        let lines = loaded.describe();
        assert_eq!(lines.len(), SPECS.len());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("settle.base_confirmations = 2") && l.contains("(file)"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("(env PSP_CONFIRMATION_TIERS)"))
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
            "[settle]\nbase_confirmation = 3\nlock_confirmations = true\n[scan]\ninterval_secs = 1.5\nloose = 1\n",
        )
        .unwrap();
        let problems = load(&dir, &env_of(&[])).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("unknown setting `settle.base_confirmation`"))
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

        // the health settings, alone: exactly these two are reported
        let mut c = Config::defaults("/h");
        c.health_intervals = 0;
        c.health_grace_secs = 86_401;
        let problems = c.validate().unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("scan.health_intervals"));
        assert!(problems[1].contains("scan.health_grace_secs"));
    }

    #[test]
    fn the_base_confirmations_are_at_least_the_lock_confirmations() {
        let mut c = Config::defaults("/h");
        c.lock_confirmations = 4;
        c.base_confirmations = 3;
        c.confirmation_tiers = vec![];
        let problems = c.validate().unwrap_err();
        assert!(problems[0].starts_with("settle.base_confirmations"));
        c.base_confirmations = 4;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn the_tiers_are_read_written_and_checked_as_a_table() {
        let k = "settle.confirmation_tiers";
        assert_eq!(parse_tiers(k, ""), Ok(vec![]));
        assert_eq!(parse_tiers(k, " 5:3 , 20:4 "), Ok(vec![(5, 3), (20, 4)]));
        assert_eq!(tiers_text(&[(5, 3), (20, 4)]), "5:3,20:4");
        for bad in ["5", "5:", ":3", "5:3,", "-5:3", "5.5:3", "5:3:4", "a:b"] {
            let err = parse_tiers(k, bad).unwrap_err();
            assert!(err.starts_with(&format!("{k} = `{bad}`")), "{bad}: {err}");
        }
        let mut c = Config::defaults("/h");
        assert!(c.validate().is_ok());
        // each of these breaks one rule, and the mint refuses to start on it
        for (tiers, why) in [
            ("50000:5,10000:6", "must ascend"),
            ("10000:6,10000:7", "must ascend"),
            ("10000:6,50000:5", "fewer confirmations than 6"),
            (
                "10000:2",
                "fewer confirmations than settle.base_confirmations (3)",
            ),
            ("0:4", "from 1 to"),
            ("10000:1001", "must be from settle.lock_confirmations"),
            ("1000000000001:4", "from 1 to"),
        ] {
            c.set(k, tiers).unwrap();
            let problems = c.validate().unwrap_err();
            assert!(
                problems.iter().any(|p| p.contains(why)),
                "{tiers}: {problems:?}"
            );
        }
        // equal confirmations in two tiers are fine, and so is no tier at all
        c.set(k, "10000:4,50000:4").unwrap();
        assert!(c.validate().is_ok());
        c.set(k, "").unwrap();
        assert!(c.validate().is_ok());
        assert_eq!(c.terms().highest(), 3);
    }

    #[test]
    fn a_removed_setting_is_refused_in_the_file_and_the_environment_with_what_replaces_it() {
        let dir = scratch("removed");
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\nmin_confirmations = 6\nprice_tolerance = 0.01\n[reconciliation]\nmax_attempts = 2\n",
        )
        .unwrap();
        let problems = load(&dir, &env_of(&[])).unwrap_err();
        for want in [
            "settle.min_confirmations was removed in psp-mint 0.8.0: a payment now waits for the confirmation tier",
            "settle.price_tolerance was removed in psp-mint 0.8.0: there is no tolerance",
            "reconciliation.max_attempts was removed in psp-mint 0.8.0: it is now claim.max_attempts",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(want)),
                "{want}: {problems:?}"
            );
        }
        fs::remove_file(dir.join(FILE_NAME)).unwrap();
        for (key, var, _) in REMOVED {
            let problems = load(&dir, &env_of(&[(var, "1")])).unwrap_err();
            assert!(
                problems.iter().any(|p| p.starts_with(&format!(
                    "environment variable {var}: {key} was removed in psp-mint 0.8.0"
                ))),
                "{var}: {problems:?}"
            );
        }
        let mut c = Config::defaults("/h");
        for (key, _, _) in REMOVED {
            assert!(
                c.set(key, "1").unwrap_err().contains("was removed"),
                "{key}"
            );
            assert!(SPECS.iter().all(|s| s.key != *key), "{key} is still a spec");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `mint.max_denomination_units` is gone: every keyset has the 64 keys 1 to
    /// 2^63. Fails if the key in the file or `PSP_MAX_DENOMINATION_UNITS` is
    /// accepted (whatever the value), or if it is still a setting.
    #[test]
    fn the_removed_max_denomination_units_is_refused_in_the_file_and_the_environment() {
        let dir = scratch("max-denomination");
        let why = "mint.max_denomination_units was removed in psp-mint 0.8.0: every keyset has one key per power of two, 1 to 2^63 (64 keys), as fsp-mint's: the notes no longer ride in the payment, so there is nothing to cap. Remove it";
        fs::write(
            dir.join(FILE_NAME),
            "[mint]\nmax_denomination_units = 16777216\n",
        )
        .unwrap();
        let problems = load(&dir, &env_of(&[])).unwrap_err();
        assert!(problems.iter().any(|p| p.contains(why)), "{problems:?}");
        fs::remove_file(dir.join(FILE_NAME)).unwrap();
        let problems =
            load(&dir, &env_of(&[("PSP_MAX_DENOMINATION_UNITS", "16777216")])).unwrap_err();
        let want = format!("environment variable PSP_MAX_DENOMINATION_UNITS: {why}");
        assert!(problems.contains(&want), "{problems:?}");
        assert!(SPECS.iter().all(|s| s.key != "mint.max_denomination_units"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_settings_reach_the_parts_that_use_them() {
        let mut c = Config::defaults("/h");
        c.lock_confirmations = 2;
        c.base_confirmations = 4;
        c.confirmation_tiers = vec![(500, 8), (2_000, 10)];
        c.price_timeout_secs = 4;
        c.melt_max_request_bytes = 5000;
        c.rollover_max_request_bytes = 6000;
        c.rollover_max_items = 7;
        c.rpc_url = "http://node:8332/".into();
        assert_eq!(
            c.terms(),
            Terms {
                lock_confirmations: 2,
                base_confirmations: 4,
                tiers: vec![
                    settle::Tier {
                        min_cents: 50_000,
                        confirmations: 8
                    },
                    settle::Tier {
                        min_cents: 200_000,
                        confirmations: 10
                    },
                ],
            }
        );
        assert_eq!(c.terms().highest(), 10);
        assert_eq!(c.price().timeout_secs, 4);
        assert_eq!(c.limits().melt_bytes, 5000);
        assert_eq!(c.limits().rollover_bytes, 6000);
        assert_eq!(c.limits().rollover_notes, 7);
        assert_eq!(c.wallet_url(), "http://node:8332/wallet/psp-main");
    }

    #[test]
    fn a_reload_swaps_in_the_live_settings_and_keeps_the_old_ones_on_any_problem() {
        let dir = scratch("reload");
        let handle = Handle::new(load(&dir, &env_of(&[])).unwrap().config, None);
        let before = handle.current();
        assert_eq!(before.base_confirmations, 3);
        assert!(!before.burn_enabled);

        fs::write(
            dir.join(FILE_NAME),
            "[settle]\nbase_confirmations = 4\n[burn]\nenabled = true\nthreshold_psp = 500\n",
        )
        .unwrap();
        let reloaded = handle.reload(&dir, &env_of(&[])).unwrap();
        let lines = reloaded.lines;
        assert!(
            reloaded.needs_restart.is_empty(),
            "{:?}",
            reloaded.needs_restart
        );
        let now = handle.current();
        assert_eq!(now.base_confirmations, 4);
        assert_eq!(
            now.burn(),
            crate::rotation::BurnPolicy {
                enabled: true,
                threshold_psp: 500
            }
        );
        for line in [
            "  settle.base_confirmations changed from 3 to 4 confirmations",
            "  burn.enabled changed from false to true",
            "  burn.threshold_psp changed from 0 to 500 PSP units",
        ] {
            assert!(
                lines.iter().any(|l| l.starts_with(line)),
                "{line:?} not in {lines:?}"
            );
        }
        // A scan or request that took its settings before still has the old ones.
        assert_eq!(before.base_confirmations, 3);

        // A bad file changes nothing, and says what is wrong.
        fs::write(
            dir.join(FILE_NAME),
            "[settle]\nbase_confirmations = \"many\"\n",
        )
        .unwrap();
        let problems = handle.reload(&dir, &env_of(&[])).unwrap_err();
        assert!(!problems.is_empty(), "{problems:?}");
        assert_eq!(handle.current().base_confirmations, 4);
        assert!(handle.current().burn_enabled);

        // Taking the file away puts the defaults back, and a second reload
        // with nothing changed says so.
        fs::remove_file(dir.join(FILE_NAME)).unwrap();
        handle.reload(&dir, &env_of(&[])).unwrap();
        assert_eq!(handle.current().base_confirmations, 3);
        assert!(!handle.current().burn_enabled);
        let reloaded = handle.reload(&dir, &env_of(&[])).unwrap();
        assert_eq!(reloaded.lines, vec!["  no setting changed".to_string()]);
        assert!(reloaded.needs_restart.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reload_keeps_the_restart_only_settings_and_says_so() {
        let dir = scratch("reload-restart");
        let handle = Handle::new(load(&dir, &env_of(&[])).unwrap().config, None);
        fs::write(
            dir.join(FILE_NAME),
            "[server]\nbind = \"127.0.0.1:4100\"\n[bitcoin]\nwallet = \"other\"\n[rotation]\nperiod_hours = 800\n[scan]\ninterval_secs = 60\n",
        )
        .unwrap();
        let reloaded = handle.reload(&dir, &env_of(&[])).unwrap();
        let lines = &reloaded.lines;
        // The file's values for the restart-only settings are handed back, so
        // that the caller can name them; none is applied.
        assert_eq!(
            reloaded.needs_restart,
            vec![
                ("server.bind", "127.0.0.1:4100".to_string()),
                ("bitcoin.wallet", "other".to_string()),
                ("rotation.period_hours", "800".to_string()),
            ]
        );
        let c = handle.current();
        assert_eq!(c.bind, "127.0.0.1:3000");
        assert_eq!(c.wallet, "psp-main");
        assert_eq!(c.get("rotation.period_hours").as_deref(), Some("87600"));
        // The live one in the same file did change.
        assert_eq!(c.scan_interval_secs, 60);
        for (key, from, to) in [
            ("server.bind", "127.0.0.1:3000", "127.0.0.1:4100"),
            ("bitcoin.wallet", "psp-main", "other"),
            ("rotation.period_hours", "87600", "800"),
        ] {
            let line = format!(
                "  {key} is changed in the file from {from} to {to}: it needs a restart (a brief downtime) and waits for one"
            );
            assert!(lines.contains(&line), "{line:?} not in {lines:?}");
        }
        // The next reload says it again while the file still differs.
        let again = handle.reload(&dir, &env_of(&[])).unwrap();
        assert!(
            again
                .lines
                .iter()
                .any(|l| l.starts_with("  server.bind is changed in the file")),
            "{:?}",
            again.lines
        );
        assert_eq!(again.needs_restart.len(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reload_changes_the_health_allowance_with_no_restart() {
        let dir = scratch("reload-health");
        let handle = Handle::new(load(&dir, &env_of(&[])).unwrap().config, None);
        assert_eq!(handle.current().health_stale_after_secs(), 3 * 120 + 60);
        fs::write(
            dir.join(FILE_NAME),
            "[scan]\nhealth_intervals = 5\nhealth_grace_secs = 10\n",
        )
        .unwrap();
        let reloaded = handle.reload(&dir, &env_of(&[])).unwrap();
        assert!(
            reloaded.needs_restart.is_empty(),
            "{:?}",
            reloaded.needs_restart
        );
        for line in [
            "  scan.health_intervals changed from 3 to 5 scan intervals",
            "  scan.health_grace_secs changed from 60 to 10 seconds",
        ] {
            assert!(
                reloaded.lines.iter().any(|l| l == line),
                "{line:?} not in {:?}",
                reloaded.lines
            );
        }
        assert_eq!(handle.current().health_intervals, 5);
        assert_eq!(handle.current().health_grace_secs, 10);
        assert_eq!(handle.current().health_stale_after_secs(), 5 * 120 + 10);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_restart_only_settings_are_exactly_the_ones_a_reload_cannot_apply() {
        let restart: Vec<&str> = SPECS.iter().filter(|s| !s.reload).map(|s| s.key).collect();
        assert_eq!(
            restart,
            vec![
                "server.bind",
                "bitcoin.network",
                "bitcoin.wallet",
                "rotation.activation_hours",
                "rotation.period_hours",
                "storage.mint_state",
                "air.server",
                "air.host",
                "air.held_reads",
                "air.timeout_secs",
            ]
        );
    }

    #[test]
    fn the_burn_settings_are_off_by_default_and_refuse_what_is_not_a_choice() {
        let c = Config::defaults("/h");
        assert_eq!(
            c.burn(),
            crate::rotation::BurnPolicy {
                enabled: false,
                threshold_psp: 0
            }
        );
        assert_eq!(c.limits().burn, c.burn());
        // The figure is read from the data folder, and is good for 24 hours, unless told.
        assert_eq!(c.limits().balance, crate::balance::Source::default());
        let dir = scratch("burn");
        fs::write(dir.join(FILE_NAME), "[burn]\nenabled = \"yes\"\n").unwrap();
        let p = load(&dir, &env_of(&[])).unwrap_err();
        assert!(!p.is_empty(), "{p:?}");
        // The file is gone before the environment cases, or only its error
        // would come back.
        fs::remove_file(dir.join(FILE_NAME)).unwrap();
        let p = load(&dir, &env_of(&[("PSP_BURN_ENABLED", "maybe")])).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("burn.enabled") && x.contains("true or false")),
            "{p:?}"
        );
        let p = load(
            &dir,
            &env_of(&[
                ("PSP_BURN_ENABLED", "true"),
                ("PSP_BURN_THRESHOLD_PSP", "5000000000000000000"),
            ]),
        )
        .unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("burn.threshold_psp") && x.contains("out of range")),
            "{p:?}"
        );
        let ok = load(
            &dir,
            &env_of(&[
                ("PSP_BURN_ENABLED", "true"),
                ("PSP_BURN_THRESHOLD_PSP", "250"),
            ]),
        )
        .unwrap();
        assert_eq!(
            ok.config.burn(),
            crate::rotation::BurnPolicy {
                enabled: true,
                threshold_psp: 250
            }
        );
        // Where the figure is and how old it may be: a path, and 1 s to a week.
        let p = load(&dir, &env_of(&[("PSP_BURN_BALANCE_MAX_AGE_SECS", "0")])).unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("burn.balance_max_age_secs") && x.contains("out of range")),
            "{p:?}"
        );
        let p = load(
            &dir,
            &env_of(&[("PSP_BURN_BALANCE_MAX_AGE_SECS", "604801")]),
        )
        .unwrap_err();
        assert!(
            p.iter()
                .any(|x| x.contains("burn.balance_max_age_secs") && x.contains("out of range")),
            "{p:?}"
        );
        let ok = load(
            &dir,
            &env_of(&[
                (
                    "PSP_BURN_BALANCE_FILE",
                    "/srv/spi-share/wallet_balance.json",
                ),
                ("PSP_BURN_BALANCE_MAX_AGE_SECS", "600"),
            ]),
        )
        .unwrap();
        let source = ok.config.limits().balance;
        assert_eq!(source.max_age_secs, 600);
        assert_eq!(
            source.path(std::path::Path::new("/data")),
            std::path::PathBuf::from("/srv/spi-share/wallet_balance.json")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A settings file that sets `key` (`section.name`) to `value` is refused at
    /// load, and the message names the setting, the value and the range.
    fn refused_at_load(key: &str, value: u64, range: &str) {
        let (section, name) = key.split_once('.').unwrap();
        let dir = scratch(&format!("cap-{section}-{name}-{value}"));
        fs::write(
            dir.join(FILE_NAME),
            format!("[{section}]\n{name} = {value}\n"),
        )
        .unwrap();
        let problems = load(&dir, &env_of(&[])).unwrap_err();
        let want = format!("{key} = {value} is out of range: it must be from {range} (");
        assert!(problems.iter().any(|p| p.contains(&want)), "{problems:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn melt_max_request_bytes_past_its_cap_is_refused_at_load() {
        refused_at_load(
            "server.melt_max_request_bytes",
            15_728_641,
            "16384 to 15728640",
        );
        refused_at_load("server.melt_max_request_bytes", 16_383, "16384 to 15728640");
    }

    #[test]
    fn burns_max_request_bytes_past_its_cap_is_refused_at_load() {
        refused_at_load(
            "server.burns_max_request_bytes",
            15_728_641,
            "16384 to 15728640",
        );
    }

    #[test]
    fn rollover_max_request_bytes_past_its_cap_is_refused_at_load() {
        refused_at_load(
            "server.rollover_max_request_bytes",
            15_728_641,
            "16384 to 15728640",
        );
    }

    #[test]
    fn check_state_max_request_bytes_past_its_cap_is_refused_at_load() {
        refused_at_load(
            "server.check_state_max_request_bytes",
            15_728_641,
            "16384 to 15728640",
        );
    }

    #[test]
    fn rollover_max_items_past_its_cap_is_refused_at_load() {
        // above what fits one AIR message, and below the outputs one amount can need
        refused_at_load("rollover.max_items", 25_001, "64 to 25000");
        refused_at_load("rollover.max_items", 63, "64 to 25000");
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
            for_this_os(
                "/h/Library/Application Support/Bitcoin/regtest/.cookie",
                "/h/.bitcoin/regtest/.cookie"
            )
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
            for_this_os(
                "/h/Library/Application Support/Bitcoin/regtest/.cookie",
                "/h/.bitcoin/regtest/.cookie"
            )
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
        assert_eq!(
            load_with(&d, &bad, None).unwrap_err(),
            vec![
                "environment variable PSP_NETWORK: `moon` is not a network: use main, regtest, signet or testnet"
                    .to_string()
            ]
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_retired_scan_window_is_refused_with_the_reason() {
        let d = dir("retired");
        fs::write(d.join(FILE_NAME), "[scan]\nmax_transactions = 1000\n").unwrap();
        let e = load_with(&d, &env_of(&[("HOME", "/h")]), None).unwrap_err();
        assert!(
            e.iter()
                .any(|p| p.contains("scan.max_transactions is retired")),
            "{e:?}"
        );
        fs::remove_file(d.join(FILE_NAME)).unwrap();
        let e = load_with(
            &d,
            &env_of(&[("HOME", "/h"), ("PSP_SCAN_MAX_TRANSACTIONS", "5")]),
            None,
        )
        .unwrap_err();
        assert!(
            e.iter()
                .any(|p| p.contains("PSP_SCAN_MAX_TRANSACTIONS: scan.max_transactions is retired")),
            "{e:?}"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_claim_fees_and_limits_map_to_the_policy_and_are_ranged() {
        let mut c = Config::defaults("/h");
        assert!(c.validate().is_ok());
        let policy = c.policy();
        // no fee unless one is set: the fees stay 0 by default
        assert_eq!(policy.fees, settle::ClaimFees::default());
        assert_eq!(policy.terms, c.terms());

        c.set("claim.max_attempts", "5").unwrap();
        c.set("claim.write_fee", "2").unwrap();
        c.set("delivery.read_fee", "1").unwrap();
        assert!(c.validate().is_ok());
        assert_eq!(
            c.policy().fees,
            settle::ClaimFees {
                max_attempts: 5,
                write_fee_units: 2,
                read_fee_units: 1
            }
        );

        // out of range, or not a whole number
        for (key, bad) in [
            ("claim.max_attempts", "0"),
            ("claim.max_attempts", "1001"),
            ("claim.write_fee", "4611686018427387905"),
            ("delivery.read_fee", "4611686018427387905"),
        ] {
            let mut c = Config::defaults("/h");
            c.set(key, bad).unwrap();
            let problems = c.validate().unwrap_err();
            assert!(
                problems.iter().any(|p| p.starts_with(key)),
                "{key}={bad}: {problems:?}"
            );
        }
        for (key, bad) in [("claim.write_fee", "-1"), ("claim.max_attempts", "1.5")] {
            assert_eq!(
                c.set(key, bad),
                Err(format!(
                    "{key} = `{bad}` is not a whole number (no sign, no decimals)"
                )),
                "{key}={bad}"
            );
        }
    }
}
