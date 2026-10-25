//! Which memo each burned PSP note was burned under.
//!
//! The ESP mint burns PSP notes in an exchange and binds the burn to a memo that
//! every note's owner signed. If the ESP mint stops after the burn, its retry
//! must be able to tell a note it burned itself from a note that someone else
//! burned in the meantime. A bare "spent" flag cannot say, so the PSP keeps the
//! memo of each burn, and `POST /burns` returns it.
//!
//! The memo is recorded in `melt_memos.json` in the data directory, one object
//! from note nonce (hex) to memo. It is a placeholder store: the whole file is
//! rewritten on every change (temporary file, fsync, rename, directory fsync). A
//! missing file means no records. An unreadable file is an error and never an
//! empty log.
//!
//! The memo is recorded before the burn, with the mint lock held, for every note
//! of a `/melt` that has passed every check (keysets, proofs, spend signatures,
//! no duplicates, unspent, the amount's key). A melt that is refused records
//! nothing, so a `400` from `/melt` means nothing was burned and nothing was
//! recorded. The record goes before the burn, not after, so that a crash between
//! the two leaves a record for notes that are not spent (harmless, and
//! overwritten by the retry), and never notes that are spent with no record (the
//! retry would take them for notes spent by someone else).
//!
//! Two other calls burn notes without this log: a rollover (kind 5) and a spend
//! (kind 14). Each of them clears the record of every input that is unspent
//! (`MeltLog::clear`, under the mint lock, before it burns anything), so that a
//! note they burn never shows a memo that an earlier `/melt` left on it. A note
//! that is spent keeps its record: it may be an exchange's real burn.
//!
//! So a note that is spent, with a memo on record, was spent by the `/melt`
//! that recorded the memo: the lock lets one burn run at a time, and every burn
//! (`/melt`, a rollover, a spend) either records its memo or clears the record
//! first. A note that is spent with no record (spent by a rollover or a spend, or
//! before this log existed) reports no memo.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use mint_types::Nonce;

#[derive(Debug)]
pub enum MeltLogError {
    Io(io::Error),
    Corrupt(serde_json::Error),
}

impl fmt::Display for MeltLogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeltLogError::Io(e) => write!(f, "burn memos: {e}"),
            MeltLogError::Corrupt(e) => write!(f, "burn memos are not valid json: {e}"),
        }
    }
}

impl Error for MeltLogError {}

impl From<io::Error> for MeltLogError {
    fn from(e: io::Error) -> Self {
        MeltLogError::Io(e)
    }
}

fn key(nonce: &Nonce) -> String {
    hex::encode(nonce.0.serialize())
}

/// The file's name in the network's data folder.
pub const FILE: &str = "melt_memos.json";

pub struct MeltLog {
    path: PathBuf,
    rows: BTreeMap<String, String>,
}

impl MeltLog {
    pub fn open(path: &Path) -> Result<Self, MeltLogError> {
        let rows = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(MeltLogError::Corrupt)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(MeltLog {
            path: path.to_path_buf(),
            rows,
        })
    }

    /// The memo recorded for this note, if there is one.
    pub fn get(&self, nonce: &Nonce) -> Option<&str> {
        self.rows.get(&key(nonce)).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Record `memo` for each of these notes, replacing any earlier record, and
    /// write the whole file. Nothing is recorded if the write fails.
    pub fn record(&mut self, nonces: &[Nonce], memo: &str) -> Result<(), MeltLogError> {
        if nonces.is_empty() {
            return Ok(());
        }
        let mut next = self.rows.clone();
        for nonce in nonces {
            next.insert(key(nonce), memo.to_string());
        }
        self.write(&next)?;
        self.rows = next;
        Ok(())
    }

    /// Forget the memo of each of these notes, and write the whole file. A note
    /// with no record is skipped, and nothing is written when none has one.
    /// Returns how many records went. Nothing is cleared if the write fails.
    pub fn clear(&mut self, nonces: &[Nonce]) -> Result<usize, MeltLogError> {
        let mut next = self.rows.clone();
        let before = next.len();
        for nonce in nonces {
            next.remove(&key(nonce));
        }
        let gone = before - next.len();
        if gone == 0 {
            return Ok(0);
        }
        self.write(&next)?;
        self.rows = next;
        Ok(gone)
    }

    fn write(&self, rows: &BTreeMap<String, String>) -> Result<(), MeltLogError> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(rows).map_err(io::Error::other)?;
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        // Make the rename itself durable. Not every platform allows opening a
        // directory, so a failure here is ignored.
        if let Some(dir) = self.path.parent()
            && let Ok(d) = File::open(dir)
        {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use secp256k1::{Keypair, SECP256K1};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psp-mint-melts-{}-{name}", {
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

    fn nonce() -> Nonce {
        Nonce(Keypair::new(SECP256K1, &mut OsRng).public_key())
    }

    #[test]
    fn a_missing_file_is_empty_and_records_survive_a_reopen() {
        let dir = scratch("reopen");
        let path = dir.join("melt_memos.json");
        let mut log = MeltLog::open(&path).unwrap();
        assert!(log.is_empty());
        let (a, b, c) = (nonce(), nonce(), nonce());
        log.record(&[a, b], "memo one").unwrap();
        let again = MeltLog::open(&path).unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again.get(&a), Some("memo one"));
        assert_eq!(again.get(&b), Some("memo one"));
        assert_eq!(again.get(&c), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_later_record_replaces_an_earlier_one_for_the_same_note() {
        let dir = scratch("replace");
        let mut log = MeltLog::open(&dir.join("melt_memos.json")).unwrap();
        let (a, b) = (nonce(), nonce());
        log.record(&[a, b], "first").unwrap();
        log.record(&[a], "second").unwrap();
        assert_eq!(log.get(&a), Some("second"));
        assert_eq!(log.get(&b), Some("first"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_file_is_an_error_and_is_left_alone() {
        let dir = scratch("damaged");
        let path = dir.join("melt_memos.json");
        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            MeltLog::open(&path),
            Err(MeltLogError::Corrupt(_))
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        fs::remove_dir_all(&dir).unwrap();
    }
}
