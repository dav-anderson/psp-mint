//! Key rotation: at most two key sets (generations), each with its own spent
//! list, current and previous.
//!
//! Generation 0 is the mint as it was before rotation existed: `mint_keys.json`
//! and `db/` at the top of the network's data folder, so existing data is used
//! as it is. Generation N (1 and later) is `gen-N/mint_keys.json` and
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

/// `GET /rotation`, the same shape as fsp-mint's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysetWindow {
    pub id: String,
    pub epoch: u64,
    pub starts: Option<u64>,
    pub current_until: Option<u64>,
    pub retires: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationResponse {
    pub active: bool,
    pub first_issue: Option<u64>,
    pub rotation_activation_hours: u64,
    pub activation: Option<u64>,
    pub rotation_period_hours: u64,
    pub current: KeysetWindow,
    pub previous: Option<KeysetWindow>,
    pub next_rotation: Option<u64>,
    pub now: u64,
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
    /// Live, but not the current set: new outputs are signed only by the
    /// current one.
    NotCurrent { named: String, current: String },
}

impl KeysetError {
    pub fn code(&self) -> &'static str {
        match self {
            KeysetError::Retired(_) => "keyset_retired",
            KeysetError::Unknown(_) => "unknown_keyset",
            KeysetError::NotCurrent { .. } => "keyset_not_current",
        }
    }
    pub fn why(&self, what: &str) -> String {
        match self {
            KeysetError::Retired(id) => {
                format!("{what}: keyset {id} was rotated out and deleted with its spent list")
            }
            KeysetError::Unknown(id) => format!("{what}: keyset {id} is not a keyset of this mint"),
            KeysetError::NotCurrent { named, current } => format!(
                "{what}: keyset {named} is the previous keyset; new outputs are signed only by the current keyset {current}. Re-blind the outputs for {current} and send the request again. Nothing was burned"
            ),
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

/// The keyset ID: 8 bytes of SHA-256 over a tag and the 1-unit public key.
pub fn keyset_id(pk: &BTreeMap<Amount, PublicKey>) -> String {
    use sha2::{Digest, Sha256};
    let mut data = b"psp-mint/keyset-id/v1".to_vec();
    if let Some(k) = pk.values().next() {
        data.extend_from_slice(&k.serialize());
    }
    hex::encode(&Sha256::digest(&data)[..8])
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
    #[serde(default)]
    ids: Vec<String>,
    /// Burn proofs taken from a spent list just before it is deleted: nonces
    /// (hex) a pending rollover held that were spent in the retiring set.
    /// Saved together with the retired ID, before anything is deleted.
    #[serde(default)]
    burn_proofs: std::collections::BTreeSet<String>,
    issued: u64,
    redeemed: u64,
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
                {
                    if exists(base, n) {
                        newest = newest.max(n);
                    }
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

    /// Add a generation's totals to `retired.json` (once), then delete it.
    fn retire(&mut self, epoch: u64) -> Result<(), Box<dyn Error>> {
        if !self.retired.epochs.contains(&epoch) {
            let (keys, logic) = open_gen(&self.base, epoch, self.max)?;
            // Burn proofs from this set's spent list, the last moment it
            // exists: notes a pending rollover holds that are spent here were
            // burned by that rollover (no other request may take them).
            let held = Rollovers::open(&self.base)?.held_nonces("");
            self.retired.burn_proofs.retain(|n| held.contains(n));
            for n in &held {
                let Ok(bytes) = hex::decode(n) else { continue };
                let Ok(pk) = secp256k1::PublicKey::from_slice(&bytes) else {
                    continue;
                };
                if logic
                    .is_spent(mint_types::Nonce(pk))
                    .map_err(|e| e.to_string())?
                {
                    self.retired.burn_proofs.insert(n.clone());
                }
            }
            let (i, d) = logic.audit_totals().map_err(|e| e.to_string())?;
            drop(logic);
            self.retired.epochs.push(epoch);
            self.retired.ids.push(keys.id);
            self.trim_retired();
            self.retired.issued = self.retired.issued.saturating_add(i.units);
            self.retired.redeemed = self.retired.redeemed.saturating_add(d.units);
            save_json(&self.base.join(RETIRED_FILE), &self.retired)?;
        }
        delete_gen(&self.base, epoch)
    }

    fn retire_logic(&mut self, keys: GenKeys, logic: MintLogic) -> Result<(), Box<dyn Error>> {
        if !self.retired.epochs.contains(&keys.epoch) {
            // Burn proofs from this set's spent list, the last moment it
            // exists: notes a pending rollover holds that are spent here were
            // burned by that rollover (no other request may take them).
            let held = Rollovers::open(&self.base)?.held_nonces("");
            self.retired.burn_proofs.retain(|n| held.contains(n));
            for n in &held {
                let Ok(bytes) = hex::decode(n) else { continue };
                let Ok(pk) = secp256k1::PublicKey::from_slice(&bytes) else {
                    continue;
                };
                if logic
                    .is_spent(mint_types::Nonce(pk))
                    .map_err(|e| e.to_string())?
                {
                    self.retired.burn_proofs.insert(n.clone());
                }
            }
            let (i, d) = logic.audit_totals().map_err(|e| e.to_string())?;
            self.retired.epochs.push(keys.epoch);
            self.retired.ids.push(keys.id.clone());
            self.trim_retired();
            self.retired.issued = self.retired.issued.saturating_add(i.units);
            self.retired.redeemed = self.retired.redeemed.saturating_add(d.units);
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
            if issued.units > 0 || self.previous.is_some() || self.current.epoch > 0 {
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

    /// Issued and redeemed of the retired generations.
    /// Whether `input` was proven burned (by the pending rollover holding it)
    /// when its keyset was deleted; see `Retired::burn_proofs`.
    pub fn burn_proven(&self, input: &MintInput) -> bool {
        self.retired
            .burn_proofs
            .contains(&hex::encode(input.note.nonce.0.serialize()))
    }

    pub fn retired_totals(&self) -> (u64, u64) {
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

use mint_types::{MintInput, MintOutput, MintOutputOutcome, transaction_sighash};

pub const ROLLOVERS_FILE: &str = "rollovers.json";
/// Most notes in one rollover.
pub const MAX_ROLLOVER_NOTES: usize = 1000;

/// `POST /rollover`: notes in, fresh notes of exactly the same amounts out,
/// signed with the current keyset. Every input signs `transaction_sighash`
/// over the inputs and outputs, as in a mint-core swap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloverRequest {
    /// The keyset of each input note, in order. Each note is verified, and
    /// looked up in the spent list, of the keyset it names.
    #[serde(default)]
    pub input_keysets: Vec<String>,
    pub inputs: Vec<MintInput>,
    /// The keyset the outputs are to be signed with: the current or the
    /// previous one (normally the current).
    #[serde(default)]
    pub keyset: String,
    pub outputs: Vec<MintOutput>,
}

impl RolloverRequest {
    pub fn hash(&self) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(
            serde_json::to_vec(self).expect("serializable"),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloverResponse {
    pub keyset: String,
    pub epoch: u64,
    pub outcomes: Vec<MintOutputOutcome>,
    /// The public key each output was signed with, for unblinding.
    pub keys: Vec<PublicKey>,
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
    #[serde(default)]
    burned: std::collections::BTreeSet<String>,
    /// The input nonces (hex) each pending rollover holds, saved with its
    /// pending entry before anything is burned. No other rollover or melt may
    /// take them meanwhile.
    #[serde(default)]
    holds: BTreeMap<String, Vec<String>>,
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

    /// Drop stored answers whose keyset was deleted.
    fn prune(&mut self, retired: impl Fn(&str) -> bool) {
        let before = self.answers.len();
        self.answers.retain(|_, a| !retired(&a.keyset));
        if self.answers.len() != before {
            let _ = self.save();
        }
    }
}

/// Why a rollover was refused (`400`, with a code) or failed (`500`, retry).
#[derive(Debug, PartialEq, Eq)]
pub enum RolloverError {
    Refused(String),
    Keyset(&'static str, String),
    /// Input `index` is already spent. The body says which, so the wallet can
    /// stop offering that note (`code: note_spent_elsewhere`, `spent: [index]`).
    Spent(usize),
    Internal(String),
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
) -> Result<RolloverResponse, RolloverError> {
    use RolloverError::*;
    if req.inputs.is_empty() || req.inputs.len() > MAX_ROLLOVER_NOTES {
        return Err(Refused(format!(
            "a rollover takes 1 to {MAX_ROLLOVER_NOTES} notes"
        )));
    }
    if req.input_keysets.len() != req.inputs.len() || req.keyset.is_empty() {
        return Err(Keyset(
            "missing_keyset",
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
    let gen_of = |side: Side| -> (&MintLogic, &GenKeys) {
        match side {
            Side::Current => (current, &rotor.current),
            Side::Previous => {
                let (k, l) = rotor.previous.as_ref().expect("resolved");
                (l, k)
            }
        }
    };
    // New signing is by the current set only; a resumed request is signed
    // by the current set whatever it named (the reply names it).
    let (out_logic, out_keys) = if resumed {
        (current, &rotor.current)
    } else {
        match rotor.resolve(&req.keyset) {
            Ok(Side::Current) => (current, &rotor.current),
            Ok(Side::Previous) => {
                let e = KeysetError::NotCurrent {
                    named: req.keyset.clone(),
                    current: rotor.current.id.clone(),
                };
                return Err(Keyset(e.code(), e.why("the outputs")));
            }
            Err(e) => return Err(Keyset(e.code(), e.why("the outputs"))),
        }
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
            // burning only if the stored state proves this request burned it.
            Err(_) if proven || rotor.burn_proven(input) => continue,
            Err(e) if resumed => {
                return Err(Keyset(
                    "burn_not_proven",
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
    if !resumed {
        store.pending.insert(id.clone(), rotor.current.epoch);
        store.holds.insert(
            id.clone(),
            req.inputs
                .iter()
                .map(|i| hex::encode(i.note.nonce.0.serialize()))
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
        let sk = &out_keys.sk[&o.amount];
        let out = match out_logic.issue_with_key(sk, o) {
            Ok(out) => out,
            Err(mint_types::MintOutputError::BlindNonceAlreadyUsed) if resumed => {
                MintOutputOutcome(bdhke::sign_message(sk, &o.blind_nonce.0))
            }
            Err(e) => return Err(Internal(e.to_string())),
        };
        outcomes.push(out);
        keys.push(out_keys.pk[&o.amount]);
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
    store.save().map_err(Internal)?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(RETIRED_KEYSETS_CAP, 131_072, "1 MB of 8-byte IDs");
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
    fn generation_zero_is_the_existing_mint_and_old_ones_are_deleted() {
        let d = scratch("gens");
        // An existing mint, from before rotation.
        let old = crate::mint::open(&d, Amount::from_units(16)).unwrap();
        let keys0 = old.public_keys().clone();
        drop(old);
        let (mut r, mut cur) = Rotor::open(&d, Amount::from_units(16), 1, 1).unwrap();
        assert_eq!(
            cur.public_keys(),
            &keys0,
            "generation 0 = the existing keys"
        );
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
        let (r2, _) = Rotor::open_at(&d, Amount::from_units(16), 1, 1, 10_000 + 2 * 3600).unwrap();
        assert_eq!(r2.first_issue, first);
        assert_eq!(r2.current.epoch, 2);
        assert_eq!(r2.previous.as_ref().unwrap().0.epoch, 1);
        fs::remove_dir_all(&d).unwrap();
    }
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
            .unwrap_or_else(|| "(made at the next start)".into())
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
