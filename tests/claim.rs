//! The purchase claim over AIR (kind 15): the one claim of an owed payment,
//! checked against the owed record stored when the payment reached its tier.
//!
//! The mint is the one `psp-mint serve` builds (`common::PspMint`), and every
//! owed record is made the way the scan makes it: a payment goes through
//! `service::run_once_shared`. Most cases call `claim::claim` with the clock
//! passed in; the cases at the bottom go through the listener's own entry
//! (`server::air_served_with`) and over a real `air_server` (see `common`),
//! which needs port 5702 free like the other AIR tests. Every claim is signed
//! over the DID of the mint it is sent to (`ClaimRequest::digest`), and every
//! owed record is stored on AIR (`Records::land`) before it is claimed, as the
//! deliverer does it (`tests/delivery.rs` runs that part for real).

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note};
use psp_mint::claim::{self, Ignore};
use psp_mint::records::{PendingClaim, Record};
use psp_mint::scan::Received;
use psp_mint::server::{self, Limits};
use psp_mint::service;
use psp_mint::settle::Policy;
use rand::rngs::OsRng;
use secp256k1::{Keypair, PublicKey, SECP256K1, SecretKey};
use sp_wire::ServiceDid;
use sp_wire::air_request::ask::AskError;
use sp_wire::air_request::delivery::record_id;
use sp_wire::air_request::{KIND_PSP_CLAIM, Served};
use sp_wire::codes;
use sp_wire::psp::{ClaimRequest, ClaimResponse};

const T: u64 = 1_800_000_000;
/// The claim window, 30 days, written out.
const WINDOW: u64 = 2_592_000;
/// `settle.retention_days` by default, 90 days, written out.
const RETENTION: u64 = 7_776_000;
/// `settle.prune_horizon_days` by default, 210 days, written out.
const HORIZON: u64 = 18_144_000;
const DAY: u64 = 86_400;
/// $100,000 per BTC in cents: 100 sats are one PSP unit, so 700 sats are the 7
/// units every payment below is owed (worth 70 cents, the base tier of 3).
const BTC_100K: u64 = 10_000_000;

/// The payer of one purchase: the throwaway claim key.
struct Wallet {
    key: Keypair,
    did: ServiceDid,
}

fn wallet() -> Wallet {
    let key = Keypair::new(SECP256K1, &mut OsRng);
    Wallet {
        did: ServiceDid::from(key.public_key()),
        key,
    }
}

/// What the wallet keeps to unblind one output.
struct Kept {
    key: Keypair,
    blind: SecretKey,
    amount: u64,
}

struct Built {
    req: ClaimRequest,
    kept: Vec<Kept>,
}

/// A claim for record `index` of `w` at the mint `m`, for `keyset`, one output
/// per amount, signed by the claim key over `m`'s DID.
fn build(m: &common::PspMint, w: &Wallet, index: u32, keyset: &str, amounts: &[u64]) -> Built {
    build_for(&m.did, w, index, keyset, amounts)
}

/// As [`build`], signed over the DID `psp`, which need not be the mint it is
/// sent to.
fn build_for(psp: &ServiceDid, w: &Wallet, index: u32, keyset: &str, amounts: &[u64]) -> Built {
    let mut kept = Vec::new();
    let mut outputs = Vec::new();
    for &amount in amounts {
        let key = Keypair::new(SECP256K1, &mut OsRng);
        let (b, blind) = bdhke::blind_message(&Nonce(key.public_key()).as_hash_preimage(), None);
        outputs.push(MintOutput {
            amount: Amount::from_units(amount),
            blind_nonce: BlindNonce(b),
        });
        kept.push(Kept { key, blind, amount });
    }
    let mut req = ClaimRequest {
        claim_did: w.did,
        record_id: record_id(&w.did, index),
        keyset: keyset.to_string(),
        outputs,
        signature: secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap(),
    };
    req.signature = SECP256K1.sign_schnorr(&req.digest(psp), &w.key);
    Built { req, kept }
}

fn mint(name: &str) -> common::PspMint {
    common::PspMint::open_in(&common::mint_dir(name), 1, 1, T, Limits::default())
}

impl common::PspMint {
    fn keyset(&self) -> String {
        self.rot.rotor.lock().unwrap().current.id.clone()
    }

    /// A payment of `sats` to `w`'s claim key at `confirmations`, through the
    /// scan's own entry, at time `T`; and, once it is owed a record, that record
    /// stored on AIR at `T` (what the deliverer does: `Records::land`).
    fn pay(&self, w: &Wallet, txid: &str, confirmations: u32, sats: u64) {
        self.pay_unstored(w, txid, confirmations, sats);
        if self.record(txid).delivery.is_some() {
            self.land(txid, T);
        }
    }

    /// The owed record is stored on AIR at `at`: the claim window opens.
    fn land(&self, txid: &str, at: u64) {
        self.records.lock().unwrap().land(txid, at).unwrap();
    }

    /// As [`pay`](Self::pay), and the owed record is left unstored.
    fn pay_unstored(&self, w: &Wallet, txid: &str, confirmations: u32, sats: u64) {
        let found = [Received {
            txid: txid.to_string(),
            confirmations,
            value_sats: sats,
            payload: Some(w.did.as_bytes().to_vec()),
            block_time: Some(T),
        }];
        service::run_once_shared(
            &self.state.logic,
            &self.rot,
            &self.records,
            &found,
            &mut || Ok(BTC_100K),
            &mut |_, _| {},
            &Policy::default(),
            T,
        )
        .unwrap();
    }

    fn record(&self, txid: &str) -> Record {
        self.records.lock().unwrap().get(txid).unwrap().clone()
    }

    fn issued(&self) -> u64 {
        u64::try_from(self.state.logic.lock().unwrap().audit_totals().unwrap().0).unwrap()
    }

    fn ask(&self, req: &ClaimRequest, now: u64) -> Result<ClaimResponse, Ignore> {
        let mut records = self.records.lock().unwrap();
        let logic = self.state.logic.lock().unwrap();
        let rotor = self.rot.rotor.lock().unwrap();
        claim::claim(&mut records, &logic, &rotor, req, &self.did, now)
    }

    fn serve(&self, body: &[u8]) -> Option<Served> {
        let desk = server::ClaimDesk {
            records: Arc::clone(&self.records),
            psp: self.did,
        };
        server::air_served_with(
            &self.state,
            &self.rot,
            &desk,
            &self.health,
            &self.limits,
            KIND_PSP_CLAIM,
            body,
        )
    }

    /// Put the state a crash leaves: the claim is accepted and saved in full.
    fn accept(&self, txid: &str, built: &Built) {
        let mut record = self.record(txid);
        record.claim_pending = Some(PendingClaim {
            hash: built.req.hash(&self.did),
            epoch: self.rot.rotor.lock().unwrap().current.epoch,
            keyset: built.req.keyset.clone(),
            outputs: built.req.outputs.clone(),
        });
        self.records.lock().unwrap().put(txid, record).unwrap();
    }
}

fn refused(result: Result<ClaimResponse, Ignore>) -> Ignore {
    match result {
        Ok(_) => panic!("the claim was answered"),
        Err(e) => e,
    }
}

/// The notes the wallet gets from `answer`, each checked genuine by the mint.
fn notes(m: &common::PspMint, built: &Built, answer: &ClaimResponse) -> Vec<Note> {
    assert_eq!(answer.signatures.len(), built.kept.len());
    let logic = m.state.logic.lock().unwrap();
    built
        .kept
        .iter()
        .zip(&answer.signatures)
        .zip(&built.req.outputs)
        .map(|((k, s), output)| {
            assert_eq!(s.blind_nonce, output.blind_nonce.0.to_string());
            let signature: PublicKey = s.signature.parse().unwrap();
            let key = answer.keysets.keysets[&answer.keyset][&k.amount];
            let note = Note {
                nonce: Nonce(k.key.public_key()),
                signature: bdhke::unblind_message(&signature, &k.blind, &key),
            };
            logic
                .verify_note(&MintInput {
                    amount: Amount::from_units(k.amount),
                    note,
                    spend_signature: secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap(),
                })
                .expect("the note is genuine");
            note
        })
        .collect()
}

#[test]
fn an_owed_payment_is_claimed_once_for_exactly_its_units_and_the_notes_are_genuine() {
    let m = mint("claim");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    assert_eq!(m.issued(), 0, "nothing is signed before the claim");
    let built = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let answer = m.ask(&built.req, T + 5).unwrap();
    assert_eq!((answer.issued_units, answer.signatures.len()), (7, 3));
    assert_eq!(
        (answer.read_prepaid_units, answer.write_prepaid_units),
        (0, 0)
    );
    notes(&m, &built, &answer);
    assert_eq!(m.issued(), 7);
    let rec = m.record("tx1");
    assert!(rec.claim_pending.is_none());
    assert_eq!((rec.claimed_at, rec.attempts), (Some(T + 5), 0));
}

/// An output of 2^40 units, above the old default top denomination (2^24), is
/// signed, and its note is genuine: every keyset has a key for every power of two
/// up to 2^63. Fails if the key set is capped, as `mint.max_denomination_units`
/// capped it (the claim would be refused as an unknown denomination).
#[test]
fn a_claim_for_one_output_of_2_to_the_40_units_is_signed() {
    let m = mint("big-output");
    let w = wallet();
    // 2^40 = 1,099,511,627,776 units; 100 sats a unit; $109.9 billion: top tier, 6
    m.pay(&w, "tx1", 6, 109_951_162_777_600);
    assert_eq!(m.record("tx1").due_units, 1_099_511_627_776);
    let built = build(&m, &w, 0, &m.keyset(), &[1_099_511_627_776]);
    let answer = m.ask(&built.req, T + 5).unwrap();
    assert_eq!(
        (answer.issued_units, answer.signatures.len()),
        (1_099_511_627_776, 1)
    );
    notes(&m, &built, &answer);
    assert_eq!(m.issued(), 1_099_511_627_776);
}

#[test]
fn a_claim_that_does_not_add_up_to_the_units_owed_is_refused_counted_and_signs_nothing() {
    // one unit under, then one unit over
    for amounts in [&[4, 2][..], &[4, 2, 1, 1][..]] {
        let m = mint("sum");
        let w = wallet();
        m.pay(&w, "tx1", 3, 700);
        let wrong = build(&m, &w, 0, &m.keyset(), amounts);
        let e = refused(m.ask(&wrong.req, T));
        assert_eq!((e.code.as_str(), e.counted), (codes::WRONG_TOTAL, true));
        assert_eq!(m.record("tx1").attempts, 1);
        assert_eq!(m.issued(), 0);
        assert!(m.record("tx1").claimed.is_none());
        // the refused claim again is not counted twice, and the exact one gets through
        let e = refused(m.ask(&wrong.req, T));
        assert_eq!((e.code.as_str(), e.counted), (codes::WRONG_TOTAL, false));
        assert_eq!(m.record("tx1").attempts, 1);
        let right = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
        assert_eq!(m.ask(&right.req, T).unwrap().issued_units, 7);
        assert_eq!(m.issued(), 7);
    }
}

#[test]
fn a_denomination_the_mint_has_no_key_for_and_a_repeated_blind_nonce_are_refused() {
    let m = mint("content");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    // 7 units, but 3 is not a power of two, so no keyset has a key for it
    let odd = build(&m, &w, 0, &m.keyset(), &[3, 4]);
    assert_eq!(
        refused(m.ask(&odd.req, T)).code,
        codes::UNKNOWN_DENOMINATION
    );
    let mut twice = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    twice.req.outputs[1].blind_nonce = twice.req.outputs[0].blind_nonce;
    twice.req.signature = SECP256K1.sign_schnorr(&twice.req.digest(&m.did), &w.key);
    assert_eq!(
        refused(m.ask(&twice.req, T)).code,
        codes::DUPLICATE_BLIND_NONCE
    );
    assert_eq!(m.issued(), 0);
    // two refusals used the two attempts: even the exact claim is now ignored
    let right = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let e = refused(m.ask(&right.req, T));
    assert_eq!(
        (e.code.as_str(), e.counted),
        (codes::TOO_MANY_ATTEMPTS, false)
    );
}

#[test]
fn a_second_claim_gets_the_stored_answer_or_is_refused_and_signs_nothing_more() {
    let m = mint("twice");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let built = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let first = m.ask(&built.req, T).unwrap();
    // the identical resend: the same signatures, byte for byte, counted once
    let again = m.ask(&built.req, T + 60).unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&again).unwrap()
    );
    assert_eq!(m.issued(), 7);
    // a different claim for the same record after acceptance is refused
    let other = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let e = refused(m.ask(&other.req, T + 60));
    assert_eq!(
        (e.code.as_str(), e.counted),
        (codes::ALREADY_CLAIMED, false)
    );
    assert_eq!(m.issued(), 7);
    assert_eq!(m.record("tx1").attempts, 0);
}

#[test]
fn a_claim_is_refused_at_the_close_of_its_window_and_the_payment_is_audited_as_expired() {
    let m = mint("window");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let until = m.record("tx1").open_until.unwrap();
    assert_eq!(until, T + WINDOW);
    assert_eq!(until, 1_802_592_000);
    // the last second inside the window is accepted (another payment, the same window)
    let w2 = wallet();
    m.pay(&w2, "tx2", 3, 700);
    let inside = build(&m, &w2, 0, &m.keyset(), &[4, 2, 1]);
    assert_eq!(m.ask(&inside.req, until - 1).unwrap().issued_units, 7);
    // at open_until it is refused, whether or not the expiry pass has run
    let late = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let e = refused(m.ask(&late.req, until));
    assert_eq!((e.code.as_str(), e.counted), (codes::CLAIM_EXPIRED, false));
    service::expire(&m.records, until, RETENTION, HORIZON);
    assert_eq!(m.record("tx1").expired_at, Some(until));
    assert_eq!(
        refused(m.ask(&late.req, until + 1)).code,
        codes::CLAIM_EXPIRED
    );
    // the BTC is kept, apart from what was claimed, with its sats and units
    let claims = m.records.lock().unwrap().claims();
    assert_eq!(
        (
            claims.expired.payments,
            claims.expired.sats,
            claims.expired.units
        ),
        (1, 700, 7)
    );
    assert_eq!(
        (
            claims.claimed.payments,
            claims.claimed.sats,
            claims.claimed.units
        ),
        (1, 700, 7)
    );
    assert_eq!(m.issued(), 7);
}

#[test]
fn nothing_is_claimable_before_the_tier_or_for_a_payment_that_left_the_chain() {
    let m = mint("tier");
    let w = wallet();
    // locked at 1 confirmation, tier 3: no owed record, so a claim finds none
    m.pay(&w, "tx1", 1, 700);
    m.pay(&w, "tx1", 2, 700);
    assert!(m.record("tx1").delivery.is_none());
    let early = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    assert_eq!(refused(m.ask(&early.req, T)).code, codes::UNKNOWN_OWED);
    // a reorg takes it out of the chain: still not owed
    m.pay(&w, "tx1", 0, 700);
    assert_eq!(refused(m.ask(&early.req, T)).code, codes::UNKNOWN_OWED);
    assert_eq!(m.issued(), 0);
    // back at its tier it is owed, and the same claim is accepted
    m.pay(&w, "tx1", 3, 700);
    assert_eq!(m.ask(&early.req, T).unwrap().issued_units, 7);
}

#[test]
fn a_copied_payload_pays_the_claim_key_holder_each_time_and_each_record_is_claimed_once() {
    let m = mint("copied");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    m.pay(&w, "tx2", 3, 300);
    let (a, b) = (
        m.record("tx1").delivery.unwrap(),
        m.record("tx2").delivery.unwrap(),
    );
    assert_eq!((a.index, b.index), (0, 1));
    // record 1 is 3 units; a claim for record 0's 7 units under record 1's id is refused
    let wrong = build(&m, &w, 1, &m.keyset(), &[4, 2, 1]);
    assert_eq!(refused(m.ask(&wrong.req, T)).code, codes::WRONG_TOTAL);
    let one = build(&m, &w, 1, &m.keyset(), &[2, 1]);
    assert_eq!(m.ask(&one.req, T).unwrap().issued_units, 3);
    let zero = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    assert_eq!(m.ask(&zero.req, T).unwrap().issued_units, 7);
    assert_eq!(m.issued(), 10);
    // a claim for an index the mint never made is unknown
    let none = build(&m, &w, 2, &m.keyset(), &[4, 2, 1]);
    assert_eq!(refused(m.ask(&none.req, T)).code, codes::UNKNOWN_OWED);
}

// ---- crash recovery ----

/// The mint as a restart finds it: its folder opened again, as `serve` opens it.
fn restart(m: common::PspMint) -> common::PspMint {
    let dir = m.dir.clone();
    drop(m);
    common::PspMint::open_in(&dir, 1, 1, T, Limits::default())
}

fn recover(m: &common::PspMint) -> Result<usize, psp_mint::settle::SettleError> {
    service::recover(&m.state.logic, &m.rot, &m.records, T + 10)
}

#[test]
fn a_crash_before_anything_was_signed_is_finished_at_startup_without_a_resend() {
    let m = mint("crash-before");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let built = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    m.accept("tx1", &built);
    assert_eq!(m.issued(), 0);
    // another claim meanwhile is told this one is being worked on
    let other = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let e = refused(m.ask(&other.req, T));
    assert_eq!((e.code.as_str(), e.counted), (codes::IN_PROGRESS, false));

    let m = restart(m);
    assert_eq!(recover(&m).unwrap(), 1);
    assert_eq!(m.issued(), 7);
    assert!(m.record("tx1").claim_pending.is_none());
    // the wallet's resend gets the stored answer, with notes that are genuine
    let answer = m.ask(&built.req, T + 99).unwrap();
    notes(&m, &built, &answer);
    assert_eq!(m.issued(), 7);
    assert_eq!(recover(&m).unwrap(), 0);
}

#[test]
fn a_crash_after_the_first_signature_gives_the_same_signatures_and_counts_them_once() {
    let m = mint("crash-after");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let built = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    m.accept("tx1", &built);
    // the first output was signed and counted before the crash: its nonce is used
    let before = m
        .state
        .logic
        .lock()
        .unwrap()
        .issue(&built.req.outputs[0])
        .unwrap();
    assert_eq!(m.issued(), 4);

    let m = restart(m);
    assert_eq!(recover(&m).unwrap(), 1);
    assert_eq!(
        m.issued(),
        7,
        "issued once: the first output is not counted twice"
    );
    let answer = m.ask(&built.req, T + 1).unwrap();
    assert_eq!(answer.signatures[0].signature, before.0.to_string());
    assert_eq!(answer.signatures.len(), 3);
    notes(&m, &built, &answer);
}

#[test]
fn an_answer_that_could_not_be_written_is_an_error_and_is_finished_on_the_next_pass() {
    let m = mint("crash-write");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let built = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    m.accept("tx1", &built);
    // the records cannot be written while a folder sits where the temporary file goes
    let tmp = m.records_path().with_extension("json.tmp");
    std::fs::create_dir(&tmp).unwrap();
    assert!(recover(&m).is_err());
    assert!(m.record("tx1").claim_pending.is_some());
    assert!(m.record("tx1").claimed.is_none());
    std::fs::remove_dir(&tmp).unwrap();
    assert_eq!(recover(&m).unwrap(), 1);
    assert_eq!(
        m.issued(),
        7,
        "the failed pass signed the notes once and counted them once"
    );
    assert!(m.record("tx1").claimed.is_some());
}

#[test]
fn a_claim_accepted_before_the_window_closed_is_finished_after_it() {
    let m = mint("pending-expiry");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let until = m.record("tx1").open_until.unwrap();
    let built = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    m.accept("tx1", &built);
    service::expire(&m.records, until + 1_000, RETENTION, HORIZON);
    assert!(m.record("tx1").expired_at.is_none());
    assert_eq!(m.ask(&built.req, until + 1_000).unwrap().issued_units, 7);
}

// ---- through the listener's entry, and over AIR ----

fn ignored(served: Option<Served>) -> String {
    match served {
        Some(Served::Ignored(why)) => why,
        Some(Served::Body(_)) => panic!("the claim was answered"),
        Some(_) => panic!("another kind of outcome"),
        None => panic!("the kind is not served"),
    }
}

/// Poison the records lock and the mint's the way a panicking holder does.
fn poison(m: &common::PspMint) {
    let (records, state) = (Arc::clone(&m.records), Arc::clone(&m.state));
    let joined = std::thread::spawn(move || {
        let _records = records.lock().unwrap();
        let _logic = state.logic.lock().unwrap();
        panic!("poisoning the locks on purpose");
    })
    .join();
    assert!(joined.is_err() && m.records.is_poisoned() && m.state.logic.is_poisoned());
}

fn file_of(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[test]
fn a_claim_without_the_claim_key_is_ignored_before_anything_is_read_or_counted() {
    let m = mint("forged");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let before = file_of(&m.records_path());
    // both locks the claim would need are poisoned: had the signature been
    // checked after them, the answer would be "unavailable", not "bad_signature"
    poison(&m);
    let mut forged = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]).req;
    forged.signature = SECP256K1.sign_schnorr(&forged.digest(&m.did), &wallet().key);
    let why = ignored(m.serve(&serde_json::to_vec(&forged).unwrap()));
    assert!(why.starts_with(codes::BAD_SIGNATURE), "{why}");
    assert_eq!(file_of(&m.records_path()), before);
    // the same over AIR: no reply
    match m
        .air()
        .ask_unanswered(KIND_PSP_CLAIM, &serde_json::to_vec(&forged).unwrap())
    {
        Err(AskError::Unreachable { attempts: 1, .. }) => {}
        Ok(_) => panic!("a forged claim got a reply"),
        Err(e) => panic!("a forged claim must meet silence, not: {e}"),
    }
}

#[test]
fn the_listener_answers_an_accepted_claim_and_ignores_the_rest() {
    let m = mint("entry");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    // not JSON, a stranger's claim and a wrong total: no reply
    assert!(ignored(m.serve(b"not json")).starts_with(codes::MALFORMED_REQUEST));
    let stranger = build(&m, &wallet(), 0, &m.keyset(), &[4, 2, 1]);
    let why = ignored(m.serve(&serde_json::to_vec(&stranger.req).unwrap()));
    assert!(why.starts_with(codes::UNKNOWN_OWED), "{why}");
    let wrong = build(&m, &w, 0, &m.keyset(), &[4, 2]);
    let why = ignored(m.serve(&serde_json::to_vec(&wrong.req).unwrap()));
    assert!(why.starts_with(codes::WRONG_TOTAL), "{why}");
    assert_eq!((m.record("tx1").attempts, m.issued()), (1, 0));
    // the right one is answered, and a resend gets the same bytes
    let right = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let body = serde_json::to_vec(&right.req).unwrap();
    let Some(Served::Body(bytes)) = m.serve(&body) else {
        panic!("not answered");
    };
    let answer: ClaimResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!((answer.issued_units, m.issued()), (7, 7));
    let Some(Served::Body(again)) = m.serve(&body) else {
        panic!("a resend is answered");
    };
    assert_eq!((again, m.issued()), (bytes, 7));
}

#[test]
fn over_air_an_accepted_claim_gets_its_notes_and_a_refused_one_gets_nothing() {
    let m = mint("air");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let air = m.air();
    let wrong = build(&m, &w, 0, &m.keyset(), &[2]);
    match air.ask_unanswered(KIND_PSP_CLAIM, &serde_json::to_vec(&wrong.req).unwrap()) {
        Err(AskError::Unreachable { attempts: 1, .. }) => {}
        Ok(_) => panic!("a refused claim got a reply"),
        Err(e) => panic!("a refused claim must meet silence, not: {e}"),
    }
    assert_eq!((m.record("tx1").attempts, m.issued()), (1, 0));
    let right = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let answer: ClaimResponse = air.post_ok("/claim", &right.req);
    assert_eq!(answer.issued_units, 7);
    drop(air);
    notes(&m, &right, &answer);
}

// ---- the claim is for one PSP, one window, one keyset ----

/// Fails if the claim digest leaves the PSP's DID out: mint B would then accept
/// what was signed over mint A's DID.
#[test]
fn a_claim_signed_for_one_psp_is_ignored_by_another_and_accepted_by_its_own() {
    let a = mint("psp-a");
    let b = mint("psp-b");
    assert_ne!(a.did, b.did);
    let w = wallet();
    // the same claim key paid both mints, so each owes it record 0
    a.pay(&w, "tx1", 3, 700);
    b.pay(&w, "tx1", 3, 700);
    // right for B in everything but the DID it was signed over
    let for_a = build_for(&a.did, &w, 0, &b.keyset(), &[4, 2, 1]);
    let before = file_of(&b.records_path());
    let why = ignored(b.serve(&serde_json::to_vec(&for_a.req).unwrap()));
    assert!(why.starts_with(codes::BAD_SIGNATURE), "{why}");
    // nothing was read or changed: the file is as it was, nothing is signed
    assert_eq!(file_of(&b.records_path()), before);
    assert_eq!((b.record("tx1").attempts, b.issued()), (0, 0));
    // the same claim signed over B's DID is accepted by B, and A's own by A
    let for_b = build(&b, &w, 0, &b.keyset(), &[4, 2, 1]);
    assert!(matches!(
        b.serve(&serde_json::to_vec(&for_b.req).unwrap()),
        Some(Served::Body(_))
    ));
    let own = build(&a, &w, 0, &a.keyset(), &[4, 2, 1]);
    assert!(matches!(
        a.serve(&serde_json::to_vec(&own.req).unwrap()),
        Some(Served::Body(_))
    ));
    assert_eq!((a.issued(), b.issued()), (7, 7));
}

/// Fails if the window starts when the record is made: the claim at `T` + 31 days
/// would be refused, and the unstored record would expire.
#[test]
fn the_window_starts_when_the_record_is_stored_and_an_unstored_record_cannot_be_claimed() {
    let m = mint("landing");
    let w = wallet();
    // made at T, but the AIR server has not taken it
    m.pay_unstored(&w, "tx1", 3, 700);
    // the second payment is seen now too: a payment first seen after the
    // expiry pass below would be older than the prune horizon's cutoff
    m.pay_unstored(&w, "tx2", 3, 700);
    assert_eq!(m.record("tx1").open_until, None);
    let early = build(&m, &w, 0, &m.keyset(), &[4, 2, 1]);
    let e = refused(m.ask(&early.req, T + DAY));
    assert_eq!((e.code.as_str(), e.counted), (codes::UNKNOWN_OWED, false));
    assert_eq!(m.record("tx1").attempts, 0);
    // however late it is, an unstored record is never expired
    service::expire(&m.records, T + 400 * DAY, RETENTION, HORIZON);
    assert!(m.record("tx1").expired_at.is_none());
    // stored five days later: the window is counted from then
    m.land("tx1", T + 5 * DAY);
    let until = m.record("tx1").open_until.unwrap();
    assert_eq!(until, 1_803_024_000);
    assert_eq!(m.record("tx1").landed_at, Some(1_800_432_000));
    // 31 days after it was made is inside the window
    assert_eq!(m.ask(&early.req, T + 31 * DAY).unwrap().issued_units, 7);
    // and a second payment is refused at the very second the window closes
    m.land("tx2", T + 5 * DAY);
    let second = build(&m, &w, 1, &m.keyset(), &[4, 2, 1]);
    let e = refused(m.ask(&second.req, until));
    assert_eq!((e.code.as_str(), e.counted), (codes::CLAIM_EXPIRED, false));
    assert_eq!(m.ask(&second.req, until - 1).unwrap().issued_units, 7);
}

/// Move the mint's clock to `hours` after the first issue and rotate as far as it
/// says. The rotor is the one `serve` runs; only the clock is set.
fn rotate(m: &common::PspMint, hours: u64) {
    let mut logic = m.state.logic.lock().unwrap();
    let mut rotor = m.rot.rotor.lock().unwrap();
    rotor.first_issue = Some(T);
    rotor.clock.store(T + hours * 3600, Ordering::SeqCst);
    rotor.refresh(&mut logic).unwrap();
}

/// What the previous generation has issued: the set a rotation left behind.
fn previous_issued(m: &common::PspMint) -> u128 {
    let rotor = m.rot.rotor.lock().unwrap();
    let (_, logic) = rotor.previous.as_ref().expect("a previous generation");
    logic.audit_totals().unwrap().0
}

/// Fails if recovery signs with the keyset the claim named (A) and not the one
/// that is current when it runs (B).
#[test]
fn a_claim_accepted_for_one_keyset_is_finished_with_the_current_one_after_a_rotation() {
    let m = mint("rotate-between");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let a = m.keyset();
    let built = build(&m, &w, 0, &a, &[4, 2, 1]);
    m.accept("tx1", &built);
    // the mint rotates to B before it recovers; A is now the previous set
    rotate(&m, 1);
    let b = m.keyset();
    assert_ne!(a, b);
    assert_eq!(recover(&m).unwrap(), 1);
    let answer = m.record("tx1").claimed.unwrap().response;
    assert_eq!(answer.keyset, b);
    assert_eq!(answer.keysets.current, b);
    assert_eq!(answer.keysets.previous.as_deref(), Some(a.as_str()));
    // the notes verify under B's keys (`notes` unblinds with the key of the set
    // that signed and asks the current mint, which is B's, to check them)
    notes(&m, &built, &answer);
    // A signed nothing; B counts the claim once
    assert_eq!((previous_issued(&m), m.issued()), (0, 7));
    // the wallet's resend gets the stored answer
    let again = m.ask(&built.req, T + 99).unwrap();
    assert_eq!(
        serde_json::to_vec(&again).unwrap(),
        serde_json::to_vec(&answer).unwrap()
    );
}

/// Fails if a keyset refusal is counted against the record's attempts, or if
/// the previous, a retired and an unknown keyset are not told apart.
#[test]
fn a_claim_that_names_a_keyset_that_is_not_the_current_one_is_refused_and_not_counted() {
    let m = mint("keyset-refusals");
    let w = wallet();
    m.pay(&w, "tx1", 3, 700);
    let first = m.keyset();
    rotate(&m, 1);
    let second = m.keyset();
    rotate(&m, 2);
    let current = m.keyset();
    // the first set is retired, the second is the previous, the third is current
    assert_eq!(
        (first == second, second == current, first == current),
        (false, false, false)
    );
    let unknown = "0badc0de0badc0de".to_string();
    for (named, code) in [
        (&second, codes::KEYSET_NOT_CURRENT),
        (&first, codes::KEYSET_RETIRED),
        (&unknown, codes::UNKNOWN_KEYSET),
    ] {
        let claim = build(&m, &w, 0, named, &[4, 2, 1]);
        let e = refused(m.ask(&claim.req, T));
        assert_eq!((e.code.as_str(), e.counted), (code, false), "{named}");
        // and through the listener's entry, which logs it as not counted
        let why = ignored(m.serve(&serde_json::to_vec(&claim.req).unwrap()));
        assert!(why.starts_with(code), "{why}");
        assert_eq!(m.record("tx1").attempts, 0, "{named}");
        assert!(m.record("tx1").refusals.is_empty());
    }
    // a wrong total with the current keyset is still counted
    let wrong = build(&m, &w, 0, &current, &[4, 2]);
    let e = refused(m.ask(&wrong.req, T));
    assert_eq!((e.code.as_str(), e.counted), (codes::WRONG_TOTAL, true));
    assert_eq!(m.record("tx1").attempts, 1);
    // three keyset refusals did not use the two attempts: the right claim is accepted
    let right = build(&m, &w, 0, &current, &[4, 2, 1]);
    assert_eq!(m.ask(&right.req, T).unwrap().issued_units, 7);
    assert_eq!(m.issued(), 7);
}
