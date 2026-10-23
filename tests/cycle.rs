//! One scan pass as `psp-mint` runs it: the wallet is scanned (`service::cycle`),
//! what is owed is delivered over AIR (`service::deliver`) and the windows that
//! closed are forfeited (`service::forfeit`). The wallet is a stand-in for
//! bitcoind (`scan::Node`) and the price is a closure; the mint, its records and
//! the AIR server (the shared local one, see `common`) are the real ones, and the
//! payer's side is the code the wallet runs.

mod common;

use mint_types::{Amount, BlindNonce, MintOutput};
use psp_mint::delivery::Deliverer;
use psp_mint::rpc::RpcError;
use psp_mint::scan::{CHECKPOINT_FILE, Checkpoint, Node};
use psp_mint::server::Limits;
use psp_mint::service::{self, Pass};
use psp_mint::settle::{Policy, ReconcileTerms, Terms};
use rand::rngs::OsRng;
use secp256k1::SECP256K1;
use serde_json::{Value, json};
use sp_wire::ServiceDid;
use sp_wire::air_request::addr;
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::delivery::{self as wire, Outcome};
use sp_wire::payload::{Payload, encode};
use sp_wire::seal::{PublicKey, SecretKey};

/// $10,000 per BTC: 1,000 sats is one PSP unit.
const BTC_10K: u64 = 1_000_000;
const NOW: u64 = 1_800_000_000;

/// A transaction the wallet received: its txid, the sats and the OP_RETURN script.
struct Tx {
    txid: &'static str,
    sats: u64,
    script: Vec<u8>,
}

/// bitcoind's wallet as the scan sees it: `listsinceblock` lists every receive,
/// `gettransaction` describes one. With `down` set every call fails.
struct Wallet {
    txs: Vec<Tx>,
    down: bool,
}

impl Node for Wallet {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        if self.down {
            return Err(RpcError::Fault {
                code: -28,
                message: "loading block index".to_string(),
            });
        }
        match method {
            "listsinceblock" => Ok(json!({
                "transactions": self.txs.iter()
                    .map(|t| json!({"txid": t.txid, "category": "receive"}))
                    .collect::<Vec<_>>(),
                "removed": [],
                "lastblock": "block-9",
            })),
            "gettransaction" => {
                let id = params[0].as_str().unwrap();
                let Some(tx) = self.txs.iter().find(|t| t.txid == id) else {
                    return Err(RpcError::Fault {
                        code: -5,
                        message: "Invalid or non-wallet transaction id".to_string(),
                    });
                };
                Ok(json!({
                    "txid": id,
                    "confirmations": 6,
                    "details": [{"category": "receive", "amount": tx.sats as f64 / 100_000_000.0}],
                    "decoded": {"vout": [{"scriptPubKey": {"hex": hex::encode(&tx.script)}}]},
                }))
            }
            other => panic!("unexpected call {other}"),
        }
    }
}

/// An OP_RETURN script pushing `data` (at most 255 bytes).
fn op_return(data: &[u8]) -> Vec<u8> {
    let mut script = vec![0x6a, 0x4c, u8::try_from(data.len()).unwrap()];
    script.extend_from_slice(data);
    script
}

/// The payer's one-off key for a purchase, and the DID it stands for.
struct Payer {
    key: SecretKey,
    did: ServiceDid,
}

fn payer() -> Payer {
    let key = SecretKey::new();
    let did = addr::key_to_did(&key.public_key());
    Payer { key, did }
}

/// The payload a wallet builds for `units`, one output each, with the bytes the
/// wallet sealed to the mint.
fn purchase(payer: &Payer, units: &[u64]) -> Vec<u8> {
    encode(&Payload {
        did: *payer.did.as_bytes(),
        outputs: units
            .iter()
            .map(|&u| MintOutput {
                amount: Amount::from_units(u),
                blind_nonce: BlindNonce(secp256k1::PublicKey::from_secret_key(
                    SECP256K1,
                    &secp256k1::SecretKey::new(&mut OsRng),
                )),
            })
            .collect(),
        keyset: None,
    })
    .unwrap()
}

/// What the payer does: read the cell it computes, once.
fn read(server: &ServerRef, address: &PublicKey) -> Option<Vec<u8>> {
    block_on_new_runtime(|| async {
        let c = client::connect(server).await.unwrap();
        wire::read_record(&c, address, 10, 0).await.unwrap()
    })
    .unwrap()
}

/// Fees on, so that the passes below run the paths that charge them.
fn policy() -> Policy {
    Policy {
        terms: Terms::default(),
        fees: ReconcileTerms {
            fee_units: 2,
            write_fee_units: 1,
            read_fee_units: 1,
            ..ReconcileTerms::default()
        },
    }
}

#[test]
fn a_pass_settles_delivers_and_forfeits_and_saves_the_checkpoint() {
    let server = common::server();
    let m = common::PspMint::open("cycle", NOW, Limits::default());
    let did = SecretKey::new();
    let psp = addr::key_to_did(&did.public_key());
    let policy = policy();

    // tx1: 6 units paid, less the first read (1), the 5 asked for. tx2: 12 units
    // paid, less the first read, against 16 asked for: held. tx3: data nobody
    // sealed to this mint.
    let (first, second) = (payer(), payer());
    let exact = purchase(&first, &[4, 1]);
    let short = purchase(&second, &[16]);
    let seal = |bytes: &[u8]| psp_mint::seal::encrypt(&did.public_key(), bytes).unwrap();
    let wallet = Wallet {
        txs: vec![
            Tx {
                txid: "tx1",
                sats: 6_000,
                script: op_return(&seal(&exact)),
            },
            Tx {
                txid: "tx2",
                sats: 12_000,
                script: op_return(&seal(&short)),
            },
            Tx {
                txid: "tx3",
                sats: 3_000,
                script: op_return(&[7u8; 100]),
            },
        ],
        down: false,
    };

    let pass = Pass {
        data_dir: &m.dir,
        wallet: "psp",
        min_confirmations: 6,
        policy: &policy,
        did: &did,
        quiet: true,
    };
    service::cycle(&pass, &wallet, &m.state, &m.rot, &m.records, &mut || {
        Ok(BTC_10K)
    })
    .unwrap();

    // settled: the exact one issued its 5 units, the short one is held, and the
    // one that was not for this mint left no record
    {
        let records = m.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records.get("tx1").unwrap().requested_units, 5);
        assert_eq!(records.get("tx1").unwrap().read_prepaid_units, 1);
        assert_eq!(records.get("tx2").unwrap().read_prepaid_units, 1);
        assert!(records.get("tx3").is_none());
    }
    assert_eq!(
        m.state
            .logic
            .lock()
            .unwrap()
            .audit_totals()
            .unwrap()
            .0
            .units,
        5
    );
    // the checkpoint was saved, once every payment was processed
    assert_eq!(
        Checkpoint::load(&m.dir).unwrap(),
        Some(Checkpoint {
            block_hash: "block-9".to_string()
        })
    );

    // delivered over AIR: both records are owed, and the payers read them
    let mut deliverer = Some(Deliverer::new((*server).clone(), 2, did.clone()));
    service::deliver(&mut deliverer, &m.records, true);
    assert!(m.records.lock().unwrap().undelivered().is_empty());
    let id = wire::purchase_id(&exact);
    let sealed = read(&server, &wire::delivery_address(&psp, &first.did, &id))
        .expect("the signatures are on the server");
    let opened = wire::open_record(&first.key, &psp, &id, &sealed).unwrap();
    let Outcome::Signed(signed) = opened.outcome else {
        panic!("an exact purchase delivers its signatures");
    };
    assert_eq!(signed.signatures.len(), 2);
    let id = wire::purchase_id(&short);
    let sealed = read(&server, &wire::delivery_address(&psp, &second.did, &id))
        .expect("the held record is on the server");
    let opened = wire::open_record(&second.key, &psp, &id, &sealed).unwrap();
    let Outcome::Held(held) = opened.outcome else {
        panic!("a short purchase delivers a held record");
    };
    // 12 paid, less the first read (1), the fee (2), the write (1) and the
    // follow-up's read (1)
    assert_eq!(held.rules.as_ref().unwrap().due_units, 7);

    // inside the window nothing is forfeited; once it has closed the payment is
    // forfeited and shown apart from the open ones
    service::forfeit(&m.records, held.open_until - 1);
    assert_eq!(m.records.lock().unwrap().reconciliation().open.payments, 1);
    service::forfeit(&m.records, held.open_until);
    let report = m.records.lock().unwrap().reconciliation();
    assert_eq!((report.open.payments, report.forfeited.payments), (0, 1));
    assert_eq!(report.forfeited.sats, 12_000);
    assert_eq!(report.read_prepaid_units, 2);
}

#[test]
fn a_pass_that_fails_changes_nothing_and_leaves_the_checkpoint_alone() {
    let m = common::PspMint::open("cycle-down", NOW, Limits::default());
    let did = SecretKey::new();
    let policy = policy();
    let wallet = Wallet {
        txs: Vec::new(),
        down: true,
    };
    let pass = Pass {
        data_dir: &m.dir,
        wallet: "psp",
        min_confirmations: 6,
        policy: &policy,
        did: &did,
        quiet: true,
    };
    let records_before = std::fs::read(m.records_path()).ok();

    let e = service::cycle(&pass, &wallet, &m.state, &m.rot, &m.records, &mut || {
        Ok(BTC_10K)
    })
    .unwrap_err();
    assert_eq!(e.to_string(), "rpc error -28: loading block index");

    assert!(!m.dir.join(CHECKPOINT_FILE).exists());
    assert_eq!(Checkpoint::load(&m.dir).unwrap(), None);
    assert!(m.records.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(m.records_path()).ok(), records_before);
    assert_eq!(
        m.state
            .logic
            .lock()
            .unwrap()
            .audit_totals()
            .unwrap()
            .0
            .units,
        0
    );
}
