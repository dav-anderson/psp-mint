//! Which Bitcoin network the mint runs on, and what follows from it.
//!
//! The default is mainnet. `--regtest`, `--signet` or `--testnet` on the
//! command line (strongest), `PSP_NETWORK`, or `bitcoin.network` in the
//! settings file choose another. The network picks the defaults for the RPC
//! port, the cookie file and the wallet name (explicit settings still win), and
//! the mint's data folder, `<data dir>/<network>/`, so keys, notes and records
//! never cross networks. At startup the mint asks bitcoind which chain it is on
//! (`getblockchaininfo`) and refuses to start if it is not the selected one.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// A Bitcoin network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Network {
    #[default]
    Main,
    Regtest,
    Signet,
    Testnet,
}

pub const ALL: [Network; 4] = [
    Network::Main,
    Network::Regtest,
    Network::Signet,
    Network::Testnet,
];

impl Network {
    /// The name used in settings, folders, wallet names and replies.
    pub fn name(self) -> &'static str {
        match self {
            Network::Main => "main",
            Network::Regtest => "regtest",
            Network::Signet => "signet",
            Network::Testnet => "testnet",
        }
    }

    /// Read a setting value: `main` (or `mainnet`), `regtest`, `signet`,
    /// `testnet`.
    pub fn parse(text: &str) -> Result<Network, String> {
        match text.trim() {
            "main" | "mainnet" => Ok(Network::Main),
            "regtest" => Ok(Network::Regtest),
            "signet" => Ok(Network::Signet),
            "testnet" => Ok(Network::Testnet),
            other => Err(format!(
                "`{other}` is not a network: use main, regtest, signet or testnet"
            )),
        }
    }

    /// What bitcoind's `getblockchaininfo` reports as `chain` on this network.
    pub fn chain(self) -> &'static str {
        match self {
            Network::Main => "main",
            Network::Regtest => "regtest",
            Network::Signet => "signet",
            Network::Testnet => "test",
        }
    }

    /// bitcoind's default RPC port on this network.
    pub fn rpc_port(self) -> u16 {
        match self {
            Network::Main => 8332,
            Network::Regtest => 18443,
            Network::Signet => 38332,
            Network::Testnet => 18332,
        }
    }

    pub fn default_rpc_url(self) -> String {
        format!("http://127.0.0.1:{}", self.rpc_port())
    }

    /// bitcoind's cookie file on this network, under its default data directory
    /// (macOS `~/Library/Application Support/Bitcoin`, Linux `~/.bitcoin`).
    pub fn default_cookie(self, home: &str) -> String {
        let sub = match self {
            Network::Main => "",
            Network::Regtest => "regtest/",
            Network::Signet => "signet/",
            Network::Testnet => "testnet3/",
        };
        Self::cookie_for_os(self, std::env::consts::OS, home, sub)
    }

    /// The cookie path on `os`: `~/Library/Application Support/Bitcoin/` on
    /// macOS, `~/.bitcoin/` on Linux (and anything else), Bitcoin Core's own
    /// defaults.
    fn cookie_for_os(self, os: &str, home: &str, sub: &str) -> String {
        if os == "macos" {
            format!("{home}/Library/Application Support/Bitcoin/{sub}.cookie")
        } else {
            format!("{home}/.bitcoin/{sub}.cookie")
        }
    }

    /// The default cookie on a given OS (`macos`, `linux`, ...).
    pub fn default_cookie_on(self, os: &str, home: &str) -> String {
        let sub = match self {
            Network::Main => "",
            Network::Regtest => "regtest/",
            Network::Signet => "signet/",
            Network::Testnet => "testnet3/",
        };
        self.cookie_for_os(os, home, sub)
    }

    /// The default wallet name: `psp-main`, `psp-regtest`, ...
    pub fn default_wallet(self) -> String {
        format!("psp-{}", self.name())
    }

    /// The mint's data folder for this network under the base data directory.
    pub fn data_dir(self, base: &Path) -> PathBuf {
        base.join(self.name())
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Take `--regtest`, `--signet`, `--testnet` or `--mainnet` out of the
/// arguments. More than one is an error.
pub fn take_flag(args: &mut Vec<String>) -> Result<Option<Network>, String> {
    let mut found = None;
    let mut rest = Vec::new();
    for a in args.drain(..) {
        let net = match a.as_str() {
            "--mainnet" | "--main" => Some(Network::Main),
            "--regtest" => Some(Network::Regtest),
            "--signet" => Some(Network::Signet),
            "--testnet" => Some(Network::Testnet),
            _ => None,
        };
        match (net, found) {
            (Some(n), None) => found = Some(n),
            (Some(n), Some(m)) if n != m => {
                return Err(format!(
                    "both --{m} and --{n} were given: choose one network"
                ));
            }
            (Some(_), Some(_)) => {}
            (None, _) => rest.push(a),
        }
    }
    *args = rest;
    Ok(found)
}

/// Check bitcoind's `getblockchaininfo` reply against the selected network.
pub fn check_chain(info: &Value, network: Network) -> Result<(), String> {
    let chain = info
        .get("chain")
        .and_then(Value::as_str)
        .ok_or_else(|| "getblockchaininfo did not say which chain bitcoind is on".to_string())?;
    if chain == network.chain() {
        Ok(())
    } else {
        Err(format!(
            "bitcoind is on chain `{chain}`, but this mint is set to {network} (chain `{}`). Refusing to start. Choose the network with --regtest, --signet, --testnet (default mainnet), PSP_NETWORK or bitcoin.network",
            network.chain()
        ))
    }
}

/// Ask bitcoind which chain it is on and check it.
pub fn verify(node: &dyn crate::scan::Node, network: Network) -> Result<(), String> {
    let info = node
        .call("getblockchaininfo", json!([]))
        .map_err(|e| format!("cannot ask bitcoind which network it is on: {e}"))?;
    check_chain(&info, network)
}

/// The files and folders of a data directory from before networks had their own
/// folders.
pub const LEGACY_ITEMS: [&str; 7] = [
    "mint_keys.json",
    "mint_did_key.json",
    "db",
    "settlements.json",
    "melt_memos.json",
    "processed_txids.json",
    "scan_checkpoint.json",
];

/// The legacy items present directly in `base`.
pub fn legacy_items(base: &Path) -> Vec<&'static str> {
    LEGACY_ITEMS
        .iter()
        .copied()
        .filter(|name| fs::symlink_metadata(base.join(name)).is_ok())
        .collect()
}

/// Move a pre-network data directory's state into `<base>/regtest/`, where it
/// came from (the mint ran only on regtest before). Each item is renamed, never
/// copied and never deleted. Refused, with nothing moved, if `regtest/` already
/// holds any of them. Returns what was moved.
pub fn migrate_to_regtest(base: &Path) -> Result<Vec<&'static str>, String> {
    let items = legacy_items(base);
    if items.is_empty() {
        return Err(format!(
            "nothing to migrate: {} has no mint files of its own",
            base.display()
        ));
    }
    let target = Network::Regtest.data_dir(base);
    let clash: Vec<&str> = items
        .iter()
        .copied()
        .filter(|name| fs::symlink_metadata(target.join(name)).is_ok())
        .collect();
    if !clash.is_empty() {
        return Err(format!(
            "{} already has {}: nothing was moved. Sort it out by hand; nothing is ever deleted",
            target.display(),
            clash.join(", ")
        ));
    }
    fs::create_dir_all(&target).map_err(|e| format!("cannot make {}: {e}", target.display()))?;
    let mut moved = Vec::new();
    for name in items {
        fs::rename(base.join(name), target.join(name)).map_err(|e| {
            format!(
                "moved {} so far; could not move {name}: {e}. Run migrate again to move the rest",
                if moved.is_empty() {
                    "nothing".to_string()
                } else {
                    moved.join(", ")
                }
            )
        })?;
        moved.push(name);
    }
    Ok(moved)
}

/// Refuse to run while state from before networks is still at the top of the
/// data directory, so it is never silently left behind.
pub fn refuse_legacy(base: &Path) -> Result<(), String> {
    let items = legacy_items(base);
    if items.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} still holds mint files from before networks had their own folders ({}). They are from regtest. Run `psp-mint migrate` once to move them into {}/ (nothing is deleted), then start again",
            base.display(),
            items.join(", "),
            Network::Regtest.data_dir(base).display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psp-mint-net-{}-{name}", {
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
    fn per_network_defaults() {
        assert_eq!(Network::default(), Network::Main);
        let ports: Vec<u16> = ALL.iter().map(|n| n.rpc_port()).collect();
        assert_eq!(ports, vec![8332, 18443, 38332, 18332]);
        assert_eq!(Network::Main.default_wallet(), "psp-main");
        assert_eq!(Network::Regtest.default_wallet(), "psp-regtest");
        assert_eq!(
            Network::Main.default_cookie_on("macos", "/h"),
            "/h/Library/Application Support/Bitcoin/.cookie"
        );
        assert_eq!(
            Network::Main.default_cookie_on("linux", "/h"),
            "/h/.bitcoin/.cookie"
        );
        assert_eq!(
            Network::Regtest.default_cookie_on("linux", "/h"),
            "/h/.bitcoin/regtest/.cookie"
        );
        assert_eq!(
            Network::Testnet.default_cookie_on("linux", "/h"),
            "/h/.bitcoin/testnet3/.cookie"
        );
        assert_eq!(
            Network::Signet.default_cookie_on("macos", "/h"),
            "/h/Library/Application Support/Bitcoin/signet/.cookie"
        );
        assert_eq!(
            Network::Main.default_cookie("/h"),
            Network::Main.default_cookie_on(std::env::consts::OS, "/h")
        );
        assert_eq!(
            Network::Testnet.default_cookie("/h"),
            "/h/Library/Application Support/Bitcoin/testnet3/.cookie"
        );
        assert_eq!(Network::Testnet.chain(), "test");
        for n in ALL {
            assert_eq!(Network::parse(n.name()), Ok(n));
        }
        assert_eq!(Network::parse("mainnet"), Ok(Network::Main));
        assert!(Network::parse("testnet4").is_err());
    }

    #[test]
    fn flags_are_taken_out_and_two_different_ones_are_refused() {
        let mut a: Vec<String> = ["serve", "--regtest"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(take_flag(&mut a), Ok(Some(Network::Regtest)));
        assert_eq!(a, vec!["serve".to_string()]);
        let mut b: Vec<String> = vec!["config".into()];
        assert_eq!(take_flag(&mut b), Ok(None));
        let mut c: Vec<String> = vec!["--regtest".into(), "--signet".into()];
        assert!(take_flag(&mut c).is_err());
    }

    #[test]
    fn a_node_on_another_chain_is_refused() {
        assert_eq!(
            check_chain(&json!({"chain": "regtest"}), Network::Regtest),
            Ok(())
        );
        assert_eq!(
            check_chain(&json!({"chain": "test"}), Network::Testnet),
            Ok(())
        );
        let e = check_chain(&json!({"chain": "regtest"}), Network::Main).unwrap_err();
        assert!(
            e.contains("Refusing to start") && e.contains("`regtest`"),
            "{e}"
        );
        assert!(check_chain(&json!({"chain": "main"}), Network::Signet).is_err());
        assert!(check_chain(&json!({}), Network::Main).is_err());
    }

    #[test]
    fn each_network_has_its_own_folder() {
        let base = Path::new("/d");
        let dirs: Vec<PathBuf> = ALL.iter().map(|n| n.data_dir(base)).collect();
        assert_eq!(dirs[0], PathBuf::from("/d/main"));
        assert_eq!(dirs[1], PathBuf::from("/d/regtest"));
        let unique: std::collections::BTreeSet<_> = dirs.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn migration_moves_the_old_state_into_regtest_and_never_deletes() {
        let base = scratch("migrate");
        fs::write(base.join("mint_keys.json"), "keys").unwrap();
        fs::write(base.join("settlements.json"), "{}").unwrap();
        fs::create_dir_all(base.join("db")).unwrap();
        fs::write(base.join("db").join("x"), "data").unwrap();
        fs::write(base.join("psp-mint.toml"), "").unwrap();
        assert!(
            refuse_legacy(&base)
                .unwrap_err()
                .contains("psp-mint migrate")
        );
        let moved = migrate_to_regtest(&base).unwrap();
        assert_eq!(moved, vec!["mint_keys.json", "db", "settlements.json"]);
        let reg = base.join("regtest");
        assert_eq!(
            fs::read_to_string(reg.join("mint_keys.json")).unwrap(),
            "keys"
        );
        assert_eq!(
            fs::read_to_string(reg.join("db").join("x")).unwrap(),
            "data"
        );
        assert!(
            base.join("psp-mint.toml").exists(),
            "the settings file stays where it is"
        );
        assert_eq!(refuse_legacy(&base), Ok(()));
        assert!(
            migrate_to_regtest(&base).is_err(),
            "a second run has nothing to do"
        );
        // a clash moves nothing
        fs::write(base.join("mint_keys.json"), "other").unwrap();
        fs::write(base.join("melt_memos.json"), "{}").unwrap();
        let e = migrate_to_regtest(&base).unwrap_err();
        assert!(
            e.contains("mint_keys.json") && e.contains("nothing was moved"),
            "{e}"
        );
        assert!(base.join("melt_memos.json").exists());
        assert_eq!(
            fs::read_to_string(reg.join("mint_keys.json")).unwrap(),
            "keys"
        );
        fs::remove_dir_all(&base).unwrap();
    }
}
