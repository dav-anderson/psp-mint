use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ProcessedError {
    Io(io::Error),
    Corrupt(serde_json::Error),
}

impl fmt::Display for ProcessedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessedError::Io(e) => write!(f, "processed txid file: {e}"),
            ProcessedError::Corrupt(e) => write!(f, "processed txid file is not valid json: {e}"),
        }
    }
}

impl Error for ProcessedError {}

impl From<io::Error> for ProcessedError {
    fn from(e: io::Error) -> Self {
        ProcessedError::Io(e)
    }
}

pub struct Processed {
    path: PathBuf,
    txids: BTreeSet<String>,
}

impl Processed {
    pub fn open(path: &Path) -> Result<Self, ProcessedError> {
        let txids = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(ProcessedError::Corrupt)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path: path.to_path_buf(),
            txids,
        })
    }

    pub fn contains(&self, txid: &str) -> bool {
        self.txids.contains(txid)
    }

    pub fn insert(&mut self, txid: &str) -> Result<bool, ProcessedError> {
        if self.txids.contains(txid) {
            return Ok(false);
        }
        let mut next = self.txids.clone();
        next.insert(txid.to_string());
        self.write(&next)?;
        self.txids = next;
        Ok(true)
    }

    fn write(&self, txids: &BTreeSet<String>) -> Result<(), ProcessedError> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(txids).map_err(|e| io::Error::other(e))?;
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psp-mint-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = scratch("missing");
        let p = Processed::open(&dir.join("processed.json")).unwrap();
        assert!(!p.contains("aa"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn insert_persists_across_reopen() {
        let dir = scratch("persist");
        let path = dir.join("processed.json");
        let mut p = Processed::open(&path).unwrap();
        assert!(p.insert("bb").unwrap());
        assert!(p.insert("aa").unwrap());
        let reopened = Processed::open(&path).unwrap();
        assert!(reopened.contains("aa"));
        assert!(reopened.contains("bb"));
        assert!(!reopened.contains("cc"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn second_insert_reports_already_processed() {
        let dir = scratch("twice");
        let mut p = Processed::open(&dir.join("processed.json")).unwrap();
        assert!(p.insert("aa").unwrap());
        assert!(!p.insert("aa").unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_is_a_sorted_json_array_with_no_temp_left() {
        let dir = scratch("format");
        let path = dir.join("processed.json");
        let mut p = Processed::open(&path).unwrap();
        p.insert("bb").unwrap();
        p.insert("aa").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let parsed: Vec<String> = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, vec!["aa", "bb"]);
        assert!(!dir.join("processed.json.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_file_is_an_error_not_an_empty_set() {
        let dir = scratch("corrupt");
        let path = dir.join("processed.json");
        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            Processed::open(&path),
            Err(ProcessedError::Corrupt(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }
}
