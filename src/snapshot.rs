//! A copy of the network's data folder, taken while `serve` is running.
//!
//! `serve` takes it on `SIGUSR1`, between two openings of the mint: it stops the
//! scan, the AIR listener and the local API and drops every store (so that
//! sled has written everything and nobody has the files open), calls [`take`],
//! and opens the mint again (`main.rs`). Requests that arrive meanwhile wait:
//! HTTP connections in the listening socket's backlog, AIR requests in their
//! mailbox slots.
//!
//! The copy goes to `<data folder>/snapshots/<UTC time>/`, a folder only the
//! mint's account can open (0700): **it holds the mint's keys.** The file
//! `snapshots/LATEST` names the newest complete folder and is written last, by
//! a rename, so whoever reads it never sees a folder that is still being copied.
//! A copy that fails is removed and `LATEST` is left as it was.
//!
//! A snapshot is the network's folder only (keys, notes, records, the scan
//! checkpoint, the AIR counter, `revenue/`), not the settings file, which is the
//! operator's. To restore, put the folder's contents where the network's folder
//! is, with the mint stopped.

use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// The folder, in the network's data folder, that holds the snapshots. It is
/// not copied into a snapshot.
pub const DIR: &str = "snapshots";
/// The file in [`DIR`] that names the newest complete snapshot.
pub const LATEST: &str = "LATEST";

/// `unix_secs` as `YYYYMMDDTHHMMSSZ` in UTC: sortable, and a valid folder name
/// on every system.
pub fn utc_stamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rest = unix_secs % 86_400;
    let (hour, minute, second) = (rest / 3_600, rest % 3_600 / 60, rest % 60);
    // Days since 1970-01-01 to a civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

fn make_dir(path: &Path) -> Result<(), String> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Copy the files and folders under `from` into `to`, which exists. At the top
/// level the snapshots folder is left out. Every file is on disk when this
/// returns. A link or anything that is not a file or a folder is an error: the
/// mint's folder has none, and a copy that silently skipped one would not be
/// the folder.
fn copy_tree(from: &Path, to: &Path, top: bool) -> Result<(), String> {
    let entries = fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let name = entry.file_name();
        if top && name == DIR {
            continue;
        }
        let source = entry.path();
        let target = to.join(&name);
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", source.display()))?;
        if kind.is_dir() {
            make_dir(&target)?;
            copy_tree(&source, &target, false)?;
            sync_dir(&target)?;
        } else if kind.is_file() {
            fs::copy(&source, &target)
                .map_err(|e| format!("copy {} to {}: {e}", source.display(), target.display()))?;
            File::open(&target)
                .and_then(|file| file.sync_all())
                .map_err(|e| format!("{}: {e}", target.display()))?;
        } else {
            return Err(format!(
                "{} is not a file or a folder, so the snapshot does not copy it",
                source.display()
            ));
        }
    }
    Ok(())
}

fn write_latest(root: &Path, name: &str) -> Result<(), String> {
    let temp = root.join(format!("{LATEST}.tmp"));
    let written = fs::write(&temp, format!("{name}\n"))
        .and_then(|()| File::open(&temp).and_then(|file| file.sync_all()))
        .and_then(|()| fs::rename(&temp, root.join(LATEST)));
    written.map_err(|e| format!("{}: {e}", root.join(LATEST).display()))?;
    sync_dir(root)
}

/// Copy `data_dir` to `snapshots/<stamp of unix_secs>/` under it and name the
/// copy in `snapshots/LATEST`. Returns the new folder. The stores must not be
/// open: the caller has dropped them. On any failure the partial folder is
/// removed and `LATEST` is as it was.
pub fn take(data_dir: &Path, unix_secs: u64) -> Result<PathBuf, String> {
    let root = data_dir.join(DIR);
    make_dir(&root)?;
    let name = utc_stamp(unix_secs);
    let dest = root.join(&name);
    if dest.exists() {
        return Err(format!(
            "{} exists already (two snapshots in one second)",
            dest.display()
        ));
    }
    let copied = make_dir(&dest)
        .and_then(|()| copy_tree(data_dir, &dest, true))
        .and_then(|()| sync_dir(&dest))
        .and_then(|()| sync_dir(&root));
    if let Err(e) = copied {
        if let Err(removed) = fs::remove_dir_all(&dest) {
            return Err(format!(
                "{e} (and the partial copy could not be removed: {removed})"
            ));
        }
        return Err(e);
    }
    write_latest(&root, &name)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("psp-mint-snapshot-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A folder shaped like a network's: key files, a store folder with nested
    /// files, the records and an older snapshot.
    fn network_folder(dir: &Path) {
        fs::write(dir.join("mint_did_key.json"), b"{\"key\":\"did\"}").unwrap();
        fs::write(dir.join("settlements.json"), b"{\"entries\":{}}").unwrap();
        fs::create_dir_all(dir.join("gen-0").join("db")).unwrap();
        fs::write(dir.join("gen-0").join("mint_keys.json"), b"keys").unwrap();
        fs::write(dir.join("gen-0").join("db").join("blob.0"), [7u8; 300]).unwrap();
        fs::create_dir_all(dir.join(DIR).join("20200101T000000Z")).unwrap();
        fs::write(dir.join(DIR).join("20200101T000000Z").join("old"), b"older").unwrap();
        fs::write(dir.join(DIR).join(LATEST), b"20200101T000000Z\n").unwrap();
    }

    #[test]
    fn the_stamp_is_the_utc_time_in_a_sortable_form() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
        assert_eq!(utc_stamp(1_700_000_000), "20231114T221320Z");
        assert_eq!(utc_stamp(4_102_444_800), "21000101T000000Z");
        assert!(utc_stamp(1_700_000_000) < utc_stamp(1_700_000_001));
    }

    #[test]
    fn a_snapshot_is_a_private_copy_of_the_folder_without_the_snapshots() {
        let dir = scratch("copy");
        network_folder(&dir);
        let dest = take(&dir, 1_700_000_000).unwrap();
        assert_eq!(dest, dir.join(DIR).join("20231114T221320Z"));
        assert_eq!(
            fs::read(dest.join("mint_did_key.json")).unwrap(),
            b"{\"key\":\"did\"}"
        );
        assert_eq!(
            fs::read(dest.join("settlements.json")).unwrap(),
            b"{\"entries\":{}}"
        );
        assert_eq!(
            fs::read(dest.join("gen-0").join("mint_keys.json")).unwrap(),
            b"keys"
        );
        assert_eq!(
            fs::read(dest.join("gen-0").join("db").join("blob.0")).unwrap(),
            vec![7u8; 300]
        );
        // The earlier snapshots are not copied into this one.
        assert!(!dest.join(DIR).exists());
        // Only the mint's account can open it.
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dest.join("gen-0"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        // LATEST names it, and the old one is still there.
        assert_eq!(
            fs::read_to_string(dir.join(DIR).join(LATEST)).unwrap(),
            "20231114T221320Z\n"
        );
        assert!(dir.join(DIR).join("20200101T000000Z").join("old").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_snapshot_in_the_same_second_is_refused_and_changes_nothing() {
        let dir = scratch("same-second");
        network_folder(&dir);
        take(&dir, 1_700_000_000).unwrap();
        let again = take(&dir, 1_700_000_000).unwrap_err();
        assert!(
            again.ends_with("exists already (two snapshots in one second)"),
            "{again}"
        );
        assert_eq!(
            fs::read_to_string(dir.join(DIR).join(LATEST)).unwrap(),
            "20231114T221320Z\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_copy_that_fails_is_removed_and_latest_still_names_the_last_good_one() {
        let dir = scratch("fails");
        network_folder(&dir);
        // A link is not something the mint's folder holds; the copy refuses it.
        std::os::unix::fs::symlink("settlements.json", dir.join("link")).unwrap();
        let why = take(&dir, 1_700_000_000).unwrap_err();
        assert!(
            why.ends_with("link is not a file or a folder, so the snapshot does not copy it"),
            "{why}"
        );
        assert!(!dir.join(DIR).join("20231114T221320Z").exists());
        assert_eq!(
            fs::read_to_string(dir.join(DIR).join(LATEST)).unwrap(),
            "20200101T000000Z\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
