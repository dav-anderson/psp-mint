use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::Path;

use mint_core::MintLogic;
use mint_core::keygen;
use mint_core::store::Store;
use mint_types::{Amount, DEFAULT_DENOMINATION_BASE, MintConfig, gen_denominations};

const KEYS_FILENAME: &str = "mint_keys.json";
const DB_DIRNAME: &str = "db";

pub fn open(data_dir: &Path, max_denomination: Amount) -> Result<MintLogic, Box<dyn Error>> {
    fs::create_dir_all(data_dir)?;
    let cfg = load_or_generate(&data_dir.join(KEYS_FILENAME), max_denomination)?;
    let store = Store::open(&data_dir.join(DB_DIRNAME))?;
    Ok(MintLogic::new(cfg, store))
}

fn load_or_generate(path: &Path, max_denomination: Amount) -> Result<MintConfig, Box<dyn Error>> {
    if path.exists() {
        let bytes = fs::read(path)?;
        return Ok(serde_json::from_slice(&bytes)?);
    }
    let cfg = keygen::generate(&gen_denominations(
        DEFAULT_DENOMINATION_BASE,
        max_denomination,
    ));
    write_restricted(path, &cfg)?;
    Ok(cfg)
}

fn write_restricted(path: &Path, cfg: &MintConfig) -> Result<(), Box<dyn Error>> {
    let bytes = serde_json::to_vec_pretty(cfg)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psp-mint-{}-{name}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn keys_are_generated_once_and_reloaded() {
        let dir = scratch("keys");
        let first = open(&dir, Amount::from_units(1024)).unwrap();
        let keys = first.public_keys().clone();
        assert_eq!(keys.len(), 11);
        drop(first);
        let second = open(&dir, Amount::from_units(1024)).unwrap();
        assert_eq!(&keys, second.public_keys());
        drop(second);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn keyfile_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        drop(open(&dir, Amount::from_units(16)).unwrap());
        let mode = fs::metadata(dir.join(KEYS_FILENAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
}
