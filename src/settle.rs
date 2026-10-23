use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use mint_core::MintLogic;
use mint_types::{Amount, BlindNonce, MintOutput, MintOutputError, MintOutputOutcome};

use crate::payload::{self, DID_LEN, DecodeError};
use crate::records::{Delivery, Record, Records, RecordsError, Status};
use crate::scan::Received;
use sp_wire::air_request::KIND_PSP_RECONCILE;
use sp_wire::air_request::delivery::{
    FORFEIT_NOTICE, Held, Outcome, ReconcileRules, Refused, SEND_CORRECTED_REQUEST,
    SEND_CREDIT_CLAIM, Signed, SignedOutput, purchase_id,
};
use sp_wire::psp::KeysetsResponse;

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
/// How long a credit, a held payment or a rejected purchase stays open (14 days,
/// in seconds). In that time the payer can settle it with a reconciliation over
/// AIR (kind 15, [`crate::reconcile`]); when it closes unsettled the mint forfeits
/// the payment: it keeps the BTC and owes nothing. The operator sets it in days
/// (`settle.reconciliation_window_days`).
pub const RECONCILIATION_WINDOW_SECS: u64 = 14 * 24 * 60 * 60;

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
    /// `settle.reconciliation_window_days`, in seconds. See
    /// [`RECONCILIATION_WINDOW_SECS`].
    pub reconciliation_window_secs: u64,
}

impl Default for Terms {
    fn default() -> Self {
        Terms {
            lock_confirmations: LOCK_CONFIRMATIONS,
            min_confirmations: MIN_CONFIRMATIONS,
            price_tolerance_bps: PRICE_TOLERANCE_BPS,
            reconciliation_target_bps: PAYMENT_RECONCILIATION_TARGET_BPS,
            reconciliation_window_secs: RECONCILIATION_WINDOW_SECS,
        }
    }
}

/// `reconciliation.max_attempts` by default: the requests refused for their
/// content that are counted against a payment. The second exists only for the
/// keyset-rotation race.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 2;
/// `reconciliation.round_up_from` by default, in cents: a credit's remainder of
/// 9 cents or more rounds up to the next whole PSP unit.
pub const DEFAULT_ROUND_UP_FROM_CENTS: u64 = 9;

/// The fees and limits of reconciliation (`reconciliation.*` and
/// `delivery.read_fee`), in PSP units unless it says otherwise. Every one is an
/// operator setting, and fees are 0 by default. They are fixed for a payment when
/// its window opens (see [`corrected_rules`] and [`credit_rules`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileTerms {
    /// `reconciliation.fee`: taken from what a reconciliation issues.
    pub fee_units: u64,
    /// `reconciliation.max_attempts`: how many refused requests are counted.
    pub max_attempts: u32,
    /// `reconciliation.write_fee`: the cost of one kind 15 write, prepaid by the
    /// SPI for the wallet and taken from what is issued, once.
    pub write_fee_units: u64,
    /// `reconciliation.round_up_from`, in cents (1 to 10).
    pub round_up_from_cents: u64,
    /// `delivery.read_fee`: the read cost the PSP prepays for each record a payer
    /// must read, the first delivery and the follow-up.
    pub read_fee_units: u64,
}

impl Default for ReconcileTerms {
    fn default() -> Self {
        ReconcileTerms {
            fee_units: 0,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            write_fee_units: 0,
            round_up_from_cents: DEFAULT_ROUND_UP_FROM_CENTS,
            read_fee_units: 0,
        }
    }
}

/// Everything settlement is run under: the [`Terms`] and the [`ReconcileTerms`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    pub terms: Terms,
    pub fees: ReconcileTerms,
}

/// A credit in whole PSP units, and the cents rounding left over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditUnits {
    pub units: u64,
    /// The remainder the PSP keeps (rounded down).
    pub kept_cents: u64,
    /// The cents the PSP gives away (rounded up).
    pub given_cents: u64,
}

/// Round a credit in cents to whole PSP units (10 cents each). With
/// `whole = credit_cents / 10` and `rest = credit_cents % 10`: when `rest` is
/// above 0 and at least `round_up_from_cents`, the credit rounds up to `whole + 1`
/// units and the PSP gives away `10 - rest` cents; otherwise it rounds down to
/// `whole` and the PSP keeps `rest`. At the default of 9, X.9 and above rounds up,
/// so the PSP gives away at most 1 cent and keeps at most 8; 10 never rounds up.
pub fn credit_units(credit_cents: u64, round_up_from_cents: u64) -> CreditUnits {
    let (whole, rest) = (credit_cents / 10, credit_cents % 10);
    if rest > 0 && rest >= round_up_from_cents {
        CreditUnits {
            units: whole + 1,
            kept_cents: 0,
            given_cents: 10 - rest,
        }
    } else {
        CreditUnits {
            units: whole,
            kept_cents: rest,
            given_cents: 0,
        }
    }
}

fn rules(send: &str, due_units: u64, open_until: u64, fees: &ReconcileTerms) -> ReconcileRules {
    ReconcileRules {
        send: send.to_string(),
        kind: KIND_PSP_RECONCILE,
        due_units,
        reconciliation_fee_units: fees.fee_units,
        // The first record and the follow-up.
        read_prepaid_units: fees.read_fee_units.saturating_mul(2),
        write_prepaid_units: fees.write_fee_units,
        open_until,
        max_attempts: fees.max_attempts,
        on_failure: FORFEIT_NOTICE.to_string(),
    }
}

/// The rules of a held payment or a rejected purchase: what one corrected request
/// must carry. `due_units` is what the payment buys at the locked price, less the
/// first record's read cost, the fee, the one prepaid write and the follow-up's
/// read cost, floored at 0.
pub fn corrected_rules(
    value_sats: u64,
    price_cents: u64,
    open_until: u64,
    fees: &ReconcileTerms,
) -> ReconcileRules {
    let due = allowed_units(value_sats, price_cents)
        .saturating_sub(fees.read_fee_units)
        .saturating_sub(fees.fee_units)
        .saturating_sub(fees.write_fee_units)
        .saturating_sub(fees.read_fee_units);
    rules(SEND_CORRECTED_REQUEST, due, open_until, fees)
}

/// The rules of a credit claim. `units` is the credit in whole PSP units
/// ([`credit_units`]); the first record's read cost already came off the payment
/// before it was assessed, so only the fee, the prepaid write and the follow-up's
/// read cost come off here. Floored at 0.
pub fn credit_rules(units: u64, open_until: u64, fees: &ReconcileTerms) -> ReconcileRules {
    let due = units
        .saturating_sub(fees.fee_units)
        .saturating_sub(fees.write_fee_units)
        .saturating_sub(fees.read_fee_units);
    rules(SEND_CREDIT_CLAIM, due, open_until, fees)
}

/// The sats that pay for `units` PSP units at the locked price, rounded up.
fn sats_for_units(units: u64, price_cents: u64) -> u64 {
    if units == 0 || price_cents == 0 {
        return 0;
    }
    let sat_cents = u128::from(units) * SAT_CENTS_PER_UNIT;
    u64::try_from(sat_cents.div_ceil(u128::from(price_cents))).unwrap_or(u64::MAX)
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
    /// The payload was already used by another payment (one payload, two bitcoin
    /// payments). The payload is one-time use; this payment is kept and audited
    /// apart, and is never reconcilable.
    DuplicatePayment,
}

#[derive(Debug)]
pub enum SettleError {
    Records(RecordsError),
    Mint(String),
    /// A thread panicked while holding the settlement records. Fatal, like a
    /// poisoned mint lock: the pass fails, and `serve` exits for a supervisor to
    /// restart it (see `service::lock_records`).
    RecordsPoisoned,
}

impl fmt::Display for SettleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettleError::Records(e) => write!(f, "{e}"),
            SettleError::Mint(e) => write!(f, "mint store: {e}"),
            SettleError::RecordsPoisoned => write!(f, "the settlement records lock is poisoned"),
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
    /// The PSP units of the outputs that were signed.
    pub signed_units: u64,
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
/// with the payer's DID, and the payer is told over AIR (a `Held` delivery that
/// carries the rules). The payer settles it with a corrected request over AIR
/// (kind 15, [`crate::reconcile`]), which is checked against this record and the
/// rules fixed when the window opened; if none arrives the payment waits out its
/// reconciliation window and is then forfeited.
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

/// What a payment was worth at the locked price, in whole USD cents (rounded down).
pub fn paid_cents(value_sats: u64, price_cents: u64) -> u64 {
    let cents = u128::from(value_sats) * u128::from(price_cents) / SAT_CENTS_PER_CENT;
    u64::try_from(cents).unwrap_or(u64::MAX)
}

fn now() -> u64 {
    crate::server::now_secs()
}

/// What a payer is told when a purchase is rejected. The record's own `reason`
/// stays the `Debug` text of the [`Rejection`].
fn rejection_text(why: &Rejection) -> String {
    match why {
        Rejection::NoDataOutput => "the transaction has no single data output".to_string(),
        Rejection::BadPayload(e) => format!("the payload could not be read: {e:?}"),
        Rejection::UnknownDenomination(amount) => {
            format!("no key for a note of {} units", amount.units)
        }
        Rejection::DuplicateBlindNonce => "the request uses the same blind nonce twice".to_string(),
        Rejection::BlindNonceAlreadyUsed => {
            "a blind nonce in the request was already signed".to_string()
        }
        Rejection::Overflow => "the total of the request is too large".to_string(),
        Rejection::DuplicatePayment => {
            "the payload was already used by another payment; a payload is one-time use".to_string()
        }
    }
}

/// The delivery of a rejection whose payload could be read, so it names a reply
/// DID. `payload` is the unsealed payload, which identifies the purchase. The
/// purchase stays open for the reconciliation window, like a held payment: the
/// payer keeps it (and its reply key) and is told what was paid.
fn refusal(
    payload: &[u8],
    did: &[u8; DID_LEN],
    why: &Rejection,
    value_sats: u64,
    price_cents: u64,
    policy: &Policy,
) -> Delivery {
    let open_until = now().saturating_add(policy.terms.reconciliation_window_secs);
    Delivery {
        reply_did: hex::encode(did),
        purchase: hex::encode(purchase_id(payload)),
        outcome: Outcome::Refused(Refused {
            reason: rejection_text(why),
            paid_sats: value_sats,
            paid_cents: paid_cents(value_sats, price_cents),
            open_until,
            rules: Some(corrected_rules(
                value_sats,
                price_cents,
                open_until,
                &policy.fees,
            )),
        }),
        sealed: None,
        delivered: false,
        abandoned: None,
    }
}

/// The PSP's live keysets as the keysets call gives them, from the mint
/// (current) and, when it rotates, the previous one. Kept with a delivery so the
/// payer can unblind from the record alone, whatever the keysets are by the time
/// it reads it.
fn keysets_of(current: &MintLogic, previous: Option<(&str, &MintLogic)>) -> KeysetsResponse {
    let units = |keys: &std::collections::BTreeMap<Amount, secp256k1::PublicKey>| -> std::collections::BTreeMap<u64, secp256k1::PublicKey> {
        keys.iter()
            .map(|(amount, key)| (amount.units, *key))
            .collect()
    };
    let current_id = crate::rotation::keyset_id(current.public_keys());
    let mut keysets: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<u64, secp256k1::PublicKey>,
    > = std::collections::BTreeMap::new();
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

/// What a rejection records.
struct Rejected {
    why: Rejection,
    over_under_bps: Option<i64>,
    value_sats: u64,
    /// The record owed to the payer, when the payload could be read and the
    /// payment is not a duplicate.
    delivery: Option<Delivery>,
    /// The first record's read cost, prepaid; recorded when there is a delivery.
    read_units: u64,
    /// The txid of the first payment of the same payload, for a duplicate.
    duplicate_of: Option<String>,
}

impl Rejected {
    /// A rejection nobody can be told about: no readable payload, no delivery.
    fn unreadable(why: Rejection, value_sats: u64) -> Self {
        Rejected {
            why,
            over_under_bps: None,
            value_sats,
            delivery: None,
            read_units: 0,
            duplicate_of: None,
        }
    }
}

fn reject(
    records: &mut Records,
    txid: &str,
    mut record: Record,
    rejected: Rejected,
) -> Result<Settlement, SettleError> {
    let Rejected {
        why,
        over_under_bps,
        value_sats,
        delivery,
        read_units,
        duplicate_of,
    } = rejected;
    record.status = Status::Rejected;
    record.issued = false;
    record.over_under_bps = over_under_bps;
    record.skipped_units = 0;
    record.value_sats = value_sats;
    record.reason = Some(format!("{why:?}"));
    record.duplicate_of = duplicate_of;
    // A rejection whose payer can be told stays open, like a held payment; one
    // with no readable payload has nobody to tell, and the payer takes the loss.
    // A duplicate has no delivery, so no window.
    record.open_until = delivery.as_ref().and_then(|d| d.outcome.open_until());
    record.rules = delivery.as_ref().and_then(|d| d.outcome.rules().cloned());
    record.read_prepaid_units = if delivery.is_some() { read_units } else { 0 };
    if let Some(d) = &delivery {
        record.payer_did = Some(d.reply_did.clone());
    }
    record.delivery = delivery;
    records.put(txid, record)?;
    Ok(Settlement::Rejected(why))
}

/// Advance one payment as far as it can go this run, under the settlement
/// `policy`: the [`Terms`] and the reconciliation fees.
///
/// - At 1 confirmation the price is locked and stored. The first run that sees
///   the payment does this, even if it already has more confirmations (the
///   record is then marked late).
/// - At 6 confirmations it is settled with the stored price, never a live one.
/// - Every payment that gets a record is settled once, whether or not anything
///   is signed. A settled payment is not looked at again.
///
/// `logic` is the current key set's mint. `previous` is the previous key set's
/// ID and mint, when the mint has one (a rotor that has not rotated yet has
/// none). A payload that names the previous set is signed by it, as every other
/// request is; one that names the current set, no set, or a set that is no
/// longer live (the BTC is already paid, and blinding does not depend on the
/// key) is signed by `logic`. The settlement names the set that signed.
///
/// A payment whose payload was already used by another is rejected as a duplicate
/// (no delivery, no window). The first delivery's read cost comes off the paid
/// value before it is assessed. The delivery of a purchase that opens a window
/// carries the rules, fixed here.
pub fn settle_ruled(
    logic: &MintLogic,
    previous: Option<(&str, &MintLogic)>,
    records: &mut Records,
    received: &Received,
    price: &mut dyn FnMut() -> Result<u64, String>,
    policy: &Policy,
) -> Result<Settlement, SettleError> {
    let (terms, fees) = (&policy.terms, &policy.fees);
    let txid = received.txid.as_str();
    if let Some(existing) = records.get(txid)
        && existing.status != Status::Locked
    {
        return Ok(Settlement::AlreadySettled(existing.status));
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
        return reject(
            records,
            txid,
            record,
            Rejected::unreadable(Rejection::NoDataOutput, received.value_sats),
        );
    };
    let payload = match payload::decode(bytes) {
        Ok(payload) => payload,
        Err(e) => {
            return reject(
                records,
                txid,
                record,
                Rejected::unreadable(Rejection::BadPayload(e), received.value_sats),
            );
        }
    };
    // The payload is one-time use. A second payment that carries it buys nothing:
    // it is kept and audited apart, with no delivery (the cell is the first
    // payment's) and no window.
    let payer_did_hex = hex::encode(payload.did);
    if let Some(first) =
        records.first_of_purchase(&payer_did_hex, &hex::encode(purchase_id(bytes)), txid)
    {
        return reject(
            records,
            txid,
            record,
            Rejected {
                why: Rejection::DuplicatePayment,
                over_under_bps: None,
                value_sats: received.value_sats,
                delivery: None,
                read_units: 0,
                duplicate_of: Some(first),
            },
        );
    }
    let (issuer, issuer_keyset) = match (payload.keyset.as_deref(), previous) {
        (Some(named), Some((id, prev))) if named == id => (prev, id.to_string()),
        _ => (logic, crate::rotation::keyset_id(logic.public_keys())),
    };
    let keys = issuer.public_keys();
    let requested = match check(|amount| keys.contains_key(&amount), &payload.outputs) {
        Ok(requested) => requested,
        Err(why) => {
            let delivery = refusal(
                bytes,
                &payload.did,
                &why,
                received.value_sats,
                price_cents,
                policy,
            );
            return reject(
                records,
                txid,
                record,
                Rejected {
                    why,
                    over_under_bps: None,
                    value_sats: received.value_sats,
                    delivery: Some(delivery),
                    read_units: fees.read_fee_units,
                    duplicate_of: None,
                },
            );
        }
    };
    // The first delivery's read is prepaid by the PSP: its cost comes off what was
    // paid before the payment is assessed. With the default fee of 0 nothing changes.
    let assessed_sats = received
        .value_sats
        .saturating_sub(sats_for_units(fees.read_fee_units, price_cents));
    let assessment = assess_with(assessed_sats, price_cents, requested, terms);
    for output in &payload.outputs {
        let used = issuer
            .is_blind_nonce_used(&output.blind_nonce)
            .map_err(|e| SettleError::Mint(e.to_string()))?;
        if used {
            let delivery = refusal(
                bytes,
                &payload.did,
                &Rejection::BlindNonceAlreadyUsed,
                received.value_sats,
                price_cents,
                policy,
            );
            return reject(
                records,
                txid,
                record,
                Rejected {
                    why: Rejection::BlindNonceAlreadyUsed,
                    over_under_bps: Some(assessment.over_under_bps),
                    value_sats: received.value_sats,
                    delivery: Some(delivery),
                    read_units: fees.read_fee_units,
                    duplicate_of: None,
                },
            );
        }
    }
    let plan = plan(&payload.outputs, &assessment, terms);

    if plan.unsettled {
        // Short beyond the target and no drop gets within it. Sign nothing, keep
        // the BTC, and record the payment under the payer's DID. This record is
        // the whole of the mint's side today; see `Unsettled`.
        let until = now().saturating_add(terms.reconciliation_window_secs);
        let reason = "the payment is short of the request by more than the PSP absorbs, and no \
                      smaller request fits what was paid"
            .to_string();
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
        held.payer_did = Some(payer_did_hex.clone());
        held.requested_units = requested;
        held.value_sats = received.value_sats;
        held.open_until = Some(until);
        let held_rules = corrected_rules(received.value_sats, price_cents, until, fees);
        held.rules = Some(held_rules.clone());
        held.read_prepaid_units = fees.read_fee_units;
        // Tell the payer, so the wallet knows the payment is held and what was
        // paid. Nothing is signed, so there is no crash window: this is the one
        // write.
        held.delivery = Some(Delivery {
            reply_did: payer_did_hex,
            purchase: hex::encode(purchase_id(bytes)),
            outcome: Outcome::Held(Held {
                reason,
                paid_sats: received.value_sats,
                paid_cents: paid_cents(received.value_sats, price_cents),
                requested_units: requested,
                over_under_bps: assessment.over_under_bps,
                open_until: until,
                rules: Some(held_rules),
            }),
            sealed: None,
            delivered: false,
            abandoned: None,
        });
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
    // A credit is owed for the reconciliation window; after it the mint keeps
    // the overpayment. Set in the first write, so it holds whatever happens next.
    let open_until = (assessment.credit_cents > 0)
        .then(|| now().saturating_add(terms.reconciliation_window_secs));
    settled.open_until = open_until;
    // The rules of a claim are fixed here too, with the window: the credit in
    // whole PSP units, the fees and the cap in force now.
    let credit = credit_units(assessment.credit_cents, fees.round_up_from_cents);
    settled.rules = open_until.map(|until| credit_rules(credit.units, until, fees));
    if open_until.is_some() {
        settled.kept_cents = credit.kept_cents;
        settled.given_cents = credit.given_cents;
    }
    settled.read_prepaid_units = fees.read_fee_units;
    records.put(txid, settled.clone())?;

    // The set that signs is the one the payload names if it is live, else the
    // current one. PSP uses MintLogic::issue, not issue_with_key. Every PSP note of a given
    // amount is signed by the same key from the mint's fixed key table, and
    // issue looks that key up by amount. issue_with_key is for mints whose key
    // depends on more than the amount, such as the ESP, where it also depends
    // on the note's terms. See the docs on MintLogic::issue in mint-core.
    let results: Vec<Result<MintOutputOutcome, MintOutputError>> = plan
        .sign
        .iter()
        .map(|&i| issuer.issue(&payload.outputs[i]))
        .collect();
    let blind_nonces: Vec<BlindNonce> = plan
        .sign
        .iter()
        .map(|&i| payload.outputs[i].blind_nonce)
        .collect();
    // Keep what the payer is owed in the record, so that delivering it survives a
    // restart and an AIR server that is down. The purchase is issued by now, so a
    // failure to write this is reported and does not undo it.
    let signatures: Vec<SignedOutput> = blind_nonces
        .iter()
        .zip(&results)
        .filter_map(|(blind_nonce, result)| {
            result.as_ref().ok().map(|signature| SignedOutput {
                blind_nonce: blind_nonce.0.to_string(),
                signature: signature.0.to_string(),
            })
        })
        .collect();
    settled.delivery = Some(Delivery {
        reply_did: hex::encode(payload.did),
        purchase: hex::encode(purchase_id(bytes)),
        outcome: Outcome::Signed(Signed {
            keyset: issuer_keyset.clone(),
            signatures,
            keysets: keysets_of(logic, previous),
            skipped_units: plan.skipped_units,
            credit_cents: assessment.credit_cents,
            open_until: open_until.unwrap_or(0),
            rules: settled.rules.clone(),
        }),
        sealed: None,
        delivered: false,
        abandoned: None,
    });
    if let Err(e) = records.put(txid, settled) {
        eprintln!(
            "{txid}: issued, but what the payer is owed could not be stored ({e}); the signatures were not kept"
        );
    }
    let signed_units: u64 = plan
        .sign
        .iter()
        .zip(&results)
        .filter(|(_, result)| result.is_ok())
        .map(|(&i, _)| payload.outputs[i].amount.units)
        .sum();
    Ok(Settlement::Issued(Issue {
        keyset: issuer_keyset,
        results,
        blind_nonces,
        signed_units,
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
    use crate::rotation::Rotor;
    use secp256k1::{PublicKey, SECP256K1, SecretKey};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    // DID 0279be66..1798, then outputs 4 -> 3G and 1 -> 5G (5 units in all)
    const PAYLOAD_HEX: &str = "210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";
    // A payload with a 10-byte reply ("reply-test") where a 33-byte DID belongs
    const SHORT_REPLY_PAYLOAD_HEX: &str = "0a7265706c792d7465737402000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";

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
        // it rounds down to whole units
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
        // it truncates toward zero:
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
        assert_eq!(check(any, &[out(4, 1), out(1, 2)]), Ok(5));
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psp-mint-{}-{name}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// The mint the way `Settings::open_mint` opens it, with rotation off
    /// (`rotation.activation_hours = 0`), and an empty record store.
    fn fixture(name: &str) -> (MintLogic, Records, PathBuf) {
        let dir = scratch(name);
        let (_rotor, logic) = Rotor::open(&dir, Amount::from_units(1024), 0, 0).unwrap();
        let records = Records::open(&dir.join("settlements.json")).unwrap();
        (logic, records, dir)
    }

    /// A mint that has rotated once, as `serve` would find it an hour after the
    /// first issuance: the current key set is generation 1, and generation 0 is
    /// the previous one (in `rotor.previous`).
    fn rotated(name: &str) -> (Rotor, MintLogic, Records, PathBuf) {
        let dir = scratch(name);
        let t = 10_000;
        let (mut rotor, mut logic) =
            Rotor::open_at(&dir, Amount::from_units(1024), 1, 1, t).unwrap();
        rotor.first_issue = Some(t);
        rotor.clock.store(t + 3600, Ordering::SeqCst);
        rotor.refresh(&mut logic).unwrap();
        assert_eq!(rotor.current.epoch, 1);
        assert_eq!(rotor.previous.as_ref().unwrap().0.epoch, 0);
        let records = Records::open(&dir.join("settlements.json")).unwrap();
        (rotor, logic, records, dir)
    }

    /// The previous key set as `service::settle_one` hands it to `settle_ruled`.
    fn previous_of(rotor: &Rotor) -> Option<(&str, &MintLogic)> {
        rotor
            .previous
            .as_ref()
            .map(|(keys, logic)| (keys.id.as_str(), logic))
    }

    /// The default rules with these terms and no fees.
    fn under(terms: Terms) -> Policy {
        Policy {
            terms,
            ..Policy::default()
        }
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
        settle_ruled(
            logic,
            None,
            records,
            r,
            &mut || price.ask(),
            &Policy::default(),
        )
        .unwrap()
    }

    /// Settle the 4 + 1 payload naming `named` (`prev` is the previous set's
    /// ID, `cur` the current one's, or something else), and say which set signed.
    fn signed_by(
        case: &str,
        named: &dyn Fn(&str, &str) -> Option<String>,
    ) -> (String, String, String) {
        let (rotor, current, mut records, dir) = rotated(&format!("named-{case}"));
        let (cur_id, prev_id) = (
            crate::rotation::keyset_id(current.public_keys()),
            rotor.previous.as_ref().unwrap().0.id.clone(),
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
            settle_ruled(
                &current,
                previous_of(&rotor),
                &mut records,
                &r,
                &mut || price.ask(),
                &Policy::default(),
            )
            .unwrap(),
        );
        fs::remove_dir_all(&dir).unwrap();
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
        let old = received("cc", 6, 5_000, Some(SHORT_REPLY_PAYLOAD_HEX));
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
        settle_ruled(
            logic,
            None,
            records,
            r,
            &mut || price.ask(),
            &Policy::default(),
        )
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
        // it is open on the payer's account (inside its window, counted in
        // /audit's open payments with the bitcoin held for it), not looked at
        // again, and not retried
        assert!(rec.is_open());
        assert!(rec.open_until.is_some());
        let open = records.reconciliation().open;
        assert_eq!((open.payments, open.sats, open.credit_cents), (1, 3_500, 0));
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
            ruled(&logic, &mut records, &r, &mut price, &under(strict())),
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
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.credit_cents, 0);
        // nothing is owed: no window was opened
        assert!(!rec.is_open());
        assert_eq!(rec.open_until, None);
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
        // the credit is open for the payer to claim
        assert!(rec.is_open());
        assert!(rec.open_until.is_some());
        let open = records.reconciliation().open;
        assert_eq!((open.payments, open.credit_cents), (1, 10));
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
        assert!(
            matches!(
                try_run(&logic, &mut records, &settled, &mut price),
                Err(SettleError::Records(RecordsError::Io(_)))
            ),
            "the failed write is a records I/O error"
        );
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
        assert!(
            matches!(
                try_run(&logic, &mut records, &settled, &mut price),
                Err(SettleError::Records(RecordsError::Io(_)))
            ),
            "the failed write is a records I/O error"
        );
        assert_eq!(records.get("aa").unwrap().status, Status::Locked);
        assert_eq!(total(&logic), Amount::ZERO);
        // still waiting to settle, so the next scan picks it up again
        assert_eq!(records.unsettled(), vec!["aa".to_string()]);
        unblock_records_writes(&dir);
        assert!(matches!(
            run(&logic, &mut records, &settled, &mut price),
            Settlement::PaidUnsettled(_)
        ));
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.status, Status::PaidUnsettled);
        assert!(rec.is_open());
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
        let until = records.get("aa").unwrap().open_until;
        assert!(until.is_some());
        // the process dies; a new one opens the same files
        drop(records);
        let mut records = Records::open(&dir.join("settlements.json")).unwrap();
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.status, Status::PaidUnsettled);
        assert!(rec.is_open());
        assert_eq!(rec.open_until, until);
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
    fn an_issued_purchase_keeps_what_the_payer_is_owed_in_its_record() {
        let (logic, mut records, dir) = fixture("delivery-issued");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 6, 5_000, Some(PAYLOAD_HEX));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        let reopened = Records::open(&dir.join("settlements.json")).unwrap();
        let d = reopened.get("aa").unwrap().delivery.clone().unwrap();
        assert!(!d.delivered);
        assert_eq!(d.sealed, None);
        assert_eq!(d.reply_did, DID_HEX);
        assert_eq!(
            d.purchase,
            hex::encode(purchase_id(&hex::decode(PAYLOAD_HEX).unwrap()))
        );
        let Outcome::Signed(signed) = &d.outcome else {
            panic!(
                "an issued purchase delivers its signatures: {:?}",
                d.outcome
            );
        };
        assert_eq!(signed.keyset, issue.keyset);
        assert_eq!(signed.signatures.len(), 2);
        for ((nonce, result), pair) in issue
            .blind_nonces
            .iter()
            .zip(&issue.results)
            .zip(&signed.signatures)
        {
            assert_eq!(pair.blind_nonce, nonce.0.to_string());
            assert_eq!(pair.signature, result.as_ref().unwrap().0.to_string());
        }
        assert_eq!(signed.keysets.current, issue.keyset);
        assert_eq!(signed.keysets.previous, None);
        assert!(signed.keysets.keysets.contains_key(&issue.keyset));
        assert_eq!((signed.skipped_units, signed.credit_cents), (0, 0));
        // a settled purchase is not looked at again, and keeps its delivery
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::AlreadySettled(Status::Issued)
        );
        assert_eq!(records.get("aa").unwrap().delivery.as_ref(), Some(&d));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_delivery_names_both_live_keysets_when_the_mint_rotates() {
        let (rotor, current, mut records, dir) = rotated("delivery-rotated");
        let (cur_id, prev_id) = (
            crate::rotation::keyset_id(current.public_keys()),
            rotor.previous.as_ref().unwrap().0.id.clone(),
        );
        let r = received("aa", 6, 5_000, Some(PAYLOAD_HEX));
        let mut price = Price::at(BTC_10K);
        settle_ruled(
            &current,
            previous_of(&rotor),
            &mut records,
            &r,
            &mut || price.ask(),
            &Policy::default(),
        )
        .unwrap();
        let d = records.get("aa").unwrap().delivery.clone().unwrap();
        let Outcome::Signed(signed) = d.outcome else {
            panic!("signed");
        };
        assert_eq!(signed.keysets.current, cur_id);
        assert_eq!(signed.keysets.previous.as_deref(), Some(prev_id.as_str()));
        assert_eq!(signed.keysets.keysets.len(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_partial_fill_and_a_credit_say_so_in_the_delivery() {
        let (logic, mut records, dir) = fixture("delivery-fill");
        let mut price = Price::at(BTC_10K);
        // 4,300 sats buys 4 of the 5 units asked: the 1 is dropped
        let short = received("aa", 6, 4_300, Some(&payload_bytes(&[(4, 1), (1, 2)])));
        let issue = issued(run(&logic, &mut records, &short, &mut price));
        let Some(Delivery {
            outcome: Outcome::Signed(signed),
            ..
        }) = records.get("aa").unwrap().delivery.clone()
        else {
            panic!("a delivery");
        };
        assert_eq!(signed.skipped_units, 1);
        assert_eq!(signed.signatures.len(), issue.results.len());
        assert_eq!(signed.signatures.len(), 1);
        assert_eq!(signed.credit_cents, 0);

        // 6,000 sats is 20% over: signed in full, and $0.10 is owed back
        let over = received("bb", 6, 6_000, Some(&payload_bytes(&[(4, 3), (1, 4)])));
        issued(run(&logic, &mut records, &over, &mut price));
        let Some(Delivery {
            outcome: Outcome::Signed(signed),
            ..
        }) = records.get("bb").unwrap().delivery.clone()
        else {
            panic!("a delivery");
        };
        assert_eq!((signed.skipped_units, signed.credit_cents), (0, 10));
        assert_eq!(signed.signatures.len(), 2);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rejection_with_a_readable_payload_keeps_a_refusal_and_one_without_keeps_none() {
        let (logic, mut records, dir) = fixture("delivery-refused");
        let mut price = Price::at(BTC_10K);
        let first = received("aa", 6, 5_000, Some(&payload_bytes(&[(4, 1), (1, 2)])));
        issued(run(&logic, &mut records, &first, &mut price));

        // a blind nonce already signed
        let reused_hex = payload_bytes(&[(2, 3), (1, 2)]);
        let reused = received("bb", 6, 5_000, Some(&reused_hex));
        run(&logic, &mut records, &reused, &mut price);
        let d = records.get("bb").unwrap().delivery.clone().unwrap();
        assert_eq!(d.reply_did, hex::encode([7u8; 33]));
        assert_eq!(
            d.purchase,
            hex::encode(purchase_id(&hex::decode(&reused_hex).unwrap()))
        );
        let Outcome::Refused(refused) = &d.outcome else {
            panic!("a refusal: {:?}", d.outcome);
        };
        assert_eq!(
            refused.reason,
            rejection_text(&Rejection::BlindNonceAlreadyUsed)
        );
        // it says what was paid: 5,000 sats at $10,000 per BTC is $0.50
        assert_eq!((refused.paid_sats, refused.paid_cents), (5_000, 50));
        // and the purchase stays open for the reconciliation window
        let until = records.get("bb").unwrap().open_until.expect("open");
        assert_eq!(refused.open_until, until);
        assert_window(until, RECONCILIATION_WINDOW_SECS);
        assert_eq!(records.get("bb").unwrap().value_sats, 5_000);
        assert!(!d.delivered);

        // a denomination the mint has no key for
        let unknown = received("cc", 6, 5_000, Some(&payload_bytes(&[(2048, 4)])));
        run(&logic, &mut records, &unknown, &mut price);
        let Some(Delivery {
            outcome: Outcome::Refused(refused),
            ..
        }) = records.get("cc").unwrap().delivery.clone()
        else {
            panic!("a refusal");
        };
        assert!(refused.reason.contains("2048"), "{}", refused.reason);
        assert!(refused.open_until > 0);

        // nothing readable: no reply DID, so nothing can be addressed
        for (txid, payload) in [("dd", None), ("ee", Some("00"))] {
            let bad = received(txid, 6, 5_000, payload);
            run(&logic, &mut records, &bad, &mut price);
            let rec = records.get(txid).unwrap();
            assert_eq!(rec.status, Status::Rejected);
            assert_eq!(rec.delivery, None, "{txid}");
            // nobody to tell, so no window: the payer takes the loss
            assert_eq!(rec.open_until, None, "{txid}");
            assert_eq!(rec.value_sats, 5_000, "{txid}");
        }
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `until` is `window` seconds from about now.
    fn assert_window(until: u64, window: u64) {
        let now = crate::server::now_secs();
        assert!(
            until + 30 >= now + window && until <= now + window + 30,
            "window closes at {until}, expected about {}",
            now + window
        );
    }

    #[test]
    fn a_held_payment_delivers_a_held_record_with_what_was_paid() {
        let (logic, mut records, dir) = fixture("delivery-held");
        let mut price = Price::at(BTC_10K);
        // far short: 1,000 sats is $0.10 against the $0.50 asked, so nothing is signed
        let r = received("aa", 6, 1_000, Some(PAYLOAD_HEX));
        assert!(matches!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::PaidUnsettled(_)
        ));
        let rec = records.get("aa").unwrap();
        let d = rec
            .delivery
            .clone()
            .expect("a held payment tells the payer");
        assert_eq!(d.reply_did, DID_HEX);
        assert_eq!(
            d.purchase,
            hex::encode(purchase_id(&hex::decode(PAYLOAD_HEX).unwrap()))
        );
        let Outcome::Held(held) = &d.outcome else {
            panic!("a held record: {:?}", d.outcome);
        };
        assert_eq!((held.paid_sats, held.paid_cents), (1_000, 10));
        assert_eq!(held.requested_units, 5);
        assert_eq!(Some(held.over_under_bps), rec.over_under_bps);
        assert!(held.over_under_bps < 0);
        assert_eq!(rec.open_until, Some(held.open_until));
        assert_window(held.open_until, RECONCILIATION_WINDOW_SECS);
        assert_eq!(rec.status, Status::PaidUnsettled);
        assert_eq!(rec.forfeited_at, None);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_reconciliation_window_comes_from_the_terms_and_only_open_payments_have_one() {
        let (logic, mut records, dir) = fixture("window");
        let terms = Terms {
            reconciliation_window_secs: 3_600,
            ..Terms::default()
        };
        let mut price = Price::at(BTC_10K);
        let mut go = |records: &mut Records, txid: &str, sats: u64, payload: &str| {
            let r = received(txid, 6, sats, Some(payload));
            ruled(&logic, records, &r, &mut price, &under(terms))
        };
        // exact: closed, no window, and the signed record says nothing is owed
        go(&mut records, "aa", 5_000, PAYLOAD_HEX);
        let exact = records.get("aa").unwrap();
        assert_eq!(exact.open_until, None);
        let Outcome::Signed(signed) = &exact.delivery.as_ref().unwrap().outcome else {
            panic!("signed");
        };
        assert_eq!((signed.credit_cents, signed.open_until), (0, 0));
        // 20% over: signed in full, and the credit is open for the window
        go(&mut records, "bb", 6_000, &payload_bytes(&[(4, 3), (1, 4)]));
        let over = records.get("bb").unwrap();
        let until = over.open_until.expect("a credit is open");
        assert_window(until, 3_600);
        assert_eq!(over.status, Status::Issued);
        let Outcome::Signed(signed) = &over.delivery.as_ref().unwrap().outcome else {
            panic!("signed");
        };
        assert_eq!((signed.credit_cents, signed.open_until), (10, until));
        // short by a little: signed in part, the mint keeps the remainder, closed
        go(&mut records, "cc", 4_300, &payload_bytes(&[(4, 5), (1, 6)]));
        assert_eq!(records.get("cc").unwrap().open_until, None);
        // held, with the same window
        go(&mut records, "dd", 1_000, &payload_bytes(&[(4, 7), (1, 8)]));
        assert_window(records.get("dd").unwrap().open_until.unwrap(), 3_600);
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
            ruled(&logic, records, &r, &mut price, &under(terms))
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
    fn within_the_tolerance_either_way_settles_in_full_with_no_credit() {
        // 0.9% under: the 65-unit request is settled in full as if exact
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

        // 0.9% over: also in full, and the excess is kept, not credited
        let (logic, mut records, dir) = fixture("tol-over");
        let mut price = Price::at(BTC_10K);
        let r = received("aa", 6, 65_585, Some(&big_payload()));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 2);
        assert_eq!(issue.skipped_units, 0);
        assert_eq!(issue.credit_cents, 0);
        assert!(issue.within_tolerance);
        assert_eq!(issue.over_under_bps, 90);
        // nothing is owed: no window was opened
        let rec = records.get("aa").unwrap();
        assert!(!rec.is_open());
        assert_eq!(rec.open_until, None);
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
        let issue = issued(ruled(&logic, &mut records, &r, &mut price, &under(tight)));
        assert_eq!(issue.credit_cents, 7);
        assert!(!issue.within_tolerance);
        assert_eq!(records.get("bb").unwrap().credit_cents, 7);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    // ---- the rules of a reconciliation, fixed when the window opens ----

    /// The default policy with these fees.
    fn with_fees(f: impl FnOnce(&mut ReconcileTerms)) -> Policy {
        let mut policy = Policy::default();
        f(&mut policy.fees);
        policy
    }

    fn ruled(
        logic: &MintLogic,
        records: &mut Records,
        r: &Received,
        price: &mut Price,
        policy: &Policy,
    ) -> Settlement {
        settle_ruled(logic, None, records, r, &mut || price.ask(), policy).unwrap()
    }

    #[test]
    fn a_credit_rounds_to_whole_units_by_round_up_from() {
        // 250 cents: 25 units, nothing left
        assert_eq!(
            credit_units(250, 9),
            CreditUnits {
                units: 25,
                kept_cents: 0,
                given_cents: 0
            }
        );
        // 255: the 5 cents are kept; 258: the 8 are kept; 259: 1 cent is given
        assert_eq!(credit_units(255, 9).kept_cents, 5);
        assert_eq!(credit_units(255, 9).units, 25);
        assert_eq!(credit_units(258, 9).kept_cents, 8);
        assert_eq!(credit_units(258, 9).units, 25);
        assert_eq!(
            credit_units(259, 9),
            CreditUnits {
                units: 26,
                kept_cents: 0,
                given_cents: 1
            }
        );
        // 10 never rounds up; 1 rounds up from 0.1 (but a rest of 0 is not a rest)
        assert_eq!(credit_units(259, 10).units, 25);
        assert_eq!(credit_units(259, 10).kept_cents, 9);
        assert_eq!(credit_units(251, 1).units, 26);
        assert_eq!(credit_units(251, 1).given_cents, 9);
        assert_eq!(credit_units(250, 1).units, 25);
        assert_eq!(credit_units(255, 5).units, 26);
        assert_eq!(credit_units(255, 5).given_cents, 5);
        // less than a unit
        assert_eq!(credit_units(5, 9).units, 0);
        assert_eq!(credit_units(5, 9).kept_cents, 5);
    }

    #[test]
    fn the_rules_take_every_fee_off_what_is_due_and_floor_at_zero() {
        let fees = ReconcileTerms {
            fee_units: 1,
            write_fee_units: 2,
            read_fee_units: 3,
            ..ReconcileTerms::default()
        };
        // 20 units paid: less the first read (3), the fee (1), the write (2) and
        // the follow-up's read (3)
        let held = corrected_rules(20_000, BTC_10K, 777, &fees);
        assert_eq!(held.send, SEND_CORRECTED_REQUEST);
        assert_eq!(held.kind, KIND_PSP_RECONCILE);
        assert_eq!(held.due_units, 20 - 3 - 1 - 2 - 3);
        assert_eq!(held.reconciliation_fee_units, 1);
        assert_eq!(held.read_prepaid_units, 6);
        assert_eq!(held.write_prepaid_units, 2);
        assert_eq!(held.open_until, 777);
        assert_eq!(held.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(held.on_failure, FORFEIT_NOTICE);
        // the first read already came off the payment of a credit
        let credit = credit_rules(10, 777, &fees);
        assert_eq!(credit.send, SEND_CREDIT_CLAIM);
        assert_eq!(credit.due_units, 10 - 1 - 2 - 3);
        // fees beyond what there is leave nothing due, never a wrapped number
        assert_eq!(corrected_rules(5_000, BTC_10K, 1, &fees).due_units, 0);
        assert_eq!(credit_rules(2, 1, &fees).due_units, 0);
        // with no fees the whole is due
        let plain = ReconcileTerms::default();
        assert_eq!(corrected_rules(20_000, BTC_10K, 1, &plain).due_units, 20);
        assert_eq!(credit_rules(7, 1, &plain).due_units, 7);
        assert_eq!(plain.max_attempts, 2);
        assert_eq!(plain.round_up_from_cents, 9);
    }

    #[test]
    fn a_held_payment_stores_the_rules_in_force_and_tells_the_payer_the_same() {
        let (logic, mut records, dir) = fixture("rules-held");
        let mut price = Price::at(BTC_10K);
        let payload = payload_bytes(&[(64, 1), (1, 2)]);
        // 30,000 sats buys 30 units of the 65 asked: held
        let r = received("aa", 6, 30_000, Some(&payload));
        assert!(matches!(
            ruled(&logic, &mut records, &r, &mut price, &Policy::default()),
            Settlement::PaidUnsettled(_)
        ));
        let rec = records.get("aa").unwrap();
        let rules = rec.rules.clone().expect("a held payment has rules");
        assert_eq!(rules.send, SEND_CORRECTED_REQUEST);
        assert_eq!(rules.due_units, 30);
        assert_eq!(rules.open_until, rec.open_until.unwrap());
        assert_eq!(rules.max_attempts, 2);
        assert_eq!(rec.read_prepaid_units, 0);
        let Outcome::Held(held) = &rec.delivery.as_ref().unwrap().outcome else {
            panic!("held");
        };
        assert_eq!(held.rules.as_ref(), Some(&rules));

        // the same payment under fees: one unit of each, so 31 units paid less
        // 1 (first read) 1 (fee) 1 (write) 1 (follow-up read)
        let policy = with_fees(|f| {
            f.fee_units = 1;
            f.write_fee_units = 1;
            f.read_fee_units = 1;
            f.max_attempts = 3;
        });
        let r = received("bb", 6, 31_000, Some(&payload_bytes(&[(64, 3), (1, 4)])));
        assert!(matches!(
            ruled(&logic, &mut records, &r, &mut price, &policy),
            Settlement::PaidUnsettled(_)
        ));
        let rec = records.get("bb").unwrap();
        let rules = rec.rules.clone().unwrap();
        assert_eq!(rules.due_units, 27);
        assert_eq!(rules.reconciliation_fee_units, 1);
        assert_eq!(rules.read_prepaid_units, 2);
        assert_eq!(rules.write_prepaid_units, 1);
        assert_eq!(rules.max_attempts, 3);
        assert_eq!(rec.read_prepaid_units, 1);
        // the earlier payment's rules are untouched by the later policy
        assert_eq!(
            records.get("aa").unwrap().rules.as_ref().unwrap().due_units,
            30
        );
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rejected_purchase_with_a_readable_payload_opens_a_window_with_rules() {
        let (logic, mut records, dir) = fixture("rules-refused");
        let mut price = Price::at(BTC_10K);
        // the same blind nonce twice: refused, with 8,000 sats (8 units) paid
        let r = received("aa", 6, 8_000, Some(&payload_bytes(&[(2, 1), (1, 1)])));
        assert_eq!(
            ruled(&logic, &mut records, &r, &mut price, &Policy::default()),
            Settlement::Rejected(Rejection::DuplicateBlindNonce)
        );
        let rec = records.get("aa").unwrap();
        let rules = rec
            .rules
            .clone()
            .expect("a refusal with a reply DID is open");
        assert_eq!(
            (rules.send.as_str(), rules.due_units),
            (SEND_CORRECTED_REQUEST, 8)
        );
        let Outcome::Refused(refused) = &rec.delivery.as_ref().unwrap().outcome else {
            panic!("refused");
        };
        assert_eq!(refused.rules.as_ref(), Some(&rules));
        assert_eq!(refused.open_until, rules.open_until);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_credit_stores_its_claim_with_the_rounding_and_the_fees_in_force() {
        let (logic, mut records, dir) = fixture("rules-credit");
        let mut price = Price::at(BTC_10K);
        // 5 units asked ($0.50); at $10,000 per BTC, S sats is S / 100 cents, so
        // S = 7,500 is $0.75 and a credit of 25 cents
        let cases = [
            // (sats, round_up_from, credit, units, kept, given)
            (7_000, 9, 20, 2, 0, 0),
            (7_500, 9, 25, 2, 5, 0),
            (7_800, 9, 28, 2, 8, 0),
            (7_900, 9, 29, 3, 0, 1),
            (7_900, 10, 29, 2, 9, 0),
            (7_500, 5, 25, 3, 0, 5),
        ];
        for (i, &(sats, from, credit, units, kept, given)) in cases.iter().enumerate() {
            let i = i as u8 * 2 + 1;
            let policy = with_fees(|f| f.round_up_from_cents = from);
            let txid = format!("tx{i}");
            let r = received(&txid, 6, sats, Some(&payload_bytes(&[(4, i), (1, i + 1)])));
            let issue = issued(ruled(&logic, &mut records, &r, &mut price, &policy));
            assert_eq!(issue.credit_cents, credit, "{sats} sats");
            let rec = records.get(&txid).unwrap();
            let rules = rec.rules.clone().expect("a credit is claimable");
            assert_eq!(rules.send, SEND_CREDIT_CLAIM);
            assert_eq!(rules.due_units, units, "{sats} sats, round up from {from}");
            assert_eq!(
                (rec.kept_cents, rec.given_cents),
                (kept, given),
                "{sats}/{from}"
            );
            assert_eq!(rules.open_until, rec.open_until.unwrap());
            let Outcome::Signed(signed) = &rec.delivery.as_ref().unwrap().outcome else {
                panic!("signed");
            };
            assert_eq!(signed.rules.as_ref(), Some(&rules));
        }
        // a payment that is not owed a credit has no rules and no window
        let r = received("exact", 6, 5_000, Some(&payload_bytes(&[(4, 31), (1, 32)])));
        issued(ruled(
            &logic,
            &mut records,
            &r,
            &mut price,
            &Policy::default(),
        ));
        let rec = records.get("exact").unwrap();
        assert!(rec.rules.is_none() && rec.open_until.is_none());
        let Outcome::Signed(signed) = &rec.delivery.as_ref().unwrap().outcome else {
            panic!("signed");
        };
        assert!(signed.rules.is_none());
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_first_delivery_read_comes_off_what_was_paid_before_it_is_assessed() {
        let (logic, mut records, dir) = fixture("read-fee");
        let mut price = Price::at(BTC_10K);
        let policy = with_fees(|f| f.read_fee_units = 1);
        // 6,000 sats less the 1,000 the read costs is exactly the 5 units asked
        let r = received("aa", 6, 6_000, Some(&payload_bytes(&[(4, 1), (1, 2)])));
        let issue = issued(ruled(&logic, &mut records, &r, &mut price, &policy));
        assert_eq!((issue.credit_cents, issue.over_under_bps), (0, 0));
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.read_prepaid_units, 1);
        assert!(rec.rules.is_none() && rec.open_until.is_none());
        // 4,500 sats less the read is 3.5 units against the one note of 4 asked:
        // held, with nothing to drop
        let r = received("bb", 6, 4_500, Some(&payload_bytes(&[(4, 3)])));
        assert!(matches!(
            ruled(&logic, &mut records, &r, &mut price, &policy),
            Settlement::PaidUnsettled(_)
        ));
        // 4 units paid, less the first read, the fee and the follow-up's read:
        // 4 - 1 - 0 - 0 - 1 = 2
        assert_eq!(
            records.get("bb").unwrap().rules.as_ref().unwrap().due_units,
            2
        );
        assert_eq!(records.get("bb").unwrap().read_prepaid_units, 1);
        // with the fee at 0 an identical payment is not touched
        let r = received("cc", 6, 5_000, Some(&payload_bytes(&[(4, 5), (1, 6)])));
        issued(ruled(
            &logic,
            &mut records,
            &r,
            &mut price,
            &Policy::default(),
        ));
        assert_eq!(records.get("cc").unwrap().read_prepaid_units, 0);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_payment_of_the_same_payload_is_a_duplicate_with_no_window() {
        let (logic, mut records, dir) = fixture("duplicate");
        let mut price = Price::at(BTC_10K);
        // the first is short and held, with a window and a delivery
        let payload = payload_bytes(&[(64, 1), (1, 2)]);
        let first = received("aa", 6, 30_000, Some(&payload));
        assert!(matches!(
            ruled(&logic, &mut records, &first, &mut price, &Policy::default()),
            Settlement::PaidUnsettled(_)
        ));
        // the same payload paid again, in full: nothing is bought
        let second = received("bb", 6, 65_000, Some(&payload));
        assert_eq!(
            ruled(
                &logic,
                &mut records,
                &second,
                &mut price,
                &Policy::default()
            ),
            Settlement::Rejected(Rejection::DuplicatePayment)
        );
        let dup = records.get("bb").unwrap();
        assert_eq!(dup.status, Status::Rejected);
        assert_eq!(dup.duplicate_of.as_deref(), Some("aa"));
        assert!(dup.delivery.is_none() && dup.rules.is_none() && dup.open_until.is_none());
        assert_eq!(dup.value_sats, 65_000);
        assert!(!dup.issued);
        assert_eq!(total(&logic), Amount::ZERO);
        // it never matches a reconciliation lookup: only the first does
        let did_hex = "07".repeat(33);
        let purchase = records
            .get("aa")
            .unwrap()
            .delivery
            .as_ref()
            .unwrap()
            .purchase
            .clone();
        assert_eq!(
            records.matching(&did_hex, &purchase),
            vec!["aa".to_string()]
        );
        // the first is untouched
        assert_eq!(records.get("aa").unwrap().status, Status::PaidUnsettled);
        // it is counted apart and is never forfeited
        records.forfeit_expired(u64::MAX).unwrap();
        let recon = records.reconciliation();
        assert_eq!(recon.duplicates.payments, 1);
        assert_eq!(recon.duplicates.sats, 65_000);
        assert_eq!(recon.forfeited.payments, 1, "only the first payment");
        assert_eq!(recon.forfeited.sats, 30_000);
        // and a second settle of the duplicate does not look at it again
        assert_eq!(
            ruled(
                &logic,
                &mut records,
                &second,
                &mut price,
                &Policy::default()
            ),
            Settlement::AlreadySettled(Status::Rejected)
        );
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();

        // the first issued: payment aa signs the 4 and drops the 1
        let (logic, mut records, dir) = fixture("claim-twice");
        let mut price = Price::at(BTC_10K);
        let first = received("aa", 6, 4_300, Some(PAYLOAD_HEX));
        issued(run(&logic, &mut records, &first, &mut price));
        assert_eq!(total(&logic), Amount::from_units(4));
        // the same payment again signs nothing more
        assert_eq!(
            run(&logic, &mut records, &first, &mut price),
            Settlement::AlreadySettled(Status::Issued)
        );
        // a second payment carrying the same payload is a duplicate payment,
        // whatever it pays: it signs nothing, has no delivery and no window. (A
        // different payload that reuses a signed blind nonce is the case of
        // `a_reused_blind_nonce_rejects_the_whole_payment_and_is_recorded`.)
        let second = received("bb", 6, 9_000, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &second, &mut price),
            Settlement::Rejected(Rejection::DuplicatePayment)
        );
        assert_eq!(total(&logic), Amount::from_units(4));
        let dup = records.get("bb").unwrap();
        assert_eq!(dup.status, Status::Rejected);
        assert_eq!(dup.duplicate_of.as_deref(), Some("aa"));
        assert!(dup.delivery.is_none());
        assert_eq!(dup.open_until, None);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }
}
