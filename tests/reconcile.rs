//! Reconciliation over AIR (kind 15): the corrected request for a held or rejected
//! payment and the credit claim, checked against the rules stored when the window
//! opened.
//!
//! Most cases call `psp_mint::reconcile::reconcile` on a mint and a records file in
//! this process, with the clock passed in. The cases at the bottom go through the
//! listener's own entry (`server::air_served_with`) and over a real `air_server`
//! (see `common`), which need port 5702 free like the other AIR tests.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use mint_core::MintLogic;
use mint_server::state::AppState;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note};
use psp_mint::reconcile::{self, Ignore};
use psp_mint::records::{Delivery, PendingReconcile, Record, Records, SharedRecords, Status};
use psp_mint::rotation::{Rollovers, Rotor};
use psp_mint::server::{self, Health, Limits, Rotating, SharedRotating};
use psp_mint::settle::{self, ReconcileTerms};
use rand::rngs::OsRng;
use secp256k1::{Keypair, PublicKey, SECP256K1, SecretKey};
use sp_wire::ServiceDid;
use sp_wire::air_request::delivery::{
    FORFEIT_NOTICE, Held, Outcome, ReconcileRules, SEND_CORRECTED_REQUEST, SEND_CREDIT_CLAIM,
};
use sp_wire::air_request::{KIND_PSP_RECONCILE, Served};
use sp_wire::codes;
use sp_wire::psp::{ReconcileRequest, ReconcileResponse};

const T: u64 = 1_800_000_000;
const H: u64 = 3600;
/// The deadline of the payments in the direct cases.
const OPEN: u64 = T + 7 * 86_400;
/// $10,000 per BTC in cents: 1,000 sats is one PSP unit.
const BTC_10K: u64 = 1_000_000;

fn scratch(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "psp-mint-reconcile-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn zero_sig() -> secp256k1::schnorr::Signature {
    secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap()
}

/// The payer of one purchase: the one-off reply key and the purchase id.
struct Wallet {
    key: Keypair,
    did: ServiceDid,
    purchase: [u8; 32],
}

fn wallet(n: u8) -> Wallet {
    let key = Keypair::new(SECP256K1, &mut OsRng);
    Wallet {
        did: ServiceDid::from(key.public_key()),
        key,
        purchase: [n; 32],
    }
}

/// What the wallet keeps to unblind one output.
struct Kept {
    key: Keypair,
    blind: SecretKey,
    amount: u64,
}

struct Built {
    req: ReconcileRequest,
    kept: Vec<Kept>,
}

fn sign(w: &Wallet, req: &mut ReconcileRequest) {
    req.signature = SECP256K1.sign_schnorr(&req.digest(), &w.key);
}

/// A request for `keyset` with one output per amount, signed by the reply key.
fn build(w: &Wallet, keyset: &str, amounts: &[u64]) -> Built {
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
    let mut req = ReconcileRequest {
        reply_did: w.did,
        purchase: w.purchase,
        keyset: keyset.to_string(),
        outputs,
        signature: zero_sig(),
    };
    sign(w, &mut req);
    Built { req, kept }
}

fn rules(send: &str, due: u64) -> ReconcileRules {
    ReconcileRules {
        send: send.to_string(),
        kind: KIND_PSP_RECONCILE,
        due_units: due,
        reconciliation_fee_units: 0,
        read_prepaid_units: 0,
        write_prepaid_units: 0,
        open_until: OPEN,
        max_attempts: 2,
        on_failure: FORFEIT_NOTICE.to_string(),
    }
}

/// A payment the payer was told to correct: held, open until `rules.open_until`.
fn held(w: &Wallet, rules: ReconcileRules) -> Record {
    let mut r = Record::locked(BTC_10K, false);
    r.status = Status::PaidUnsettled;
    r.value_sats = rules.due_units * 1_000;
    r.requested_units = rules.due_units + 5;
    r.payer_did = Some(w.did.to_hex());
    r.open_until = Some(rules.open_until);
    r.delivery = Some(Delivery {
        reply_did: w.did.to_hex(),
        purchase: hex::encode(w.purchase),
        outcome: Outcome::Held(Held {
            reason: "short".to_string(),
            paid_sats: r.value_sats,
            paid_cents: r.value_sats / 10,
            requested_units: r.requested_units,
            over_under_bps: -5_000,
            open_until: rules.open_until,
            rules: Some(rules.clone()),
        }),
        sealed: None,
        delivered: true,
        abandoned: None,
    });
    r.rules = Some(rules);
    r
}

/// A purchase that was signed with `credit_cents` owed.
fn credited(w: &Wallet, rules: ReconcileRules, credit_cents: u64, kept: u64, given: u64) -> Record {
    let mut r = held(w, rules);
    r.status = Status::Issued;
    r.issued = true;
    r.credit_cents = credit_cents;
    r.kept_cents = kept;
    r.given_cents = given;
    r
}

/// A mint (current keyset, rotating) and a records file.
struct World {
    records: Records,
    rotor: Rotor,
    logic: MintLogic,
}

fn world(name: &str) -> World {
    let dir = scratch(name);
    let (rotor, logic) = Rotor::open_at(&dir, Amount::from_units(1024), 1, 1, T).unwrap();
    let records = Records::open(&dir.join("settlements.json")).unwrap();
    World {
        records,
        rotor,
        logic,
    }
}

impl World {
    fn keyset(&self) -> String {
        self.rotor.current.id.clone()
    }

    fn put(&mut self, txid: &str, record: Record) {
        self.records.put(txid, record).unwrap();
    }

    fn record(&self, txid: &str) -> &Record {
        self.records.get(txid).unwrap()
    }

    fn ask(&mut self, req: &ReconcileRequest, now: u64) -> Result<ReconcileResponse, Ignore> {
        reconcile::reconcile(&mut self.records, &self.logic, &self.rotor, req, now)
    }

    /// PSP units issued so far.
    fn issued(&self) -> u64 {
        self.logic.audit_totals().unwrap().0.units
    }

    /// Issue one note and look at the clock, so the first issue is recorded and
    /// the mint rotates when the clock moves (it counts epochs from the first
    /// issue). The note is in the first keyset, which is the previous one, then
    /// retired, after the rotations.
    fn arm(&mut self) {
        let key = Keypair::new(SECP256K1, &mut OsRng);
        let (b, _) = bdhke::blind_message(&Nonce(key.public_key()).as_hash_preimage(), None);
        self.logic
            .issue(&MintOutput {
                amount: Amount::from_units(1),
                blind_nonce: BlindNonce(b),
            })
            .unwrap();
        self.rotor.refresh(&mut self.logic).unwrap();
    }

    /// Move the clock of the mint and rotate to the keyset of that time. After
    /// it `issued` counts the new keyset's notes only.
    fn rotate_to(&mut self, t: u64) {
        self.rotor.clock.store(t, Ordering::SeqCst);
        self.rotor.refresh(&mut self.logic).unwrap();
    }

    /// The notes the wallet gets from `answer`, each checked genuine by the mint
    /// that signed them (the current one).
    fn notes(&self, built: &Built, answer: &ReconcileResponse) -> Vec<Note> {
        assert_eq!(answer.signatures.len(), built.kept.len());
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
                self.logic
                    .verify_note(&MintInput {
                        amount: Amount::from_units(k.amount),
                        note,
                        spend_signature: zero_sig(),
                    })
                    .expect("the note is genuine");
                note
            })
            .collect()
    }
}

fn refused(result: Result<ReconcileResponse, Ignore>) -> Ignore {
    match result {
        Ok(_) => panic!("the request was accepted"),
        Err(e) => e,
    }
}

// ---- accepted requests ----

#[test]
fn a_held_payment_is_settled_at_the_amount_due_and_the_notes_are_genuine() {
    let mut w = world("held");
    let p = wallet(1);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 7)));
    let built = build(&p, &w.keyset(), &[4, 2, 1]);
    let answer = w.ask(&built.req, T).unwrap();
    assert_eq!(answer.issued_units, 7);
    assert_eq!(answer.keyset, w.keyset());
    assert_eq!(answer.keysets.current, w.keyset());
    assert_eq!(w.issued(), 7);
    assert_eq!(w.notes(&built, &answer).len(), 3);
    let rec = w.record("tx1");
    assert_eq!(rec.status, Status::Issued);
    assert!(rec.issued);
    assert_eq!(rec.settled_at, Some(T));
    assert!(rec.reconcile_pending.is_none());
    assert!(!rec.is_open());
    assert_eq!(rec.open_until, Some(OPEN), "the window stays as history");
    assert_eq!(rec.reconciled.as_ref().unwrap().response, answer);
    // an identical resend gets the stored answer and signs nothing more
    assert_eq!(w.ask(&built.req, T + 5).unwrap(), answer);
    assert_eq!(w.issued(), 7);
}

#[test]
fn a_rejected_purchase_is_settled_the_same_way() {
    let mut w = world("rejected");
    let p = wallet(2);
    let mut record = held(&p, rules(SEND_CORRECTED_REQUEST, 3));
    record.status = Status::Rejected;
    w.put("tx1", record);
    let built = build(&p, &w.keyset(), &[2, 1]);
    let answer = w.ask(&built.req, T).unwrap();
    assert_eq!(w.issued(), 3);
    w.notes(&built, &answer);
    let rec = w.record("tx1");
    assert_eq!((rec.status, rec.issued), (Status::Issued, true));
}

#[test]
fn a_credit_claim_signs_the_credit_and_records_it_claimed() {
    let mut w = world("credit");
    let p = wallet(3);
    // 29 cents rounded up to 3 units: 1 cent given
    w.put("tx1", credited(&p, rules(SEND_CREDIT_CLAIM, 3), 29, 0, 1));
    let built = build(&p, &w.keyset(), &[2, 1]);
    let answer = w.ask(&built.req, T).unwrap();
    assert_eq!(
        (answer.issued_units, answer.kept_cents, answer.given_cents),
        (3, 0, 1)
    );
    assert_eq!(w.issued(), 3);
    w.notes(&built, &answer);
    let rec = w.record("tx1");
    assert_eq!(rec.credit_claimed_cents, 29);
    assert_eq!(
        rec.status,
        Status::Issued,
        "the purchase was already issued"
    );
    assert!(!rec.is_open());
    assert!(w.records.owed_to(&p.did.to_hex()).is_empty());
    let recon = w.records.reconciliation();
    assert_eq!(recon.settled.payments, 1);
    assert_eq!(recon.settled.credit_cents, 29);
    assert_eq!(
        (recon.rounding_kept_cents, recon.rounding_given_cents),
        (0, 1)
    );
}

#[test]
fn the_fees_come_off_what_is_due_and_are_counted_as_revenue() {
    let mut w = world("fees");
    let p = wallet(4);
    let fees = ReconcileTerms {
        fee_units: 2,
        write_fee_units: 1,
        read_fee_units: 1,
        ..ReconcileTerms::default()
    };
    // 20 units paid: less the first read (1), the fee (2), the write (1) and the
    // follow-up's read (1) leaves 15
    let rules = settle::corrected_rules(20_000, BTC_10K, OPEN, &fees);
    assert_eq!(rules.due_units, 15);
    let mut record = held(&p, rules);
    record.read_prepaid_units = 1;
    w.put("tx1", record);
    // asking for the whole 20 is refused; 15 is signed
    let too_much = build(&p, &w.keyset(), &[16, 4]);
    assert_eq!(refused(w.ask(&too_much.req, T)).code, codes::WRONG_TOTAL);
    let built = build(&p, &w.keyset(), &[8, 4, 2, 1]);
    let answer = w.ask(&built.req, T).unwrap();
    assert_eq!(answer.issued_units, 15);
    assert_eq!(w.issued(), 15);
    // the fee, the follow-up's read and the one write are the reply's, once
    assert_eq!(
        (
            answer.fee_units,
            answer.read_prepaid_units,
            answer.write_prepaid_units
        ),
        (2, 1, 1)
    );
    let recon = w.records.reconciliation();
    assert_eq!(recon.fees_units, 2);
    assert_eq!(recon.write_prepaid_units, 1);
    assert_eq!(
        recon.read_prepaid_units, 2,
        "the first read and the follow-up"
    );
    assert_eq!(recon.refused_attempts, 1);
    // a resend does not charge again
    w.ask(&built.req, T).unwrap();
    assert_eq!(w.records.reconciliation().write_prepaid_units, 1);
}

#[test]
fn the_previous_keyset_may_sign_as_in_a_rollover() {
    let mut w = world("previous");
    let p = wallet(5);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 2)));
    w.arm();
    let old = w.keyset();
    w.rotate_to(T + H);
    assert_ne!(w.keyset(), old);
    let built = build(&p, &old, &[2]);
    let answer = w.ask(&built.req, T + H).unwrap();
    assert_eq!(answer.keyset, old);
    assert_eq!(answer.keysets.previous.as_deref(), Some(old.as_str()));
}

// ---- refused requests: counted, and never answered ----

#[test]
fn a_wrong_total_is_refused_counted_and_signs_nothing() {
    let mut w = world("wrong-total");
    let p = wallet(6);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 7)));
    let short = build(&p, &w.keyset(), &[4, 2]);
    let e = refused(w.ask(&short.req, T));
    assert_eq!((e.code.as_str(), e.counted), (codes::WRONG_TOTAL, true));
    assert_eq!(w.issued(), 0);
    let rec = w.record("tx1");
    assert_eq!(rec.attempts, 1);
    assert_eq!(rec.refusals.len(), 1);
    assert_eq!(rec.refusals[0].code, codes::WRONG_TOTAL);
    assert_eq!(rec.refusals[0].hash, short.req.hash());
    assert!(rec.is_open());
    // the same request again is not counted twice
    let e = refused(w.ask(&short.req, T + 1));
    assert!(!e.counted);
    assert_eq!(w.record("tx1").attempts, 1);
    // a different wrong one is the second, and the last
    let long = build(&p, &w.keyset(), &[8]);
    assert!(refused(w.ask(&long.req, T + 2)).counted);
    assert_eq!(w.record("tx1").attempts, 2);
    // the right request now meets the cap, and is not counted
    let right = build(&p, &w.keyset(), &[4, 2, 1]);
    let e = refused(w.ask(&right.req, T + 3));
    assert_eq!(
        (e.code.as_str(), e.counted),
        (codes::TOO_MANY_ATTEMPTS, false)
    );
    assert_eq!(w.issued(), 0);
    assert_eq!(w.record("tx1").attempts, 2);
    // and the payment is forfeited when its window closes
    assert_eq!(
        w.records.forfeit_expired(OPEN).unwrap(),
        vec!["tx1".to_string()]
    );
    assert_eq!(w.records.reconciliation().refused_attempts, 2);
}

#[test]
fn a_keyset_that_was_rotated_out_uses_one_attempt_and_the_second_gets_through() {
    let mut w = world("race");
    let p = wallet(7);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 2)));
    w.arm();
    let old = w.keyset();
    w.rotate_to(T + H);
    w.rotate_to(T + 2 * H);
    assert_ne!(w.keyset(), old);
    let stale = build(&p, &old, &[2]);
    let e = refused(w.ask(&stale.req, T + 2 * H));
    assert_eq!((e.code.as_str(), e.counted), (codes::KEYSET_RETIRED, true));
    assert_eq!(w.issued(), 0);
    let fresh = build(&p, &w.keyset(), &[2]);
    w.ask(&fresh.req, T + 2 * H).unwrap();
    assert_eq!(w.issued(), 2);
    assert_eq!(w.record("tx1").attempts, 1);
}

#[test]
fn a_denomination_the_mint_has_no_key_for_and_a_repeated_blind_nonce_are_refused() {
    let mut w = world("content");
    let p = wallet(8);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 3000)));
    let big = build(&p, &w.keyset(), &[2048, 952]);
    assert_eq!(
        refused(w.ask(&big.req, T)).code,
        codes::UNKNOWN_DENOMINATION
    );
    let mut twice = build(&p, &w.keyset(), &[2, 1]);
    twice.req.outputs[1].blind_nonce = twice.req.outputs[0].blind_nonce;
    sign(&p, &mut twice.req);
    // the second counted attempt, which the cap of 2 allows
    assert_eq!(
        refused(w.ask(&twice.req, T)).code,
        codes::DUPLICATE_BLIND_NONCE
    );
    assert_eq!(w.issued(), 0);
}

#[test]
fn a_blind_nonce_the_mint_already_signed_is_refused() {
    let mut w = world("used");
    let p = wallet(9);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 3)));
    let built = build(&p, &w.keyset(), &[2, 1]);
    // the first output was signed before (by a payment, say)
    w.logic.issue(&built.req.outputs[0]).unwrap();
    let e = refused(w.ask(&built.req, T));
    assert_eq!(
        (e.code.as_str(), e.counted),
        (codes::BLIND_NONCE_USED, true)
    );
    assert_eq!(w.issued(), 2, "only what was signed before");
}

#[test]
fn nothing_is_due_when_the_fees_take_it_all() {
    let mut w = world("nothing");
    let p = wallet(10);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 0)));
    let built = build(&p, &w.keyset(), &[1]);
    let e = refused(w.ask(&built.req, T));
    assert_eq!(
        (e.code.as_str(), e.counted),
        (codes::NOTHING_TO_ISSUE, true)
    );
    assert_eq!(w.issued(), 0);
}

// ---- requests that are ignored and change nothing ----

#[test]
fn the_signature_must_be_the_reply_keys() {
    let p = wallet(11);
    let mut w = world("signature");
    let built = build(&p, &w.keyset(), &[2, 1]);
    reconcile::verify_signature(&built.req).unwrap();
    // changed after signing
    let mut tampered = built.req.clone();
    tampered.outputs[0].amount = Amount::from_units(4);
    let e = reconcile::verify_signature(&tampered).unwrap_err();
    assert_eq!((e.code.as_str(), e.counted), (codes::BAD_SIGNATURE, false));
    // signed by another key
    let mut forged = built.req.clone();
    forged.signature = SECP256K1.sign_schnorr(&forged.digest(), &wallet(12).key);
    assert_eq!(
        reconcile::verify_signature(&forged).unwrap_err().code,
        codes::BAD_SIGNATURE
    );
    // the shape: at least one output and a keyset, and not too many
    let mut empty = built.req.clone();
    empty.outputs.clear();
    assert_eq!(
        reconcile::check_shape(&empty, 10).unwrap_err().code,
        codes::MALFORMED_REQUEST
    );
    assert_eq!(
        reconcile::check_shape(&built.req, 1).unwrap_err().code,
        codes::REQUEST_TOO_LARGE
    );
    reconcile::check_shape(&built.req, 2).unwrap();
    // nothing was recorded
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 3)));
    assert_eq!(w.record("tx1").attempts, 0);
}

#[test]
fn a_request_for_a_payment_the_mint_does_not_have_is_ignored_and_not_counted() {
    let mut w = world("unknown");
    let p = wallet(13);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 3)));
    // another purchase of the same key
    let mut other = wallet(14);
    other.key = p.key;
    other.did = p.did;
    let built = build(&other, &w.keyset(), &[2, 1]);
    let e = refused(w.ask(&built.req, T));
    assert_eq!(
        (e.code.as_str(), e.counted),
        (codes::UNKNOWN_PAYMENT, false)
    );
    // the purchase of the payment, from another reply key
    let mut stranger = wallet(13);
    stranger.purchase = p.purchase;
    let built = build(&stranger, &w.keyset(), &[2, 1]);
    assert_eq!(refused(w.ask(&built.req, T)).code, codes::UNKNOWN_PAYMENT);
    assert_eq!(w.record("tx1").attempts, 0);
    assert_eq!(w.issued(), 0);
}

#[test]
fn a_payment_that_is_not_open_is_ignored_and_not_counted() {
    let mut w = world("closed");
    let p = wallet(15);
    // no rules: the payment was never open
    let mut never = held(&p, rules(SEND_CORRECTED_REQUEST, 3));
    never.rules = None;
    never.open_until = None;
    w.put("never", never);
    let built = build(&p, &w.keyset(), &[2, 1]);
    assert_eq!(refused(w.ask(&built.req, T)).code, codes::PAYMENT_CLOSED);
    assert_eq!(w.record("never").attempts, 0);

    // past its deadline, forfeited or not
    let q = wallet(16);
    w.put("late", held(&q, rules(SEND_CORRECTED_REQUEST, 3)));
    let built = build(&q, &w.keyset(), &[2, 1]);
    let e = refused(w.ask(&built.req, OPEN));
    assert_eq!((e.code.as_str(), e.counted), (codes::PAYMENT_CLOSED, false));
    w.records.forfeit_expired(OPEN).unwrap();
    assert!(w.record("late").forfeited_at.is_some());
    assert_eq!(refused(w.ask(&built.req, T)).code, codes::PAYMENT_CLOSED);

    // settled
    let r = wallet(17);
    w.put("done", held(&r, rules(SEND_CORRECTED_REQUEST, 3)));
    let first = build(&r, &w.keyset(), &[2, 1]);
    w.ask(&first.req, T).unwrap();
    let again = build(&r, &w.keyset(), &[2, 1]);
    assert_eq!(
        refused(w.ask(&again.req, T + 1)).code,
        codes::PAYMENT_CLOSED
    );
    assert_eq!(w.issued(), 3, "settled once");
    assert_eq!(w.record("done").attempts, 0);
}

#[test]
fn two_payments_with_one_reply_key_and_purchase_are_ambiguous_and_ignored() {
    let mut w = world("ambiguous");
    let p = wallet(18);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 3)));
    w.put("tx2", held(&p, rules(SEND_CORRECTED_REQUEST, 3)));
    let built = build(&p, &w.keyset(), &[2, 1]);
    let e = refused(w.ask(&built.req, T));
    assert_eq!(
        (e.code.as_str(), e.counted),
        (codes::AMBIGUOUS_PAYMENT, false)
    );
    assert_eq!(w.issued(), 0);
    assert_eq!((w.record("tx1").attempts, w.record("tx2").attempts), (0, 0));
}

#[test]
fn a_duplicate_payment_never_matches_a_request() {
    let mut w = world("duplicate");
    let p = wallet(19);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 3)));
    // the second payment of the payload: kept, no payer and no delivery
    let mut dup = Record::locked(BTC_10K, false);
    dup.status = Status::Rejected;
    dup.value_sats = 9_000;
    dup.duplicate_of = Some("tx1".to_string());
    w.put("tx2", dup);
    let built = build(&p, &w.keyset(), &[2, 1]);
    w.ask(&built.req, T).unwrap();
    assert_eq!(w.issued(), 3);
    let recon = w.records.reconciliation();
    assert_eq!(
        (recon.duplicates.payments, recon.duplicates.sats),
        (1, 9_000)
    );
    assert_eq!(recon.settled.payments, 1);
}

// ---- crash safety ----

#[test]
fn a_crash_between_pending_and_close_is_finished_by_the_resend_with_one_issue() {
    let mut w = world("crash");
    let p = wallet(20);
    let built = build(&p, &w.keyset(), &[4, 2, 1]);
    // the state a crash after the first signature leaves: pending is saved, one
    // output is signed (and counted by the mint), the answer is not stored
    let mut record = held(&p, rules(SEND_CORRECTED_REQUEST, 7));
    record.reconcile_pending = Some(PendingReconcile {
        hash: built.req.hash(),
        epoch: w.rotor.current.epoch,
        keyset: w.keyset(),
        nonces: built
            .req
            .outputs
            .iter()
            .map(|o| hex::encode(o.blind_nonce.0.serialize()))
            .collect(),
    });
    w.put("tx1", record);
    w.logic.issue(&built.req.outputs[0]).unwrap();
    assert_eq!(w.issued(), 4);
    // another request meanwhile is told the payment is being worked on
    let other = build(&p, &w.keyset(), &[4, 2, 1]);
    let e = refused(w.ask(&other.req, T));
    assert_eq!((e.code.as_str(), e.counted), (codes::IN_PROGRESS, false));
    // the identical request finishes it: all three signed, the first again
    let answer = w.ask(&built.req, T).unwrap();
    assert_eq!(answer.signatures.len(), 3);
    assert_eq!(
        w.issued(),
        7,
        "issued once, the first output is not counted twice"
    );
    w.notes(&built, &answer);
    let rec = w.record("tx1");
    assert!(rec.reconcile_pending.is_none());
    assert_eq!(rec.settled_at, Some(T));
    assert_eq!(rec.attempts, 0);
}

#[test]
fn an_accepted_request_is_finished_after_the_window_closes() {
    let mut w = world("pending-forfeit");
    let p = wallet(21);
    let built = build(&p, &w.keyset(), &[2, 1]);
    let mut record = held(&p, rules(SEND_CORRECTED_REQUEST, 3));
    record.reconcile_pending = Some(PendingReconcile {
        hash: built.req.hash(),
        epoch: 0,
        keyset: w.keyset(),
        nonces: Vec::new(),
    });
    w.put("tx1", record);
    // the window closes with the request pending: the payment is not forfeited
    assert!(w.records.forfeit_expired(OPEN + 1_000).unwrap().is_empty());
    assert!(w.record("tx1").forfeited_at.is_none());
    // and the identical resend, after the deadline, is finished
    let answer = w.ask(&built.req, OPEN + 1_000).unwrap();
    assert_eq!(answer.issued_units, 3);
    assert_eq!(w.issued(), 3);
    assert!(w.record("tx1").forfeited_at.is_none());
}

#[test]
fn a_resend_after_the_keyset_was_deleted_is_signed_by_the_current_keyset() {
    let mut w = world("resume-retired");
    let p = wallet(22);
    let old = w.keyset();
    let built = build(&p, &old, &[2, 1]);
    let mut record = held(&p, rules(SEND_CORRECTED_REQUEST, 3));
    record.reconcile_pending = Some(PendingReconcile {
        hash: built.req.hash(),
        epoch: 0,
        keyset: old.clone(),
        nonces: Vec::new(),
    });
    w.put("tx1", record);
    w.arm();
    w.rotate_to(T + H);
    w.rotate_to(T + 2 * H);
    let current = w.keyset();
    assert_ne!(current, old);
    let answer = w.ask(&built.req, T + 2 * H).unwrap();
    assert_eq!(
        answer.keyset, current,
        "the reply names the keyset that signed"
    );
    assert_eq!(w.issued(), 3);
    w.notes(&built, &answer);
}

#[test]
fn the_rules_in_force_when_the_window_opened_are_the_ones_checked() {
    // The stored rules say 7; nothing in the settings can change that.
    let mut w = world("stored-rules");
    let p = wallet(23);
    w.put("tx1", held(&p, rules(SEND_CORRECTED_REQUEST, 7)));
    let now_it_would_be = settle::corrected_rules(
        7_000,
        BTC_10K,
        OPEN,
        &ReconcileTerms {
            fee_units: 3,
            ..ReconcileTerms::default()
        },
    );
    assert_eq!(now_it_would_be.due_units, 4);
    let by_new_settings = build(&p, &w.keyset(), &[4]);
    assert_eq!(
        refused(w.ask(&by_new_settings.req, T)).code,
        codes::WRONG_TOTAL
    );
    let by_stored = build(&p, &w.keyset(), &[4, 2, 1]);
    w.ask(&by_stored.req, T).unwrap();
    assert_eq!(w.issued(), 7);
}

// ---- through the listener's entry, and over AIR ----

/// A mint with its records, as `psp-mint serve` shares them.
struct Running {
    state: Arc<AppState>,
    rot: SharedRotating,
    records: SharedRecords,
    keyset: String,
}

fn serving(name: &str) -> Running {
    let dir = scratch(name);
    let (rotor, logic) = Rotor::open_at(&dir, Amount::from_units(1024), 1, 1, T).unwrap();
    let keyset = rotor.current.id.clone();
    let rot = Rotating::new(rotor, Rollovers::open(&dir).unwrap());
    let records: SharedRecords = Arc::new(Mutex::new(
        Records::open(&dir.join("settlements.json")).unwrap(),
    ));
    Running {
        state: server::app_state(logic),
        rot,
        records,
        keyset,
    }
}

/// Rules whose deadline is on the real clock, which the entry reads.
fn live_rules(send: &str, due: u64) -> ReconcileRules {
    ReconcileRules {
        open_until: server::now_secs() + 86_400,
        ..rules(send, due)
    }
}

impl Running {
    fn put(&self, txid: &str, record: Record) {
        self.records.lock().unwrap().put(txid, record).unwrap();
    }

    fn attempts(&self, txid: &str) -> u32 {
        self.records.lock().unwrap().get(txid).unwrap().attempts
    }

    fn issued(&self) -> u64 {
        self.state
            .logic
            .lock()
            .unwrap()
            .audit_totals()
            .unwrap()
            .0
            .units
    }

    fn serve(&self, body: &[u8]) -> Option<Served> {
        server::air_served_with(
            &self.state,
            &self.rot,
            Some(&self.records),
            &Health::new(100),
            &Limits::default(),
            KIND_PSP_RECONCILE,
            body,
        )
    }
}

fn ignored(served: Option<Served>) -> String {
    match served {
        Some(Served::Ignored(why)) => why,
        Some(Served::Body(_)) => panic!("the request was answered"),
        Some(_) => panic!("another kind of outcome"),
        None => panic!("the kind is not served"),
    }
}

#[test]
fn the_listener_answers_an_accepted_request_and_ignores_the_rest() {
    let m = serving("entry");
    let p = wallet(30);
    let rules = live_rules(SEND_CORRECTED_REQUEST, 3);
    m.put("tx1", held(&p, rules));

    // a wrong total: no reply, one attempt counted
    let wrong = build(&p, &m.keyset, &[2]);
    let why = ignored(m.serve(&serde_json::to_vec(&wrong.req).unwrap()));
    assert!(why.starts_with(codes::WRONG_TOTAL), "{why}");
    assert_eq!(m.attempts("tx1"), 1);

    // not JSON, a bad signature and an unknown payment: no reply, nothing counted
    assert!(ignored(m.serve(b"not json")).starts_with(codes::MALFORMED_REQUEST));
    let right = build(&p, &m.keyset, &[2, 1]);
    let mut forged = right.req.clone();
    forged.signature = SECP256K1.sign_schnorr(&forged.digest(), &wallet(31).key);
    let why = ignored(m.serve(&serde_json::to_vec(&forged).unwrap()));
    assert!(why.starts_with(codes::BAD_SIGNATURE), "{why}");
    let stranger = wallet(32);
    let unknown = build(&stranger, &m.keyset, &[2, 1]);
    let why = ignored(m.serve(&serde_json::to_vec(&unknown.req).unwrap()));
    assert!(why.starts_with(codes::UNKNOWN_PAYMENT), "{why}");
    assert_eq!(m.attempts("tx1"), 1);
    assert_eq!(m.issued(), 0);

    // the accepted one is answered with the signatures, and again on a resend
    let body = serde_json::to_vec(&right.req).unwrap();
    let Some(Served::Body(bytes)) = m.serve(&body) else {
        panic!("not answered");
    };
    let answer: ReconcileResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!((answer.issued_units, answer.signatures.len()), (3, 2));
    assert_eq!(m.issued(), 3);
    let Some(Served::Body(again)) = m.serve(&body) else {
        panic!("a resend is answered");
    };
    assert_eq!(again, bytes);
    assert_eq!(m.issued(), 3);
}

#[test]
fn a_mint_started_without_its_records_ignores_reconciliations() {
    let m = serving("no-records");
    let p = wallet(33);
    let built = build(&p, &m.keyset, &[1]);
    let served = server::air_served_with(
        &m.state,
        &m.rot,
        None,
        &Health::new(100),
        &Limits::default(),
        KIND_PSP_RECONCILE,
        &serde_json::to_vec(&built.req).unwrap(),
    );
    assert!(ignored(served).starts_with(codes::UNAVAILABLE));
}

#[test]
fn over_air_an_accepted_request_gets_its_notes_and_a_refused_one_gets_nothing() {
    let m = serving("air");
    let p = wallet(34);
    m.put("tx1", held(&p, live_rules(SEND_CORRECTED_REQUEST, 3)));
    let air = common::Air::start_with_records(&m.state, &m.rot, &m.records, Limits::default());

    // refused: nothing is written to the reply cell, so the asker finds none
    let wrong = build(&p, &m.keyset, &[2]);
    let result = air.ask_unanswered(KIND_PSP_RECONCILE, &serde_json::to_vec(&wrong.req).unwrap());
    assert!(result.is_err(), "a refused request got a reply");
    assert_eq!(m.attempts("tx1"), 1);
    assert_eq!(m.issued(), 0);

    // accepted: the reply is the signatures, which unblind to genuine notes
    let right = build(&p, &m.keyset, &[2, 1]);
    let answer: ReconcileResponse = air.post_ok("/reconcile", &right.req);
    assert_eq!((answer.issued_units, answer.signatures.len()), (3, 2));
    let logic = m.state.logic.lock().unwrap();
    for (k, s) in right.kept.iter().zip(&answer.signatures) {
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
                spend_signature: zero_sig(),
            })
            .expect("genuine");
    }
    assert_eq!(logic.audit_totals().unwrap().0.units, 3);
}
