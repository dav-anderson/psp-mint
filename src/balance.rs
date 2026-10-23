//! The SPI wallet's PSP balance, as the mint can know it: a small file that the
//! SPI's tooling (`spi`) writes and the mint reads at each spend.
//!
//! The mint cannot see the SPI's ecash-wallet, but `burn.threshold_psp` is a
//! statement about what that wallet holds. So the SPI publishes a figure,
//! `{"psp": W, "reissued": R, "at": T}`: `W` is the wallet's PSP balance, `R` is
//! the mint's own `reissued` total (the `/audit` figure) read right after `W`,
//! and `T` is the time of the reading in seconds. At a spend the mint works out
//! what the wallet holds now as `W` plus whatever the mint has reissued since
//! `R` (those notes may not be in the wallet yet), and keeps for the SPI only
//! what fits under the threshold (see [`crate::rotation::BurnPolicy::keep`]).
//!
//! **No figure means no burning.** A file that is missing, cannot be read, is not
//! the shape below, or is older than `burn.balance_max_age_secs` (or dated more
//! than a minute ahead of the clock) gives the mint no figure, and then the
//! whole of `pay` is reissued. The mint never destroys revenue on a guess.
//!
//! **The figure must be current.** After the SPI's wallet spends, the figure
//! still says the wallet holds more than it does, so `keep` is too small and the
//! mint burns more than it should. `spi` therefore publishes it after every
//! spend it makes and on a timer; a spend made with `ecash-wallet` directly is
//! not seen until the next publish.
//!
//! The race, and why it is safe: the SPI reads the wallet first and the mint's
//! total second, so a note the mint reissues between the two is in `R` and not
//! in `W`. The mint then counts that note as not held, keeps more for the SPI
//! than the threshold asks and burns less. The error is always towards keeping.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The default file name, in the network's data folder.
pub const FILE: &str = "wallet_balance.json";

/// The default of `burn.balance_max_age_secs`: 24 hours. `spi` publishes the
/// figure on `collect` and `audit` (no timer), and the figure only has to be
/// close; a figure older than this is not trusted and the whole payment is
/// reissued.
pub const DEFAULT_MAX_AGE_SECS: u64 = 86_400;

/// How far ahead of the clock a figure's `at` may be before it is refused as
/// stale (a clock that is wrong in the other direction must not keep it fresh
/// for ever).
pub const FUTURE_SLACK_SECS: u64 = 60;

/// What the SPI publishes. A file with another field, or a missing one, is not
/// a figure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Figure {
    /// The SPI wallet's PSP balance, in PSP units.
    pub psp: u64,
    /// The mint's `reissued` total (PSP units) when the balance was read.
    pub reissued: u64,
    /// When the balance was read, in seconds since the Unix epoch.
    pub at: u64,
}

/// Where the figure is and how old it may be: `burn.balance_file` and
/// `burn.balance_max_age_secs`. A copy value, so that it can ride in
/// [`crate::server::Limits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    /// The file the setting names; `None`: [`FILE`] in the network's data folder.
    pub file: Option<&'static Path>,
    pub max_age_secs: u64,
}

impl Default for Source {
    fn default() -> Self {
        Source {
            file: None,
            max_age_secs: DEFAULT_MAX_AGE_SECS,
        }
    }
}

/// Paths already made `'static`, so that a setting that is read again on every
/// reload costs memory only once for each distinct value ever written.
static PATHS: Mutex<BTreeMap<String, &'static Path>> = Mutex::new(BTreeMap::new());

/// The path a setting names, as a copy value. Empty: no path (the default place).
pub fn intern(text: &str) -> Option<&'static Path> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut paths = PATHS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(path) = paths.get(text) {
        return Some(*path);
    }
    let path: &'static Path = Box::leak(PathBuf::from(text).into_boxed_path());
    paths.insert(text.to_string(), path);
    Some(path)
}

impl Source {
    /// The file to read: the setting, or the default in `dir` (the network's
    /// data folder).
    pub fn path(&self, dir: &Path) -> PathBuf {
        match self.file {
            Some(file) => file.to_path_buf(),
            None => dir.join(FILE),
        }
    }
}

/// Read a figure and check that it is fresh at `now`. `Err` says why there is none.
pub fn read(path: &Path, now: u64, max_age_secs: u64) -> Result<Figure, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let figure: Figure = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a wallet balance figure: {e}", path.display()))?;
    if figure.at > now.saturating_add(FUTURE_SLACK_SECS) {
        return Err(format!(
            "{} is dated {} s ahead of the clock",
            path.display(),
            figure.at - now
        ));
    }
    let age = now.saturating_sub(figure.at);
    if age > max_age_secs {
        return Err(format!(
            "{} is {age} s old, and burn.balance_max_age_secs is {max_age_secs}",
            path.display()
        ));
    }
    Ok(figure)
}

/// What the last spend found, so that a change is logged once and a steady state
/// is not logged at every spend. `None`: nothing logged yet; `Some(None)`: the
/// figure was fine; `Some(Some(why))`: there was none, for that reason.
static LAST: Mutex<Option<Option<String>>> = Mutex::new(None);

/// The figure for a spend at `now`, or `None` (and the reason is logged once
/// per change). `dir` is the network's data folder.
pub fn current(source: &Source, dir: &Path, now: u64) -> Option<Figure> {
    let result = read(&source.path(dir), now, source.max_age_secs);
    let state = result.as_ref().err().cloned();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if (*last).as_ref() != Some(&state) {
        match &state {
            Some(why) => eprintln!(
                "burn: no usable wallet balance figure, so every paid unit is reissued: {why}"
            ),
            None => println!("burn: reading the SPI wallet's balance figure again"),
        }
        *last = Some(state);
    }
    result.ok()
}

/// What the SPI wallet holds now, as far as the mint can tell: the balance
/// published, plus every unit the mint has reissued since the published total.
pub fn held(figure: &Figure, reissued_now: u64) -> u64 {
    figure
        .psp
        .saturating_add(reissued_now.saturating_sub(figure.reissued))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("psp-mint-balance-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, bytes: &[u8]) -> PathBuf {
        let path = dir.join(FILE);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_fresh_figure_is_read_and_each_way_to_have_none_says_why() {
        let dir = scratch("read");
        // A valid figure is the real type, serialized as `spi` writes it.
        let figure = Figure {
            psp: 40,
            reissued: 7,
            at: 1000,
        };
        let path = write(&dir, &serde_json::to_vec(&figure).unwrap());
        assert_eq!(read(&path, 1000, 3600), Ok(figure));
        // Exactly as old as the limit still counts; one second older does not.
        assert!(read(&path, 4600, 3600).is_ok());
        let why = read(&path, 4601, 3600).unwrap_err();
        assert!(why.contains("3601 s old") && why.contains("3600"), "{why}");
        // Dated ahead of the clock by more than the slack.
        assert!(read(&path, 940, 3600).is_ok());
        let why = read(&path, 939, 3600).unwrap_err();
        assert!(why.contains("61 s ahead"), "{why}");
        // Not a figure. Each is written by hand because the real type cannot
        // make it.
        for bad in [
            // A field the figure does not have.
            r#"{"psp":40,"reissued":7,"at":1000,"extra":1}"#,
            // `reissued` is missing.
            r#"{"psp":40,"at":1000}"#,
            // Not JSON.
            "forty",
        ] {
            let path = write(&dir, bad.as_bytes());
            let why = read(&path, 1000, 3600).unwrap_err();
            assert!(why.contains("is not a wallet balance figure"), "{why}");
        }
        std::fs::remove_file(dir.join(FILE)).unwrap();
        assert!(
            read(&dir.join(FILE), 1000, 3600)
                .unwrap_err()
                .contains(FILE)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_wallet_holds_the_published_balance_plus_what_was_reissued_since() {
        let figure = Figure {
            psp: 10,
            reissued: 100,
            at: 0,
        };
        assert_eq!(held(&figure, 100), 10);
        assert_eq!(held(&figure, 105), 15);
        // A total below the published one (a wrong file) never lowers the balance.
        assert_eq!(held(&figure, 90), 10);
        assert_eq!(
            held(
                &Figure {
                    psp: u64::MAX,
                    reissued: 0,
                    at: 0
                },
                5
            ),
            u64::MAX
        );
    }

    #[test]
    fn a_path_setting_becomes_one_copy_value_and_empty_is_the_default_place() {
        assert_eq!(intern(""), None);
        assert_eq!(intern("  "), None);
        let a = intern("/srv/share/wallet_balance.json").unwrap();
        let b = intern(" /srv/share/wallet_balance.json ").unwrap();
        assert!(std::ptr::eq(a, b), "the same text is interned once");
        let source = Source {
            file: Some(a),
            max_age_secs: 1,
        };
        assert_eq!(
            source.path(Path::new("/data/regtest")),
            PathBuf::from("/srv/share/wallet_balance.json")
        );
        assert_eq!(
            Source::default().path(Path::new("/data/regtest")),
            PathBuf::from("/data/regtest/wallet_balance.json")
        );
    }
}
