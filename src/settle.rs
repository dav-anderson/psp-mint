use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use mint_core::MintLogic;
use mint_types::{Amount, MintOutput, MintOutputError, MintOutputOutcome};

use crate::payload::{self, DecodeError};
use crate::records::{Record, Records, RecordsError, Status};
use crate::scan::Received;

/// A payment is priced when it is first seen with this many confirmations.
pub const LOCK_CONFIRMATIONS: u32 = 1;
/// A payment is settled once it has this many confirmations.
pub const MIN_CONFIRMATIONS: u32 = 6;
/// Up to this much short (1%, in basis points) and every output is still signed.
pub const UNDERPAY_TOLERANCE_BPS: u128 = 100;
/// This much over (10%, in basis points) or more and the excess is credited.
pub const CREDIT_THRESHOLD_BPS: u128 = 1000;

// PSP units are US dollars: 1 PSP = 1 USD, paid in BTC. With the price in USD
// cents per BTC, sats * price is worth that many (sat-cents):
//   1 cent = 10^8 sat-cents (100,000,000 sats per BTC)
//   1 unit = 10^10 sat-cents (100 cents per USD)
const SAT_CENTS_PER_CENT: u128 = 100_000_000;
const SAT_CENTS_PER_UNIT: u128 = 10_000_000_000;

#[derive(Debug, PartialEq)]
pub enum Rejection {
    /// The transaction does not have exactly one data output.
    NoDataOutput,
    BadPayload(DecodeError),
    UnknownDenomination(Amount),
    DuplicateBlindNonce,
    BlindNonceAlreadyUsed,
    Overflow,
    /// Short by more than 1% and not even the smallest output fits what was paid.
    TooSmall { requested: u64, paid: u64 },
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
    /// One result per signed output, in payload order.
    pub results: Vec<Result<MintOutputOutcome, MintOutputError>>,
    /// Requested units that were not signed because of a large underpayment.
    pub skipped_units: u64,
    /// Overpayment of 10% or more, in USD cents, owed back to the payer.
    pub credit_cents: u64,
    pub over_under_bps: i64,
}

#[derive(Debug, PartialEq)]
pub enum Settlement {
    /// Not in a block yet.
    Waiting { confirmations: u32 },
    /// Price locked, waiting for 6 confirmations.
    Locked { confirmations: u32, price_cents: u64 },
    AlreadySettled(Status),
    Rejected(Rejection),
    /// No usable BTC/USD price to lock this run. No record is made and the
    /// payment is tried again on the next run.
    PriceUnavailable(String),
    Issued(Issue),
}

/// Whole PSP units a payment buys: floor(sats * usd_cents_per_btc / 10^10).
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
    /// True if every output is signed: paid in full, over, or short by no more
    /// than 1%.
    pub sign_all: bool,
    /// Overpayment of 10% or more, in whole USD cents (rounded down). Else 0.
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
    let paid = u128::from(value_sats) * u128::from(usd_cents_per_btc);
    let required = u128::from(requested_units) * SAT_CENTS_PER_UNIT;
    let (sign_all, credit_cents) = if paid >= required {
        let excess = paid - required;
        let credited = excess.saturating_mul(10_000) >= required.saturating_mul(CREDIT_THRESHOLD_BPS);
        let credit = if credited {
            u64::try_from(excess / SAT_CENTS_PER_CENT).unwrap_or(u64::MAX)
        } else {
            0
        };
        (true, credit)
    } else {
        let short = required - paid;
        (short * 10_000 <= required * UNDERPAY_TOLERANCE_BPS, 0)
    };
    Assessment {
        over_under_bps: basis_points(paid, required),
        paid_units: allowed_units(value_sats, usd_cents_per_btc),
        sign_all,
        credit_cents,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Indexes into the outputs to sign, ascending.
    pub sign: Vec<usize>,
    pub skipped_units: u64,
}

/// Which outputs to sign. All of them unless the payment is short by more than
/// 1%. Then take outputs largest first, each one only if the running total
/// still fits in the whole units paid, and skip the rest.
pub fn plan(outputs: &[MintOutput], assessment: &Assessment) -> Plan {
    if assessment.sign_all {
        return Plan {
            sign: (0..outputs.len()).collect(),
            skipped_units: 0,
        };
    }
    let mut order: Vec<usize> = (0..outputs.len()).collect();
    order.sort_by(|&a, &b| outputs[b].amount.units.cmp(&outputs[a].amount.units));
    let mut total: u64 = 0;
    let mut sign = Vec::new();
    for i in order {
        if let Some(next) = total.checked_add(outputs[i].amount.units) {
            if next <= assessment.paid_units {
                total = next;
                sign.push(i);
            }
        }
    }
    sign.sort_unstable();
    // Summed in u128 so the skipped amount is exact even for a request that
    // `check` would already have rejected as overflowing.
    let requested: u128 = outputs.iter().map(|o| u128::from(o.amount.units)).sum();
    Plan {
        sign,
        skipped_units: u64::try_from(requested - u128::from(total)).unwrap_or(u64::MAX),
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
    let txid = received.txid.as_str();
    if let Some(existing) = records.get(txid) {
        if existing.status != Status::Locked {
            return Ok(Settlement::AlreadySettled(existing.status));
        }
    }
    if received.confirmations < LOCK_CONFIRMATIONS {
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
            let record = Record::locked(cents, received.confirmations > LOCK_CONFIRMATIONS);
            records.put(txid, record.clone())?;
            record
        }
    };
    let Some(price_cents) = record.locked_price_cents else {
        return Err(SettleError::Mint(format!("record {txid} has no locked price")));
    };
    if received.confirmations < MIN_CONFIRMATIONS {
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
    let keys = logic.public_keys();
    let requested = match check(|amount| keys.contains_key(&amount), &payload.outputs) {
        Ok(requested) => requested,
        Err(why) => return reject(records, txid, record, why, None, 0),
    };
    let assessment = assess(received.value_sats, price_cents, requested);
    for output in &payload.outputs {
        let used = logic
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
    let plan = plan(&payload.outputs, &assessment);
    if plan.sign.is_empty() {
        return reject(
            records,
            txid,
            record,
            Rejection::TooSmall {
                requested,
                paid: assessment.paid_units,
            },
            Some(assessment.over_under_bps),
            plan.skipped_units,
        );
    }

    // Record first, then issue, so a payment is issued at most once.
    let mut settled = record;
    settled.status = Status::Issued;
    settled.issued = true;
    settled.over_under_bps = Some(assessment.over_under_bps);
    settled.skipped_units = plan.skipped_units;
    settled.credit_cents = assessment.credit_cents;
    records.put(txid, settled)?;

    // PSP uses MintLogic::issue, not issue_with_key. Every PSP note of a given
    // amount is signed by the same key from the mint's fixed key table, and
    // issue looks that key up by amount. issue_with_key is for mints whose key
    // depends on more than the amount, such as the ESP, where it also depends
    // on the note's terms. See the docs on MintLogic::issue in mint-core.
    let results = plan
        .sign
        .iter()
        .map(|&i| logic.issue(&payload.outputs[i]))
        .collect();
    Ok(Settlement::Issued(Issue {
        results,
        skipped_units: plan.skipped_units,
        credit_cents: assessment.credit_cents,
        over_under_bps: assessment.over_under_bps,
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

    /// $100,000 per BTC, in cents. Then 100,000 sats is exactly 100 units and
    /// 5,000 sats is exactly 5 units (the 4 + 1 payload above).
    const BTC_100K: u64 = 10_000_000;

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
    fn allowed_units_is_one_usd_per_unit() {
        assert_eq!(allowed_units(100_000, BTC_100K), 100);
        assert_eq!(allowed_units(5_000, BTC_100K), 5);
        assert_eq!(allowed_units(100_000_000, BTC_100K), 100_000);
        assert_eq!(allowed_units(100_000_000, 8_167_355), 81_673);
    }

    #[test]
    fn allowed_units_rounds_down_to_whole_units() {
        assert_eq!(allowed_units(4_999, BTC_100K), 4);
        assert_eq!(allowed_units(999, BTC_100K), 0);
        assert_eq!(allowed_units(0, BTC_100K), 0);
        assert_eq!(allowed_units(5_000, 0), 0);
    }

    #[test]
    fn allowed_units_does_not_overflow() {
        assert_eq!(allowed_units(u64::MAX, u64::MAX), u64::MAX);
        // all 21 million BTC at $1,000,000,000 per BTC
        assert_eq!(
            allowed_units(2_100_000_000_000_000, 100_000_000_000),
            21_000_000_000_000_000
        );
    }

    #[test]
    fn over_under_is_reported_in_basis_points_of_the_request() {
        // 100 units requested, which is exactly 100,000 sats at $100,000
        assert_eq!(assess(100_000, BTC_100K, 100).over_under_bps, 0);
        assert_eq!(assess(112_500, BTC_100K, 100).over_under_bps, 1250);
        assert_eq!(assess(99_200, BTC_100K, 100).over_under_bps, -80);
        assert_eq!(assess(0, BTC_100K, 100).over_under_bps, -10_000);
    }

    #[test]
    fn over_under_truncates_toward_zero() {
        // 5 units requested, 5,499 sats is 9.98% over (998.0 bps, exact here)
        assert_eq!(assess(5_499, BTC_100K, 5).over_under_bps, 998);
        // 100 units, 100,001 sats is +0.001% = 0.1 bps, truncated to 0
        assert_eq!(assess(100_001, BTC_100K, 100).over_under_bps, 0);
        assert_eq!(assess(99_999, BTC_100K, 100).over_under_bps, 0);
    }

    #[test]
    fn short_by_up_to_one_percent_signs_everything() {
        assert!(assess(99_000, BTC_100K, 100).sign_all); // exactly 1% short
        assert!(!assess(98_999, BTC_100K, 100).sign_all);
        assert!(assess(100_000, BTC_100K, 100).sign_all);
        assert!(assess(150_000, BTC_100K, 100).sign_all);
    }

    #[test]
    fn overpayment_of_ten_percent_or_more_is_credited() {
        let ten = assess(110_000, BTC_100K, 100);
        assert_eq!(ten.credit_cents, 1000); // $10.00
        assert!(ten.sign_all);
        assert_eq!(assess(109_999, BTC_100K, 100).credit_cents, 0);
        assert_eq!(assess(112_500, BTC_100K, 100).credit_cents, 1250);
        assert_eq!(assess(100_000, BTC_100K, 100).credit_cents, 0);
        assert_eq!(assess(99_000, BTC_100K, 100).credit_cents, 0);
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let a = assess(u64::MAX, u64::MAX, 1);
        assert!(a.sign_all);
        assert_eq!(a.over_under_bps, i64::MAX);
        let b = assess(0, 0, u64::MAX);
        assert!(!b.sign_all);
        assert_eq!(b.over_under_bps, -10_000);
    }

    fn short(paid_units: u64) -> Assessment {
        Assessment {
            over_under_bps: -5_000,
            paid_units,
            sign_all: false,
            credit_cents: 0,
        }
    }

    #[test]
    fn plan_signs_everything_when_the_payment_is_enough() {
        let outputs = [out(4, 1), out(1, 2)];
        let a = assess(5_000, BTC_100K, 5);
        assert_eq!(
            plan(&outputs, &a),
            Plan {
                sign: vec![0, 1],
                skipped_units: 0
            }
        );
    }

    #[test]
    fn plan_takes_the_largest_outputs_that_fit() {
        let outputs = [out(1, 1), out(4, 2), out(2, 3)];
        // paid 5: take 4, 2 does not fit (6), 1 fits (5)
        assert_eq!(
            plan(&outputs, &short(5)),
            Plan {
                sign: vec![0, 1],
                skipped_units: 2
            }
        );
        // paid 4: take 4, nothing else fits
        assert_eq!(
            plan(&outputs, &short(4)),
            Plan {
                sign: vec![1],
                skipped_units: 3
            }
        );
        // paid 3: 4 does not fit, take 2, then 1
        assert_eq!(
            plan(&outputs, &short(3)),
            Plan {
                sign: vec![0, 2],
                skipped_units: 4
            }
        );
        // paid 0: nothing
        assert_eq!(
            plan(&outputs, &short(0)),
            Plan {
                sign: vec![],
                skipped_units: 7
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
        assert_eq!(check(any, &[out(half, 1), out(half, 2), out(1, 3)]), Ok(u64::MAX));
        assert_eq!(check(any, &[out(u64::MAX, 1)]), Ok(u64::MAX));
        assert_eq!(
            check(any, &[out(half, 1), out(half, 2), out(2, 3)]),
            Err(Rejection::Overflow)
        );
    }

    #[test]
    fn plan_does_not_wrap_at_the_u64_limit() {
        // two notes that together exceed u64: only the first can fit
        let outputs = [out(u64::MAX, 1), out(1, 2)];
        assert_eq!(
            plan(&outputs, &short(u64::MAX)),
            Plan {
                sign: vec![0],
                skipped_units: 1
            }
        );
        // everything fits exactly at the limit
        let half = u64::MAX / 2;
        let outputs = [out(half, 1), out(half + 1, 2)];
        assert_eq!(
            plan(&outputs, &short(u64::MAX)),
            Plan {
                sign: vec![0, 1],
                skipped_units: 0
            }
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
        let dir = std::env::temp_dir().join(format!("psp-mint-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let logic = crate::mint::open(&dir, Amount::from_units(1024)).unwrap();
        let records = Records::open(&dir.join("settlements.json")).unwrap();
        (logic, records, dir)
    }

    fn received(txid: &str, confirmations: u32, value_sats: u64, payload: Option<&str>) -> Received {
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

    fn run(logic: &MintLogic, records: &mut Records, r: &Received, price: &mut Price) -> Settlement {
        settle(logic, records, r, &mut || price.ask()).unwrap()
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
        let mut price = Price::at(BTC_100K);
        let r = received("aa", 1, 5_000, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::Locked {
                confirmations: 1,
                price_cents: BTC_100K
            }
        );
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.locked_price_cents, Some(BTC_100K));
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
                price_cents: BTC_100K
            }
        );
        assert_eq!(other.asked, 0);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settlement_at_six_uses_the_locked_price_never_a_live_one() {
        let (logic, mut records, dir) = fixture("stored");
        let mut at_lock = Price::at(BTC_100K);
        run(&logic, &mut records, &received("aa", 1, 5_000, Some(PAYLOAD_HEX)), &mut at_lock);
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
        assert_eq!(rec.locked_price_cents, Some(BTC_100K));
        assert_eq!(rec.over_under_bps, Some(0));
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_payment_first_seen_at_six_is_locked_now_and_marked_late() {
        let (logic, mut records, dir) = fixture("late");
        let mut price = Price::at(BTC_100K);
        let r = received("aa", 6, 5_000, Some(PAYLOAD_HEX));
        assert!(matches!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::Issued(_)
        ));
        let rec = records.get("aa").unwrap();
        assert!(rec.late);
        assert_eq!(rec.locked_price_cents, Some(BTC_100K));
        assert_eq!(price.asked, 1);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_payment_not_yet_in_a_block_gets_no_record() {
        let (logic, mut records, dir) = fixture("zero");
        let mut price = Price::at(BTC_100K);
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
        let mut up = Price::at(BTC_100K);
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
        let mut price = Price::at(BTC_100K);
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
        let mut price = Price::at(BTC_100K);
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

    #[test]
    fn short_by_exactly_one_percent_still_signs_everything() {
        let (logic, mut records, dir) = fixture("onepct");
        let mut price = Price::at(BTC_100K);
        // 4,950 sats is 1% short of the 5,000 needed for 5 units
        let r = received("aa", 6, 4_950, Some(PAYLOAD_HEX));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 2);
        assert_eq!(issue.skipped_units, 0);
        assert_eq!(issue.over_under_bps, -100);
        assert_eq!(records.get("aa").unwrap().over_under_bps, Some(-100));
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn short_by_more_than_one_percent_signs_only_what_fits() {
        let (logic, mut records, dir) = fixture("short");
        let mut price = Price::at(BTC_100K);
        // 4,900 sats is 2% short and buys 4 whole units: sign the 4, skip the 1
        let r = received("aa", 6, 4_900, Some(PAYLOAD_HEX));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 1);
        assert_eq!(issue.skipped_units, 1);
        assert_eq!(issue.over_under_bps, -200);
        let rec = records.get("aa").unwrap();
        assert!(rec.issued);
        assert_eq!(rec.skipped_units, 1);
        assert_eq!(rec.status, Status::Issued);
        assert_eq!(total(&logic), Amount::from_units(4));
        // the skipped 1-unit output was not used up
        let decoded = payload::decode(&hex::decode(PAYLOAD_HEX).unwrap()).unwrap();
        assert!(!logic.is_blind_nonce_used(&decoded.outputs[1].blind_nonce).unwrap());
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_payment_too_small_for_any_output_is_recorded_and_signs_nothing() {
        let (logic, mut records, dir) = fixture("tiny");
        let mut price = Price::at(BTC_100K);
        // 900 sats buys 0 whole units
        let r = received("aa", 6, 900, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut records, &r, &mut price),
            Settlement::Rejected(Rejection::TooSmall {
                requested: 5,
                paid: 0
            })
        );
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.status, Status::Rejected);
        assert!(!rec.issued);
        assert_eq!(rec.over_under_bps, Some(-8200));
        assert_eq!(rec.skipped_units, 5);
        assert_eq!(total(&logic), Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overpayment_of_ten_percent_is_issued_in_full_and_credited() {
        let (logic, mut records, dir) = fixture("credit");
        let mut price = Price::at(BTC_100K);
        // 5,500 sats is exactly 10% over: $5.50 paid for $5, so 50 cents credit
        let r = received("aa", 6, 5_500, Some(PAYLOAD_HEX));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.results.len(), 2);
        assert_eq!(issue.credit_cents, 50);
        assert_eq!(issue.over_under_bps, 1000);
        let rec = records.get("aa").unwrap();
        assert_eq!(rec.credit_cents, 50);
        assert_eq!(rec.over_under_bps, Some(1000));
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overpayment_below_ten_percent_is_kept_by_the_mint() {
        let (logic, mut records, dir) = fixture("keep");
        let mut price = Price::at(BTC_100K);
        let r = received("aa", 6, 5_499, Some(PAYLOAD_HEX));
        let issue = issued(run(&logic, &mut records, &r, &mut price));
        assert_eq!(issue.credit_cents, 0);
        assert_eq!(issue.over_under_bps, 998);
        assert_eq!(records.get("aa").unwrap().credit_cents, 0);
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
        let mut price = Price::at(BTC_100K);
        let first = received("aa", 6, 5_000, Some(&payload_bytes(&[(4, 1), (1, 2)])));
        issued(run(&logic, &mut records, &first, &mut price));
        let second = received("bb", 6, 5_000, Some(&payload_bytes(&[(2, 3), (1, 2)])));
        assert_eq!(
            run(&logic, &mut records, &second, &mut price),
            Settlement::Rejected(Rejection::BlindNonceAlreadyUsed)
        );
        let rec = records.get("bb").unwrap();
        assert_eq!(rec.status, Status::Rejected);
        // 3 units requested ($3), $5 paid: (5 - 3) / 3 = +6666.67 bps, truncated
        assert_eq!(rec.over_under_bps, Some(6666));
        assert!(!logic.is_blind_nonce_used(&point(3)).unwrap());
        assert_eq!(total(&logic), Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }
}
