//! Key rotation: at most two key sets (generations), each with its own spent
//! list, current and previous.
//!
//! Generation 0 is `mint_keys.json` and `db/` at the top of the network's data
//! folder. Generation N (1 and later) is `gen-N/mint_keys.json` and
//! `gen-N/db/`. Each generation is a separate mint-core `MintLogic` with its
//! own store, so mint-core is unchanged.
//!
//! Rotation is off until the first issuance, which is recorded once in
//! `first_issue.json` (the first time the mint is seen with anything issued)
//! and never changed. The first rotation is `rotation.activation_hours` after
//! it, then one every `rotation.period_hours`. At each rotation a new
//! generation is made current, the current one becomes previous, and the one
//! before is deleted with its spent list. Its issued and redeemed totals are
//! added to `retired.json` first, so `/audit` still adds up.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mint_core::MintLogic;
use mint_core::keygen;
use mint_core::store::Store;
use mint_types::{Amount, DEFAULT_DENOMINATION_BASE, MintConfig, gen_denominations};
use secp256k1::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};

// The wire types live in sp-wire; they are re-exported so the paths are unchanged.
pub use sp_wire::keyset::{KeysetWindow, RotationResponse, keyset_id};
pub use sp_wire::psp::{
    MAX_ROLLOVER_NOTES, MAX_SPEND_MEMO_BYTES, PspSpendRequest, PspSpendResponse, RolloverRequest,
    RolloverResponse,
};

pub const FIRST_ISSUE_FILE: &str = "first_issue.json";
/// The most retired keyset IDs kept: 1 MB of 8-byte IDs (131,072). A
/// request naming a listed ID gets `keyset_retired` (its notes expired); any
/// other unknown ID gets `unknown_keyset`, so a wallet can tell expired notes
/// from junk and explain a loss. When the list is full the oldest ID is dropped
/// to make room, which only turns a very old `keyset_retired` into
/// `unknown_keyset`. 131,072 covers hourly rotation for about 15 years; a mint
/// rotating yearly never comes close. It is a constant, the same in every
/// mint, and deliberately not a setting: it is too cheap to be worth tuning.
pub const RETIRED_KEYSETS_CAP: usize = 131_072;
pub const RETIRED_FILE: &str = "retired.json";
const KEYS: &str = "mint_keys.json";
const DB: &str = "db";

/// The rotation settings, in seconds (the same rule as fsp-mint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    pub first_issue: Option<u64>,
    pub activation_secs: u64,
    pub period_secs: u64,
}

impl Rotation {
    pub fn activation(&self) -> Option<u64> {
        let first = self.first_issue?;
        (self.activation_secs > 0 && self.period_secs > 0)
            .then(|| first.saturating_add(self.activation_secs))
    }
    pub fn start_of(&self, epoch: u64) -> Option<u64> {
        let t0 = self.activation()?;
        (epoch >= 1).then(|| t0.saturating_add((epoch - 1).saturating_mul(self.period_secs.max(1))))
    }
    pub fn epoch_at(&self, now: u64) -> u64 {
        match self.activation() {
            Some(t0) if now >= t0 => 1 + (now - t0) / self.period_secs.max(1),
            _ => 0,
        }
    }
}

/// A live generation, as a keyset ID names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Current,
    Previous,
}

/// A keyset ID that names no live generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeysetError {
    /// It was this mint's, and has been deleted with its spent list.
    Retired(String),
    /// This mint never had it.
    Unknown(String),
}

impl KeysetError {
    pub fn code(&self) -> &'static str {
        match self {
            KeysetError::Retired(_) => sp_wire::codes::KEYSET_RETIRED,
            KeysetError::Unknown(_) => sp_wire::codes::UNKNOWN_KEYSET,
        }
    }
    pub fn why(&self, what: &str) -> String {
        match self {
            KeysetError::Retired(id) => {
                format!("{what}: keyset {id} was rotated out and deleted with its spent list")
            }
            KeysetError::Unknown(id) => format!("{what}: keyset {id} is not a keyset of this mint"),
        }
    }
}

/// One generation's keys.
pub struct GenKeys {
    pub epoch: u64,
    pub id: String,
    pub sk: BTreeMap<Amount, SecretKey>,
    pub pk: BTreeMap<Amount, PublicKey>,
}

fn gen_dir(base: &Path, epoch: u64) -> PathBuf {
    if epoch == 0 {
        base.to_path_buf()
    } else {
        base.join(format!("gen-{epoch}"))
    }
}

fn exists(base: &Path, epoch: u64) -> bool {
    gen_dir(base, epoch).join(KEYS).exists()
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    if let Some(d) = path.parent() {
        fs::File::open(d)?.sync_all()?;
    }
    Ok(())
}

/// Open (or make) generation `epoch`.
pub fn open_gen(
    base: &Path,
    epoch: u64,
    max: Amount,
) -> Result<(GenKeys, MintLogic), Box<dyn Error>> {
    let dir = gen_dir(base, epoch);
    fs::create_dir_all(&dir)?;
    let path = dir.join(KEYS);
    let cfg: MintConfig = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)
            .map_err(|e| format!("{} is damaged: {e}", path.display()))?
    } else {
        let cfg = keygen::generate(&gen_denominations(DEFAULT_DENOMINATION_BASE, max));
        write_new(&path, &serde_json::to_vec_pretty(&cfg)?)?;
        cfg
    };
    let pk = keygen::public_keys(&cfg);
    let keys = GenKeys {
        epoch,
        id: keyset_id(&pk),
        sk: cfg.sk.clone(),
        pk,
    };
    let logic = MintLogic::new(cfg, Store::open(&dir.join(DB))?);
    Ok((keys, logic))
}

/// Delete a generation: its private keys and its spent list. Crash-safe:
/// everything is first moved into `retiring-gen-N/` (renames, the keys last,
/// so a half-moved generation still opens and is retired again), then that
/// folder is deleted. `sweep_retiring` finishes the deletion after a crash.
fn delete_gen(base: &Path, epoch: u64) -> Result<(), Box<dyn Error>> {
    let dir = gen_dir(base, epoch);
    let trash = base.join(format!("{RETIRING_PREFIX}{epoch}"));
    fs::create_dir_all(&trash)?;
    if dir.join(DB).exists() {
        fs::rename(dir.join(DB), trash.join(DB))?;
    }
    if dir.join(KEYS).exists() {
        fs::rename(dir.join(KEYS), trash.join(KEYS))?;
    }
    if epoch != 0 {
        let _ = fs::remove_dir_all(&dir);
    }
    fs::remove_dir_all(&trash)?;
    Ok(())
}

const RETIRING_PREFIX: &str = "retiring-gen-";

/// Finish deleting generations a crash left half deleted.
fn sweep_retiring(base: &Path) -> Result<(), Box<dyn Error>> {
    if let Ok(rd) = fs::read_dir(base) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with(RETIRING_PREFIX) {
                fs::remove_dir_all(e.path())?;
            }
        }
    }
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
struct Retired {
    epochs: Vec<u64>,
    /// The keyset IDs of the deleted generations, so a request naming one is
    /// told `keyset_retired` rather than `unknown_keyset`.
    ids: Vec<String>,
    /// Burn proofs taken from a spent list just before it is deleted, by the
    /// request that held the notes: request hash -> the nonces (hex) it held
    /// that were spent in the retiring set. Only that request may use them
    /// (`Rotor::burn_proven`). Saved together with the retired ID, before
    /// anything is deleted, and removed when the request finishes
    /// (`Rotor::forget_proofs`). A file written before this shape (a flat list
    /// of nonces) does not parse, and `serve` stops naming the file.
    burn_proofs: BTreeMap<String, std::collections::BTreeSet<String>>,
    /// The issued and redeemed totals of the deleted generations, in units, as
    /// decimal strings (a `u128` is more than a JSON number can hold). A file
    /// with these as numbers (written before 0.7.0) does not open, and `serve`
    /// stops naming the file.
    #[serde(with = "decimal")]
    issued: u128,
    #[serde(with = "decimal")]
    redeemed: u128,
}

/// A `u128` written to JSON as a decimal string.
pub(crate) mod decimal {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

fn load_retired(base: &Path) -> Result<Retired, Box<dyn Error>> {
    let p = base.join(RETIRED_FILE);
    match fs::read(&p) {
        Ok(b) => {
            Ok(serde_json::from_slice(&b)
                .map_err(|e| format!("{} is damaged: {e}", p.display()))?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Retired::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_json<T: Serialize>(path: &Path, v: &T) -> Result<(), Box<dyn Error>> {
    let _ = fs::remove_file(path.with_extension("tmp"));
    write_new(path, &serde_json::to_vec_pretty(v)?)
}

#[derive(Serialize, Deserialize)]
struct FirstIssue {
    first_issue: u64,
}

pub fn load_first_issue(base: &Path) -> Result<Option<u64>, Box<dyn Error>> {
    let p = base.join(FIRST_ISSUE_FILE);
    match fs::read(&p) {
        Ok(b) => Ok(Some(
            serde_json::from_slice::<FirstIssue>(&b)
                .map_err(|e| format!("{} is damaged: {e}", p.display()))?
                .first_issue,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The previous generation, and everything the rotation needs besides the
/// current `MintLogic` (which stays in the shared `AppState`).
pub struct Rotor {
    pub base: PathBuf,
    max: Amount,
    pub activation_hours: u64,
    pub period_hours: u64,
    pub first_issue: Option<u64>,
    pub current: GenKeys,
    pub previous: Option<(GenKeys, MintLogic)>,
    retired: Retired,
    /// The cap on retired keyset IDs: [`RETIRED_KEYSETS_CAP`] (smaller only
    /// in tests).
    retired_keep: usize,
    /// A fixed clock for tests; 0 is the system clock.
    pub clock: std::sync::Arc<AtomicU64>,
}

impl Rotor {
    /// Open the generations live at `now`: make the current one if needed,
    /// open the previous one if it is on disk, and delete any older one.
    pub fn open(
        base: &Path,
        max: Amount,
        activation_hours: u64,
        period_hours: u64,
    ) -> Result<(Rotor, MintLogic), Box<dyn Error>> {
        Self::open_at(base, max, activation_hours, period_hours, 0)
    }

    /// As `open`, with a fixed clock (`0` is the system clock).
    pub fn open_at(
        base: &Path,
        max: Amount,
        activation_hours: u64,
        period_hours: u64,
        clock: u64,
    ) -> Result<(Rotor, MintLogic), Box<dyn Error>> {
        fs::create_dir_all(base)?;
        sweep_retiring(base)?;
        let mut r = Rotor {
            base: base.to_path_buf(),
            max,
            activation_hours,
            period_hours,
            first_issue: load_first_issue(base)?,
            current: GenKeys {
                epoch: 0,
                id: String::new(),
                sk: BTreeMap::new(),
                pk: BTreeMap::new(),
            },
            previous: None,
            retired: load_retired(base)?,
            retired_keep: RETIRED_KEYSETS_CAP,
            clock: std::sync::Arc::new(AtomicU64::new(clock)),
        };
        // Start from the newest generation on disk, then roll forward.
        let mut newest = 0;
        if let Ok(rd) = fs::read_dir(base) {
            for e in rd.flatten() {
                if let Some(n) = e
                    .file_name()
                    .to_str()
                    .and_then(|n| n.strip_prefix("gen-"))
                    .and_then(|n| n.parse::<u64>().ok())
                    && exists(base, n)
                {
                    newest = newest.max(n);
                }
            }
        }
        let (keys, logic) = open_gen(base, newest, max)?;
        r.current = keys;
        if newest > 0 && exists(base, newest - 1) {
            r.previous = Some(open_gen(base, newest - 1, max)?);
        }
        for old in 0..newest.saturating_sub(1) {
            if exists(base, old) {
                r.retire(old)?;
            }
        }
        let mut logic = logic;
        r.refresh(&mut logic)?;
        Ok((r, logic))
    }

    pub fn now(&self) -> u64 {
        match self.clock.load(Ordering::SeqCst) {
            0 => crate::server::now_secs(),
            t => t,
        }
    }

    pub fn rotation(&self) -> Rotation {
        Rotation {
            first_issue: self.first_issue,
            activation_secs: self.activation_hours.saturating_mul(3600),
            period_secs: self.period_hours.saturating_mul(3600),
        }
    }

    /// Burn proofs from this set's spent list, the last moment it exists: for
    /// each unfinished request, the notes it holds that are spent here were
    /// burned by that request (no other request may take them). Each proof is
    /// kept under the request that held the note. A proof whose request no
    /// longer holds the note (it finished) is dropped. Saved by the caller with
    /// the retired ID.
    fn take_proofs(&mut self, logic: &MintLogic) -> Result<(), Box<dyn Error>> {
        let rollovers = Rollovers::open(&self.base)?;
        let mut proofs = BTreeMap::new();
        for (id, held) in rollovers.holds() {
            let mut got = self.retired.burn_proofs.remove(id).unwrap_or_default();
            got.retain(|n| held.contains(n));
            for n in held {
                let Ok(bytes) = hex::decode(n) else { continue };
                let Ok(pk) = secp256k1::PublicKey::from_slice(&bytes) else {
                    continue;
                };
                if logic
                    .is_spent(mint_types::Nonce(pk))
                    .map_err(|e| e.to_string())?
                {
                    got.insert(n.clone());
                }
            }
            if !got.is_empty() {
                proofs.insert(id.clone(), got);
            }
        }
        self.retired.burn_proofs = proofs;
        Ok(())
    }

    /// The retired totals with a generation's `issued` and `redeemed` added,
    /// each add checked: an overflow stops the retirement with nothing changed.
    fn retired_sums(&self, issued: u128, redeemed: u128) -> Result<(u128, u128), Box<dyn Error>> {
        let issued = (self.retired.issued)
            .checked_add(issued)
            .ok_or("the issued total of the deleted generations would overflow")?;
        let redeemed = (self.retired.redeemed)
            .checked_add(redeemed)
            .ok_or("the redeemed total of the deleted generations would overflow")?;
        Ok((issued, redeemed))
    }

    /// Add a generation's totals to `retired.json` (once), then delete it.
    fn retire(&mut self, epoch: u64) -> Result<(), Box<dyn Error>> {
        if !self.retired.epochs.contains(&epoch) {
            let (keys, logic) = open_gen(&self.base, epoch, self.max)?;
            self.take_proofs(&logic)?;
            let (i, d) = logic.audit_totals().map_err(|e| e.to_string())?;
            drop(logic);
            let (issued, redeemed) = self.retired_sums(i, d)?;
            self.retired.epochs.push(epoch);
            self.retired.ids.push(keys.id);
            self.trim_retired();
            self.retired.issued = issued;
            self.retired.redeemed = redeemed;
            save_json(&self.base.join(RETIRED_FILE), &self.retired)?;
        }
        delete_gen(&self.base, epoch)
    }

    fn retire_logic(&mut self, keys: GenKeys, logic: MintLogic) -> Result<(), Box<dyn Error>> {
        if !self.retired.epochs.contains(&keys.epoch) {
            self.take_proofs(&logic)?;
            let (i, d) = logic.audit_totals().map_err(|e| e.to_string())?;
            let (issued, redeemed) = self.retired_sums(i, d)?;
            self.retired.epochs.push(keys.epoch);
            self.retired.ids.push(keys.id.clone());
            self.trim_retired();
            self.retired.issued = issued;
            self.retired.redeemed = redeemed;
            save_json(&self.base.join(RETIRED_FILE), &self.retired)?;
        }
        drop(logic);
        delete_gen(&self.base, keys.epoch)
    }

    /// Record the first issuance if it has happened, and rotate as far as the
    /// clock says. `current` is the shared current `MintLogic`; the caller
    /// holds its lock.
    pub fn refresh(&mut self, current: &mut MintLogic) -> Result<(), Box<dyn Error>> {
        let now = self.now();
        if self.first_issue.is_none() {
            let issued = current.audit_totals().map_err(|e| e.to_string())?.0;
            if issued > 0 || self.previous.is_some() || self.current.epoch > 0 {
                save_json(
                    &self.base.join(FIRST_ISSUE_FILE),
                    &FirstIssue { first_issue: now },
                )?;
                self.first_issue = Some(now);
            }
        }
        let target = self.rotation().epoch_at(now);
        while self.current.epoch < target {
            // More than one period passed while the mint was down: the sets
            // in between never signed anything; skip to the one before
            // `target` so the loop is at most two steps.
            let next = if target - self.current.epoch > 2 {
                target - 1
            } else {
                self.current.epoch + 1
            };
            let (keys, logic) = open_gen(&self.base, next, self.max)?;
            let old_logic = std::mem::replace(current, logic);
            let old_keys = std::mem::replace(&mut self.current, keys);
            if let Some((k, l)) = self.previous.take() {
                self.retire_logic(k, l)?;
            }
            println!(
                "key rotation: keyset {} (epoch {}) is now current; {} (epoch {}) is previous",
                self.current.id, self.current.epoch, old_keys.id, old_keys.epoch
            );
            self.previous = Some((old_keys, old_logic));
        }
        Ok(())
    }

    /// Which live generation a keyset ID names.
    pub fn resolve(&self, id: &str) -> Result<Side, KeysetError> {
        if id == self.current.id {
            Ok(Side::Current)
        } else if self.previous.as_ref().is_some_and(|(k, _)| k.id == id) {
            Ok(Side::Previous)
        } else if self.retired.ids.iter().any(|r| r == id) {
            Err(KeysetError::Retired(id.to_string()))
        } else {
            Err(KeysetError::Unknown(id.to_string()))
        }
    }

    /// Test hook: a smaller cap than [`RETIRED_KEYSETS_CAP`], applied now and
    /// saved. Production always uses the constant.
    #[doc(hidden)]
    pub fn set_retired_keep(&mut self, keep: usize) -> Result<(), Box<dyn Error>> {
        self.retired_keep = keep;
        let before = self.retired.ids.len();
        self.trim_retired();
        if self.retired.ids.len() != before {
            save_json(&self.base.join(RETIRED_FILE), &self.retired)?;
        }
        Ok(())
    }

    fn trim_retired(&mut self) {
        let n = self.retired.ids.len();
        if n > self.retired_keep {
            self.retired.ids.drain(..n - self.retired_keep);
        }
    }

    /// Whether a keyset ID names a deleted generation.
    pub fn is_retired(&self, id: &str) -> bool {
        self.retired.ids.iter().any(|r| r == id)
    }

    /// Whether `input` was proven burned by request `id` (the one holding it)
    /// when its keyset was deleted; see `Retired::burn_proofs`. No other
    /// request can use the proof.
    pub fn burn_proven(&self, id: &str, input: &MintInput) -> bool {
        self.retired
            .burn_proofs
            .get(id)
            .is_some_and(|n| n.contains(&hex::encode(input.note.nonce.0.serialize())))
    }

    /// Drop the burn proofs of request `id`, which has finished. Saved at once.
    pub fn forget_proofs(&mut self, id: &str) -> Result<(), Box<dyn Error>> {
        if self.retired.burn_proofs.remove(id).is_some() {
            save_json(&self.base.join(RETIRED_FILE), &self.retired)?;
        }
        Ok(())
    }

    /// Issued and redeemed of the retired generations.
    pub fn retired_totals(&self) -> (u128, u128) {
        (self.retired.issued, self.retired.redeemed)
    }

    pub fn window(&self) -> RotationResponse {
        let r = self.rotation();
        let now = self.now();
        let win = |epoch: u64, id: &str| KeysetWindow {
            id: id.to_string(),
            epoch,
            starts: r.start_of(epoch),
            current_until: r.activation().map(|_| r.start_of(epoch + 1).unwrap_or(0)),
            retires: r.activation().map(|_| r.start_of(epoch + 2).unwrap_or(0)),
        };
        let current = win(self.current.epoch, &self.current.id);
        RotationResponse {
            active: r.activation().is_some_and(|t| now >= t),
            first_issue: self.first_issue,
            rotation_activation_hours: self.activation_hours,
            activation: r.activation(),
            rotation_period_hours: self.period_hours,
            next_rotation: current.current_until,
            previous: self.previous.as_ref().map(|(k, _)| win(k.epoch, &k.id)),
            current,
            now,
        }
    }

    /// The operator's view, for `config` and the startup log.
    pub fn describe(&self) -> Vec<String> {
        let w = self.window();
        let t = |o: Option<u64>| o.map(|v| v.to_string()).unwrap_or_else(|| "not yet".into());
        let mut out = vec![
            format!(
                "rotation: activation {} h after the first issue, period {} h",
                w.rotation_activation_hours, w.rotation_period_hours
            ),
            format!(
                "  first issue: {}; activation: {}; next rotation: {} (Unix seconds)",
                t(w.first_issue),
                t(w.activation),
                t(w.next_rotation)
            ),
            format!(
                "  current keyset {} (epoch {})",
                w.current.id, w.current.epoch
            ),
        ];
        if let Some(p) = &w.previous {
            out.push(format!(
                "  previous keyset {} (epoch {}), retires at {}",
                p.id,
                p.epoch,
                t(p.retires)
            ));
        }
        if w.rotation_activation_hours == 0 {
            out.push("  rotation is off (rotation.activation_hours = 0)".into());
        }
        out
    }
}

// ---- the rollover swap ------------------------------------------------------

use mint_types::{
    BlindNonce, MintInput, MintOutput, MintOutputOutcome, Nonce, Note, transaction_sighash,
};
use rand::rngs::OsRng;
use sp_wire::psp::{HandedPspNote, PspHandover};

pub const ROLLOVERS_FILE: &str = "rollovers.json";
/// The folder under the network's data folder where the PSP the mint reissues to
/// the SPI's wallet is written, one file for each spend (`<request hash>.json`, a
/// [`PspHandover`]).
pub const REVENUE_DIR: &str = "revenue";

/// What a spend does with the PSP it is paid (`burn.enabled` and
/// `burn.threshold_psp`). The threshold is measured against what the SPI's
/// wallet holds now (see [`crate::balance`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BurnPolicy {
    /// Whether any paid PSP may be burned. Off (the default): none is.
    pub enabled: bool,
    /// With burning on: how many PSP units the SPI's wallet is kept at. Paid
    /// PSP is reissued while the wallet holds less than this, and burned once
    /// it holds this much or more.
    pub threshold_psp: u64,
}

impl BurnPolicy {
    /// How much of `pay` is reissued to the SPI's wallet, given `held`, the PSP
    /// the wallet is taken to hold now (`None`: there is no usable figure, see
    /// [`crate::balance`]). All of it unless burning is on and there is a
    /// figure, and then only what fits under the threshold: the room between
    /// `held` and `burn.threshold_psp`. The rest of `pay` is burned.
    pub fn keep(&self, pay: u64, held: Option<u64>) -> u64 {
        match held {
            Some(held) if self.enabled => pay.min(self.threshold_psp.saturating_sub(held)),
            _ => pay,
        }
    }
}

/// One note planned for the SPI's wallet, saved before anything of its spend is
/// burned so that a crash cannot lose its secrets or issue it twice. The note is
/// made from this and the mint's key when the spend finishes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RevenuePlan {
    amount: u64,
    /// The keyset that signs it (the current one when the spend began).
    keyset: String,
    /// The secret of the note's nonce: the key that spends it.
    nonce_secret: SecretKey,
    /// The blinding factor, so a retry blinds, issues and unblinds the same note.
    blinding_key: SecretKey,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Rollovers {
    answers: BTreeMap<String, RolloverResponse>,
    /// Request hash -> the epoch it was started under. Written before any note
    /// is marked spent, so a retry after a crash finishes it.
    pending: BTreeMap<String, u64>,
    /// Pending rollovers whose inputs have all been burned under that request
    /// hash, written after the burn and before any signing. Only this proves
    /// the burn once the inputs' keyset has been deleted with its spent list.
    burned: std::collections::BTreeSet<String>,
    /// The input nonces (hex) each pending rollover holds, saved with its
    /// pending entry before anything is burned. No other rollover or melt may
    /// take them meanwhile.
    holds: BTreeMap<String, Vec<String>>,
    /// The output blind nonces (hex) each pending rollover or spend will sign,
    /// saved with its pending entry before anything is burned and dropped when
    /// it finishes. No other rollover or spend may sign them meanwhile.
    out_holds: BTreeMap<String, Vec<String>>,
    /// The answers to PSP spends (kind 14), by request hash. A spend uses the
    /// same `pending`, `burned` and `holds` as a rollover, under its own hash
    /// (the two hashes have different tags, so they never collide), so one
    /// request in flight holds its notes against every other call.
    spends: BTreeMap<String, PspSpendResponse>,
    /// The PSP units burned as payments in finished spends: the sum of `pay`
    /// over every answer ever stored in `spends`, kept apart from the answers
    /// because answers of a retired keyset are pruned and the revenue must not
    /// shrink with them. Saved in the same write as the answer it counts, so a
    /// crash cannot count a payment twice or lose one.
    paid: u64,
    /// Of `paid`: the PSP units burned (the revenue the mint destroyed).
    paid_burned: u64,
    /// Of `paid`: the PSP units reissued to the SPI's wallet, as notes in a
    /// [`PspHandover`] file. `paid_burned` plus `paid_kept` is `paid`, except for
    /// notes a deleted keyset could no longer sign (counted as burned).
    paid_kept: u64,
    /// The notes each spend in flight will reissue, by request hash, decided and
    /// saved with the spend's `pending` entry. An empty list is a decision to
    /// reissue nothing.
    plans: BTreeMap<String, Vec<RevenuePlan>>,
    /// The notes of finished spends that are not yet in a file in `revenue/`,
    /// by request hash. Written by [`Rollovers::flush_revenue`].
    revenue: BTreeMap<String, Vec<HandedPspNote>>,
    #[serde(skip)]
    path: PathBuf,
}

impl Rollovers {
    pub fn open(base: &Path) -> Result<Rollovers, Box<dyn Error>> {
        let path = base.join(ROLLOVERS_FILE);
        let mut r: Rollovers = match fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| format!("{} is damaged: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Rollovers::default(),
            Err(e) => return Err(e.into()),
        };
        r.path = path;
        Ok(r)
    }
    fn save(&self) -> Result<(), String> {
        save_json(&self.path, self).map_err(|e| e.to_string())
    }

    /// The notes (nonce hex) pending rollovers other than `except` hold.
    pub fn held_nonces(&self, except: &str) -> std::collections::HashSet<String> {
        self.holds
            .iter()
            .filter(|(h, _)| h.as_str() != except)
            .flat_map(|(_, n)| n.iter().cloned())
            .collect()
    }

    /// The notes each pending request holds, by request hash.
    pub fn holds(&self) -> impl Iterator<Item = (&String, &Vec<String>)> + '_ {
        self.holds.iter()
    }

    /// The output blind nonces (hex) pending requests other than `except` hold.
    pub fn held_outputs(&self, except: &str) -> std::collections::HashSet<String> {
        self.out_holds
            .iter()
            .filter(|(h, _)| h.as_str() != except)
            .flat_map(|(_, n)| n.iter().cloned())
            .collect()
    }

    /// The PSP units paid in finished spends (see the field): burned plus
    /// reissued to the SPI's wallet.
    pub fn paid(&self) -> u64 {
        self.paid
    }

    /// Of [`Rollovers::paid`], the units burned.
    pub fn paid_burned(&self) -> u64 {
        self.paid_burned
    }

    /// Of [`Rollovers::paid`], the units reissued to the SPI's wallet.
    pub fn paid_kept(&self) -> u64 {
        self.paid_kept
    }

    /// The units reissued so far plus those the spends in flight will reissue:
    /// the mint's side of what the SPI wallet is taken to hold (see
    /// [`crate::balance::held`]), which `burn.threshold_psp` is measured against.
    fn reissue_total(&self) -> u64 {
        let planned: u64 = self
            .plans
            .values()
            .flatten()
            .fold(0u64, |sum, p| sum.saturating_add(p.amount));
        self.paid_kept.saturating_add(planned)
    }

    /// The network's data folder, where this store's file is.
    pub fn dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// Write the notes of finished spends to `revenue/<request hash>.json` under
    /// the network's data folder, then forget them here. A file is rewritten
    /// whole if a crash left it half done, and a note is never written under two
    /// names. The files are readable by the mint's group, so the person who runs
    /// the SPI's wallet can take them in; whoever can read one holds its notes
    /// until the wallet has swapped them. Returns how many files were written.
    pub fn flush_revenue(&mut self) -> Result<usize, String> {
        if self.revenue.is_empty() {
            return Ok(0);
        }
        let base = self
            .path
            .parent()
            .ok_or_else(|| format!("{} has no folder", self.path.display()))?;
        let dir = base.join(REVENUE_DIR);
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o750))
                .map_err(|e| format!("cannot set the mode of {}: {e}", dir.display()))?;
        }
        let ids: Vec<String> = self.revenue.keys().cloned().collect();
        for id in &ids {
            let file = PspHandover {
                notes: self.revenue[id].clone(),
            };
            let bytes = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
            let path = dir.join(format!("{id}.json"));
            write_new(&path, &bytes)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
                    .map_err(|e| format!("cannot set the mode of {}: {e}", path.display()))?;
            }
        }
        for id in &ids {
            self.revenue.remove(id);
        }
        self.save()?;
        Ok(ids.len())
    }

    /// Drop stored answers (rollovers and spends) whose keyset was deleted.
    fn prune(&mut self, retired: impl Fn(&str) -> bool) {
        let before = self.answers.len() + self.spends.len();
        self.answers.retain(|_, a| !retired(&a.keyset));
        self.spends.retain(|_, a| !retired(&a.keyset));
        if self.answers.len() + self.spends.len() != before {
            let _ = self.save();
        }
    }
}

/// Why a rollover was refused (`400`, with a code) or failed (`500`, retry).
#[derive(Debug, PartialEq, Eq)]
pub enum RolloverError {
    Refused(String),
    Keyset(&'static str, String),
    /// Refused with a `code` that is not about a keyset: an output's blind
    /// nonce (`duplicate_blind_nonce`, `blind_nonce_used`, `note_held`).
    Coded(&'static str, String),
    /// Input `index` is already spent. The body says which, so the wallet can
    /// stop offering that note (`code: note_spent_elsewhere`, `spent: [index]`).
    Spent(usize),
    Internal(String),
}

/// The position of the first output whose blind nonce an earlier one repeats.
fn repeated_blind_nonce(outputs: &[MintOutput]) -> Option<usize> {
    let mut seen = std::collections::HashSet::new();
    outputs.iter().position(|o| !seen.insert(o.blind_nonce))
}

/// The checks on a new request's outputs that look at the signing set and at
/// the other unfinished requests, made before anything is written or burned: a
/// blind nonce the set has signed already, or one another pending rollover or
/// spend holds. `Ok(Some(..))` is a refusal, its `code` and reason; `Err` is a
/// fault of the store.
fn check_outputs_free(
    out_logic: &MintLogic,
    store: &Rollovers,
    id: &str,
    outputs: &[MintOutput],
) -> Result<Option<(&'static str, String)>, String> {
    let held = store.held_outputs(id);
    for (o, output) in outputs.iter().enumerate() {
        if out_logic
            .is_blind_nonce_used(&output.blind_nonce)
            .map_err(|e| e.to_string())?
        {
            return Ok(Some((
                sp_wire::codes::BLIND_NONCE_USED,
                format!("output {o}: the blind nonce was already used. Nothing was burned"),
            )));
        }
        if held.contains(&hex::encode(output.blind_nonce.0.serialize())) {
            return Ok(Some((
                sp_wire::codes::NOTE_HELD,
                format!(
                    "output {o}: the blind nonce is held by an unfinished request. Nothing was burned"
                ),
            )));
        }
    }
    Ok(None)
}

/// Run a rollover. The caller holds the mint lock and the rotor.
///
/// Every note is verified, and its spent list chosen, by the keyset ID it
/// carries (`input_keysets`); the outputs are signed with the keyset the
/// request names, if it is current or previous. A new request naming a
/// deleted keyset is refused `keyset_retired`, an unknown one
/// `unknown_keyset`; nothing is burned. A request that was started (its
/// inputs may be burned) and is retried after a rotation is finished: notes
/// already burned are skipped, and outputs whose keyset was deleted meanwhile
/// are signed with the current keyset (the reply names it; blinding does not
/// depend on the key, so the wallet unblinds with the key it is given).
pub fn rollover(
    current: &MintLogic,
    rotor: &Rotor,
    store: &mut Rollovers,
    req: &RolloverRequest,
    max_notes: usize,
) -> Result<RolloverResponse, RolloverError> {
    use RolloverError::*;
    if req.inputs.is_empty() || req.inputs.len() > max_notes {
        return Err(Refused(format!("a rollover takes 1 to {max_notes} notes")));
    }
    if req.input_keysets.len() != req.inputs.len() || req.keyset.is_empty() {
        return Err(Keyset(
            sp_wire::codes::MISSING_KEYSET,
            "every input needs its keyset (input_keysets) and the outputs need one (keyset)".into(),
        ));
    }
    let mut a: Vec<u64> = req.inputs.iter().map(|i| i.amount.units).collect();
    let mut b: Vec<u64> = req.outputs.iter().map(|o| o.amount.units).collect();
    a.sort_unstable();
    b.sort_unstable();
    if a != b {
        return Err(Refused(
            "the outputs must have exactly the amounts of the inputs: a swap only replaces notes"
                .into(),
        ));
    }
    if let Some(o) = req
        .outputs
        .iter()
        .find(|o| !rotor.current.sk.contains_key(&o.amount))
    {
        return Err(Refused(format!(
            "{} is not a denomination of this mint",
            o.amount.units
        )));
    }
    // Answers for deleted keysets are of no use (their notes cannot be
    // spent), so they go; a retry of one is answered from the spent list.
    store.prune(|k| rotor.is_retired(k));
    let id = req.hash();
    if let Some(done) = store.answers.get(&id) {
        return Ok(done.clone());
    }
    let resumed = store.pending.contains_key(&id);
    let proven = resumed && store.burned.contains(&id);
    let others = store.held_nonces(&id);
    let own = store.holds.get(&id).cloned().unwrap_or_default();
    let gen_of = |side: Side| -> (&MintLogic, &GenKeys) {
        match side {
            Side::Current => (current, &rotor.current),
            Side::Previous => {
                let (k, l) = rotor.previous.as_ref().expect("resolved");
                (l, k)
            }
        }
    };
    // The outputs are signed by the live set the request names, current or
    // previous. A set that is not live is refused (`keyset_retired` or
    // `unknown_keyset`), except for a request resumed after a crash: its
    // inputs are already burned, so if the set it named has since been
    // deleted it is signed by the current set (the reply names it).
    let (out_logic, out_keys) = match rotor.resolve(&req.keyset) {
        Ok(side) => gen_of(side),
        Err(_) if resumed => (current, &rotor.current),
        Err(e) => return Err(Keyset(e.code(), e.why("the outputs"))),
    };
    let sighash = transaction_sighash(&req.inputs, &req.outputs);
    let mut seen = std::collections::HashSet::new();
    let mut plan: Vec<(&MintLogic, &SecretKey, &MintInput, bool)> = Vec::new();
    for (i, input) in req.inputs.iter().enumerate() {
        if !seen.insert(input.note.nonce) {
            return Err(Refused(format!("input {i} is in the request twice")));
        }
        if others.contains(&hex::encode(input.note.nonce.0.serialize())) {
            return Err(Refused(format!(
                "input {i} is held by an unfinished rollover"
            )));
        }
        let (logic, keys) = match rotor.resolve(&req.input_keysets[i]) {
            Ok(side) => gen_of(side),
            // Started before its keyset was deleted: finished without
            // burning only if the stored state proves this request burned it:
            // its own flag, or the proof taken for this request when the set
            // was deleted, for a note it holds. No other request has a proof.
            Err(_)
                if proven
                    || (resumed
                        && own.contains(&hex::encode(input.note.nonce.0.serialize()))
                        && rotor.burn_proven(&id, input)) =>
            {
                continue;
            }
            Err(e) if resumed => {
                return Err(Keyset(
                    sp_wire::codes::BURN_NOT_PROVEN,
                    format!(
                        "input {i}: {}; this request was started before, but nothing stored proves its inputs were burned under this request, so it cannot be finished without them",
                        e.why("its keyset")
                    ),
                ));
            }
            Err(e) => return Err(Keyset(e.code(), e.why(&format!("input {i}")))),
        };
        logic
            .verify_note(input)
            .map_err(|_| Refused(format!("input {i} is not a note of keyset {}", keys.id)))?;
        logic
            .verify_spend_authorization(input, sighash)
            .map_err(|e| Refused(format!("input {i}: {e}")))?;
        let spent = logic
            .is_spent(input.note.nonce)
            .map_err(|e| Internal(e.to_string()))?;
        if spent && !resumed {
            return Err(Spent(i));
        }
        let sk = keys
            .sk
            .get(&input.amount)
            .ok_or_else(|| Refused(format!("input {i}: not a denomination")))?;
        plan.push((logic, sk, input, spent));
    }
    // The outputs, before anything is written or burned. The set that signs
    // must have a key for each amount (the two sets are made from one maximum,
    // but a key file already on disk is used as it is).
    if let Some(o) = req
        .outputs
        .iter()
        .find(|o| !out_keys.sk.contains_key(&o.amount) || !out_keys.pk.contains_key(&o.amount))
    {
        let why = format!(
            "{} is not a denomination of keyset {}",
            o.amount.units, out_keys.id
        );
        return Err(if resumed { Internal(why) } else { Refused(why) });
    }
    if !resumed {
        if let Some(o) = repeated_blind_nonce(&req.outputs) {
            return Err(Coded(
                sp_wire::codes::DUPLICATE_BLIND_NONCE,
                format!("output {o} repeats a blind nonce. Nothing was burned"),
            ));
        }
        if let Some((code, why)) =
            check_outputs_free(out_logic, store, &id, &req.outputs).map_err(Internal)?
        {
            return Err(Coded(code, why));
        }
        store.pending.insert(id.clone(), rotor.current.epoch);
        store.holds.insert(
            id.clone(),
            req.inputs
                .iter()
                .map(|i| hex::encode(i.note.nonce.0.serialize()))
                .collect(),
        );
        store.out_holds.insert(
            id.clone(),
            req.outputs
                .iter()
                .map(|o| hex::encode(o.blind_nonce.0.serialize()))
                .collect(),
        );
        store.save().map_err(Internal)?;
    }
    for (logic, sk, input, spent) in plan {
        if !spent {
            logic
                .redeem_with_key(sk, input, sighash)
                .map_err(|e| Internal(e.to_string()))?;
        }
    }
    if !proven {
        store.burned.insert(id.clone());
        store.save().map_err(Internal)?;
    }
    let mut outcomes = Vec::new();
    let mut keys = Vec::new();
    for o in &req.outputs {
        let (Some(sk), Some(pk)) = (out_keys.sk.get(&o.amount), out_keys.pk.get(&o.amount)) else {
            return Err(Internal(format!("{} has no key", o.amount.units)));
        };
        let out = match out_logic.issue_with_key(sk, o) {
            Ok(out) => out,
            Err(mint_types::MintOutputError::BlindNonceAlreadyUsed) if resumed => {
                MintOutputOutcome(bdhke::sign_message(sk, &o.blind_nonce.0))
            }
            Err(e) => return Err(Internal(e.to_string())),
        };
        outcomes.push(out);
        keys.push(*pk);
    }
    let answer = RolloverResponse {
        keyset: out_keys.id.clone(),
        epoch: out_keys.epoch,
        outcomes,
        keys,
    };
    store.answers.insert(id.clone(), answer.clone());
    store.pending.remove(&id);
    store.burned.remove(&id);
    store.holds.remove(&id);
    store.out_holds.remove(&id);
    store.save().map_err(Internal)?;
    Ok(answer)
}

// ---- the spend --------------------------------------------------------------

/// Why a spend was refused (`400`, with a code) or failed (`500`, retry).
#[derive(Debug, PartialEq, Eq)]
pub enum SpendError {
    /// Refused, with the `code` of the body. Nothing was burned.
    Refused(&'static str, String),
    /// Input `index` is already spent (`note_spent_elsewhere`, `spent: [index]`).
    Spent(usize),
    Internal(String),
}

fn refuse(code: &'static str, why: impl Into<String>) -> SpendError {
    SpendError::Refused(code, why.into())
}

/// The checks of a spend that need neither the mint's keys nor its lock, cheapest
/// first: how many notes, the memo, the keysets named, `pay`, then that the
/// notes are worth the change plus `pay` exactly (in `u128`), then no note and
/// no blind nonce twice. `max_items` is the most notes in and the most change
/// outputs out (the rollover's `rollover.max_items`).
pub fn check_spend_shape(req: &PspSpendRequest, max_items: usize) -> Result<(), SpendError> {
    if req.inputs.is_empty() {
        return Err(refuse("no_inputs", "a spend needs at least one note"));
    }
    if req.inputs.len() > max_items || req.outputs.len() > max_items {
        return Err(refuse(
            "too_many_items",
            format!("a spend takes at most {max_items} notes and {max_items} change outputs"),
        ));
    }
    if req.memo.len() > MAX_SPEND_MEMO_BYTES {
        return Err(refuse(
            "memo_too_long",
            format!("the memo is at most {MAX_SPEND_MEMO_BYTES} bytes"),
        ));
    }
    // The change needs a keyset to be signed with; a spend with no change has
    // nothing to sign and needs none.
    if req.input_keysets.len() != req.inputs.len()
        || (!req.outputs.is_empty() && req.keyset.is_empty())
    {
        return Err(refuse(
            sp_wire::codes::MISSING_KEYSET,
            "every input needs its keyset (input_keysets) and the change needs one (keyset)",
        ));
    }
    if req.pay == 0 {
        return Err(refuse(
            sp_wire::codes::NOTHING_TO_PAY,
            "a spend must pay something: with pay 0 it is a rollover, which has its own call",
        ));
    }
    let inputs: u128 = req.inputs.iter().map(|i| u128::from(i.amount.units)).sum();
    let change: u128 = req.outputs.iter().map(|o| u128::from(o.amount.units)).sum();
    if inputs != change + u128::from(req.pay) {
        return Err(refuse(
            sp_wire::codes::NOT_EXACT,
            format!(
                "the notes are worth {inputs}, the payment is {} and the change {change}: they must add up exactly",
                req.pay
            ),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for (i, input) in req.inputs.iter().enumerate() {
        if !seen.insert(input.note.nonce) {
            return Err(refuse(
                "duplicate_input",
                format!("input {i} is in the request twice"),
            ));
        }
    }
    if let Some(o) = repeated_blind_nonce(&req.outputs) {
        return Err(refuse(
            "duplicate_blind_nonce",
            format!("output {o} repeats a blind nonce"),
        ));
    }
    Ok(())
}

/// Run a PSP spend. The caller holds the mint lock and the rotor.
///
/// It is a rollover (see [`rollover`]) whose outputs add up to the inputs less
/// `pay`, and it takes the rollover's crash safety whole: the request is saved
/// as pending, with the notes it holds, before anything is burned; the burn is
/// recorded; the change is signed (a blind nonce already claimed is signed
/// again, deterministically); and the answer is stored with `pay` added to the
/// paid total in one write, only then is the reply made. A retry of the same
/// request gets the stored answer; a retry after a crash finishes the request.
///
/// `pay` is never returned to the payer: the inputs are redeemed in full, the
/// change is issued, and the difference is the SPI's revenue. By default the mint
/// reissues it as PSP notes for the SPI's own wallet (written to a file in
/// `revenue/` by [`Rollovers::flush_revenue`]); with `burn.enabled` it reissues
/// only what fits between what the SPI's wallet holds and `burn.threshold_psp`,
/// and burns the rest. What the wallet holds comes from `figure`, the balance the
/// SPI published, which the caller read before taking any lock; with no figure
/// (`None`) everything is reissued. Only paid PSP is ever burned here. The notes
/// reissued are planned and saved with the request before anything is burned, so
/// a crash cannot lose them or issue them twice, a retry makes the same notes,
/// and a request resumed after a crash keeps the decision it was first given
/// whatever the figure says by then.
pub fn spend(
    current: &MintLogic,
    rotor: &Rotor,
    store: &mut Rollovers,
    req: &PspSpendRequest,
    max_items: usize,
    burn: BurnPolicy,
    figure: Option<crate::balance::Figure>,
) -> Result<PspSpendResponse, SpendError> {
    use SpendError::*;
    check_spend_shape(req, max_items)?;
    for (i, input) in req.inputs.iter().enumerate() {
        if !rotor.current.sk.contains_key(&input.amount) {
            return Err(refuse(
                "bad_denomination",
                format!(
                    "input {i}: {} is not a denomination of this mint",
                    input.amount.units
                ),
            ));
        }
    }
    for (o, output) in req.outputs.iter().enumerate() {
        if !rotor.current.sk.contains_key(&output.amount) {
            return Err(refuse(
                "bad_denomination",
                format!(
                    "output {o}: {} is not a denomination of this mint",
                    output.amount.units
                ),
            ));
        }
    }
    // Answers for deleted keysets are of no use, so they go; a retry of one is
    // answered from the spent list (`note_spent_elsewhere`).
    store.prune(|k| rotor.is_retired(k));
    let id = req.hash();
    if let Some(done) = store.spends.get(&id) {
        return Ok(done.clone());
    }
    let resumed = store.pending.contains_key(&id);
    let proven = resumed && store.burned.contains(&id);
    let others = store.held_nonces(&id);
    let own = store.holds.get(&id).cloned().unwrap_or_default();
    let gen_of = |side: Side| -> (&MintLogic, &GenKeys) {
        match side {
            Side::Current => (current, &rotor.current),
            Side::Previous => {
                let (k, l) = rotor.previous.as_ref().expect("resolved");
                (l, k)
            }
        }
    };
    // The change is signed by the live set the request names, current or
    // previous (a spend with no change names none, and the answer names the
    // current set). A set that is not live is refused, except for a request
    // resumed after a crash, whose inputs are already burned: it is signed by
    // the current set and the reply names it.
    let (out_logic, out_keys) = if req.outputs.is_empty() {
        (current, &rotor.current)
    } else {
        match rotor.resolve(&req.keyset) {
            Ok(side) => gen_of(side),
            Err(_) if resumed => (current, &rotor.current),
            Err(e) => return Err(refuse(e.code(), e.why("the change"))),
        }
    };
    let sighash = req.digest();
    let mut plan: Vec<(&MintLogic, &SecretKey, &MintInput, bool)> = Vec::new();
    for (i, input) in req.inputs.iter().enumerate() {
        if others.contains(&hex::encode(input.note.nonce.0.serialize())) {
            return Err(refuse(
                sp_wire::codes::NOTE_HELD,
                format!("input {i} is held by an unfinished request. Nothing was burned"),
            ));
        }
        let (logic, keys) = match rotor.resolve(&req.input_keysets[i]) {
            Ok(side) => gen_of(side),
            // Started before its keyset was deleted: finished without burning
            // only if the stored state proves this request burned it (see
            // `rollover`).
            Err(_)
                if proven
                    || (resumed
                        && own.contains(&hex::encode(input.note.nonce.0.serialize()))
                        && rotor.burn_proven(&id, input)) =>
            {
                continue;
            }
            Err(e) if resumed => {
                return Err(refuse(
                    sp_wire::codes::BURN_NOT_PROVEN,
                    format!(
                        "input {i}: {}; this request was started before, but nothing stored proves its inputs were burned under this request, so it cannot be finished without them",
                        e.why("its keyset")
                    ),
                ));
            }
            Err(e) => return Err(refuse(e.code(), e.why(&format!("input {i}")))),
        };
        logic.verify_note(input).map_err(|_| {
            refuse(
                "bad_note",
                format!("input {i} is not a note of keyset {}", keys.id),
            )
        })?;
        logic
            .verify_spend_authorization(input, sighash)
            .map_err(|_| {
                refuse(
                    "bad_spend_signature",
                    format!("input {i}: the spend signature does not cover this request"),
                )
            })?;
        let spent = logic
            .is_spent(input.note.nonce)
            .map_err(|e| Internal(e.to_string()))?;
        if spent && !resumed {
            return Err(Spent(i));
        }
        let sk = keys
            .sk
            .get(&input.amount)
            .ok_or_else(|| refuse("bad_denomination", format!("input {i}: not a denomination")))?;
        plan.push((logic, sk, input, spent));
    }
    // The change, before anything is written or burned (see `rollover`; a
    // repeated blind nonce is refused by `check_spend_shape`).
    if let Some(o) = req
        .outputs
        .iter()
        .find(|o| !out_keys.sk.contains_key(&o.amount) || !out_keys.pk.contains_key(&o.amount))
    {
        let why = format!(
            "output: {} is not a denomination of keyset {}",
            o.amount.units, out_keys.id
        );
        return Err(if resumed {
            Internal(why)
        } else {
            refuse("bad_denomination", why)
        });
    }
    if !resumed {
        if let Some((code, why)) =
            check_outputs_free(out_logic, store, &id, &req.outputs).map_err(Internal)?
        {
            return Err(refuse(code, why));
        }
        let held = figure.map(|f| crate::balance::held(&f, store.reissue_total()));
        let keep = burn.keep(req.pay, held);
        let plans = plan_revenue(&rotor.current, keep)?;
        store.pending.insert(id.clone(), rotor.current.epoch);
        store.holds.insert(
            id.clone(),
            req.inputs
                .iter()
                .map(|i| hex::encode(i.note.nonce.0.serialize()))
                .collect(),
        );
        store.out_holds.insert(
            id.clone(),
            req.outputs
                .iter()
                .map(|o| hex::encode(o.blind_nonce.0.serialize()))
                .collect(),
        );
        store.plans.insert(id.clone(), plans);
        store.save().map_err(Internal)?;
    }
    for (logic, sk, input, spent) in plan {
        if !spent {
            logic
                .redeem_with_key(sk, input, sighash)
                .map_err(|e| Internal(e.to_string()))?;
        }
    }
    if !proven {
        store.burned.insert(id.clone());
        store.save().map_err(Internal)?;
    }
    let mut outcomes = Vec::new();
    let mut keys = Vec::new();
    for o in &req.outputs {
        let (Some(sk), Some(pk)) = (out_keys.sk.get(&o.amount), out_keys.pk.get(&o.amount)) else {
            return Err(Internal(format!("{} has no key", o.amount.units)));
        };
        let out = match out_logic.issue_with_key(sk, o) {
            Ok(out) => out,
            Err(mint_types::MintOutputError::BlindNonceAlreadyUsed) if resumed => {
                MintOutputOutcome(bdhke::sign_message(sk, &o.blind_nonce.0))
            }
            Err(e) => return Err(Internal(e.to_string())),
        };
        outcomes.push(out);
        keys.push(*pk);
    }
    // The notes for the SPI's wallet, from the plans saved with the request. A
    // plan whose keyset has been deleted since (a request resumed more than a
    // key period later) cannot be signed: its units count as burned.
    let mut revenue: Vec<HandedPspNote> = Vec::new();
    for planned in store.plans.get(&id).cloned().unwrap_or_default() {
        let Ok(side) = rotor.resolve(&planned.keyset) else {
            continue;
        };
        let (logic, keys) = gen_of(side);
        revenue.push(make_revenue_note(logic, keys, &planned, resumed)?);
    }
    let kept: u64 = revenue.iter().map(|n| n.amount).sum();
    let answer = PspSpendResponse {
        keyset: out_keys.id.clone(),
        epoch: out_keys.epoch,
        outcomes,
        keys,
        paid: req.pay,
    };
    // The answer, the payment it counts and the notes reissued go in one write.
    store.spends.insert(id.clone(), answer.clone());
    store.paid = store.paid.saturating_add(req.pay);
    store.paid_kept = store.paid_kept.saturating_add(kept);
    store.paid_burned = store
        .paid_burned
        .saturating_add(req.pay.saturating_sub(kept));
    if !revenue.is_empty() {
        store.revenue.insert(id.clone(), revenue);
    }
    store.plans.remove(&id);
    store.pending.remove(&id);
    store.burned.remove(&id);
    store.holds.remove(&id);
    store.out_holds.remove(&id);
    store.save().map_err(Internal)?;
    // Best effort: the answer is stored, so a failed write is tried again by the
    // next flush (the scan loop makes one every pass) and costs the payer nothing.
    if let Err(e) = store.flush_revenue() {
        eprintln!("could not write the reissued PSP to revenue/, will retry: {e}");
    }
    Ok(answer)
}

/// The denominations that add up to `units`, largest first, from the keys of
/// `keys`. `None` when they cannot (no denomination of 1).
fn split_units(keys: &GenKeys, mut units: u64) -> Option<Vec<u64>> {
    let mut sizes: Vec<u64> = keys.sk.keys().map(|a| a.units).collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let mut out = Vec::new();
    for size in sizes {
        while size > 0 && units >= size {
            out.push(size);
            units -= size;
        }
    }
    (units == 0).then_some(out)
}

/// The notes to reissue for `keep` units of `pay`: fresh keys for each, none of
/// them signed yet.
fn plan_revenue(keys: &GenKeys, keep: u64) -> Result<Vec<RevenuePlan>, SpendError> {
    if keep == 0 {
        return Ok(Vec::new());
    }
    let sizes = split_units(keys, keep).ok_or_else(|| {
        SpendError::Internal(format!(
            "cannot split {keep} units into this mint's denominations"
        ))
    })?;
    Ok(sizes
        .into_iter()
        .map(|amount| RevenuePlan {
            amount,
            keyset: keys.id.clone(),
            nonce_secret: SecretKey::new(&mut OsRng),
            blinding_key: SecretKey::new(&mut OsRng),
        })
        .collect())
}

/// Make the note a plan describes: blind its nonce with the planned factor, have
/// the mint issue and sign it (a blind nonce already used is signed again, as
/// for the change, when `resumed`), and unblind it. The note is the mint's own
/// work from end to end, which is what the SPI's wallet then swaps for notes only
/// it knows.
fn make_revenue_note(
    logic: &MintLogic,
    keys: &GenKeys,
    plan: &RevenuePlan,
    resumed: bool,
) -> Result<HandedPspNote, SpendError> {
    let amount = Amount::from_units(plan.amount);
    let sk = keys
        .sk
        .get(&amount)
        .ok_or_else(|| SpendError::Internal(format!("{} is not a denomination", plan.amount)))?;
    let pk = keys
        .pk
        .get(&amount)
        .ok_or_else(|| SpendError::Internal(format!("{} has no public key", plan.amount)))?;
    let nonce = Nonce(PublicKey::from_secret_key(
        secp256k1::SECP256K1,
        &plan.nonce_secret,
    ));
    let (blinded, blinding) =
        bdhke::blind_message(&nonce.as_hash_preimage(), Some(plan.blinding_key));
    let output = MintOutput {
        amount,
        blind_nonce: BlindNonce(blinded),
    };
    let outcome = match logic.issue_with_key(sk, &output) {
        Ok(out) => out,
        Err(mint_types::MintOutputError::BlindNonceAlreadyUsed) if resumed => {
            MintOutputOutcome(bdhke::sign_message(sk, &blinded))
        }
        Err(e) => return Err(SpendError::Internal(e.to_string())),
    };
    Ok(HandedPspNote {
        amount: plan.amount,
        note: Note {
            nonce,
            signature: bdhke::unblind_message(&outcome.0, &blinding, pk),
        },
        spend_secret: plan.nonce_secret,
        keyset: plan.keyset.clone(),
    })
}

/// The schedule for `config`, read from the files only (no database is opened,
/// so it works while `serve` runs).
pub fn describe_offline(base: &Path, activation_hours: u64, period_hours: u64) -> Vec<String> {
    let first = load_first_issue(base).ok().flatten();
    let r = Rotation {
        first_issue: first,
        activation_secs: activation_hours.saturating_mul(3600),
        period_secs: period_hours.saturating_mul(3600),
    };
    let now = crate::server::now_secs();
    let e = r.epoch_at(now);
    let id = |epoch: u64| -> String {
        fs::read(gen_dir(base, epoch).join(KEYS))
            .ok()
            .and_then(|b| serde_json::from_slice::<MintConfig>(&b).ok())
            .map(|c| keyset_id(&keygen::public_keys(&c)))
            .unwrap_or_else(|| "(made when the mint next opens its data)".into())
    };
    let t = |o: Option<u64>| o.map(|v| v.to_string()).unwrap_or_else(|| "not yet".into());
    let mut out = vec![
        format!(
            "rotation: activation {activation_hours} h after the first issue, period {period_hours} h{}",
            if activation_hours == 0 { " (off)" } else { "" }
        ),
        format!(
            "  first issue: {}; activation: {}; next rotation: {} (Unix seconds)",
            t(first),
            t(r.activation()),
            t(r.activation().map(|_| r.start_of(e + 1).unwrap_or(0)))
        ),
        format!("  current keyset {} (epoch {e})", id(e)),
    ];
    if e > 0 {
        out.push(format!(
            "  previous keyset {} (epoch {}), retires at {}",
            id(e - 1),
            e - 1,
            t(r.start_of(e + 1))
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mint_types::MintOutput;

    fn scratch(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("psp-rot-{}-{n}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_deletion_cut_short_by_a_crash_is_finished_at_the_next_open() {
        let d = scratch("sweep");
        fs::create_dir_all(d.join("retiring-gen-3/db")).unwrap();
        fs::write(d.join("retiring-gen-3/mint_keys.json"), b"{}").unwrap();
        let (r, _cur) = Rotor::open(&d, Amount::from_units(16), 1, 1).unwrap();
        assert!(!d.join("retiring-gen-3").exists(), "swept at open");
        assert_eq!(r.resolve("00").unwrap_err().code(), "unknown_keyset");
    }

    #[test]
    fn retired_ids_are_told_from_unknown_ones_and_trimmed_oldest_first() {
        let d = scratch("retired-list");
        let t = 10_000;
        let (mut r, mut cur) = Rotor::open_at(&d, Amount::from_units(16), 1, 1, t).unwrap();
        r.first_issue = Some(t);
        let mut ids = vec![r.current.id.clone()];
        for k in 1..=4u64 {
            r.clock.store(t + 3600 * k, Ordering::SeqCst);
            r.refresh(&mut cur).unwrap();
            ids.push(r.current.id.clone());
        }
        // Epochs 0, 1 and 2 are retired; 3 previous, 4 current.
        for id in &ids[..3] {
            assert_eq!(r.resolve(id).unwrap_err().code(), "keyset_retired");
        }
        assert_eq!(r.resolve(&ids[3]), Ok(Side::Previous));
        assert_eq!(
            r.resolve(&"ab".repeat(8)).unwrap_err().code(),
            "unknown_keyset"
        );
        r.set_retired_keep(2).unwrap();
        assert_eq!(
            r.resolve(&ids[0]).unwrap_err().code(),
            "unknown_keyset",
            "trimmed"
        );
        assert_eq!(r.resolve(&ids[1]).unwrap_err().code(), "keyset_retired");
        // The trimmed list is what was saved; the audit totals are unchanged.
        let saved = load_retired(&d).unwrap();
        assert_eq!(saved.ids, ids[1..3].to_vec());
        assert_eq!(saved.epochs, vec![0, 1, 2]);
    }

    #[test]
    fn a_crash_after_the_id_is_listed_but_before_the_set_is_deleted_retires_it_once() {
        let d = scratch("retire-crash");
        let t = 10_000;
        let (mut r, mut cur) = Rotor::open_at(&d, Amount::from_units(16), 1, 1, t).unwrap();
        r.first_issue = Some(t);
        let id0 = r.current.id.clone();
        r.clock.store(t + 3600, Ordering::SeqCst);
        r.refresh(&mut cur).unwrap();
        drop((r, cur));
        // The crash: retired.json lists generation 0, but its files are still
        // there (the list is written, and synced, before anything is deleted).
        let mut listed = load_retired(&d).unwrap();
        listed.epochs.push(0);
        listed.ids.push(id0.clone());
        save_json(&d.join(RETIRED_FILE), &listed).unwrap();
        save_json(&d.join(FIRST_ISSUE_FILE), &FirstIssue { first_issue: t }).unwrap();
        let (r, _cur) = Rotor::open_at(&d, Amount::from_units(16), 1, 1, t + 7200).unwrap();
        assert!(!d.join("mint_keys.json").exists(), "deleted at the restart");
        assert_eq!(
            load_retired(&d)
                .unwrap()
                .ids
                .iter()
                .filter(|i| **i == id0)
                .count(),
            1
        );
        assert_eq!(r.resolve(&id0).unwrap_err().code(), "keyset_retired");
    }

    #[test]
    fn stored_answers_of_a_retired_set_are_pruned_and_others_kept() {
        let d = scratch("prune");
        fs::create_dir_all(&d).unwrap();
        let mut r = Rollovers::open(&d).unwrap();
        let ans = |k: &str| RolloverResponse {
            epoch: 0,
            keyset: k.to_string(),
            outcomes: vec![],
            keys: vec![],
        };
        r.answers.insert("a".into(), ans("old"));
        r.answers.insert("b".into(), ans("new"));
        r.prune(|k| k == "old");
        assert_eq!(r.answers.keys().collect::<Vec<_>>(), vec!["b"]);
        assert!(
            Rollovers::open(&d).unwrap().answers.contains_key("b"),
            "saved"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("key-mode");
        drop(Rotor::open(&d, Amount::from_units(16), 0, 0).unwrap());
        let mode = fs::metadata(d.join(KEYS)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn epochs_follow_the_first_issue() {
        let r = Rotation {
            first_issue: Some(1000),
            activation_secs: 100,
            period_secs: 50,
        };
        assert_eq!(r.epoch_at(1099), 0);
        assert_eq!(r.epoch_at(1100), 1);
        assert_eq!(r.epoch_at(1150), 2);
        assert_eq!(r.start_of(2), Some(1150));
        let off = Rotation {
            first_issue: Some(1000),
            activation_secs: 0,
            period_secs: 50,
        };
        assert_eq!(off.epoch_at(u64::MAX), 0);
        let fresh = Rotation {
            first_issue: None,
            ..r
        };
        assert_eq!(fresh.epoch_at(u64::MAX), 0);
    }

    #[test]
    fn generation_zero_keeps_its_keys_across_restarts_and_old_ones_are_deleted() {
        let d = scratch("gens");
        // Generation 0 is made at the top of the data folder, and a restart finds
        // the same keys.
        let (first, first_logic) = Rotor::open(&d, Amount::from_units(1024), 1, 1).unwrap();
        let keys0 = first_logic.public_keys().clone();
        // 1, 2, 4 ... 1024: eleven denominations
        assert_eq!(keys0.len(), 11);
        assert!(d.join(KEYS).exists() && d.join(DB).exists());
        drop(first);
        drop(first_logic);
        let (mut r, mut cur) = Rotor::open(&d, Amount::from_units(1024), 1, 1).unwrap();
        assert_eq!(cur.public_keys(), &keys0, "generation 0 keeps its keys");
        assert_eq!(r.first_issue, None, "nothing issued: rotation not started");
        // First issuance, then the clock passes two periods.
        let out = MintOutput {
            amount: Amount::from_units(1),
            blind_nonce: mint_types::BlindNonce(
                secp256k1::SecretKey::new(&mut rand::thread_rng()).public_key(secp256k1::SECP256K1),
            ),
        };
        cur.issue(&out).unwrap();
        r.clock.store(10_000, Ordering::SeqCst);
        r.refresh(&mut cur).unwrap();
        assert_eq!(r.first_issue, Some(10_000));
        assert!(d.join(FIRST_ISSUE_FILE).exists());
        r.clock.store(10_000 + 3600, Ordering::SeqCst);
        r.refresh(&mut cur).unwrap();
        assert_eq!(r.current.epoch, 1);
        assert_eq!(r.previous.as_ref().unwrap().0.epoch, 0);
        assert!(d.join(KEYS).exists(), "generation 0 still live as previous");
        r.clock.store(10_000 + 2 * 3600, Ordering::SeqCst);
        r.refresh(&mut cur).unwrap();
        assert_eq!(r.current.epoch, 2);
        assert!(
            !d.join(KEYS).exists() && !d.join(DB).exists(),
            "generation 0 deleted with its spent list"
        );
        assert_eq!(r.retired_totals(), (1, 0), "its totals kept for the audit");
        let first = r.first_issue;
        drop(r);
        drop(cur);
        // A restart finds the same generations and the same first issue.
        let (r2, _) =
            Rotor::open_at(&d, Amount::from_units(1024), 1, 1, 10_000 + 2 * 3600).unwrap();
        assert_eq!(r2.first_issue, first);
        assert_eq!(r2.current.epoch, 2);
        assert_eq!(r2.previous.as_ref().unwrap().0.epoch, 1);
        fs::remove_dir_all(&d).unwrap();
    }
}
