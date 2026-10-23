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
    /// on the operating system this runs on (see [`Network::default_cookie_on`]).
    pub fn default_cookie(self, home: &str) -> String {
        self.default_cookie_on(std::env::consts::OS, home)
    }

    /// The default cookie on a given OS (`macos`, `linux`, ...):
    /// `~/Library/Application Support/Bitcoin/` on macOS, `~/.bitcoin/` on Linux
    /// (and anything else), Bitcoin Core's own defaults, then the network's
    /// folder.
    pub fn default_cookie_on(self, os: &str, home: &str) -> String {
        let sub = match self {
            Network::Main => "",
            Network::Regtest => "regtest/",
            Network::Signet => "signet/",
            Network::Testnet => "testnet3/",
        };
        if os == "macos" {
            format!("{home}/Library/Application Support/Bitcoin/{sub}.cookie")
        } else {
            format!("{home}/.bitcoin/{sub}.cookie")
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_network_defaults() {
        assert_eq!(Network::default(), Network::Main);
        let ports: Vec<u16> = ALL.iter().map(|n| n.rpc_port()).collect();
        assert_eq!(ports, vec![8332, 18443, 38332, 18332]);
        assert_eq!(Network::Main.default_wallet(), "psp-main");
        assert_eq!(Network::Regtest.default_wallet(), "psp-regtest");
        // the default cookie on each OS, for every network
        let cookies = [
            (
                Network::Main,
                "/h/Library/Application Support/Bitcoin/.cookie",
                "/h/.bitcoin/.cookie",
            ),
            (
                Network::Regtest,
                "/h/Library/Application Support/Bitcoin/regtest/.cookie",
                "/h/.bitcoin/regtest/.cookie",
            ),
            (
                Network::Signet,
                "/h/Library/Application Support/Bitcoin/signet/.cookie",
                "/h/.bitcoin/signet/.cookie",
            ),
            (
                Network::Testnet,
                "/h/Library/Application Support/Bitcoin/testnet3/.cookie",
                "/h/.bitcoin/testnet3/.cookie",
            ),
        ];
        for (n, macos, linux) in cookies {
            assert_eq!(n.default_cookie_on("macos", "/h"), macos);
            assert_eq!(n.default_cookie_on("linux", "/h"), linux);
            // any other OS gets Bitcoin Core's Linux default
            assert_eq!(n.default_cookie_on("freebsd", "/h"), linux);
            // the default follows the OS this runs on, whichever it is
            assert_eq!(
                n.default_cookie("/h"),
                n.default_cookie_on(std::env::consts::OS, "/h")
            );
        }
        assert_eq!(Network::Testnet.chain(), "test");
        for n in ALL {
            assert_eq!(Network::parse(n.name()), Ok(n));
        }
        assert_eq!(Network::parse("mainnet"), Ok(Network::Main));
        assert_eq!(
            Network::parse("testnet4"),
            Err("`testnet4` is not a network: use main, regtest, signet or testnet".to_string())
        );
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
        let e = check_chain(&json!({"chain": "main"}), Network::Signet).unwrap_err();
        assert!(
            e.contains("bitcoind is on chain `main`")
                && e.contains("set to signet (chain `signet`)")
                && e.contains("Refusing to start"),
            "{e}"
        );
        assert_eq!(
            check_chain(&json!({}), Network::Main),
            Err("getblockchaininfo did not say which chain bitcoind is on".to_string())
        );
    }

    /// A node that answers `getblockchaininfo` with `reply`.
    struct Answers(Result<Value, (i64, &'static str)>);

    impl crate::scan::Node for Answers {
        fn call(&self, method: &str, _params: Value) -> Result<Value, crate::rpc::RpcError> {
            assert_eq!(method, "getblockchaininfo");
            match &self.0 {
                Ok(value) => Ok(value.clone()),
                Err((code, message)) => Err(crate::rpc::RpcError::Fault {
                    code: *code,
                    message: message.to_string(),
                }),
            }
        }
    }

    #[test]
    fn verify_asks_the_node_and_checks_its_chain() {
        let regtest = Answers(Ok(json!({"chain": "regtest"})));
        assert_eq!(verify(&regtest, Network::Regtest), Ok(()));
        // a node on another chain is the chain check's refusal, unchanged
        let e = verify(&regtest, Network::Signet).unwrap_err();
        assert!(
            e.starts_with("bitcoind is on chain `regtest`, but this mint is set to signet"),
            "{e}"
        );
        // a node that cannot be asked is said so, with its own reason
        let down = Answers(Err((-28, "loading block index")));
        assert_eq!(
            verify(&down, Network::Main),
            Err(
                "cannot ask bitcoind which network it is on: rpc error -28: loading block index"
                    .to_string()
            )
        );
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
}
