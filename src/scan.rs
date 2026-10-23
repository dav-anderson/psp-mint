use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::rpc::{Rpc, RpcError};

#[derive(Debug, PartialEq)]
pub struct Received {
    pub txid: String,
    pub confirmations: u32,
    pub value_sats: u64,
    pub payload: Option<Vec<u8>>,
}

pub fn op_return_data(script: &[u8]) -> Option<Vec<u8>> {
    let (&first, rest) = script.split_first()?;
    if first != 0x6a {
        return None;
    }
    let (&op, rest) = rest.split_first()?;
    let (len, body) = match op {
        0x01..=0x4b => (op as usize, rest),
        0x4c => {
            let (&n, body) = rest.split_first()?;
            (n as usize, body)
        }
        0x4d => {
            if rest.len() < 2 {
                return None;
            }
            (u16::from_le_bytes([rest[0], rest[1]]) as usize, &rest[2..])
        }
        0x4e => {
            if rest.len() < 4 {
                return None;
            }
            (
                u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize,
                &rest[4..],
            )
        }
        _ => return None,
    };
    if body.len() != len {
        return None;
    }
    Some(body.to_vec())
}

fn to_sats(btc: f64) -> u64 {
    (btc * 100_000_000.0).round() as u64
}

pub fn from_transaction(tx: &Value) -> Option<Received> {
    let txid = tx.get("txid")?.as_str()?.to_string();
    let confirmations = u32::try_from(tx.get("confirmations")?.as_i64()?).unwrap_or(0);
    let value_sats = tx
        .get("details")?
        .as_array()?
        .iter()
        .filter(|d| d.get("category").and_then(Value::as_str) == Some("receive"))
        .filter_map(|d| d.get("amount").and_then(Value::as_f64))
        .map(to_sats)
        .sum();
    let mut data: Vec<Vec<u8>> = tx
        .get("decoded")?
        .get("vout")?
        .as_array()?
        .iter()
        .filter_map(|o| o.get("scriptPubKey")?.get("hex")?.as_str())
        .filter_map(|h| hex::decode(h).ok())
        .filter_map(|script| op_return_data(&script))
        .collect();
    let payload = if data.len() == 1 { data.pop() } else { None };
    Some(Received {
        txid,
        confirmations,
        value_sats,
        payload,
    })
}

/// What the scan needs from bitcoind. [`Rpc`] is the real one; tests use a
/// stand-in.
pub trait Node {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

impl Node for Rpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        Rpc::call(self, method, params)
    }
}

/// The scan checkpoint: the block the next scan starts from. Kept in the
/// network's data folder as `scan_checkpoint.json`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    /// `lastblock` from the last successful `listsinceblock`.
    pub block_hash: String,
}

pub const CHECKPOINT_FILE: &str = "scan_checkpoint.json";

impl Checkpoint {
    /// The saved checkpoint, or `None` before the first successful scan.
    pub fn load(dir: &std::path::Path) -> Result<Option<Checkpoint>, String> {
        let path = dir.join(CHECKPOINT_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| {
                format!(
                    "{} is damaged: {e}. It is never replaced; fix or move it by hand",
                    path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Write it durably (a temporary file, then a rename).
    pub fn save(&self, dir: &std::path::Path) -> Result<(), String> {
        let path = dir.join(CHECKPOINT_FILE);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        };
        write().map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// The payments to look at in one scan, and where the next scan starts.
#[derive(Debug)]
pub struct Since {
    pub received: Vec<Received>,
    /// The checkpoint to save once these have been processed.
    pub next: Checkpoint,
}

/// Every wallet payment since `checkpoint` (all of them on the first scan), by
/// `listsinceblock`, plus `pending`: the payments already on record that are not
/// settled yet, which are looked at again each scan whatever block they are in.
///
/// `target_confirmations` is `settle.min_confirmations`: bitcoind then returns,
/// as the next checkpoint, the block that many deep, so a payment is listed
/// again on every scan until it is that deep, and a reorg inside that window is
/// seen. Transactions bitcoind reports as removed by a reorg are looked at again
/// too. Every payment is read with `gettransaction`, which gives its current
/// confirmations (0 for one that a reorg took out), so nothing is settled on a
/// block that is no longer in the chain. A payment is never settled twice:
/// settlement is keyed by txid.
pub fn since(
    node: &dyn Node,
    checkpoint: Option<&Checkpoint>,
    target_confirmations: u32,
    pending: &[String],
) -> Result<Since, RpcError> {
    let from = checkpoint.map(|c| c.block_hash.clone()).unwrap_or_default();
    let list = node.call(
        "listsinceblock",
        json!([from, target_confirmations.max(1), true, true]),
    )?;
    let mut seen = BTreeSet::new();
    let mut txids = Vec::new();
    let receives = list
        .get("transactions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            list.get("removed")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    for entry in receives {
        if entry.get("category").and_then(Value::as_str) != Some("receive") {
            continue;
        }
        if let Some(txid) = entry.get("txid").and_then(Value::as_str) {
            if seen.insert(txid.to_string()) {
                txids.push(txid.to_string());
            }
        }
    }
    for txid in pending {
        if seen.insert(txid.clone()) {
            txids.push(txid.clone());
        }
    }
    let mut out = Vec::new();
    for txid in txids {
        match node.call("gettransaction", json!([txid, true, true])) {
            Ok(tx) => {
                if let Some(r) = from_transaction(&tx) {
                    out.push(r);
                }
            }
            // -5: no longer a wallet transaction. Nothing to settle.
            Err(RpcError::Fault { code: -5, .. }) => {}
            Err(e) => return Err(e),
        }
    }
    let next = list
        .get("lastblock")
        .and_then(Value::as_str)
        .map(|h| Checkpoint {
            block_hash: h.to_string(),
        })
        .ok_or(RpcError::Fault {
            code: 0,
            message: "listsinceblock gave no lastblock".into(),
        })?;
    Ok(Since {
        received: out,
        next,
    })
}

#[cfg(test)]
mod since_tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A wallet: txid -> (confirmations, block height or none), a tip height,
    /// and block hashes "h<height>".
    struct FakeNode {
        tip: RefCell<u64>,
        txs: RefCell<BTreeMap<String, (i64, Option<u64>)>>,
        removed: RefCell<Vec<String>>,
        calls: RefCell<Vec<(String, Value)>>,
    }

    impl FakeNode {
        fn new(tip: u64) -> Self {
            FakeNode {
                tip: RefCell::new(tip),
                txs: RefCell::new(BTreeMap::new()),
                removed: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
            }
        }
        /// A payment mined at `height` (or in the mempool).
        fn pay(&self, txid: &str, height: Option<u64>) {
            self.txs.borrow_mut().insert(txid.into(), (0, height));
        }
        fn confs(&self, height: Option<u64>) -> i64 {
            match height {
                Some(h) => (*self.tip.borrow() + 1 - h) as i64,
                None => 0,
            }
        }
        fn list_calls(&self) -> Vec<Value> {
            self.calls
                .borrow()
                .iter()
                .filter(|(m, _)| m == "listsinceblock")
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    impl Node for FakeNode {
        fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            self.calls
                .borrow_mut()
                .push((method.into(), params.clone()));
            let tip = *self.tip.borrow();
            match method {
                "listsinceblock" => {
                    let from: u64 = params[0]
                        .as_str()
                        .unwrap()
                        .strip_prefix('h')
                        .map(|h| h.parse().unwrap())
                        .unwrap_or(0);
                    let target = params[1].as_u64().unwrap();
                    let txs: Vec<Value> = self
                        .txs
                        .borrow()
                        .iter()
                        // in a block after `from`, or unconfirmed
                        .filter(|(_, (_, h))| h.is_none_or(|h| h > from))
                        .map(|(id, _)| json!({"txid": id, "category": "receive"}))
                        .collect();
                    let removed: Vec<Value> = self
                        .removed
                        .borrow()
                        .iter()
                        .map(|id| json!({"txid": id, "category": "receive"}))
                        .collect();
                    Ok(
                        json!({"transactions": txs, "removed": removed, "lastblock": format!("h{}", tip + 1 - target)}),
                    )
                }
                "gettransaction" => {
                    let id = params[0].as_str().unwrap();
                    let Some((_, h)) = self.txs.borrow().get(id).cloned() else {
                        return Err(RpcError::Fault {
                            code: -5,
                            message: "Invalid or non-wallet transaction id".into(),
                        });
                    };
                    Ok(json!({
                        "txid": id,
                        "confirmations": self.confs(h),
                        "details": [{"category": "receive", "amount": 0.001}],
                        "decoded": {"vout": []},
                    }))
                }
                other => panic!("unexpected {other}"),
            }
        }
    }

    fn ids(s: &Since) -> Vec<String> {
        s.received.iter().map(|r| r.txid.clone()).collect()
    }

    #[test]
    fn a_burst_larger_than_the_old_window_is_read_in_full() {
        let node = FakeNode::new(100);
        for i in 0..2500 {
            node.pay(&format!("t{i:05}"), Some(50));
        }
        let s = since(&node, None, 6, &[]).unwrap();
        assert_eq!(s.received.len(), 2500);
        assert_eq!(
            s.next,
            Checkpoint {
                block_hash: "h95".into()
            }
        );
        // starting from the beginning: no blockhash
        assert_eq!(node.list_calls()[0][0], "");
    }

    #[test]
    fn each_block_is_read_once_after_it_is_deep_enough_and_the_checkpoint_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("psp-mint-ckpt-{}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = FakeNode::new(100);
        node.pay("old", Some(10));
        let first = since(&node, Checkpoint::load(&dir).unwrap().as_ref(), 6, &[]).unwrap();
        assert_eq!(ids(&first), vec!["old"]);
        first.next.save(&dir).unwrap();
        // restart: a fresh process reads the checkpoint and does not see "old" again
        node.pay("new", Some(101));
        *node.tip.borrow_mut() = 101;
        let saved = Checkpoint::load(&dir).unwrap().unwrap();
        assert_eq!(saved.block_hash, "h95");
        let second = since(&node, Some(&saved), 6, &[]).unwrap();
        assert_eq!(ids(&second), vec!["new"]);
        assert_eq!(node.list_calls()[1][0], "h95");
        assert_eq!(second.next.block_hash, "h96");
        // a damaged checkpoint is an error, not a silent rescan
        std::fs::write(dir.join(CHECKPOINT_FILE), "nope").unwrap();
        let e = Checkpoint::load(&dir).unwrap_err();
        assert!(
            e.starts_with(&format!(
                "{} is damaged:",
                dir.join(CHECKPOINT_FILE).display()
            )) && e.ends_with("It is never replaced; fix or move it by hand"),
            "{e}"
        );
        // and it is left as it was
        assert_eq!(
            std::fs::read_to_string(dir.join(CHECKPOINT_FILE)).unwrap(),
            "nope"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsettled_payments_and_reorged_ones_are_looked_at_again() {
        let node = FakeNode::new(100);
        node.pay("a", Some(100));
        let s = since(&node, None, 6, &[]).unwrap();
        assert_eq!(s.received[0].confirmations, 1);
        // a reorg takes "a" out of its block: bitcoind lists it as removed, and
        // it is back to 0 confirmations
        node.txs.borrow_mut().insert("a".into(), (0, None));
        node.removed.borrow_mut().push("a".into());
        let s2 = since(
            &node,
            Some(&Checkpoint {
                block_hash: "h100".into(),
            }),
            6,
            &[],
        )
        .unwrap();
        assert_eq!(ids(&s2), vec!["a"]);
        assert_eq!(s2.received[0].confirmations, 0);
        // a pending payment far behind the checkpoint is still looked at
        node.removed.borrow_mut().clear();
        node.pay("p", Some(20));
        let s3 = since(
            &node,
            Some(&Checkpoint {
                block_hash: "h90".into(),
            }),
            6,
            &["p".to_string(), "gone".to_string()],
        )
        .unwrap();
        assert!(ids(&s3).contains(&"p".to_string()));
        assert!(
            !ids(&s3).contains(&"gone".to_string()),
            "a txid the wallet no longer has is skipped"
        );
        assert_eq!(
            s3.received
                .iter()
                .find(|r| r.txid == "p")
                .unwrap()
                .confirmations,
            81
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload;

    const PAYLOAD_HEX: &str = "210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";
    const CHAIN_SCRIPT_HEX: &str = "6a4c75210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";

    fn tx(confirmations: i64, vout_hexes: &[&str]) -> Value {
        let vout: Vec<Value> = vout_hexes
            .iter()
            .enumerate()
            .map(|(n, h)| json!({ "n": n, "scriptPubKey": { "hex": h } }))
            .collect();
        json!({
            "txid": "ab",
            "confirmations": confirmations,
            "details": [{ "category": "receive", "amount": 0.001 }],
            "decoded": { "vout": vout },
        })
    }

    #[test]
    fn extracts_payload_from_real_chain_script() {
        let script = hex::decode(CHAIN_SCRIPT_HEX).unwrap();
        let data = op_return_data(&script).expect("data push");
        assert_eq!(hex::encode(&data), PAYLOAD_HEX);
        assert!(payload::decode(&data).is_ok());
    }
    #[test]
    fn op_return_data_is_read_and_malformed_scripts_are_refused() {
        // a direct push
        assert_eq!(
            op_return_data(&[0x6a, 0x02, 0xaa, 0xbb]),
            Some(vec![0xaa, 0xbb])
        );
        // OP_PUSHDATA2 and OP_PUSHDATA4
        let mut s = vec![0x6a, 0x4d, 0x02, 0x00, 1, 2];
        assert_eq!(op_return_data(&s), Some(vec![1, 2]));
        s = vec![0x6a, 0x4e, 0x01, 0x00, 0x00, 0x00, 9];
        assert_eq!(op_return_data(&s), Some(vec![9]));
        // malformed scripts are refused
        assert_eq!(op_return_data(&[]), None);
        assert_eq!(op_return_data(&[0x6a]), None);
        assert_eq!(op_return_data(&[0x6a, 0x00]), None);
        assert_eq!(op_return_data(&[0x51, 0x01, 0xaa]), None);
        assert_eq!(op_return_data(&[0x6a, 0x02, 0xaa]), None);
        assert_eq!(op_return_data(&[0x6a, 0x01, 0xaa, 0xbb]), None);
        assert_eq!(op_return_data(&[0x6a, 0x4c]), None);
        assert_eq!(op_return_data(&[0x6a, 0x4d, 0x01]), None);
        assert_eq!(op_return_data(&[0x6a, 0x4e, 0x01, 0x00, 0x00]), None);
    }

    #[test]
    fn transaction_with_one_payload() {
        let r = from_transaction(&tx(6, &["0014aabb", CHAIN_SCRIPT_HEX])).expect("parsed");
        assert_eq!(r.txid, "ab");
        assert_eq!(r.confirmations, 6);
        assert_eq!(r.value_sats, 100_000);
        assert_eq!(hex::encode(r.payload.expect("payload")), PAYLOAD_HEX);
    }

    #[test]
    fn transaction_without_payload() {
        let r = from_transaction(&tx(0, &["0014aabb"])).expect("parsed");
        assert_eq!(r.confirmations, 0);
        assert_eq!(r.payload, None);
    }

    #[test]
    fn two_payloads_is_ambiguous() {
        let r = from_transaction(&tx(6, &[CHAIN_SCRIPT_HEX, CHAIN_SCRIPT_HEX])).expect("parsed");
        assert_eq!(r.payload, None);
    }

    #[test]
    fn negative_confirmations_count_as_zero() {
        let r = from_transaction(&tx(-1, &["0014aabb"])).expect("parsed");
        assert_eq!(r.confirmations, 0);
    }

    #[test]
    fn sums_every_receive_detail() {
        let mut t = tx(6, &["0014aabb"]);
        t["details"] = json!([
            { "category": "receive", "amount": 0.001 },
            { "category": "receive", "amount": 0.00000007 },
            { "category": "send", "amount": -0.5 },
        ]);
        assert_eq!(from_transaction(&t).expect("parsed").value_sats, 100_007);
    }

    #[test]
    fn missing_fields_give_none() {
        assert_eq!(from_transaction(&json!({ "txid": "ab" })), None);
    }
}
