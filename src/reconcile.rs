//! The PSP's reconciliation over AIR (kind 15): the corrected request and the
//! credit claim for a payment that is open.
//!
//! A payment is open when it was held (paid, but short beyond the reconciliation
//! target), rejected with a readable payload, or issued with a credit owed. The
//! first delivery told the payer the rules (`ReconcileRules`): what one request
//! must carry. The holder of the purchase's one-off reply key sends one request
//! that follows them; this module checks it against the **stored** rules (the ones
//! fixed when the window opened, never the current settings), signs the new notes,
//! and closes the payment. It answers nothing else: a request that is not accepted
//! gets no reply at all, and the reason is only in the mint's log
//! ([`Ignore`]).
//!
//! **Who may ask.** AIR clients are anonymous, so the request carries a BIP340
//! signature by the reply key ([`verify_signature`]). A bad signature is ignored
//! and changes nothing. Past that, a request is matched to its payment by the reply
//! key and the purchase id ([`Records::matching`]).
//!
//! **What is counted.** A request signed by the right reply key, for an open
//! payment, refused for its content, with a new hash, is an attempt: it is added
//! to the record (`attempts`, `refusals`) and the payment accepts
//! `rules.max_attempts` of them. An unknown payment, a bad signature, a closed
//! payment, a request that is a resend of a refused one, and a transient failure
//! are ignored and not counted.
//!
//! **Crash safety, as a rollover.** The request is saved as pending on the
//! payment's record before anything is signed; the notes are signed (a blind nonce
//! that is already used, on a resume, is signed again with `bdhke::sign_message`,
//! which gives the same signature and does not count it twice); then the answer,
//! `settled_at` and the cleared pending entry go in one write. An identical resend
//! gets the stored answer and signs nothing more; a resend after a crash finishes
//! the request. `Records::forfeit_expired` skips a payment with a pending
//! reconciliation, so a request accepted before the window closed is finished
//! after it.
//!
//! **Locks.** The caller holds the records, then the mint, then the rotor, in that
//! order everywhere. Nothing in here waits on the network.

use mint_core::MintLogic;
use mint_types::{MintOutputError, MintOutputOutcome};
use secp256k1::SECP256K1;
use sp_wire::air_request::delivery::SEND_CREDIT_CLAIM;
use sp_wire::codes;
use sp_wire::psp::{KeysetsResponse, ReconcileRequest, ReconcileResponse, SignedOutput};

use crate::records::{PendingReconcile, Reconciled, Records, Refusal, Status};
use crate::rotation::{GenKeys, Rotor, Side};

/// A request that was not accepted: no reply is written. The code and the reason
/// are for the mint's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ignore {
    /// One of `sp_wire::codes` (or `in_progress`, or `unavailable` for a failure
    /// of the mint that a resend of the identical request meets).
    pub code: String,
    pub why: String,
    /// True when the request was counted against the payment's attempts.
    pub counted: bool,
}

impl Ignore {
    fn not_counted(code: &str, why: impl Into<String>) -> Self {
        Ignore {
            code: code.to_string(),
            why: why.into(),
            counted: false,
        }
    }

    fn counted(code: &str, why: impl Into<String>) -> Self {
        Ignore {
            code: code.to_string(),
            why: why.into(),
            counted: true,
        }
    }

    /// A failure of the mint, not of the request: not counted, and the asker sends
    /// the identical request again.
    fn transient(why: impl Into<String>) -> Self {
        Ignore::not_counted(codes::UNAVAILABLE, why)
    }
}

/// The shape of a request, checked before any lock is taken: at most `max_items`
/// outputs, at least one, and a keyset named. Not counted.
pub fn check_shape(req: &ReconcileRequest, max_items: usize) -> Result<(), Ignore> {
    if req.outputs.len() > max_items {
        return Err(Ignore::not_counted(
            codes::REQUEST_TOO_LARGE,
            format!("{} outputs, at most {max_items}", req.outputs.len()),
        ));
    }
    if req.outputs.is_empty() || req.keyset.is_empty() {
        return Err(Ignore::not_counted(
            codes::MALFORMED_REQUEST,
            "a reconciliation names a keyset and carries at least one output",
        ));
    }
    Ok(())
}

/// The proof of the reply key: the request's BIP340 signature over its digest,
/// checked against the key the request names. Not counted when it fails.
pub fn verify_signature(req: &ReconcileRequest) -> Result<(), Ignore> {
    SECP256K1
        .verify_schnorr(&req.signature, &req.digest(), &req.reply_did.x_only())
        .map_err(|_| {
            Ignore::not_counted(
                codes::BAD_SIGNATURE,
                "the signature does not verify under the reply key",
            )
        })
}

/// The live keysets as the keysets call gives them, so the wallet unblinds from
/// the reply alone.
pub fn keysets_of(rotor: &Rotor) -> KeysetsResponse {
    let units = |keys: &GenKeys| {
        keys.pk
            .iter()
            .map(|(amount, key)| (amount.units, *key))
            .collect()
    };
    let mut keysets = std::collections::BTreeMap::new();
    keysets.insert(rotor.current.id.clone(), units(&rotor.current));
    if let Some((previous, _)) = &rotor.previous {
        keysets.insert(previous.id.clone(), units(previous));
    }
    KeysetsResponse {
        current: rotor.current.id.clone(),
        previous: rotor.previous.as_ref().map(|(k, _)| k.id.clone()),
        keysets,
    }
}

/// What the content checks found.
enum Check {
    /// Refused for its content: counted.
    Refuse(String, String),
    /// The mint could not check: not counted.
    Transient(String),
}

fn refuse(code: &str, why: impl Into<String>) -> Check {
    Check::Refuse(code.to_string(), why.into())
}

/// The checks against the stored rules, cheapest first. A failure is counted.
fn check_content(
    req: &ReconcileRequest,
    due_units: u64,
    current: &MintLogic,
    rotor: &Rotor,
) -> Result<(), Check> {
    let side = rotor
        .resolve(&req.keyset)
        .map_err(|e| refuse(e.code(), e.why("the outputs")))?;
    let (logic, keys) = generation(side, current, rotor);
    for (o, output) in req.outputs.iter().enumerate() {
        if !keys.sk.contains_key(&output.amount) {
            return Err(refuse(
                codes::UNKNOWN_DENOMINATION,
                format!(
                    "output {o}: {} is not a denomination of this mint",
                    output.amount.units
                ),
            ));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for (o, output) in req.outputs.iter().enumerate() {
        if !seen.insert(output.blind_nonce) {
            return Err(refuse(
                codes::DUPLICATE_BLIND_NONCE,
                format!("output {o} repeats a blind nonce"),
            ));
        }
    }
    // With nothing due there is nothing to issue, whatever is asked for.
    if due_units == 0 {
        return Err(refuse(
            codes::NOTHING_TO_ISSUE,
            "the fees leave nothing to issue for this payment",
        ));
    }
    let total: u128 = req.outputs.iter().map(|o| u128::from(o.amount.units)).sum();
    if total != u128::from(due_units) {
        return Err(refuse(
            codes::WRONG_TOTAL,
            format!("the outputs add up to {total}, the rules say {due_units}"),
        ));
    }
    for (o, output) in req.outputs.iter().enumerate() {
        let used = logic
            .is_blind_nonce_used(&output.blind_nonce)
            .map_err(|e| Check::Transient(e.to_string()))?;
        if used {
            return Err(refuse(
                codes::BLIND_NONCE_USED,
                format!("output {o}: the blind nonce was already signed"),
            ));
        }
    }
    Ok(())
}

fn generation<'a>(
    side: Side,
    current: &'a MintLogic,
    rotor: &'a Rotor,
) -> (&'a MintLogic, &'a GenKeys) {
    match side {
        Side::Current => (current, &rotor.current),
        Side::Previous => {
            let (keys, logic) = rotor.previous.as_ref().expect("resolved");
            (logic, keys)
        }
    }
}

/// Reconcile one payment. `records` is held by the caller, then the mint
/// (`current`), then the rotor, in that order. `now` is Unix seconds.
///
/// Returns the answer (a new one, or the stored one for an identical resend), or
/// why the request was ignored. A failure to save is [`Ignore::transient`]: the
/// identical request, sent again, finds the pending entry and resumes.
pub fn reconcile(
    records: &mut Records,
    current: &MintLogic,
    rotor: &Rotor,
    req: &ReconcileRequest,
    now: u64,
) -> Result<ReconcileResponse, Ignore> {
    let hash = req.hash();
    let found = records.matching(&req.reply_did.to_hex(), &hex::encode(req.purchase));
    let txid = match found.as_slice() {
        [] => {
            return Err(Ignore::not_counted(
                codes::UNKNOWN_PAYMENT,
                "no payment of this mint carries this reply key and purchase",
            ));
        }
        [one] => one.clone(),
        _ => {
            return Err(Ignore::not_counted(
                codes::AMBIGUOUS_PAYMENT,
                format!("{} payments carry this reply key and purchase", found.len()),
            ));
        }
    };
    let record = records.get(&txid).expect("matched").clone();

    // An identical resend of an accepted request gets its stored answer.
    if let Some(done) = &record.reconciled {
        if done.hash == hash {
            return Ok(done.response.clone());
        }
    }
    let resumed = match &record.reconcile_pending {
        Some(pending) if pending.hash == hash => true,
        Some(_) => {
            return Err(Ignore::not_counted(
                codes::IN_PROGRESS,
                "another request for this payment is being worked on",
            ));
        }
        None => false,
    };
    let Some(rules) = record.rules.clone() else {
        return Err(Ignore::not_counted(
            codes::PAYMENT_CLOSED,
            "this payment has no rules: it was never open",
        ));
    };

    if !resumed {
        if let Some(earlier) = record.refusals.iter().find(|r| r.hash == hash) {
            return Err(Ignore::not_counted(
                &earlier.code,
                "this request was refused before; it is not counted again",
            ));
        }
        if record.forfeited_at.is_some() || record.settled_at.is_some() || now >= rules.open_until {
            return Err(Ignore::not_counted(
                codes::PAYMENT_CLOSED,
                "this payment is forfeited, settled, or past its deadline",
            ));
        }
        if record.attempts >= rules.max_attempts {
            return Err(Ignore::not_counted(
                codes::TOO_MANY_ATTEMPTS,
                format!("this payment has used its {} attempts", rules.max_attempts),
            ));
        }
        match check_content(req, rules.due_units, current, rotor) {
            Ok(()) => {}
            Err(Check::Transient(why)) => return Err(Ignore::transient(why)),
            Err(Check::Refuse(code, why)) => {
                // One counted attempt, in one write.
                let mut counted = record;
                counted.attempts = counted.attempts.saturating_add(1);
                counted.refusals.push(Refusal {
                    hash,
                    code: code.clone(),
                    at: now,
                });
                records.put(&txid, counted).map_err(|e| {
                    Ignore::transient(format!("the attempt could not be saved: {e}"))
                })?;
                return Err(Ignore::counted(&code, why));
            }
        }
        // Accepted. Pending first, before anything is signed.
        let mut pending = record.clone();
        pending.reconcile_pending = Some(PendingReconcile {
            hash: hash.clone(),
            epoch: rotor.current.epoch,
            keyset: req.keyset.clone(),
            nonces: req
                .outputs
                .iter()
                .map(|o| hex::encode(o.blind_nonce.0.serialize()))
                .collect(),
        });
        records
            .put(&txid, pending)
            .map_err(|e| Ignore::transient(format!("the request could not be saved: {e}")))?;
    }

    // Sign. The generation the request names; a resumed request whose keyset was
    // deleted since is signed by the current one, and the reply names it.
    let (logic, keys) = match rotor.resolve(&req.keyset) {
        Ok(side) => generation(side, current, rotor),
        Err(_) if resumed => (current, &rotor.current),
        Err(e) => return Err(Ignore::transient(e.why("the outputs"))),
    };
    let mut signatures = Vec::with_capacity(req.outputs.len());
    for output in &req.outputs {
        let Some(sk) = keys.sk.get(&output.amount) else {
            return Err(Ignore::transient(format!(
                "no key for {} units in keyset {}",
                output.amount.units, keys.id
            )));
        };
        let signed = match logic.issue_with_key(sk, output) {
            Ok(signed) => signed,
            // Signed before the crash: the same blind signature again, counted
            // once (the mint's issued total has it already).
            Err(MintOutputError::BlindNonceAlreadyUsed) if resumed => {
                MintOutputOutcome(bdhke::sign_message(sk, &output.blind_nonce.0))
            }
            Err(e) => return Err(Ignore::transient(e.to_string())),
        };
        signatures.push(SignedOutput {
            blind_nonce: output.blind_nonce.0.to_string(),
            signature: signed.0.to_string(),
        });
    }

    // Close, in one write: the answer, settled, and the pending entry cleared.
    // The follow-up's read is what the rules prepay beyond the first record's.
    let record = records.get(&txid).expect("matched").clone();
    let response = ReconcileResponse {
        keyset: keys.id.clone(),
        signatures,
        keysets: keysets_of(rotor),
        issued_units: rules.due_units,
        fee_units: rules.reconciliation_fee_units,
        read_prepaid_units: rules
            .read_prepaid_units
            .saturating_sub(record.read_prepaid_units),
        write_prepaid_units: rules.write_prepaid_units,
        kept_cents: record.kept_cents,
        given_cents: record.given_cents,
    };
    let mut done = record;
    done.reconcile_pending = None;
    done.settled_at = Some(now);
    if rules.send == SEND_CREDIT_CLAIM {
        done.credit_claimed_cents = done.credit_cents;
    } else {
        done.status = Status::Issued;
        done.issued = true;
    }
    done.reconciled = Some(Reconciled {
        hash,
        response: response.clone(),
    });
    records
        .put(&txid, done)
        .map_err(|e| Ignore::transient(format!("the answer could not be saved: {e}")))?;
    Ok(response)
}
