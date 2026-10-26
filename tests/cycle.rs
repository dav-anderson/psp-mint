//! One scan pass as `psp-mint` runs it: the wallet is scanned (`service::cycle`),
//! what is owed is delivered over AIR (`service::deliver`) and the claim windows
//! that closed are expired (`service::expire`). The wallet is a stand-in for
//! bitcoind (`scan::Node`) and the price is a closure; the mint, its records and
//! the AIR server (the shared local one, see `common`) are the real ones, and the
//! payer's side is the code the wallet runs.

mod common;

use psp_mint::delivery::Deliverer;
use psp_mint::records::Status;
use psp_mint::rpc::RpcError;
use psp_mint::scan::{CHECKPOINT_FILE, Checkpoint, Node};
use psp_mint::server::Limits;
use psp_mint::service::{self, Pass};
use psp_mint::settle::{ClaimFees, Policy, Terms};
use serde_json::{Value, json};
use sp_wire::ServiceDid;
use sp_wire::air_request::addr;
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::delivery::{self as wire, Outcome};
use sp_wire::seal::{PublicKey, SecretKey};

/// $100,000 per BTC: 100 sats are one PSP unit.
const BTC_100K: u64 = 10_000_000;
const NOW: u64 = 1_800_000_000;
/// The time of the block every stand-in payment is in: a little before `NOW`.
const BLOCK_TIME: u64 = 1_799_999_000;
/// The default `settle.retention_days`, 90, in seconds.
const RETENTION_SECS: u64 = 90 * 86_400;
/// The default `settle.prune_horizon_days`, 210, in seconds.
const HORIZON_SECS: u64 = 210 * 86_400;

/// A transaction the wallet received: its txid, the sats, its confirmations and
/// the OP_RETURN script.
struct Tx {
    txid: &'static str,
    sats: u64,
    confirmations: u32,
    /// The time of the block it is in (`gettransaction`'s `blocktime`).
    block_time: u64,
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
                    "confirmations": tx.confirmations,
                    "blocktime": tx.block_time,
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
        fees: ClaimFees {
            max_attempts: 2,
            write_fee_units: 1,
            read_fee_units: 1,
        },
    }
}

#[test]
fn a_pass_settles_delivers_and_expires_and_saves_the_checkpoint() {
    let server = common::server();
    let m = common::PspMint::open("cycle", NOW, Limits::default());
    let did = SecretKey::new();
    let psp = addr::key_to_did(&did.public_key());
    let policy = policy();

    // tx1: 700 sats at 6 confirmations is 7 units, less two reads (2) and the
    // write (1): 4 owed. tx2: $200,000 is in the top tier of 6 and has 5: locked,
    // owed nothing yet. tx3: data nobody sealed to this mint.
    let (first, second) = (payer(), payer());
    let seal =
        |payer: &Payer| psp_mint::seal::encrypt(&did.public_key(), payer.did.as_bytes()).unwrap();
    let wallet = Wallet {
        txs: vec![
            Tx {
                txid: "tx1",
                sats: 700,
                confirmations: 6,
                block_time: BLOCK_TIME,
                script: op_return(&seal(&first)),
            },
            Tx {
                txid: "tx2",
                sats: 200_000_000,
                confirmations: 5,
                block_time: BLOCK_TIME,
                script: op_return(&seal(&second)),
            },
            Tx {
                txid: "tx3",
                sats: 3_000,
                confirmations: 6,
                block_time: BLOCK_TIME,
                script: op_return(&[7u8; 100]),
            },
        ],
        down: false,
    };

    let pass = Pass {
        data_dir: &m.dir,
        wallet: "psp",
        policy: &policy,
        did: &did,
        quiet: true,
    };
    service::cycle(&pass, &wallet, &m.state, &m.rot, &m.records, &mut || {
        Ok(BTC_100K)
    })
    .unwrap();

    {
        let records = m.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        let (tx1, tx2) = (records.get("tx1").unwrap(), records.get("tx2").unwrap());
        assert_eq!(
            (
                tx1.due_units,
                tx1.read_prepaid_units,
                tx1.write_prepaid_units
            ),
            (4, 2, 1)
        );
        assert_eq!((tx2.status, tx2.tier_confirmations), (Status::Locked, 6));
        assert!(records.get("tx3").is_none());
    }
    // nothing is signed until the claim
    assert_eq!(m.state.logic.lock().unwrap().audit_totals().unwrap().0, 0);
    // the checkpoint was saved, once every payment was processed
    assert_eq!(
        Checkpoint::load(&m.dir).unwrap(),
        Some(Checkpoint {
            block_hash: "block-9".to_string()
        })
    );

    // delivered over AIR: the one owed record, which the payer reads
    let mut deliverer = Some(Deliverer::new((*server).clone(), 2, did));
    service::deliver(&mut deliverer, &m.records, true, NOW);
    assert!(m.records.lock().unwrap().undelivered().is_empty());
    let sealed = read(&server, &wire::owed_address(&psp, &first.did, 0))
        .expect("the owed record is on the server");
    let opened = wire::open_record(&first.key, &psp, 0, &sealed).unwrap();
    let Outcome::Owed(owed) = opened.outcome;
    assert_eq!(owed.terms.due_units, 4);
    assert_eq!(
        read(&server, &wire::owed_address(&psp, &second.did, 0)),
        None
    );

    // inside the window nothing expires; once it has closed the payment is
    // expired and shown apart from the open ones
    // (the window runs from the pass that stored the record, NOW, for 30 days)
    assert_eq!(owed.terms.window_secs, 2_592_000);
    let closes = NOW + 2_592_000;
    assert_eq!(
        m.records.lock().unwrap().get("tx1").unwrap().open_until,
        Some(closes)
    );
    service::expire(&m.records, closes - 1, RETENTION_SECS, HORIZON_SECS);
    assert_eq!(m.records.lock().unwrap().claims().unclaimed.payments, 1);
    service::expire(&m.records, closes, RETENTION_SECS, HORIZON_SECS);
    let claims = m.records.lock().unwrap().claims();
    assert_eq!((claims.unclaimed.payments, claims.expired.payments), (0, 1));
    assert_eq!((claims.expired.sats, claims.expired.units), (700, 4));
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
        policy: &policy,
        did: &did,
        quiet: true,
    };
    let records_before = std::fs::read(m.records_path()).ok();

    let e = service::cycle(&pass, &wallet, &m.state, &m.rot, &m.records, &mut || {
        Ok(BTC_100K)
    })
    .unwrap_err();
    assert_eq!(e.to_string(), "rpc error -28: loading block index");

    assert!(!m.dir.join(CHECKPOINT_FILE).exists());
    assert_eq!(Checkpoint::load(&m.dir).unwrap(), None);
    assert!(m.records.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(m.records_path()).ok(), records_before);
    assert_eq!(m.state.logic.lock().unwrap().audit_totals().unwrap().0, 0);
}

/// A payment pruned from the records is not owed again when the scan lists it,
/// and once its block is older than the cutoff (`settle.prune_horizon_days`, 210)
/// it is ignored without a price being asked or a record being made; a payment
/// from after the cutoff settles normally; and raising the horizon to 400 does
/// not bring the cutoff back, so the old payment stays ignored. Fails without the
/// pruned list or the cutoff, or if a raised horizon revives the payment.
#[test]
fn a_pruned_payment_is_not_owed_again_and_one_older_than_the_cutoff_is_ignored() {
    const CLOSES: u64 = NOW + 2_592_000;
    let m = common::PspMint::open("cycle-horizon", NOW, Limits::default());
    let did = SecretKey::new();
    let policy = policy();
    let (first, second) = (payer(), payer());
    let seal =
        |payer: &Payer| psp_mint::seal::encrypt(&did.public_key(), payer.did.as_bytes()).unwrap();
    let mut wallet = Wallet {
        txs: vec![Tx {
            txid: "tx1",
            sats: 700,
            confirmations: 6,
            block_time: BLOCK_TIME,
            script: op_return(&seal(&first)),
        }],
        down: false,
    };
    let pass = Pass {
        data_dir: &m.dir,
        wallet: "psp",
        policy: &policy,
        did: &did,
        quiet: true,
    };
    // One pass; returns how many times the price was asked.
    let run = |wallet: &Wallet| {
        let mut asked = 0;
        service::cycle(&pass, wallet, &m.state, &m.rot, &m.records, &mut || {
            asked += 1;
            Ok(BTC_100K)
        })
        .unwrap();
        asked
    };

    assert_eq!(run(&wallet), 1);
    m.records.lock().unwrap().land("tx1", NOW).unwrap();
    service::expire(&m.records, CLOSES, RETENTION_SECS, HORIZON_SECS);
    service::expire(
        &m.records,
        CLOSES + RETENTION_SECS,
        RETENTION_SECS,
        HORIZON_SECS,
    );
    assert!(m.records.lock().unwrap().is_empty());
    assert!(m.records.lock().unwrap().was_pruned("tx1"));

    // listed again: settled once already, nothing is made, no price is asked
    assert_eq!(run(&wallet), 0);
    assert!(m.records.lock().unwrap().is_empty());
    // and the /audit figures are still there
    let claims = m.records.lock().unwrap().claims();
    assert_eq!((claims.expired.payments, claims.expired.sats), (1, 700));

    // the cutoff reaches the payment's block: at it, the marker stays
    service::expire(
        &m.records,
        BLOCK_TIME + HORIZON_SECS,
        RETENTION_SECS,
        HORIZON_SECS,
    );
    assert!(m.records.lock().unwrap().was_pruned("tx1"));
    assert_eq!(m.records.lock().unwrap().cutoff(), 1_799_999_000);
    // one second on, the marker is dropped, and the payment is ignored by its age
    service::expire(
        &m.records,
        BLOCK_TIME + HORIZON_SECS + 1,
        RETENTION_SECS,
        HORIZON_SECS,
    );
    assert!(!m.records.lock().unwrap().was_pruned("tx1"));
    assert_eq!(m.records.lock().unwrap().cutoff(), 1_799_999_001);
    assert_eq!(run(&wallet), 0);
    assert!(m.records.lock().unwrap().is_empty());

    // a payment from after the cutoff settles normally
    wallet.txs.push(Tx {
        txid: "tx4",
        sats: 700,
        confirmations: 6,
        block_time: BLOCK_TIME + HORIZON_SECS,
        script: op_return(&seal(&second)),
    });
    assert_eq!(run(&wallet), 1);
    {
        let records = m.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records.get("tx4").unwrap().status, Status::Owed);
        assert!(records.get("tx1").is_none());
    }

    // the horizon is raised to 400 days: the cutoff does not move back
    service::expire(
        &m.records,
        BLOCK_TIME + HORIZON_SECS + 1,
        RETENTION_SECS,
        400 * 86_400,
    );
    assert_eq!(m.records.lock().unwrap().cutoff(), 1_799_999_001);
    assert_eq!(run(&wallet), 0);
    assert_eq!(m.records.lock().unwrap().len(), 1);
}
