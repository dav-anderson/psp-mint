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

pub fn received(rpc: &Rpc, count: u32) -> Result<Vec<Received>, RpcError> {
    let list = rpc.call("listtransactions", json!(["*", count]))?;
    let mut seen = BTreeSet::new();
    let mut txids = Vec::new();
    for entry in list.as_array().into_iter().flatten() {
        if entry.get("category").and_then(Value::as_str) != Some("receive") {
            continue;
        }
        if let Some(txid) = entry.get("txid").and_then(Value::as_str) {
            if seen.insert(txid.to_string()) {
                txids.push(txid.to_string());
            }
        }
    }
    let mut out = Vec::new();
    for txid in txids {
        let tx = rpc.call("gettransaction", json!([txid, true, true]))?;
        if let Some(r) = from_transaction(&tx) {
            out.push(r);
        }
    }
    Ok(out)
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
    fn direct_push() {
        assert_eq!(op_return_data(&[0x6a, 0x02, 0xaa, 0xbb]), Some(vec![0xaa, 0xbb]));
    }

    #[test]
    fn pushdata2_and_pushdata4() {
        let mut s = vec![0x6a, 0x4d, 0x02, 0x00, 1, 2];
        assert_eq!(op_return_data(&s), Some(vec![1, 2]));
        s = vec![0x6a, 0x4e, 0x01, 0x00, 0x00, 0x00, 9];
        assert_eq!(op_return_data(&s), Some(vec![9]));
    }

    #[test]
    fn rejects_malformed_scripts() {
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
