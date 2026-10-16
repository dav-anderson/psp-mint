use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use mint_core::MintLogic;
use mint_types::{Amount, BlindNonce, MintOutput, MintOutputError, MintOutputOutcome};

use crate::payload::{self, DID_LEN, DecodeError};
use crate::records::{Record, Records, RecordsError, Status};
use crate::scan::Received;

/// A payment is priced when it is first seen with this many confirmations.
pub const LOCK_CONFIRMATIONS: u32 = 1;
/// A payment is settled once it has this many confirmations.
pub const MIN_CONFIRMATIONS: u32 = 6;
/// The price tolerance (1%, in basis points): how far a payment may miss the
/// requested amount, in either direction, and still settle in full as if it were
/// exact. Every requested note is signed, nothing is credited, dropped or kept.
///
/// It exists because the price is locked at 1 confirmation and the payment is
/// settled at 6, an hour or so later, and BTC moves in that time. The payer
/// priced the request at one moment and the mint settles it at another, so a
/// small miss is movement in the market, not a mistake. No payer should be
/// penalised or credited for it. The operator can widen it
/// (`settle.price_tolerance`) in a volatile market. It never exceeds the
/// reconciliation target below, which the config checks at startup.
pub const PRICE_TOLERANCE_BPS: u128 = 100;
/// How far a payment may miss the requested amount before the mint stops
/// absorbing the difference and reconciles it instead (10%, in basis points).
/// This applies only to a payment that is outside the price tolerance above; it
/// is for payments that are genuinely wrong. One value covers both directions:
///
/// - Over by no more than this: the mint keeps the excess. Over by more: the
///   excess is recorded as credit for the payer's DID.
/// - Short: the smallest requested outputs are dropped until what is signed is
///   no more than what was paid. If that is within this of the amount paid, those
///   notes are signed and the mint keeps the remainder. If not, nothing is signed
///   and the payment is recorded as paid but unsettled.
pub const PAYMENT_RECONCILIATION_TARGET_BPS: u128 = 1000;

/// The settlement rules a running mint uses. The defaults are the constants
/// above; the operator sets them in `psp-mint.toml` (see `config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terms {
    /// Confirmations at which the price is locked.
    pub lock_confirmations: u32,
    /// Confirmations at which the payment is settled.
    pub min_confirmations: u32,
    /// `settle.price_tolerance`, in basis points (100 is 1%). A payment this
    /// close to the request, either way, settles in full as if exact. See
    /// [`PRICE_TOLERANCE_BPS`].
    pub price_tolerance_bps: u128,
    /// `settle.payment_reconciliation_target`, in basis points (1000 is 10%).
    /// See [`PAYMENT_RECONCILIATION_TARGET_BPS`].
    pub reconciliation_target_bps: u128,
}

impl Default for Terms {
    fn default() -> Self {
        Terms {
            lock_confirmations: LOCK_CONFIRMATIONS,
            min_confirmations: MIN_CONFIRMATIONS,
            price_tolerance_bps: PRICE_TOLERANCE_BPS,
            reconciliation_target_bps: PAYMENT_RECONCILIATION_TARGET_BPS,
        }
    }
}

// A PSP unit is ten US cents: 10 PSP = 1 USD, paid in BTC. With the price in
// USD cents per BTC, sats * price is worth that many (sat-cents):
//   1 cent = 10^8 sat-cents (100,000,000 sats per BTC)
//   1 unit = 10^9 sat-cents (10 cents per PSP unit, 10 units per USD)
const SAT_CENTS_PER_CENT: u128 = 100_000_000;
const SAT_CENTS_PER_UNIT: u128 = 1_000_000_000;

#[derive(Debug, PartialEq)]
pub enum Rejection {
    /// The transaction does not have exactly one data output.
    NoDataOutput,
    BadPayload(DecodeError),
    UnknownDenomination(Amount),
    DuplicateBlindNonce,
    BlindNonceAlreadyUsed,
    Overflow,
}

#[derive(Debug)]
pub enum SettleError {
    Records(RecordsError),
    Mint(String),
}

impl fmt::Display for SettleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettleError::Records(e) => write!(f, "{e}"),
            SettleError::Mint(e) => write!(f, "mint store: {e}"),
        }
    }
}

impl Error for SettleError {}

impl From<RecordsError> for SettleError {
    fn from(e: RecordsError) -> Self {
        SettleError::Records(e)
    }
}

#[derive(Debug, PartialEq)]
pub struct Issue {
    /// The keyset that signed (always the current one; see `Payload::keyset`).
    pub keyset: String,
    /// One result per signed output, in payload order.
    pub results: Vec<Result<MintOutputOutcome, MintOutputError>>,
    /// The blind nonce of each signed output, parallel to `results`, so a payer
    /// can match a signature to the output it asked for even when some outputs
    /// were not signed.
    pub blind_nonces: Vec<BlindNonce>,
    /// Requested units that were not signed because the payment was short. The
    /// mint keeps the value of the remainder.
    pub skipped_units: u64,
    /// Overpayment beyond the reconciliation target, in USD cents, recorded as
    /// credit for `payer_did`. 0 when the excess was within the target and kept.
    pub credit_cents: u64,
    pub over_under_bps: i64,
    /// True if the payment missed the request, but by no more than the price
    /// tolerance, so it was settled in full as if exact.
    pub within_tolerance: bool,
    /// The payer's AIR DID from the payload: who a credit is owed to.
    pub payer_did: [u8; DID_LEN],
}

/// A payment that was short by more than the reconciliation target allows. Nothing
/// was signed and the BTC is held. The record is kept under the payment's txid
/// with the payer's DID. The AIR half (asking that DID for a new request that
/// matches what was paid, checking it against this record, signing it, and marking
/// the payment settled) is not built; until it is, the payment waits.
#[derive(Debug, PartialEq, Eq)]
pub struct Unsettled {
    pub payer_did: [u8; DID_LEN],
    pub requested_units: u64,
    /// Whole units the payment buys at the locked price.
    pub paid_units: u64,
    pub value_sats: u64,
    pub over_under_bps: i64,
}

#[derive(Debug, PartialEq)]
pub enum Settlement {
    /// Not in a block yet.
    Waiting {
        confirmations: u32,
    },
    /// Price locked, waiting for 6 confirmations.
    Locked {
        confirmations: u32,
        price_cents: u64,
    },
    AlreadySettled(Status),
    Rejected(Rejection),
    /// Paid, but short beyond the reconciliation target and no set of dropped
    /// outputs gets within it. Nothing signed; the BTC is held.
    PaidUnsettled(Unsettled),
    /// No usable BTC/USD price to lock this run. No record is made and the
    /// payment is tried again on the next run.
    PriceUnavailable(String),
    Issued(Issue),
}

/// Whole PSP units a payment buys: floor(sats * usd_cents_per_btc / 10^9).
pub fn allowed_units(value_sats: u64, usd_cents_per_btc: u64) -> u64 {
    let units = u128::from(value_sats) * u128::from(usd_cents_per_btc) / SAT_CENTS_PER_UNIT;
    u64::try_from(units).unwrap_or(u64::MAX)
}

/// How a payment compares with what was requested, at the locked price.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Assessment {
    /// Basis points of the requested total, truncated toward zero. Positive is
    /// overpaid, negative underpaid.
    pub over_under_bps: i64,
    /// Whole units the payment buys.
    pub paid_units: u64,
    /// What was paid, in sat-cents (sats times the price in cents): the exact
    /// value a partial fill is measured against.
    pub paid_value: u128,
    /// True if the payment is within the price tolerance of the request, in
    /// either direction (exact counts). It then settles in full: not underpaid,
    /// no credit, and the reconciliation target does not come into it.
    pub within_tolerance: bool,
    /// True if the payment is worth less than the request by more than the price
    /// tolerance. The plan then drops outputs; it does not matter yet whether the
    /// result is within the target.
    pub underpaid: bool,
    /// Overpayment beyond the reconciliation target, in whole USD cents (rounded
    /// down). 0 when the payment is short, exact, or over by no more than the
    /// target (the mint keeps that excess).
    pub credit_cents: u64,
}

fn basis_points(paid: u128, required: u128) -> i64 {
    if required == 0 {
        return 0;
    }
    let (under, diff) = if paid >= required {
        (false, paid - required)
    } else {
        (true, required - paid)
    };
    let bps = diff
        .checked_mul(10_000)
        .map(|x| x / required)
        .unwrap_or(u128::MAX);
    let bps = i64::try_from(bps).unwrap_or(i64::MAX);
    if under { -bps } else { bps }
}

pub fn assess(value_sats: u64, usd_cents_per_btc: u64, requested_units: u64) -> Assessment {
    assess_with(
        value_sats,
        usd_cents_per_btc,
        requested_units,
        &Terms::default(),
    )
}

/// `assess` under the given settlement rules.
pub fn assess_with(
    value_sats: u64,
    usd_cents_per_btc: u64,
    requested_units: u64,
    terms: &Terms,
) -> Assessment {
    let paid = u128::from(value_sats) * u128::from(usd_cents_per_btc);
    let required = u128::from(requested_units) * SAT_CENTS_PER_UNIT;
    // Up to and including the tolerance, either way, counts as exact.
    let within_tolerance = paid.abs_diff(required).saturating_mul(10_000)
        <= required.saturating_mul(terms.price_tolerance_bps);
    let (underpaid, credit_cents) = if within_tolerance {
        (false, 0)
    } else if paid >= required {
        let excess = paid - required;
        // Within the target the mint keeps the excess; beyond it, all of the
        // excess (not just the part past the target) is credited.
        let beyond = excess.saturating_mul(10_000)
            > required.saturating_mul(terms.reconciliation_target_bps);
        let credit = if beyond {
            u64::try_from(excess / SAT_CENTS_PER_CENT).unwrap_or(u64::MAX)
        } else {
            0
        };
        (false, credit)
    } else {
        (true, 0)
    };
    Assessment {
        over_under_bps: basis_points(paid, required),
        paid_units: allowed_units(value_sats, usd_cents_per_btc),
        paid_value: paid,
        within_tolerance,
        underpaid,
        credit_cents,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Indexes into the outputs to sign, ascending. Empty when `unsettled`.
    pub sign: Vec<usize>,
    /// Requested units that are not signed. The whole request when `unsettled`.
    pub skipped_units: u64,
    /// True if the payment is short and no set of dropped outputs gets within the
    /// reconciliation target: sign nothing and hold the payment.
    pub unsettled: bool,
}

/// Which outputs to sign.
///
/// A payment that is not short signs everything. A short one drops the smallest
/// requested outputs, one at a time (among equal amounts the later one first),
/// until the units left are no more than the whole units paid. If something is
/// left and its value is at least the amount paid times (1 - target), those
/// outputs are signed and the mint keeps the small remainder. Otherwise nothing is
/// signed: dropping more would only widen the gap, and the payer has to send a
/// new request that matches what was paid.
pub fn plan(outputs: &[MintOutput], assessment: &Assessment, terms: &Terms) -> Plan {
    // Summed in u128 so the totals are exact even for a request that `check`
    // would already have rejected as overflowing.
    let requested: u128 = outputs.iter().map(|o| u128::from(o.amount.units)).sum();
    if !assessment.underpaid {
        return Plan {
            sign: (0..outputs.len()).collect(),
            skipped_units: 0,
            unsettled: false,
        };
    }
    let mut order: Vec<usize> = (0..outputs.len()).collect();
    order.sort_by(|&a, &b| {
        outputs[a]
            .amount
            .units
            .cmp(&outputs[b].amount.units)
            .then(b.cmp(&a))
    });
    let paid_units = u128::from(assessment.paid_units);
    let mut signed = requested;
    let mut dropped = 0;
    while signed > paid_units && dropped < order.len() {
        signed -= u128::from(outputs[order[dropped]].amount.units);
        dropped += 1;
    }
    let signed_value = signed.saturating_mul(SAT_CENTS_PER_UNIT);
    let remainder = assessment.paid_value.saturating_sub(signed_value);
    let within_target = remainder.saturating_mul(10_000)
        <= assessment
            .paid_value
            .saturating_mul(terms.reconciliation_target_bps);
    if dropped == order.len() || !within_target {
        return Plan {
            sign: Vec::new(),
            skipped_units: u64::try_from(requested).unwrap_or(u64::MAX),
            unsettled: true,
        };
    }
    let mut sign: Vec<usize> = order[dropped..].to_vec();
    sign.sort_unstable();
    Plan {
        sign,
        skipped_units: u64::try_from(requested - signed).unwrap_or(u64::MAX),
        unsettled: false,
    }
}

/// Checks the requested outputs on their own (denominations, duplicate blind
/// nonces, total overflow) and returns the total requested.
pub fn check(
    is_denomination: impl Fn(Amount) -> bool,
    outputs: &[MintOutput],
) -> Result<u64, Rejection> {
    let mut seen = HashSet::new();
    let mut requested: u64 = 0;
    for output in outputs {
        if !is_denomination(output.amount) {
            return Err(Rejection::UnknownDenomination(output.amount));
        }
        if !seen.insert(&output.blind_nonce) {
            return Err(Rejection::DuplicateBlindNonce);
        }
        requested = requested
            .checked_add(output.amount.units)
            .ok_or(Rejection::Overflow)?;
    }
    Ok(requested)
}

fn reject(
    records: &mut Records,
    txid: &str,
    mut record: Record,
    why: Rejection,
    over_under_bps: Option<i64>,
    skipped_units: u64,
) -> Result<Settlement, SettleError> {
    record.status = Status::Rejected;
    record.issued = false;
    record.over_under_bps = over_under_bps;
    record.skipped_units = skipped_units;
    record.reason = Some(format!("{why:?}"));
    records.put(txid, record)?;
    Ok(Settlement::Rejected(why))
}

/// Advance one payment as far as it can go this run.
///
/// - At 1 confirmation the price is locked and stored. The first run that sees
///   the payment does this, even if it already has more confirmations (the
///   record is then marked late).
/// - At 6 confirmations it is settled with the stored price, never a live one.
/// - Every payment that gets a record is settled once, whether or not anything
///   is signed. A settled payment is not looked at again.
pub fn settle(
    logic: &MintLogic,
    records: &mut Records,
    received: &Received,
    price: &mut dyn FnMut() -> Result<u64, String>,
) -> Result<Settlement, SettleError> {
    settle_with(logic, records, received, price, &Terms::default())
}

/// `settle` under the given settlement rules.
pub fn settle_with(
    logic: &MintLogic,
    records: &mut Records,
    received: &Received,
    price: &mut dyn FnMut() -> Result<u64, String>,
    terms: &Terms,
) -> Result<Settlement, SettleError> {
    settle_with_previous(logic, None, records, received, price, terms)
}

/// `settle_with`, for a mint that rotates. `previous` is the previous key set's
/// ID and mint. A payload that names the previous set is signed by it, as every
/// other request is; one that names the current set, no set, or a set that is
/// no longer live (the BTC is already paid, and blinding does not depend on the
/// key) is signed by `logic`, the current set. The settlement names the set that
/// signed.
pub fn settle_with_previous(
    logic: &MintLogic,
    previous: Option<(&str, &MintLogic)>,
    records: &mut Records,
    received: &Received,
    price: &mut dyn FnMut() -> Result<u64, String>,
    terms: &Terms,
) -> Result<Settlement, SettleError> {
    let txid = received.txid.as_str();
    if let Some(existing) = records.get(txid) {
        if existing.status != Status::Locked {
            return Ok(Settlement::AlreadySettled(existing.status));
        }
    }
    if received.confirmations < terms.lock_confirmations {
        return Ok(Settlement::Waiting {
            confirmations: received.confirmations,
        });
    }
    let record = match records.get(txid) {
        Some(existing) => existing.clone(),
        None => {
            let cents = match price() {
                Ok(cents) => cents,
                Err(why) => return Ok(Settlement::PriceUnavailable(why)),
            };
            let record = Record::locked(cents, received.confirmations > terms.lock_confirmations);
            records.put(txid, record.clone())?;
            record
        }
    };
    let Some(price_cents) = record.locked_price_cents else {
        return Err(SettleError::Mint(format!(
            "record {txid} has no locked price"
        )));
    };
    if received.confirmations < terms.min_confirmations {
        return Ok(Settlement::Locked {
            confirmations: received.confirmations,
            price_cents,
        });
    }

    let Some(bytes) = &received.payload else {
        return reject(records, txid, record, Rejection::NoDataOutput, None, 0);
    };
    let payload = match payload::decode(bytes) {
        Ok(payload) => payload,
        Err(e) => return reject(records, txid, record, Rejection::BadPayload(e), None, 0),
    };
    let (issuer, issuer_keyset) = match (payload.keyset.as_deref(), previous) {
        (Some(named), Some((id, prev))) if named == id => (prev, id.to_string()),
        _ => (logic, crate::rotation::keyset_id(logic.public_keys())),
    };
    let keys = issuer.public_keys();
    let requested = match check(|amount| keys.contains_key(&amount), &payload.outputs) {
        Ok(requested) => requested,
        Err(why) => return reject(records, txid, record, why, None, 0),
    };
    let assessment = assess_with(received.value_sats, price_cents, requested, terms);
    for output in &payload.outputs {
        let used = issuer
            .is_blind_nonce_used(&output.blind_nonce)
            .map_err(|e| SettleError::Mint(e.to_string()))?;
        if used {
            return reject(
                records,
                txid,
                record,
                Rejection::BlindNonceAlreadyUsed,
                Some(assessment.over_under_bps),
                0,
            );
        }
    }
    let plan = plan(&payload.outputs, &assessment, terms);
    let payer_did_hex = hex::encode(payload.did);

    if plan.unsettled {
        // Short beyond the target and no drop gets within it. Sign nothing, keep
        // the BTC, and record the payment under the payer's DID. This record is
        // the whole of the mint's side today; see `Unsettled`.
        let mut held = record;
        held.status = Status::PaidUnsettled;
        held.issued = false;
        held.over_under_bps = Some(assessment.over_under_bps);
        held.skipped_units = plan.skipped_units;
        held.credit_cents = 0;
        held.reason = Some(
            "paid but unsettled: short beyond settle.payment_reconciliation_target and no \
             smaller request gets within it"
                .to_string(),
        );
        held.payer_did = Some(payer_did_hex);
        held.requested_units = requested;
        held.value_sats = received.value_sats;
        records.put(txid, held)?;
        return Ok(Settlement::PaidUnsettled(Unsettled {
            payer_did: payload.did,
            requested_units: requested,
            paid_units: assessment.paid_units,
            value_sats: received.value_sats,
            over_under_bps: assessment.over_under_bps,
        }));
    }

    // Record first, then issue, so a payment is issued at most once.
    let mut settled = record;
    settled.status = Status::Issued;
    settled.issued = true;
    settled.over_under_bps = Some(assessment.over_under_bps);
    settled.skipped_units = plan.skipped_units;
    settled.credit_cents = assessment.credit_cents;
    settled.payer_did = Some(payer_did_hex);
    settled.requested_units = requested;
    settled.value_sats = received.value_sats;
    records.put(txid, settled)?;

    // The set that signs is the one the payload names if it is live, else the
    // current one. PSP uses MintLogic::issue, not issue_with_key. Every PSP note of a given
    // amount is signed by the same key from the mint's fixed key table, and
    // issue looks that key up by amount. issue_with_key is for mints whose key
    // depends on more than the amount, such as the ESP, where it also depends
    // on the note's terms. See the docs on MintLogic::issue in mint-core.
    let results = plan
        .sign
        .iter()
        .map(|&i| issuer.issue(&payload.outputs[i]))
        .collect();
    let blind_nonces = plan
        .sign
        .iter()
        .map(|&i| payload.outputs[i].blind_nonce)
        .collect();
    Ok(Settlement::Issued(Issue {
        keyset: issuer_keyset,
        results,
        blind_nonces,
        skipped_units: plan.skipped_units,
        credit_cents: assessment.credit_cents,
        over_under_bps: assessment.over_under_bps,
        within_tolerance: assessment.within_tolerance,
        payer_did: payload.did,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secp256k1::{PublicKey, SECP256K1, SecretKey};
    use std::fs;
    use std::path::PathBuf;

    // DID 0279be66..1798, then outputs 4 -> 3G and 1 -> 5G (5 units in all)
    const PAYLOAD_HEX: &str = "210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";
    // The previous layout: a 10-byte reply ("reply-test") instead of a DID
    const OLD_REPLY_PAYLOAD_HEX: &str = "0a7265706c792d7465737402000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";

    /// $10,000 per BTC, in cents. A unit is $0.10, so 1,000 sats is exactly 1
    /// unit: 100,000 sats is exactly 100 units and 5,000 sats is exactly 5 units
    /// (the 4 + 1 payload above). A low price on purpose, so the sat amounts in
    /// these tests read as one sat-thousand per unit.
    const BTC_10K: u64 = 1_000_000;

    fn point(i: u8) -> mint_types::BlindNonce {
        let sk = SecretKey::from_slice(&[i; 32]).unwrap();
        mint_types::BlindNonce(PublicKey::from_secret_key(SECP256K1, &sk))
    }

    fn out(units: u64, i: u8) -> MintOutput {
        MintOutput {
            amount: Amount::from_units(units),
            blind_nonce: point(i),
        }
    }

    fn any(_: Amount) -> bool {
        true
    }

    #[test]
    fn allowed_units_is_ten_cents_per_unit() {
        assert_eq!(allowed_units(100_000, BTC_10K), 100);
        assert_eq!(allowed_units(5_000, BTC_10K), 5);
        assert_eq!(allowed_units(100_000_000, BTC_10K), 100_000);
        assert_eq!(allowed_units(100_000_000, 8_167_355), 816_735);
    }

    #[test]
    fn allowed_units_rounds_down_to_whole_units() {
        assert_eq!(allowed_units(4_999, BTC_10K), 4);
        assert_eq!(allowed_units(999, BTC_10K), 0);
        assert_eq!(allowed_units(0, BTC_10K), 0);
        assert_eq!(allowed_units(5_000, 0), 0);
    }

    #[test]
    fn allowed_units_does_not_overflow() {
        assert_eq!(allowed_units(u64::MAX, u64::MAX), u64::MAX);
        // all 21 million BTC at $1,000,000,000 per BTC
        assert_eq!(
            allowed_units(2_100_000_000_000_000, 100_000_000_000),
            210_000_000_000_000_000
        );
    }

    #[test]
    fn over_under_is_reported_in_basis_points_of_the_request() {
        // 100 units requested, which is exactly 100,000 sats at $10,000
        assert_eq!(assess(100_000, BTC_10K, 100).over_under_bps, 0);
        assert_eq!(assess(112_500, BTC_10K, 100).over_under_bps, 1250);
        assert_eq!(assess(99_200, BTC_10K, 100).over_under_bps, -80);
        assert_eq!(assess(0, BTC_10K, 100).over_under_bps, -10_000);
    }

    #[test]
    fn over_under_truncates_toward_zero() {
        // 5 units requested, 5,499 sats is 9.98% over (998.0 bps, exact here)
        assert_eq!(assess(5_499, BTC_10K, 5).over_under_bps, 998);
        // 100 units, 100,001 sats is +0.001% = 0.1 bps, truncated to 0
        assert_eq!(assess(100_001, BTC_10K, 100).over_under_bps, 0);
        assert_eq!(assess(99_999, BTC_10K, 100).over_under_bps, 0);
    }

    #[test]
    fn a_short_payment_is_flagged_and_only_an_overpayment_beyond_the_target_is_credited() {
        // 100 units requested, which is exactly 100,000 sats at $10,000
        let exact = assess(100_000, BTC_10K, 100);
        assert!(!exact.underpaid);
        assert_eq!(exact.credit_cents, 0);
        // the price tolerance (1% by default) is off in this test, so that a
        // hair short is still short
        let strict = strict();
        assert!(assess_with(99_999, BTC_10K, 100, &strict).underpaid);
        assert!(assess(0, BTC_10K, 100).underpaid);
        // 10% over exactly is within the target: the mint keeps it
        assert_eq!(assess(110_000, BTC_10K, 100).credit_cents, 0);
        // one sat more and it is beyond: all of the excess is credited, $1.00
        let beyond = assess(110_001, BTC_10K, 100);
        assert!(!beyond.underpaid);
        assert_eq!(beyond.credit_cents, 100);
        assert_eq!(assess(112_500, BTC_10K, 100).credit_cents, 125);
        assert_eq!(assess(150_000, BTC_10K, 100).credit_cents, 500);
        assert_eq!(assess(99_000, BTC_10K, 100).credit_cents, 0);
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let a = assess(u64::MAX, u64::MAX, 1);
        assert!(!a.underpaid);
        assert_eq!(a.over_under_bps, i64::MAX);
        let b = assess(0, 0, u64::MAX);
        assert!(b.underpaid);
        assert_eq!(b.over_under_bps, -10_000);
    }

    /// The default rules with the price tolerance off, for tests of the
    /// reconciliation rules on payments that miss by less than 1%.
    fn strict() -> Terms {
        Terms {
            price_tolerance_bps: 0,
            ..Terms::default()
        }
    }

    /// An assessment for a payment worth exactly `paid_units` whole units.
    fn short(paid_units: u64) -> Assessment {
        Assessment {
            over_under_bps: -5_000,
            paid_units,
            paid_value: u128::from(paid_units) * SAT_CENTS_PER_UNIT,
            within_tolerance: false,
            underpaid: true,
            credit_cents: 0,
        }
    }

    fn unsettled_plan(requested: u64) -> Plan {
        Plan {
            sign: vec![],
            skipped_units: requested,
            unsettled: true,
        }
    }

    #[test]
    fn plan_signs_everything_when_the_payment_is_enough() {
        let outputs = [out(4, 1), out(1, 2)];
        let a = assess(5_000, BTC_10K, 5);
        assert_eq!(
            plan(&outputs, &a, &Terms::default()),
            Plan {
                sign: vec![0, 1],
                skipped_units: 0,
                unsettled: false
            }
        );
        // overpaid beyond the target still signs everything
        let a = assess(9_000, BTC_10K, 5);
        assert_eq!(plan(&outputs, &a, &Terms::default()).sign, vec![0, 1]);
    }

    #[test]
    fn plan_drops_the_smallest_outputs_until_the_rest_fits_what_was_paid() {
        let outputs = [out(1, 1), out(4, 2), out(2, 3)]; // 7 units asked
        let t = Terms::default();
        // 6 units paid: drop the 1, sign 4 + 2 exactly
        assert_eq!(
            plan(&outputs, &assess(6_000, BTC_10K, 7), &t),
            Plan {
                sign: vec![1, 2],
                skipped_units: 1,
                unsettled: false
            }
        );
        // 6.5 paid still buys 6 whole units; the 0.5 left over is 7.7% of
        // what was paid, within 10%, so the mint keeps it
        assert_eq!(
            plan(&outputs, &assess(6_500, BTC_10K, 7), &t),
            Plan {
                sign: vec![1, 2],
                skipped_units: 1,
                unsettled: false
            }
        );
        // 4.3 paid: drop the 1 (6 > 4), then the 2 (4 <= 4); the 0.3 left over
        // is 7.0% of what was paid
        assert_eq!(
            plan(&outputs, &assess(4_300, BTC_10K, 7), &t),
            Plan {
                sign: vec![1],
                skipped_units: 3,
                unsettled: false
            }
        );
        // the drop order is smallest first, not "largest that fits": with 5
        // paid, 4 is signed alone and the 20% left over is beyond the target
        assert_eq!(
            plan(&outputs, &assess(5_000, BTC_10K, 7), &t),
            unsettled_plan(7)
        );
    }

    #[test]
    fn among_equal_outputs_the_later_one_is_dropped_first() {
        let outputs = [out(2, 1), out(2, 2), out(4, 3)]; // 8 asked
        assert_eq!(
            plan(&outputs, &assess(6_000, BTC_10K, 8), &Terms::default()),
            Plan {
                sign: vec![0, 2],
                skipped_units: 2,
                unsettled: false
            }
        );
    }

    #[test]
    fn a_payment_no_set_of_drops_can_fit_signs_nothing() {
        let t = Terms::default();
        // one large note: dropping it leaves nothing
        let one = [out(100, 1)];
        assert_eq!(
            plan(&one, &assess_with(99_000, BTC_10K, 100, &strict()), &t),
            unsettled_plan(100)
        );
        // a hair short of two notes: the drop would leave 20% of the payment
        // unused, beyond the target
        let two = [out(4, 1), out(1, 2)];
        assert_eq!(
            plan(&two, &assess_with(4_999, BTC_10K, 5, &strict()), &t),
            unsettled_plan(5)
        );
        // nothing paid at all
        assert_eq!(plan(&two, &assess(0, BTC_10K, 5), &t), unsettled_plan(5));
    }

    #[test]
    fn the_target_decides_where_a_partial_fill_stops_being_acceptable() {
        let outputs = [out(4, 1), out(1, 2)];
        let paid = assess(4_300, BTC_10K, 5); // the 4 signs; 0.3 is 7.0% left over
        let at = |bps| Terms {
            reconciliation_target_bps: bps,
            ..Terms::default()
        };
        assert!(!plan(&outputs, &paid, &at(1000)).unsettled);
        assert!(!plan(&outputs, &paid, &at(700)).unsettled); // 6.98% is within 7%
        assert!(plan(&outputs, &paid, &at(600)).unsettled);
        // a target of zero accepts only an exact fit
        assert!(plan(&outputs, &paid, &at(0)).unsettled);
        let exact = assess(4_000, BTC_10K, 5);
        assert_eq!(plan(&outputs, &exact, &at(0)).sign, vec![0]);
    }

    #[test]
    fn the_overpayment_threshold_comes_from_the_terms() {
        let terms = Terms {
            reconciliation_target_bps: 500,
            ..Terms::default()
        };
        // 5% over is within a 5% target, one sat more is beyond it
        assert_eq!(assess_with(105_000, BTC_10K, 100, &terms).credit_cents, 0);
        assert_eq!(assess_with(105_001, BTC_10K, 100, &terms).credit_cents, 50);
        // the same 5.001% is inside the default 10%
        assert_eq!(assess(105_001, BTC_10K, 100).credit_cents, 0);
    }

    #[test]
    fn plan_does_not_wrap_at_the_u64_limit() {
        let t = Terms::default();
        // two notes that together exceed u64: the smaller is dropped, the
        // larger fits exactly
        let outputs = [out(u64::MAX, 1), out(1, 2)];
        assert_eq!(
            plan(&outputs, &short(u64::MAX), &t),
            Plan {
                sign: vec![0],
                skipped_units: 1,
                unsettled: false
            }
        );
        // everything fits exactly at the limit
        let half = u64::MAX / 2;
        let outputs = [out(half, 1), out(half + 1, 2)];
        assert_eq!(
            plan(&outputs, &short(u64::MAX), &t),
            Plan {
                sign: vec![0, 1],
                skipped_units: 0,
                unsettled: false
            }
        );
    }

    #[test]
    fn check_returns_the_total_requested() {
        assert_eq!(check(any, &[out(4, 1), out(1, 2)]), Ok(5));
    }

    #[test]
    fn unknown_denomination_is_rejected() {
        assert_eq!(
            check(|a| a.units != 3, &[out(4, 1), out(3, 2)]),
            Err(Rejection::UnknownDenomination(Amount::from_units(3)))
        );
    }

    #[test]
    fn duplicate_blind_nonce_is_rejected() {
        assert_eq!(
            check(any, &[out(4, 1), out(1, 1)]),
            Err(Rejection::DuplicateBlindNonce)
        );
    }

    #[test]
    fn totals_are_exact_at_the_u64_limit() {
        let half = u64::MAX / 2;
        assert_eq!(check(any, &[out(half, 1), out(half, 2)]), Ok(u64::MAX - 1));
        assert_eq!(
            check(any, &[out(half, 1), out(half, 2), out(1, 3)]),
            Ok(u64::MAX)
        );
        assert_eq!(check(any, &[out(u64::MAX, 1)]), Ok(u64::MAX));
        assert_eq!(
            check(any, &[out(half, 1), out(half, 2), out(2, 3)]),
            Err(Rejection::Overflow)
        );
    }

    #[test]
    fn request_total_overflow_is_rejected() {
        assert_eq!(
            check(any, &[out(u64::MAX, 1), out(1, 2)]),
            Err(Rejection::Overflow)
        );
    }

    fn fixture(name: &str) -> (MintLogic, Records, PathBuf) {
        let dir = std::env::temp_dir().join(format!("psp-mint-{}-{name}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = fs::remove_dir_all(&dir);
        let logic = crate::mint::open(&dir, Amount::from_units(1024)).unwrap();
        let records = Records::open(&dir.join("settlements.json")).unwrap();
        (logic, records, dir)
    }

    fn received(
        txid: &str,
        confirmations: u32,
        value_sats: u64,
        payload: Option<&str>,
    ) -> Received {
        Received {
            txid: txid.to_string(),
            confirmations,
            value_sats,
            payload: payload.map(|h| hex::decode(h).unwrap()),
        }
    }

    /// A price source for tests that counts how often it is asked.
    struct Price {
        result: Result<u64, String>,
        asked: u32,
    }

    impl Price {
        fn at(cents: u64) -> Self {
            Price {
                result: Ok(cents),
                asked: 0,
            }
        }

        fn down() -> Self {
            Price {
                result: Err("only 1 usable price".to_string()),
                asked: 0,
            }
        }

        fn ask(&mut self) -> Result<u64, String> {
            self.asked += 1;
            self.result.clone()
        }
    }

    fn run(
        logic: &MintLogic,
        records: &mut Records,
        r: &Received,
        price: &mut Price,
    ) -> Settlement {
        settle(logic, records, r, &mut || price.ask()).unwrap()
    }

    /// Settle the 4 + 1 payload naming `named` (`prev` is the previous set's
    /// ID, `cur` the current one's, or something else), and say which set signed.
    fn signed_by(
        case: &str,
        named: &dyn Fn(&str, &str) -> Option<String>,
    ) -> (String, String, String) {
        let (current, mut records, _d1) = fixture(&format!("named-current-{case}"));
        let (previous, _r2, _d2) = fixture(&format!("named-previous-{case}"));
        let (cur_id, prev_id) = (
            crate::rotation::keyset_id(current.public_keys()),
            crate::rotation::keyset_id(previous.public_keys()),
        );
        assert_ne!(cur_id, prev_id);
        let mut p = payload::decode(&hex::decode(PAYLOAD_HEX).unwrap()).unwrap();
        p.keyset = named(&cur_id, &prev_id);
        let r = received(
            "t1",
            6,
            5_000,
            Some(&hex::encode(payload::encode(&p).unwrap())),
        );
        let mut price = Price::at(BTC_10K);
        let issue = issued(
            settle_with_previous(
                &current,
                Some((&prev_id, &previous)),
                &mut records,
                &r,
                &mut || price.ask(),
                &Terms::default(),
            )
            .unwrap(),
        );
        (issue.keyset, cur_id, prev_id)
    }

    #[test]
    fn a_payload_naming_the_previous_set_is_signed_by_it() {
        let (signed, _, prev) = signed_by("prev", &|_, prev| Some(prev.to_string()));
        assert_eq!(signed, prev);
    }

    #[test]
    fn a_payload_naming_the_current_set_no_set_or_a_set_that_is_not_live_is_signed_by_the_current_set()
     {
        for (case, named) in [
            (
                "cur",
                (&|cur: &str, _: &str| Some(cur.to_string()))
                    as &dyn Fn(&str, &str) -> Option<String>,
            ),
            ("none", &|_, _| None),
            ("gone", &|_, _| Some("00000000000000ff".to_string())),
        ] {
            let (signed, cur, _) = signed_by(case, named);
            assert_eq!(signed, cur, "{case}");
        }
    }

    fn issued(s: Settlement) -> Issue {
        match s {
            Settlement::Issued(issue) => issue,
            other => panic!("{other:?}"),
        }
    }

    fn total(logic: &MintLogic) -> Amount {
        logic.audit_totals().unwrap().0
    }

    #[test]
    fn one_confirmation_locks_the_price_and_issues_nothing() {
        let (logic, mut records, dir) = fixture("lock");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 1, 5_000, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::Locked {
                confirmations: 1,
                price_cents: BTC_10K
            }
        );
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.locked_price_cents, Some(BTC_10K));
        assert_eq!(rec.status, Status::Locked);
        assert!(!rec.issued);
        assert!(!rec.late);
        assert_eq!(total(&logic), Amount::ZERO);
        assert_eq!(price.asked, 1);

        // later runs keep the stored price and do not ask again
        let mut other = Price::at(1);
        let r3 = received("aa", 3, 5_000, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &r3, &mut other),
            Settlement::Locked {
                confirmations: 3,
                price_cents: BTC_10K
            }
        );
        assert_eq!(other.asked, 0);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settlement_at_six_uses_the_locked_price_never_a_live_one() {
        let (logic, mut records, dir) = fixture("stored");
        let mut at_lock = Price::at(BTC_10K);
        run(
            &logic,
            &mut records,
            &received("aa", 1, 5_000, Some(PAYLOAD_HEX)),
            &mut at_lock,
        );
        // by now the live price has crashed, which would make 5,000 sats buy 0
        let mut live = Price::at(1);
        let issue = issued(run(
            &logic,
            &mut records,
            &received("aa", 6, 5_000, Some(PAYLOAD_HEX)),
            &mut live,
        ));
        assert_eq!(issue.results.len(), 2);
        assert!(issue.results.iter().all(|x| x.is_ok()));
        assert_eq!(issue.over_under_bps, 0);
        assert_eq!(live.asked, 0);
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.status, Status::Issued);
        assert!(rec.issued);
        assert_eq!(rec.locked_price_cents, Some(BTC_10K));
        assert_eq!(rec.over_under_bps, Some(0));
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_payment_first_seen_at_six_is_locked_now_and_marked_late() {
        let (logic, mut records, dir) = fixture("late");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 6, 5_000, Some(PAYLOAD_HEX));
        assert!(matches!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::Issued(_)
        ));
        let rec = records.get("aa").unwrap();
        assert!(rec.late);
        assert_eq!(rec.locked_price_cents, Some(BTC_10K));
        assert_eq!(price.asked, 1);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_payment_not_yet_in_a_block_gets_no_record() {
        let (logic, mut records, dir) = fixture("zero");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 0, 5_000, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::Waiting { confirmations: 0 }
        );
        assert!(records.get("aa").is_none());
        assert_eq!(price.asked, 0);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_usable_price_makes_no_record_and_is_tried_again() {
        let (logic, mut records, dir) = fixture("noprice");
        let r = received("aa", 1, 5_000, Some(PAYLOAD_HEX));
        let mut down = Price::down();
        assert_eq!(
            run(&logic, &mut records, &r, &mut down),
            Settlement::PriceUnavailable("only 1 usable price".to_string())
        );
        assert!(records.get("aa").is_none());
        let mut up = Price::at(BTC_10K);
        assert!(matches!(
            run(&logic, &mut records, &r, &mut up),
            Settlement::Locked { .. }
        ));
        assert!(records.get("aa").is_some());
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_settled_payment_is_not_issued_twice() {
        let (logic, mut records, dir) = fixture("twice");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 6, 5_000, Some(PAYLOAD_HEX));
        issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::AlreadySettled(Status::Issued)
        );
        assert_eq!(total(&logic), Amount::from_units(5));
        assert_eq!(price.asked, 1);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bad_payment_is_recorded_as_rejected_and_not_looked_at_again() {
        let (logic, mut records, dir) = fixture("bad");
        let mut price = Price::at(BTC_10K);
        let none = received("aa", 6, 5_000, None);
        assert_eq!(
            run(&logic, &mut records, &none, &mut price),
            Settlement::Rejected(Rejection::NoDataOutput)
        );
        let garbage = received("bb", 6, 5_000, Some("00"));
        assert!(matches!(
            run(&logic, &mut records, &garbage, &mut price),
            Settlement::Rejected(Rejection::BadPayload(_))
        ));
        let old = received("cc", 6, 5_000, Some(OLD_REPLY_PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &old, &mut price),
            Settlement::Rejected(Rejection::BadPayload(DecodeError::WrongReplyLength))
        );
        for txid in ["aa", "bb", "cc"] {
            let rec = records.get(txid).unwrap();
            assert_eq!(rec.status, Status::Rejected);
            assert!(!rec.issued);
            assert_eq!(rec.over_under_bps, None);
            assert!(rec.reason.is_some());
        }
        assert_eq!(
            run(&logic, &mut records, &none, &mut price),
            Settlement::AlreadySettled(Status::Rejected)
        );
        assert_eq!(total(&logic), Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    const DID_HEX: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    fn did() -> [u8; DID_LEN] {
        hex::decode(DID_HEX).unwrap().try_into().unwrap()
    }

    /// Settle under the default rules, returning the raw result so a failed
    /// records write can be looked at.
    fn try_run(
        logic: &MintLogic,
        records: &mut Records,
        r: &Received,
        price: &mut Price,
    ) -> Result<Settlement, SettleError> {
        settle(logic, records, r, &mut || price.ask())
    }

    /// Make the next records write fail, the way a full disk or a crash during
    /// the write would: the temporary file's name is taken by a directory.
    fn block_records_writes(dir: &std::path::Path) {
        fs::create_dir(dir.join("settlements.json.tmp")).unwrap();
    }

    fn unblock_records_writes(dir: &std::path::Path) {
        fs::remove_dir(dir.join("settlements.json.tmp")).unwrap();
    }

    #[test]
    fn short_but_within_the_target_signs_what_fits_and_keeps_the_remainder() {
        let (logic, mut records, dir) = fixture("partial");
        let mut price = Price::at(BTC_10K);
        // 4,300 sats buys 4 whole units of the 5 asked: the 1 is dropped, the
        // 4 is signed, and the 0.3 left over (7.0% of the payment) is kept
        let r = received("aa", 6, 4_300, Some(PAYLOAD_HEX));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 1);
        assert_eq!(issue.skipped_units, 1);
        assert_eq!(issue.credit_cents, 0);
        assert_eq!(issue.over_under_bps, -1400);
        assert_eq!(issue.payer_did, did());
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.status, Status::Issued);
        assert!(rec.issued);
        assert_eq!(rec.skipped_units, 1);
        assert_eq!(rec.payer_did.as_deref(), Some(DID_HEX));
        assert_eq!(rec.requested_units, 5);
        assert_eq!(rec.value_sats, 4_300);
        assert_eq!(total(&logic), Amount::from_units(4));
        // the dropped 1-unit output was not used up
        let decoded = payload::decode(&hex::decode(PAYLOAD_HEX).unwrap()).unwrap();
        assert!(
            !logic
                .is_blind_nonce_used(&decoded.outputs[1].blind_nonce)
                .unwrap()
        );
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_payment_no_drop_can_fit_is_held_as_paid_but_unsettled() {
        let (logic, mut records, dir) = fixture("held");
        let mut price = Price::at(BTC_10K);
        // 3,500 sats buys 3 units. The asks are 4 and 1: dropping the 1 leaves
        // 4 > 3, dropping the 4 as well leaves nothing. Nothing is signed.
        let r = received("aa", 6, 3_500, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::PaidUnsettled(Unsettled {
                payer_did: did(),
                requested_units: 5,
                paid_units: 3,
                value_sats: 3_500,
                over_under_bps: -3000,
            })
        );
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.status, Status::PaidUnsettled);
        assert!(!rec.issued);
        assert_eq!(rec.skipped_units, 5);
        assert_eq!(rec.requested_units, 5);
        assert_eq!(rec.value_sats, 3_500);
        assert_eq!(rec.payer_did.as_deref(), Some(DID_HEX));
        assert_eq!(rec.credit_cents, 0);
        assert!(rec.reason.is_some());
        assert_eq!(total(&logic), Amount::ZERO);
        // it is on the payer's account, not looked at again, and not retried
        let owed = records.owed_to(DID_HEX);
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].0, "aa");
        assert!(records.unsettled().is_empty());
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::AlreadySettled(Status::PaidUnsettled)
        );
        assert_eq!(price.asked, 1);
        // no output of the held request was used up, so the payer's new request
        // may carry them
        let decoded = payload::decode(&hex::decode(PAYLOAD_HEX).unwrap()).unwrap();
        for o in &decoded.outputs {
            assert!(!logic.is_blind_nonce_used(&o.blind_nonce).unwrap());
        }
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hair_short_of_a_request_of_two_notes_is_held_not_trimmed() {
        let (logic, mut records, dir) = fixture("hair");
        let mut price = Price::at(BTC_10K);
        // 99.98% of the request, yet dropping the 1 would leave 20% of the
        // payment unused: held, not trimmed. The price tolerance is off here;
        // with it on (the default) this payment is within 1% and settles in full.
        let r = received("aa", 6, 4_999, Some(PAYLOAD_HEX));
        assert!(matches!(
            settle_with(&logic, &mut records, &r, &mut || price.ask(), &strict()).unwrap(),
            Settlement::PaidUnsettled(_)
        ));
        assert_eq!(total(&logic), Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overpayment_within_the_target_is_issued_in_full_and_kept_by_the_mint() {
        let (logic, mut records, dir) = fixture("keep");
        let mut price = Price::at(BTC_10K);
        // 5,500 sats is exactly 10% over
        let r = received("aa", 6, 5_500, Some(PAYLOAD_HEX));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 2);
        assert_eq!(issue.credit_cents, 0);
        assert_eq!(issue.over_under_bps, 1000);
        assert_eq!(records.get("aa").unwrap().credit_cents, 0);
        assert!(records.owed_to(DID_HEX).is_empty());
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overpayment_beyond_the_target_is_issued_in_full_and_credited_to_the_payer() {
        let (logic, mut records, dir) = fixture("credit");
        let mut price = Price::at(BTC_10K);
        // 6,000 sats is $0.60 for $0.50: 20% over, so the $0.10 excess is credited
        let r = received("aa", 6, 6_000, Some(PAYLOAD_HEX));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 2);
        assert_eq!(issue.skipped_units, 0);
        assert_eq!(issue.credit_cents, 10);
        assert_eq!(issue.over_under_bps, 2000);
        assert_eq!(issue.payer_did, did());
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.credit_cents, 10);
        assert_eq!(rec.payer_did.as_deref(), Some(DID_HEX));
        assert_eq!(records.owed_to(DID_HEX).len(), 1);
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_records_write_before_a_partial_fill_signs_nothing_and_the_retry_signs_once() {
        let (logic, mut records, dir) = fixture("crash-partial");
        let mut price = Price::at(BTC_10K);
        // seen once, so the price is locked
        let seen = received("aa", 1, 4_300, Some(PAYLOAD_HEX));
        assert!(matches!(
            run(&logic, &mut records, &seen, &mut price),
            Settlement::Locked { .. }
        ));
        // the write that would record the settlement fails
        block_records_writes(&dir);
        let settled = received("aa", 6, 4_300, Some(PAYLOAD_HEX));
        assert!(try_run(&logic, &mut records, &settled, &mut price).is_err());
        assert_eq!(total(&logic), Amount::ZERO);
        assert_eq!(records.get("aa").unwrap().status, Status::Locked);
        // the retry settles it, once
        unblock_records_writes(&dir);
        let issue = issued(run(&logic, &mut records, &settled, &mut price));
        assert_eq!(issue.results.len(), 1);
        assert_eq!(total(&logic), Amount::from_units(4));
        assert_eq!(
            run(&logic, &mut records, &settled, &mut price),
            Settlement::AlreadySettled(Status::Issued)
        );
        assert_eq!(total(&logic), Amount::from_units(4));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_records_write_before_an_unsettled_payment_loses_nothing_and_the_retry_holds_it() {
        let (logic, mut records, dir) = fixture("crash-held");
        let mut price = Price::at(BTC_10K);
        let seen = received("aa", 1, 3_500, Some(PAYLOAD_HEX));
        run(&logic, &mut records, &seen, &mut price);
        block_records_writes(&dir);
        let settled = received("aa", 6, 3_500, Some(PAYLOAD_HEX));
        assert!(try_run(&logic, &mut records, &settled, &mut price).is_err());
        assert_eq!(records.get("aa").unwrap().status, Status::Locked);
        assert_eq!(total(&logic), Amount::ZERO);
        // still waiting to settle, so the next scan picks it up again
        assert_eq!(records.unsettled(), vec!["aa".to_string()]);
        unblock_records_writes(&dir);
        assert!(matches!(
            run(&logic, &mut records, &settled, &mut price),
            Settlement::PaidUnsettled(_)
        ));
        assert_eq!(records.owed_to(DID_HEX).len(), 1);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unsettled_payment_survives_a_restart_and_is_not_settled_again() {
        let (logic, mut records, dir) = fixture("restart-held");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 6, 3_500, Some(PAYLOAD_HEX));
        assert!(matches!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::PaidUnsettled(_)
        ));
        // the process dies; a new one opens the same files
        drop(records);
        let mut records = Records::open(&dir.join("settlements.json")).unwrap();
        assert_eq!(records.get("aa").unwrap().status, Status::PaidUnsettled);
        assert_eq!(records.owed_to(DID_HEX).len(), 1);
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::AlreadySettled(Status::PaidUnsettled)
        );
        assert_eq!(total(&logic), Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_crash_after_the_record_is_written_never_signs_the_payment_twice() {
        // The record is written before anything is signed, so a payment is
        // signed at most once. A crash between the two leaves it recorded as
        // issued with nothing signed, and a restart does not sign it.
        let (logic, mut records, dir) = fixture("crash-after-record");
        let mut price = Price::at(BTC_10K);
        let seen = received("aa", 1, 4_300, Some(PAYLOAD_HEX));
        run(&logic, &mut records, &seen, &mut price);
        let mut rec = records.get("aa").unwrap().clone();
        rec.status = Status::Issued;
        rec.issued = true;
        records.put("aa", rec).unwrap();
        let settled = received("aa", 6, 4_300, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &settled, &mut price),
            Settlement::AlreadySettled(Status::Issued)
        );
        assert_eq!(total(&logic), Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_request_cannot_be_claimed_twice() {
        let (logic, mut records, dir) = fixture("claim-twice");
        let mut price = Price::at(BTC_10K);
        // payment aa signs the 4 and drops the 1
        let first = received("aa", 6, 4_300, Some(PAYLOAD_HEX));
        issued(run(&logic, &mut records, &first, &mut price));
        assert_eq!(total(&logic), Amount::from_units(4));
        // the same payment again signs nothing more
        assert_eq!(
            run(&logic, &mut records, &first, &mut price),
            Settlement::AlreadySettled(Status::Issued)
        );
        // a second payment carrying the same request cannot claim the output
        // that was already signed, whatever it pays
        let second = received("bb", 6, 9_000, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &second, &mut price),
            Settlement::Rejected(Rejection::BlindNonceAlreadyUsed)
        );
        assert_eq!(total(&logic), Amount::from_units(4));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn payload_bytes(outputs: &[(u64, u8)]) -> String {
        let mut bytes = vec![33u8];
        bytes.extend_from_slice(&[7u8; 33]);
        bytes.push(outputs.len() as u8);
        for &(units, i) in outputs {
            bytes.extend_from_slice(&units.to_be_bytes());
            bytes.extend_from_slice(&point(i).0.serialize());
        }
        hex::encode(bytes)
    }

    #[test]
    fn a_reused_blind_nonce_rejects_the_whole_payment_and_is_recorded() {
        let (logic, mut records, dir) = fixture("reuse");
        let mut price = Price::at(BTC_10K);
        let first = received("aa", 6, 5_000, Some(&payload_bytes(&[(4, 1), (1, 2)])));
        issued(run(&logic, &mut records, &first, &mut price));
        let second = received("bb", 6, 5_000, Some(&payload_bytes(&[(2, 3), (1, 2)])));
        assert_eq!(
            run(&logic, &mut records, &second, &mut price),
            Settlement::Rejected(Rejection::BlindNonceAlreadyUsed)
        );
        let rec = records.get("bb").unwrap();
        assert_eq!(rec.status, Status::Rejected);
        // 3 units requested ($0.30), $0.50 paid: (5 - 3) / 3 = +6666.67 bps, truncated
        assert_eq!(rec.over_under_bps, Some(6666));
        assert!(!logic.is_blind_nonce_used(&point(3)).unwrap());
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_confirmations_come_from_the_terms() {
        let (logic, mut records, dir) = fixture("terms-conf");
        let terms = Terms {
            lock_confirmations: 2,
            min_confirmations: 3,
            ..Terms::default()
        };
        let mut price = Price::at(BTC_10K);
        let mut go = |records: &mut Records, confirmations: u32| {
            let r = received("cc", confirmations, 5_000, Some(PAYLOAD_HEX));
            settle_with(&logic, records, &r, &mut || price.ask(), &terms).unwrap()
        };
        assert_eq!(
            go(&mut records, 1),
            Settlement::Waiting { confirmations: 1 }
        );
        assert!(records.get("cc").is_none());
        assert_eq!(
            go(&mut records, 2),
            Settlement::Locked {
                confirmations: 2,
                price_cents: BTC_10K
            }
        );
        assert!(!records.get("cc").unwrap().late);
        assert!(matches!(go(&mut records, 3), Settlement::Issued(_)));
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    // ---- the price tolerance ----
    //
    // 65 units ($6.50) is 65,000 sats at $10,000 per BTC, so 1% is 650 sats.
    // 0.9% is 585 sats either way, 1.1% is 715.

    fn big_payload() -> String {
        payload_bytes(&[(64, 1), (1, 2)])
    }

    #[test]
    fn a_payment_within_the_tolerance_is_exact_either_way() {
        let t = Terms::default();
        for sats in [64_415u64, 65_000, 65_585] {
            let a = assess_with(sats, BTC_10K, 65, &t);
            assert!(a.within_tolerance, "{sats}");
            assert!(!a.underpaid, "{sats}");
            assert_eq!(a.credit_cents, 0, "{sats}");
        }
        // exactly 1% is still within it; one sat more is not
        assert!(assess_with(64_350, BTC_10K, 65, &t).within_tolerance);
        assert!(assess_with(65_650, BTC_10K, 65, &t).within_tolerance);
        assert!(!assess_with(64_349, BTC_10K, 65, &t).within_tolerance);
        assert!(!assess_with(65_651, BTC_10K, 65, &t).within_tolerance);
    }

    #[test]
    fn a_payment_beyond_the_tolerance_is_left_to_the_reconciliation_target() {
        let t = Terms::default();
        let under = assess_with(64_285, BTC_10K, 65, &t); // 1.1% under
        assert!(!under.within_tolerance && under.underpaid);
        let over = assess_with(65_715, BTC_10K, 65, &t); // 1.1% over
        assert!(!over.within_tolerance && !over.underpaid);
        // within the 10% target, so the mint keeps the excess
        assert_eq!(over.credit_cents, 0);
        // with a target of 1% the same payment is credited, all of the excess
        let tight = Terms {
            reconciliation_target_bps: 100,
            ..Terms::default()
        };
        assert_eq!(assess_with(65_715, BTC_10K, 65, &tight).credit_cents, 7);
    }

    #[test]
    fn the_tolerance_comes_from_the_terms() {
        let wide = Terms {
            price_tolerance_bps: 300,
            ..Terms::default()
        };
        assert!(assess_with(63_050, BTC_10K, 65, &wide).within_tolerance); // 3.0% under
        assert!(!assess_with(63_049, BTC_10K, 65, &wide).within_tolerance);
        assert!(!assess_with(64_415, BTC_10K, 65, &strict()).within_tolerance);
        // zero means only an exact payment is exact
        assert!(assess_with(65_000, BTC_10K, 65, &strict()).within_tolerance);
    }

    #[test]
    fn nine_tenths_of_a_percent_under_settles_in_full() {
        let (logic, mut records, dir) = fixture("tol-under");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 6, 64_415, Some(&big_payload()));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 2);
        assert!(issue.results.iter().all(|x| x.is_ok()));
        assert_eq!(issue.skipped_units, 0);
        assert_eq!(issue.credit_cents, 0);
        assert!(issue.within_tolerance);
        assert_eq!(issue.over_under_bps, -90);
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.status, Status::Issued);
        assert_eq!(rec.skipped_units, 0);
        assert_eq!(rec.credit_cents, 0);
        assert_eq!(total(&logic), Amount::from_units(65));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nine_tenths_of_a_percent_over_settles_in_full_with_no_credit() {
        let (logic, mut records, dir) = fixture("tol-over");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 6, 65_585, Some(&big_payload()));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 2);
        assert_eq!(issue.skipped_units, 0);
        assert_eq!(issue.credit_cents, 0);
        assert!(issue.within_tolerance);
        assert_eq!(issue.over_under_bps, 90);
        assert!(records.owed_to(&hex::encode([7u8; 33])).is_empty());
        assert_eq!(total(&logic), Amount::from_units(65));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_point_one_percent_under_goes_to_reconciliation() {
        let (logic, mut records, dir) = fixture("tol-beyond-under");
        let mut price = Price::at(BTC_10K);
        // 64,285 sats buys 64 whole units: the 1 is dropped and the 64 signed,
        // the target rule at work (0.44% of the payment is kept)
        let r = received("aa", 6, 64_285, Some(&big_payload()));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 1);
        assert_eq!(issue.skipped_units, 1);
        assert!(!issue.within_tolerance);
        assert_eq!(total(&logic), Amount::from_units(64));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
        // and on the 5-unit request, where no drop fits, it is held
        let (logic, mut records, dir) = fixture("tol-beyond-held");
        let mut price = Price::at(BTC_10K);
        let r = received("bb", 6, 4_945, Some(PAYLOAD_HEX));
        assert!(matches!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::PaidUnsettled(_)
        ));
        assert_eq!(total(&logic), Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_point_one_percent_over_goes_to_reconciliation() {
        // under the default 10% target the mint keeps the excess; the payment is
        // not "as if exact" (not within the tolerance) but nothing is credited
        let (logic, mut records, dir) = fixture("tol-beyond-over");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 6, 65_715, Some(&big_payload()));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert!(!issue.within_tolerance);
        assert_eq!(issue.credit_cents, 0);
        assert_eq!(issue.over_under_bps, 110);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
        // with a target of 1% the same payment is credited to the payer's DID
        let (logic, mut records, dir) = fixture("tol-beyond-credit");
        let mut price = Price::at(BTC_10K);
        let tight = Terms {
            reconciliation_target_bps: 100,
            ..Terms::default()
        };
        let r = received("bb", 6, 65_715, Some(&big_payload()));
        let issue =
            issued(settle_with(&logic, &mut records, &r, &mut || price.ask(), &tight).unwrap());
        assert_eq!(issue.credit_cents, 7);
        assert!(!issue.within_tolerance);
        assert_eq!(records.get("bb").unwrap().credit_cents, 7);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }
}
