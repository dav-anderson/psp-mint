//! The purchase claim over AIR (kind 15): the one request that turns a payment's
//! owed record into notes.
//!
//! A payment that reached its confirmation tier has an owed record (`settle`),
//! written to its claim key by `delivery`. The holder of the claim key sends one
//! claim for exactly the units the record says. This module checks it against the
//! **stored** record (what was fixed when the record was made, never the current
//! settings, except for the keyset: a claim names the one that is current when it
//! arrives), signs the notes, and keeps the answer. It answers nothing else: a
//! claim that is not accepted gets no reply at all, and the reason is only in the
//! mint's log ([`Ignore`]).
//!
//! **The checks, in this order** ([`check_shape`], [`verify_signature`], then
//! [`claim`]):
//!
//! 1. the shape of the request (size, at least one output, a keyset named);
//! 2. the BIP340 signature by the claim key over the claim's digest, which covers
//!    this mint's DID, **before anything is looked up**, so a request without the
//!    key, or one signed for another PSP, changes nothing and learns nothing;
//! 3. the owed record it names: the claim DID and the record id, of a payment this
//!    mint owes (an unknown one is ignored);
//! 4. a claim already accepted: the identical claim gets the stored answer, any
//!    other is refused;
//! 5. the window: the record must be stored on AIR (a record the wallet cannot
//!    have read is `unknown_owed`: it has no window, and nobody can know its
//!    `due_units`), and a claim at or after the record's `open_until` (its
//!    landing plus the window) is refused, as is one for a record whose window
//!    was closed by `expire_unclaimed`;
//! 6. the keyset: it must be the **current** one. The previous set, a retired
//!    set and an unknown one are each refused (`keyset_not_current`,
//!    `keyset_retired`, `unknown_keyset`) and **none of the three is counted**: a
//!    wallet that is late or has a stale keyset list asks kind 4 again and sends a
//!    claim that names the current one;
//! 7. the content: denominations the mint has, no repeated blind nonce, outputs
//!    that add up to **exactly** `due_units`, and blind nonces never signed
//!    before;
//! 8. sign, and keep the answer by the claim's hash.
//!
//! **What is counted.** A claim signed by the right key for an open record,
//! refused for its content (step 7), with a new hash, is an attempt: it is added
//! to the record (`attempts`, `refusals`) and the record accepts
//! `terms.max_attempts` of them. An unknown record (or one not stored on AIR yet),
//! a bad signature, a keyset that is not the current one (step 6), a claim after
//! the window or after the accepted one, a resend of a refused claim, and a
//! transient failure are ignored and not counted.
//!
//! **Crash safety, as a rollover.** The accepted claim is saved as pending on the
//! record, in full (every output to sign), before anything is signed; the notes
//! are signed (a blind nonce that is already used, on a resume, is signed again
//! with `bdhke::sign_message`, which gives the same signature and does not count it
//! twice); then the answer and the cleared pending entry go in one write.
//! [`finish`] does this part, and is also called for every pending claim at
//! startup and at the top of each pass, so a claim that was accepted is finished
//! without the wallet sending it again; the wallet's identical resend then gets the
//! stored answer. `Records::expire_unclaimed` skips a record with a pending claim,
//! so a claim accepted before the window closed is finished after it.
//!
//! **Recovery signs with the current keyset.** [`finish`] signs with the keyset
//! that is current when it runs, not the one the claim named. Normally that is the
//! same one, and the signatures are the same as the first time. If the mint
//! rotated between the acceptance and the recovery, the answer names the new set
//! (`ClaimResponse::keyset`, with its keys in `keysets`), and the wallet unblinds
//! with the key of the set that signed. Every set has a key for every power of
//! two, 2^0 to 2^63, so the new set can sign any accepted claim. The previous set
//! exists only for legacy notes (rollover, spend, check-state), never for a claim.
//!
//! **The issued total after a rotation.** Outputs that the old set had already
//! signed before the rotation were signed once under it and count in its issued
//! total; no wallet ever got them. The new set signs the whole claim again and
//! counts it in its own total. So the mint's issued total, summed over its sets,
//! is higher than the units claimed by **at most the outputs of the claims that
//! were pending (accepted and not answered) when the mint rotated**, each counted
//! as far as the old set had signed it: at most those claims' `due_units`.
//! Normally there is no such claim: a claim is accepted and answered in one step,
//! and [`finish`] runs for any that is not at startup and at the top of every scan
//! pass, before the keysets rotate. After a crash there is one at most, because
//! claims are taken one at a time under the records lock, so a crash leaves at most
//! one mid-signing. A claim whose answer could not be written also stays pending
//! with every output signed, for as long as recovery keeps failing, and a rotation
//! asked for through the API can come in that time. The sum `/audit` shows as
//! issued is not changed or corrected by the mint, and no counter is kept; the
//! claimed units (`claims.claimed.units`) are exact.
//!
//! **Locks.** The caller holds the records, then the mint, then the rotor, in that
//! order everywhere. Nothing in here waits on the network.

use std::collections::BTreeMap;

use mint_core::MintLogic;
use mint_types::{Amount, MintOutputError, MintOutputOutcome};
use secp256k1::SECP256K1;
use sp_wire::ServiceDid;
use sp_wire::codes;
use sp_wire::psp::{ClaimRequest, ClaimResponse, KeysetsResponse, SignedOutput};

use crate::records::{Claimed, PendingClaim, Records, Refusal, Status};
use crate::rotation::{Rotor, Side};

/// A request that was not accepted: no reply is written. The code and the reason
/// are for the mint's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ignore {
    /// One of `sp_wire::codes` (or `unavailable` for a failure of the mint that a
    /// resend of the identical claim meets).
    pub code: String,
    pub why: String,
    /// True when the claim was counted against the record's attempts.
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

    /// A failure of the mint, not of the claim: not counted, and the wallet sends
    /// the identical claim again.
    fn transient(why: impl Into<String>) -> Self {
        Ignore::not_counted(codes::UNAVAILABLE, why)
    }
}

/// The shape of a claim, checked before any lock is taken: at most `max_items`
/// outputs, at least one, and a keyset named. Not counted.
pub fn check_shape(req: &ClaimRequest, max_items: usize) -> Result<(), Ignore> {
    if req.outputs.len() > max_items {
        return Err(Ignore::not_counted(
            codes::REQUEST_TOO_LARGE,
            format!("{} outputs, at most {max_items}", req.outputs.len()),
        ));
    }
    if req.outputs.is_empty() || req.keyset.is_empty() {
        return Err(Ignore::not_counted(
            codes::MALFORMED_REQUEST,
            "a claim names a keyset and carries at least one output",
        ));
    }
    Ok(())
}

/// The proof of the claim key: the claim's BIP340 signature over its digest,
/// checked against the key the claim names. `psp` is this mint's DID: the digest
/// covers it, so a claim signed for another PSP fails here. Not counted when it
/// fails.
pub fn verify_signature(req: &ClaimRequest, psp: &ServiceDid) -> Result<(), Ignore> {
    SECP256K1
        .verify_schnorr(&req.signature, &req.digest(psp), &req.claim_did.x_only())
        .map_err(|_| {
            Ignore::not_counted(
                codes::BAD_SIGNATURE,
                "the signature does not verify under the claim key for this mint",
            )
        })
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

/// The claim must name the keyset that is current now. The previous set, a
/// retired set and an unknown set are each refused with their own code, and none
/// is counted.
fn check_keyset(req: &ClaimRequest, rotor: &Rotor) -> Result<(), Ignore> {
    match rotor.resolve(&req.keyset) {
        Ok(Side::Current) => Ok(()),
        Ok(Side::Previous) => Err(Ignore::not_counted(
            codes::KEYSET_NOT_CURRENT,
            format!(
                "keyset {} is the previous keyset; a claim names the current one, {}",
                req.keyset, rotor.current.id
            ),
        )),
        Err(e) => Err(Ignore::not_counted(e.code(), e.why("the claim"))),
    }
}

/// The checks against the stored record, cheapest first. A failure is counted.
/// The outputs are checked against the current keyset ([`check_keyset`] has
/// already found that the claim names it).
fn check_content(
    req: &ClaimRequest,
    due_units: u64,
    current: &MintLogic,
    rotor: &Rotor,
) -> Result<(), Check> {
    let (logic, keys) = (current, &rotor.current);
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
    let total: u128 = req.outputs.iter().map(|o| u128::from(o.amount.units)).sum();
    if total != u128::from(due_units) {
        return Err(refuse(
            codes::WRONG_TOTAL,
            format!("the outputs add up to {total}, the record says {due_units}"),
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

/// Claim one payment. `records` is held by the caller, then the mint (`current`),
/// then the rotor, in that order. `now` is Unix seconds. The signature has been
/// checked ([`verify_signature`]). `psp` is this mint's DID, which the claim's
/// hash covers.
///
/// Returns the answer (a new one, or the stored one for an identical resend), or
/// why the claim was ignored. A failure to save is [`Ignore::transient`]: the
/// identical claim, sent again, finds the pending entry and resumes.
pub fn claim(
    records: &mut Records,
    current: &MintLogic,
    rotor: &Rotor,
    req: &ClaimRequest,
    psp: &ServiceDid,
    now: u64,
) -> Result<ClaimResponse, Ignore> {
    let hash = req.hash(psp);
    let Some(txid) = records.owed_for(&req.claim_did, &req.record_id) else {
        return Err(Ignore::not_counted(
            codes::UNKNOWN_OWED,
            "no payment of this mint owes this claim key this record",
        ));
    };
    let record = records.get(&txid).expect("matched").clone();
    if record.status != Status::Owed {
        return Err(Ignore::not_counted(
            codes::UNKNOWN_OWED,
            "this payment is not owed",
        ));
    }
    // A record that is not stored on AIR has no window and cannot have been read:
    // to a claimant it does not exist. Not counted.
    let Some(open_until) = record.landed_at.and(record.open_until) else {
        return Err(Ignore::not_counted(
            codes::UNKNOWN_OWED,
            "the owed record of this payment is not stored on AIR yet",
        ));
    };

    // An identical resend of the accepted claim gets its stored answer; any other
    // claim for it is too late.
    if let Some(done) = &record.claimed {
        if done.hash == hash {
            return Ok(done.response.clone());
        }
        return Err(Ignore::not_counted(
            codes::ALREADY_CLAIMED,
            "this record was claimed once already",
        ));
    }
    if let Some(pending) = &record.claim_pending {
        if pending.hash == hash {
            return finish(records, current, rotor, &txid, now);
        }
        return Err(Ignore::not_counted(
            codes::IN_PROGRESS,
            "another claim for this record is being worked on",
        ));
    }
    if let Some(earlier) = record.refusals.iter().find(|r| r.hash == hash) {
        return Err(Ignore::not_counted(
            &earlier.code,
            "this claim was refused before; it is not counted again",
        ));
    }
    let terms = match &record.delivery {
        Some(d) => match &d.outcome {
            sp_wire::air_request::delivery::Outcome::Owed(owed) => owed.terms.clone(),
        },
        None => {
            return Err(Ignore::not_counted(
                codes::UNKNOWN_OWED,
                "this payment has no owed record",
            ));
        }
    };
    if record.expired_at.is_some() || now >= open_until {
        return Err(Ignore::not_counted(
            codes::CLAIM_EXPIRED,
            "the claim window of this record is closed",
        ));
    }
    if record.attempts >= terms.max_attempts {
        return Err(Ignore::not_counted(
            codes::TOO_MANY_ATTEMPTS,
            format!("this record has used its {} attempts", terms.max_attempts),
        ));
    }
    check_keyset(req, rotor)?;
    match check_content(req, terms.due_units, current, rotor) {
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
            records
                .put(&txid, counted)
                .map_err(|e| Ignore::transient(format!("the attempt could not be saved: {e}")))?;
            return Err(Ignore::counted(&code, why));
        }
    }
    // Accepted. Pending first, in full, before anything is signed.
    let mut pending = record;
    pending.claim_pending = Some(PendingClaim {
        hash,
        epoch: rotor.current.epoch,
        keyset: rotor.current.id.clone(),
        outputs: req.outputs.clone(),
    });
    records
        .put(&txid, pending)
        .map_err(|e| Ignore::transient(format!("the claim could not be saved: {e}")))?;
    finish(records, current, rotor, &txid, now)
}

/// Do the accepted claim of payment `txid`: sign every output of its pending
/// entry and, in one write, keep the answer and clear the entry. For a claim that
/// was just accepted and for one a crash left pending, which no resend is needed
/// for.
///
/// The outputs are signed with the keyset that is **current** when this runs,
/// whatever the claim named, and the answer names it. A blind nonce that is
/// already used was signed before the crash; it is signed again with the same key,
/// which gives the same signature and does not count it twice. If the mint rotated
/// in between, the new set has not seen the nonce and signs it as new (see the
/// module docs for what that does to the issued totals).
pub fn finish(
    records: &mut Records,
    current: &MintLogic,
    rotor: &Rotor,
    txid: &str,
    now: u64,
) -> Result<ClaimResponse, Ignore> {
    let Some(record) = records.get(txid).cloned() else {
        return Err(Ignore::transient(format!("no record {txid}")));
    };
    let Some(pending) = record.claim_pending.clone() else {
        return Err(Ignore::transient(format!("{txid} has no pending claim")));
    };
    let (logic, keys) = (current, &rotor.current);
    let mut signatures = Vec::with_capacity(pending.outputs.len());
    for output in &pending.outputs {
        // Defensive only, and unreachable for an accepted claim: the content check
        // admits only powers of two (`unknown_denomination`), and every keyset has
        // all 64 of them. The lookup stays so that a damaged pending entry is an
        // error and not a panic.
        let Some(sk) = keys.sk.get(&output.amount) else {
            return Err(Ignore::transient(format!(
                "no key for {} units in keyset {}",
                output.amount.units, keys.id
            )));
        };
        let signed = match logic.issue_with_key(sk, output) {
            Ok(signed) => signed,
            Err(MintOutputError::BlindNonceAlreadyUsed) => {
                MintOutputOutcome(bdhke::sign_message(sk, &output.blind_nonce.0))
            }
            Err(e) => return Err(Ignore::transient(e.to_string())),
        };
        signatures.push(SignedOutput {
            blind_nonce: output.blind_nonce.0.to_string(),
            signature: signed.0.to_string(),
        });
    }
    let response = ClaimResponse {
        keyset: keys.id.clone(),
        signatures,
        keysets: keysets_of(
            current,
            rotor.previous.as_ref().map(|(k, l)| (k.id.as_str(), l)),
        ),
        issued_units: record.due_units,
        read_prepaid_units: record.read_prepaid_units,
        write_prepaid_units: record.write_prepaid_units,
    };
    let mut done = record;
    done.claim_pending = None;
    done.claimed_at = Some(now);
    done.claimed = Some(Claimed {
        hash: pending.hash,
        response: response.clone(),
    });
    records
        .put(txid, done)
        .map_err(|e| Ignore::transient(format!("the answer could not be saved: {e}")))?;
    Ok(response)
}

/// The PSP's live keysets as the keysets call gives them, from the mint (current)
/// and, when it rotates, the previous one: the answer to a claim carries them so
/// that the wallet can unblind from it alone.
pub fn keysets_of(current: &MintLogic, previous: Option<(&str, &MintLogic)>) -> KeysetsResponse {
    let units =
        |keys: &BTreeMap<Amount, secp256k1::PublicKey>| -> BTreeMap<u64, secp256k1::PublicKey> {
            keys.iter()
                .map(|(amount, key)| (amount.units, *key))
                .collect()
        };
    let current_id = crate::rotation::keyset_id(current.public_keys());
    let mut keysets: BTreeMap<String, BTreeMap<u64, secp256k1::PublicKey>> = BTreeMap::new();
    keysets.insert(current_id.clone(), units(current.public_keys()));
    if let Some((id, logic)) = previous {
        keysets.insert(id.to_string(), units(logic.public_keys()));
    }
    KeysetsResponse {
        current: current_id,
        previous: previous.map(|(id, _)| id.to_string()),
        keysets,
    }
}
