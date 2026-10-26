//! Settling a payment: read its claim key, lock its price, wait for its
//! confirmation tier, and write the owed record.
//!
//! A purchase carries no notes: the OP_RETURN holds only a throwaway claim key,
//! sealed to this mint's DID (`payload`). What the payment is worth is decided by
//! the bitcoin and the price, never by what the payer states:
//!
//! - At `settle.lock_confirmations` (1) the price is locked and stored. From it,
//!   once, come the units the payment is owed (whole units, rounding down; the
//!   sub-unit remainder, under 10 cents, is kept), the prepaid costs that come off
//!   them, and the **confirmation tier** the payment must reach: the locked USD
//!   value picks it (`Terms::confirmations_for`). A payment with no readable
//!   payload, or that buys nothing after the prepaid costs, is kept at the lock,
//!   with no owed record (`Status::Kept`).
//! - At its tier the owed record is made (`Status::Owed`): the claim DID, the next
//!   free index for it, the units owed and the terms of the one claim
//!   (`sp_wire::air_request::delivery::Owed`). `delivery` writes it to the claim
//!   key over AIR, and the wallet claims once (`claim`). The claim window does not
//!   start here: it starts when the record is stored on AIR
//!   (`Records::land`), so a payment made here is not claimable, and cannot
//!   expire, until then.
//!
//! Nothing is signed here. A payment whose confirmations fall below its tier, or
//! that a reorg took out of the chain, stays `Locked` and is looked at again by
//! every scan (`Records::unsettled`); it is owed nothing until it is back at its
//! tier. Once the owed record is made it is not undone, so a reorg deeper than the
//! tier, after it, is the mint's loss (the tiers are the depth it accepts).

use std::error::Error;
use std::fmt;

use sp_wire::ServiceDid;
use sp_wire::air_request::KIND_PSP_CLAIM;
use sp_wire::air_request::delivery::{
    ClaimTerms, EXPIRY_NOTICE, Outcome, Owed, SEND_PURCHASE_CLAIM,
};
use sp_wire::psp::CLAIM_WINDOW_SECS;

use crate::payload::{self, DecodeError};
use crate::records::{Delivery, Record, Records, RecordsError, Status};
use crate::scan::Received;

/// A payment is priced when it is first seen with this many confirmations.
pub const LOCK_CONFIRMATIONS: u32 = 1;
/// The confirmations a payment worth less than the first tier must reach.
pub const BASE_CONFIRMATIONS: u32 = 3;
/// The default tiers: (USD, confirmations). A payment of at least $10,000 waits
/// for 4, at least $50,000 for 5, at least $100,000 for 6.
pub const DEFAULT_TIERS: [(u64, u32); 3] = [(10_000, 4), (50_000, 5), (100_000, 6)];
/// The most confirmations any setting may ask for.
pub const MAX_CONFIRMATIONS: u32 = 1000;
/// `claim.max_attempts` by default: the claims refused for their content that are
/// counted against a payment. The second leaves one retry.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 2;

/// `settle.retention_days` by default: how long a finished payment stays in the
/// records (`Records::prune`). 0 keeps every record.
pub const DEFAULT_RETENTION_DAYS: u64 = 90;
/// The fewest days `settle.retention_days` may be, unless it is 0 (keep every record).
pub const MIN_RETENTION_DAYS: u64 = 30;
/// The most days `settle.retention_days` may be.
pub const MAX_RETENTION_DAYS: u64 = 3650;
/// `settle.prune_horizon_days` by default: twice the default retention plus the
/// 30-day claim window.
pub const DEFAULT_PRUNE_HORIZON_DAYS: u64 = 210;
/// The most days `settle.prune_horizon_days` may be.
pub const MAX_PRUNE_HORIZON_DAYS: u64 = 10_000;

/// The fewest days a `settle.prune_horizon_days` above 0 may be for a
/// `settle.retention_days` above 0: twice the retention plus the claim window
/// (30 days).
pub fn prune_horizon_floor_days(retention_days: u64) -> u64 {
    retention_days.saturating_mul(2).saturating_add(30)
}

/// One confirmation tier: a payment worth at least `min_cents` (USD cents, at the
/// locked price) waits for `confirmations`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    pub min_cents: u64,
    pub confirmations: u32,
}

/// The settlement rules a running mint uses. The defaults are the constants
/// above; the operator sets them in `psp-mint.toml` (see `config`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terms {
    /// Confirmations at which the price is locked.
    pub lock_confirmations: u32,
    /// Confirmations for a payment below the first tier.
    pub base_confirmations: u32,
    /// The tiers, ascending by value, each with at least the confirmations of the
    /// one before (`config` checks it).
    pub tiers: Vec<Tier>,
}

impl Default for Terms {
    fn default() -> Self {
        Terms {
            lock_confirmations: LOCK_CONFIRMATIONS,
            base_confirmations: BASE_CONFIRMATIONS,
            tiers: DEFAULT_TIERS
                .iter()
                .map(|&(usd, confirmations)| Tier {
                    min_cents: usd * 100,
                    confirmations,
                })
                .collect(),
        }
    }
}

impl Terms {
    /// The confirmations a payment worth `value_cents` (USD cents at its locked
    /// price) must reach: that of the highest tier it meets or exceeds, or the
    /// base.
    pub fn confirmations_for(&self, value_cents: u64) -> u32 {
        self.tiers
            .iter()
            .rev()
            .find(|tier| value_cents >= tier.min_cents)
            .map_or(self.base_confirmations, |tier| tier.confirmations)
    }

    /// The most confirmations any payment waits for: what the scan trails the tip
    /// by, so a payment is listed again until it is that deep.
    pub fn highest(&self) -> u32 {
        self.tiers
            .iter()
            .map(|tier| tier.confirmations)
            .fold(self.base_confirmations, u32::max)
    }
}

/// The prepaid costs and the limit of the claim (`delivery.read_fee`,
/// `claim.write_fee`, `claim.max_attempts`), in PSP units unless it says
/// otherwise. Every one is an operator setting, and the costs are 0 by default.
/// They are fixed for a payment when it is locked (the costs) and when its owed
/// record is made (the attempts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimFees {
    /// `claim.max_attempts`: how many refused claims are counted.
    pub max_attempts: u32,
    /// `claim.write_fee`: the cost of the one kind 15 write, prepaid by the SPI
    /// for the wallet and taken from the units owed, once.
    pub write_fee_units: u64,
    /// `delivery.read_fee`: the read cost the PSP prepays for each record a
    /// payer must read, the owed record and the answer to the claim: taken from
    /// the units owed twice.
    pub read_fee_units: u64,
}

impl Default for ClaimFees {
    fn default() -> Self {
        ClaimFees {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            write_fee_units: 0,
            read_fee_units: 0,
        }
    }
}

/// Everything settlement is run under: the [`Terms`] and the [`ClaimFees`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub terms: Terms,
    pub fees: ClaimFees,
}

// A PSP unit is ten US cents: 10 PSP = 1 USD, paid in BTC. With the price in
// USD cents per BTC, sats * price is worth that many (sat-cents):
//   1 cent = 10^8 sat-cents (100,000,000 sats per BTC)
//   1 unit = 10^9 sat-cents (10 cents per PSP unit, 10 units per USD)
const SAT_CENTS_PER_CENT: u128 = 100_000_000;
const SAT_CENTS_PER_UNIT: u128 = 1_000_000_000;

/// Why a payment is kept with nothing owed.
#[derive(Debug, PartialEq)]
pub enum Kept {
    /// The transaction does not have exactly one data output.
    NoDataOutput,
    /// The payload is not a claim key.
    BadPayload(DecodeError),
    /// The payment buys no more than the prepaid costs.
    NothingOwed { value_units: u64, costs: u64 },
}

impl fmt::Display for Kept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kept::NoDataOutput => write!(f, "the transaction has no single data output"),
            Kept::BadPayload(e) => write!(f, "the payload could not be read: {e}"),
            Kept::NothingOwed { value_units, costs } => write!(
                f,
                "the payment buys {value_units} unit(s) at the locked price and the prepaid \
                 costs ({costs}) leave nothing to claim"
            ),
        }
    }
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

/// The owed record that was just made.
#[derive(Debug, PartialEq, Eq)]
pub struct OwedNow {
    /// Who it is owed to: the claim key of the payload.
    pub claim_did: ServiceDid,
    /// Which record of this claim key it is, from 0.
    pub index: u32,
    /// The units the one claim must carry.
    pub due_units: u64,
}

#[derive(Debug, PartialEq)]
pub enum Settlement {
    /// Not in a block yet.
    Waiting {
        confirmations: u32,
    },
    /// Price locked, waiting for the payment's tier.
    Locked {
        confirmations: u32,
        price_cents: u64,
        tier_confirmations: u32,
        due_units: u64,
    },
    AlreadySettled(Status),
    /// Settled earlier and since removed from the records (`Records::prune`):
    /// never settled again, however often the scan lists it.
    Pruned,
    /// Too old to settle: it has no record, and its block time is before the
    /// prune cutoff (`Records::cutoff`), so its marker may have been dropped
    /// already. Nothing is stored.
    BeyondHorizon,
    /// Kept with nothing owed, at the lock.
    Kept(Kept),
    /// No usable BTC/USD price to lock this run. No record is made and the
    /// payment is tried again on the next run.
    PriceUnavailable(String),
    /// The tier was reached and the owed record was made.
    Owed(OwedNow),
}

/// Whole PSP units a payment buys: floor(sats * usd_cents_per_btc / 10^9).
pub fn value_units(value_sats: u64, usd_cents_per_btc: u64) -> u64 {
    let units = u128::from(value_sats) * u128::from(usd_cents_per_btc) / SAT_CENTS_PER_UNIT;
    u64::try_from(units).unwrap_or(u64::MAX)
}

/// What a payment was worth at the locked price, in whole USD cents (rounded down).
pub fn paid_cents(value_sats: u64, price_cents: u64) -> u64 {
    let cents = u128::from(value_sats) * u128::from(price_cents) / SAT_CENTS_PER_CENT;
    u64::try_from(cents).unwrap_or(u64::MAX)
}

/// The sub-unit remainder the mint keeps when a payment's units round down, in
/// whole USD cents (0 to 9).
pub fn remainder_cents(value_sats: u64, price_cents: u64) -> u64 {
    let rest = u128::from(value_sats) * u128::from(price_cents) % SAT_CENTS_PER_UNIT;
    u64::try_from(rest / SAT_CENTS_PER_CENT).unwrap_or(0)
}

/// The record of a payment whose price is locked, with everything the price
/// decides: the claim key, the units, the costs and the tier. A payment that is
/// kept comes back with the reason and the time it was kept (`now`, Unix
/// seconds). The payment's block time is the scan's, or `now` if the node gave
/// none.
fn lock(
    received: &Received,
    price_cents: u64,
    late: bool,
    policy: &Policy,
    now: u64,
) -> (Record, Option<Kept>) {
    let mut record = Record::locked(price_cents, late, received.value_sats);
    record.block_time = received.block_time.unwrap_or(now);
    let keep = |mut record: Record, why: Kept| {
        record.status = Status::Kept;
        record.reason = Some(why.to_string());
        record.kept_at = Some(now);
        (record, Some(why))
    };
    let Some(bytes) = &received.payload else {
        return keep(record, Kept::NoDataOutput);
    };
    let payload = match payload::decode(bytes) {
        Ok(payload) => payload,
        Err(e) => return keep(record, Kept::BadPayload(e)),
    };
    let fees = &policy.fees;
    let units = value_units(received.value_sats, price_cents);
    let reads = fees.read_fee_units.saturating_mul(2);
    let costs = reads.saturating_add(fees.write_fee_units);
    record.claim_did = Some(payload.claim_did.to_hex());
    record.value_units = units;
    record.due_units = units.saturating_sub(costs);
    record.read_prepaid_units = reads;
    record.write_prepaid_units = fees.write_fee_units;
    record.kept_cents = remainder_cents(received.value_sats, price_cents);
    record.tier_confirmations = policy
        .terms
        .confirmations_for(paid_cents(received.value_sats, price_cents));
    if record.due_units == 0 {
        return keep(
            record,
            Kept::NothingOwed {
                value_units: units,
                costs,
            },
        );
    }
    (record, None)
}

/// Advance one payment as far as it can go this run, under the settlement
/// `policy`: the [`Terms`] and the claim fees.
///
/// - At 1 confirmation the price is locked and stored. The first run that sees
///   the payment does this, even if it already has more confirmations (the
///   record is then marked late). The claim key is read, and the units, the
///   prepaid costs and the tier are fixed, here.
/// - At its tier the owed record is made, with the stored price, never a live
///   one.
/// - Every payment that gets a record is settled once, whether or not anything is
///   owed. A settled payment is not looked at again.
///
/// A payment that was pruned (`Records::prune`) is never settled again, however
/// often the scan lists it: it comes back as `Pruned`. A payment with no record
/// whose block time is before the prune cutoff is not settled either
/// (`BeyondHorizon`); one that already has a record is never affected.
///
/// `now` is Unix seconds, and only dates what this call makes: the time a payment
/// is kept (`Record::kept_at`), and the block time if the node gave none. The owed
/// record states the length of the claim window (`window_secs`) and not its end:
/// the window starts when the record is stored on AIR, which is not known here and
/// may be much later.
pub fn settle_ruled(
    records: &mut Records,
    received: &Received,
    price: &mut dyn FnMut() -> Result<u64, String>,
    policy: &Policy,
    now: u64,
) -> Result<Settlement, SettleError> {
    let terms = &policy.terms;
    let txid = received.txid.as_str();
    if records.was_pruned(txid) {
        return Ok(Settlement::Pruned);
    }
    if let Some(existing) = records.get(txid)
        && existing.status != Status::Locked
    {
        return Ok(Settlement::AlreadySettled(existing.status));
    }
    if records.get(txid).is_none() && records.is_beyond_cutoff(received.block_time) {
        return Ok(Settlement::BeyondHorizon);
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
            let (record, kept) = lock(
                received,
                cents,
                received.confirmations > terms.lock_confirmations,
                policy,
                now,
            );
            records.put(txid, record.clone())?;
            if let Some(why) = kept {
                return Ok(Settlement::Kept(why));
            }
            record
        }
    };
    if received.confirmations < record.tier_confirmations {
        return Ok(Settlement::Locked {
            confirmations: received.confirmations,
            price_cents: record.locked_price_cents,
            tier_confirmations: record.tier_confirmations,
            due_units: record.due_units,
        });
    }
    let Some(claim_hex) = record.claim_did.clone() else {
        return Err(SettleError::Mint(format!(
            "record {txid} is locked and has no claim DID"
        )));
    };
    let claim = ServiceDid::parse(&claim_hex)
        .map_err(|e| SettleError::Mint(format!("record {txid}: the claim DID: {e}")))?;
    let index = records.next_owed_index(&claim_hex);
    let owed = Owed {
        txid: txid.to_string(),
        paid_sats: record.value_sats,
        value_units: record.value_units,
        terms: ClaimTerms {
            send: SEND_PURCHASE_CLAIM.to_string(),
            kind: KIND_PSP_CLAIM,
            due_units: record.due_units,
            read_prepaid_units: record.read_prepaid_units,
            write_prepaid_units: record.write_prepaid_units,
            window_secs: CLAIM_WINDOW_SECS,
            max_attempts: policy.fees.max_attempts,
            on_expiry: EXPIRY_NOTICE.to_string(),
        },
    };
    let due_units = record.due_units;
    // One write: the status and the owed record. No window yet: it opens when
    // the record is stored on AIR.
    let mut made = record;
    made.status = Status::Owed;
    made.delivery = Some(Delivery {
        claim_did: claim_hex,
        index,
        outcome: Outcome::Owed(owed),
        sealed: None,
        delivered: false,
        abandoned: None,
        abandoned_at: None,
    });
    records.put(txid, made)?;
    Ok(Settlement::Owed(OwedNow {
        claim_did: claim,
        index,
        due_units,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secp256k1::{PublicKey, SECP256K1, SecretKey};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    /// $100,000 per BTC, in cents. At this price 10 sats are exactly 1 USD cent
    /// and 100 sats are exactly 1 unit, so the sat amounts below read straight
    /// as dollars: 10,000,000 sats is $10,000.00, 9,999,990 is $9,999.99.
    const BTC_100K: u64 = 10_000_000;
    /// The time the payments below are settled at, and the time of their block.
    const NOW: u64 = 1_800_000_000;

    fn claim_key(i: u8) -> PublicKey {
        PublicKey::from_secret_key(SECP256K1, &SecretKey::from_slice(&[i; 32]).unwrap())
    }

    /// The payload of a payment to the claim key `i`: its 33 bytes.
    fn payload_of(i: u8) -> Vec<u8> {
        claim_key(i).serialize().to_vec()
    }

    fn scratch(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "psp-mint-settle-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// An empty record store.
    fn fixture(name: &str) -> Records {
        let dir = scratch(name);
        fs::create_dir_all(&dir).unwrap();
        Records::open(&dir.join(crate::records::FILE)).unwrap()
    }

    fn received(
        txid: &str,
        confirmations: u32,
        value_sats: u64,
        payload: Option<Vec<u8>>,
    ) -> Received {
        Received {
            txid: txid.to_string(),
            confirmations,
            value_sats,
            payload,
            block_time: Some(NOW),
        }
    }

    /// Settle under the default rules at `BTC_100K`.
    fn run(records: &mut Records, r: &Received) -> Settlement {
        run_under(records, r, &Policy::default())
    }

    fn run_under(records: &mut Records, r: &Received, policy: &Policy) -> Settlement {
        settle_ruled(records, r, &mut || Ok(BTC_100K), policy, NOW).unwrap()
    }

    #[test]
    fn the_tier_follows_the_locked_value_at_every_boundary() {
        // (sats, USD cents, tier, units) at $100,000 per BTC: one cent short of a
        // threshold stays in the tier below it, the threshold itself moves up.
        let table: [(u64, u64, u32, u64); 6] = [
            (9_999_990, 999_999, 3, 99_999),
            (10_000_000, 1_000_000, 4, 100_000),
            (49_999_990, 4_999_999, 4, 499_999),
            (50_000_000, 5_000_000, 5, 500_000),
            (99_999_990, 9_999_999, 5, 999_999),
            (100_000_000, 10_000_000, 6, 1_000_000),
        ];
        for (n, (sats, cents, tier, units)) in table.into_iter().enumerate() {
            assert_eq!(paid_cents(sats, BTC_100K), cents);
            let mut records = fixture("tiers");
            let txid = format!("t{n}");
            // Seen at 1 confirmation: the lock fixes the tier and the units.
            assert_eq!(
                run(&mut records, &received(&txid, 1, sats, Some(payload_of(7)))),
                Settlement::Locked {
                    confirmations: 1,
                    price_cents: BTC_100K,
                    tier_confirmations: tier,
                    due_units: units
                }
            );
            // One confirmation short of the tier: still nothing owed.
            let short = received(&txid, tier - 1, sats, Some(payload_of(7)));
            assert!(matches!(
                run(&mut records, &short),
                Settlement::Locked { .. }
            ));
            assert_eq!(records.get(&txid).unwrap().status, Status::Locked);
            assert!(records.get(&txid).unwrap().delivery.is_none());
            // At the tier: owed.
            let at = received(&txid, tier, sats, Some(payload_of(7)));
            let Settlement::Owed(owed) = run(&mut records, &at) else {
                panic!("{sats} sats at {tier} confirmations should be owed");
            };
            assert_eq!(owed.due_units, units);
            assert_eq!(owed.index, 0);
        }
    }

    #[test]
    fn a_configured_tier_table_is_the_one_used() {
        let policy = Policy {
            terms: Terms {
                lock_confirmations: 1,
                base_confirmations: 2,
                tiers: vec![
                    Tier {
                        min_cents: 50_000,
                        confirmations: 7,
                    },
                    Tier {
                        min_cents: 200_000,
                        confirmations: 9,
                    },
                ],
            },
            ..Policy::default()
        };
        assert_eq!(policy.terms.highest(), 9);
        // $499.99, $500.00, $1,999.99, $2,000.00
        for (sats, tier) in [(499_990, 2), (500_000, 7), (1_999_990, 7), (2_000_000, 9)] {
            let mut records = fixture("configured");
            let got = run_under(
                &mut records,
                &received("a", 1, sats, Some(payload_of(7))),
                &policy,
            );
            let Settlement::Locked {
                tier_confirmations, ..
            } = got
            else {
                panic!("{got:?}");
            };
            assert_eq!(tier_confirmations, tier, "{sats} sats");
        }
    }

    #[test]
    fn a_payment_that_leaves_the_chain_is_not_owed_until_it_is_back_at_its_tier() {
        let mut records = fixture("reorg");
        run(
            &mut records,
            &received("a", 1, 1_000_000, Some(payload_of(7))),
        );
        // A reorg takes it out of the chain: it waits, with its price and tier kept.
        assert_eq!(
            run(
                &mut records,
                &received("a", 0, 1_000_000, Some(payload_of(7)))
            ),
            Settlement::Waiting { confirmations: 0 }
        );
        let kept = records.get("a").unwrap();
        assert_eq!(kept.status, Status::Locked);
        assert_eq!(
            (kept.locked_price_cents, kept.tier_confirmations),
            (BTC_100K, 3)
        );
        assert!(kept.delivery.is_none());
        assert_eq!(records.unsettled(), vec!["a".to_string()]);
        // It comes back and reaches the tier.
        let got = run(
            &mut records,
            &received("a", 3, 1_000_000, Some(payload_of(7))),
        );
        assert!(matches!(got, Settlement::Owed(_)), "{got:?}");
        assert!(records.unsettled().is_empty());
    }

    #[test]
    fn units_round_down_and_the_remainder_is_kept_and_audited() {
        let mut records = fixture("subunit");
        // 1,234,567 sats = 123,456.7 cents: 12,345 units, 6 cents left over.
        let got = run(
            &mut records,
            &received("a", 3, 1_234_567, Some(payload_of(7))),
        );
        let Settlement::Owed(owed) = got else {
            panic!("{got:?}")
        };
        assert_eq!(owed.due_units, 12_345);
        assert_eq!(remainder_cents(1_234_567, BTC_100K), 6);
        let record = records.get("a").unwrap();
        assert_eq!((record.value_units, record.kept_cents), (12_345, 6));
        let claims = records.claims();
        assert_eq!(claims.subunit_kept_cents, 6);
        assert_eq!(
            claims.unclaimed,
            crate::records::Tally {
                payments: 1,
                sats: 1_234_567,
                units: 12_345
            }
        );
    }

    #[test]
    fn the_prepaid_costs_come_off_the_units_owed_and_a_payment_they_use_up_is_kept() {
        let mut records = fixture("fees");
        let policy = Policy {
            fees: ClaimFees {
                max_attempts: 2,
                write_fee_units: 5,
                read_fee_units: 3,
            },
            ..Policy::default()
        };
        // 12,345 units less two reads (6) and one write (5).
        let got = run_under(
            &mut records,
            &received("a", 3, 1_234_567, Some(payload_of(7))),
            &policy,
        );
        let Settlement::Owed(owed) = got else {
            panic!("{got:?}")
        };
        assert_eq!(owed.due_units, 12_334);
        let record = records.get("a").unwrap();
        assert_eq!(
            (record.read_prepaid_units, record.write_prepaid_units),
            (6, 5)
        );
        // 10 units buys less than the 11 of costs: floored at 0, so kept.
        let got = run_under(
            &mut records,
            &received("b", 1, 1_000, Some(payload_of(8))),
            &policy,
        );
        assert_eq!(
            got,
            Settlement::Kept(Kept::NothingOwed {
                value_units: 10,
                costs: 11
            })
        );
        let b = records.get("b").unwrap();
        assert_eq!((b.status, b.due_units), (Status::Kept, 0));
        assert!(b.delivery.is_none());
    }

    #[test]
    fn a_payment_with_no_usable_payload_is_kept_with_no_owed_record() {
        let mut records = fixture("kept");
        let off_curve = {
            let mut bytes = vec![0x02];
            bytes.extend_from_slice(&[
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 5,
            ]);
            bytes
        };
        let cases = [
            ("none", None, Kept::NoDataOutput),
            (
                "short",
                Some(vec![2; 32]),
                Kept::BadPayload(DecodeError::WrongLength),
            ),
            (
                "long",
                Some(vec![2; 34]),
                Kept::BadPayload(DecodeError::WrongLength),
            ),
            (
                "off",
                Some(off_curve),
                Kept::BadPayload(DecodeError::NotAKey),
            ),
        ];
        for (txid, payload, why) in cases {
            let got = run(&mut records, &received(txid, 1, 5_000_000, payload));
            assert_eq!(got, Settlement::Kept(why), "{txid}");
            let record = records.get(txid).unwrap();
            assert_eq!(record.status, Status::Kept);
            assert!(record.delivery.is_none() && record.claim_did.is_none());
            assert!(record.reason.is_some());
            // Settled once: a later run does not look at it again.
            let again = run(&mut records, &received(txid, 9, 5_000_000, None));
            assert_eq!(again, Settlement::AlreadySettled(Status::Kept));
        }
        assert_eq!(records.claims().kept.payments, 4);
    }

    /// Fails if the window is stated as an end time (`open_until`), if it starts
    /// when the record is made, or if the owed record carries keysets again.
    #[test]
    fn the_owed_record_says_what_one_claim_must_carry_and_has_no_window_yet() {
        let mut records = fixture("owed");
        let got = run(
            &mut records,
            &received("a", 3, 5_000_000, Some(payload_of(7))),
        );
        // $5,000.00 is 50,000 units
        assert_eq!(
            got,
            Settlement::Owed(OwedNow {
                claim_did: ServiceDid::from_slice(&payload_of(7)).unwrap(),
                index: 0,
                due_units: 50_000,
            })
        );
        let record = records.get("a").unwrap();
        // The window opens when the record is stored on AIR, not here.
        assert_eq!(
            (record.status, record.open_until, record.landed_at),
            (Status::Owed, None, None)
        );
        let delivery = record.delivery.clone().unwrap();
        assert_eq!(
            (
                delivery.index,
                delivery.delivered,
                delivery.sealed.is_some()
            ),
            (0, false, false)
        );
        let Outcome::Owed(owed) = delivery.outcome;
        assert_eq!(
            (owed.txid.as_str(), owed.paid_sats, owed.value_units),
            ("a", 5_000_000, 50_000)
        );
        assert_eq!(owed.terms.due_units, 50_000);
        assert_eq!(owed.terms.window_secs, 2_592_000);
        assert_eq!(owed.terms.kind, 15);
        assert_eq!(owed.terms.max_attempts, 2);
        // On the wire the record has no keysets and no end time.
        let body = serde_json::to_value(&owed).unwrap();
        assert!(body.get("keysets").is_none(), "{body}");
        assert!(body["terms"].get("open_until").is_none(), "{body}");
        // Settled once.
        assert_eq!(
            run(
                &mut records,
                &received("a", 3, 5_000_000, Some(payload_of(7)))
            ),
            Settlement::AlreadySettled(Status::Owed)
        );
    }

    /// Fails if a pruned txid is locked and owed again when the scan lists it,
    /// or if pruning hands out an index whose cell is already written.
    #[test]
    fn a_pruned_payment_is_not_owed_again_and_its_claim_key_gets_the_next_index() {
        let mut records = fixture("pruned");
        for txid in ["a", "b"] {
            assert!(matches!(
                run(
                    &mut records,
                    &received(txid, 3, 5_000_000, Some(payload_of(7)))
                ),
                Settlement::Owed(_)
            ));
        }
        // a (record 0) lands, goes unclaimed and is pruned; b (record 1) stays.
        records.land("a", 1_000).unwrap();
        records.expire_unclaimed(1_000 + 2_592_000).unwrap();
        let pruned = records
            .prune(1_000 + 2_592_000 + 90 * 86_400, 90 * 86_400, 210 * 86_400)
            .unwrap();
        assert_eq!(pruned.removed, 1);
        assert!(records.get("a").is_none());
        // The scan lists a again: no price is asked and nothing is made.
        let mut asked = false;
        let again = settle_ruled(
            &mut records,
            &received("a", 3, 5_000_000, Some(payload_of(7))),
            &mut || {
                asked = true;
                Ok(BTC_100K)
            },
            &Policy::default(),
            NOW,
        )
        .unwrap();
        assert_eq!(again, Settlement::Pruned);
        assert!(!asked && records.get("a").is_none());
        // A new payment to the same claim key is record 2: 0 and 1 were made.
        let next = run(
            &mut records,
            &received("c", 3, 2_000_000, Some(payload_of(7))),
        );
        let Settlement::Owed(c) = next else {
            panic!("{next:?}")
        };
        assert_eq!((c.index, c.due_units), (2, 20_000));
        // Another key's payment starts again at 0.
        let other = run(
            &mut records,
            &received("d", 3, 2_000_000, Some(payload_of(8))),
        );
        assert!(matches!(other, Settlement::Owed(OwedNow { index: 0, .. })));
    }

    #[test]
    fn the_price_is_locked_once_and_a_down_price_makes_no_record() {
        let mut records = fixture("price");
        let r = received("a", 1, 1_000_000, Some(payload_of(7)));
        let mut asked = 0;
        let got = settle_ruled(
            &mut records,
            &r,
            &mut || {
                asked += 1;
                Err("only 1 usable price".into())
            },
            &Policy::default(),
            NOW,
        )
        .unwrap();
        assert_eq!(
            got,
            Settlement::PriceUnavailable("only 1 usable price".into())
        );
        assert_eq!(asked, 1);
        assert!(records.get("a").is_none());
        // Priced at last; later runs, at a very different price, keep the lock.
        settle_ruled(
            &mut records,
            &r,
            &mut || Ok(BTC_100K),
            &Policy::default(),
            NOW,
        )
        .unwrap();
        let later = received("a", 3, 1_000_000, Some(payload_of(7)));
        let got = settle_ruled(
            &mut records,
            &later,
            &mut || Err("asked again".to_string()),
            &Policy::default(),
            NOW,
        )
        .unwrap();
        let Settlement::Owed(owed) = got else {
            panic!("{got:?}")
        };
        assert_eq!(owed.due_units, 10_000);
        // A payment first seen deeper than the lock is marked late.
        let deep = received("b", 5, 1_000_000, Some(payload_of(8)));
        run(&mut records, &deep);
        assert!(records.get("b").unwrap().late);
        assert!(!records.get("a").unwrap().late);
    }

    /// Fails if a payment with no record from before the prune cutoff is locked
    /// and owed (the cutoff is what lets the marker of an old pruned payment be
    /// dropped), if one at the cutoff or with no block time is ignored too, or if
    /// a payment that already has a record is affected.
    #[test]
    fn a_payment_with_no_record_from_before_the_cutoff_is_ignored() {
        const DAY: u64 = 86_400;
        let mut records = fixture("cutoff");
        // x is locked at one confirmation, before there is a cutoff
        let x = received("x", 1, 5_000_000, Some(payload_of(7)));
        assert!(matches!(run(&mut records, &x), Settlement::Locked { .. }));
        // a prune pass 400 days later: the cutoff is 400 - 210 = 190 days after NOW
        let done = records.prune(NOW + 400 * DAY, 90 * DAY, 210 * DAY).unwrap();
        assert_eq!(done.removed, 0);
        assert_eq!(records.cutoff(), NOW + 190 * DAY);
        // y, from NOW, has no record: ignored, the price is not asked
        let mut asked = false;
        let y = received("y", 3, 5_000_000, Some(payload_of(7)));
        let got = settle_ruled(
            &mut records,
            &y,
            &mut || {
                asked = true;
                Ok(BTC_100K)
            },
            &Policy::default(),
            NOW + 400 * DAY,
        )
        .unwrap();
        assert_eq!(got, Settlement::BeyondHorizon);
        assert!(!asked && records.get("y").is_none());
        // z, from the cutoff itself, is not before it: settled
        let z = Received {
            block_time: Some(NOW + 190 * DAY),
            ..received("z", 3, 5_000_000, Some(payload_of(8)))
        };
        assert!(matches!(run(&mut records, &z), Settlement::Owed(_)));
        // w, with no block time, is never beyond the cutoff
        let w = Received {
            block_time: None,
            ..received("w", 3, 5_000_000, Some(payload_of(9)))
        };
        assert!(matches!(run(&mut records, &w), Settlement::Owed(_)));
        // x has a record: its old block does not stop it reaching its tier
        let x = received("x", 3, 5_000_000, Some(payload_of(7)));
        assert!(matches!(run(&mut records, &x), Settlement::Owed(_)));
    }

    /// Fails if a pruned kept payment (it has no delivery) is reported as owed,
    /// or is locked again when the scan lists it.
    #[test]
    fn a_pruned_kept_payment_is_reported_pruned_and_not_settled_again() {
        const DAY: u64 = 86_400;
        let mut records = fixture("pruned-kept");
        let kept = received("k", 1, 5_000_000, Some(vec![2; 34]));
        assert!(matches!(run(&mut records, &kept), Settlement::Kept(_)));
        assert_eq!(records.get("k").unwrap().kept_at, Some(NOW));
        let early = records
            .prune(NOW + 90 * DAY - 1, 90 * DAY, 210 * DAY)
            .unwrap();
        assert_eq!(early.removed, 0);
        let done = records.prune(NOW + 90 * DAY, 90 * DAY, 210 * DAY).unwrap();
        assert_eq!(done.removed, 1);
        let mut asked = false;
        let again = settle_ruled(
            &mut records,
            &received("k", 1, 5_000_000, Some(payload_of(7))),
            &mut || {
                asked = true;
                Ok(BTC_100K)
            },
            &Policy::default(),
            NOW + 90 * DAY,
        )
        .unwrap();
        assert_eq!(again, Settlement::Pruned);
        assert!(!asked && records.get("k").is_none());
    }
}
