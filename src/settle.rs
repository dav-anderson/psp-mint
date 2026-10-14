use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use mint_core::MintLogic;
use mint_types::{Amount, MintOutput, MintOutputError, MintOutputOutcome};

use crate::payload::{self, DecodeError};
use crate::processed::{Processed, ProcessedError};
use crate::scan::Received;

/// PSP units are US dollars: 1 PSP = 1 USD, paid in BTC.
const SATS_PER_BTC: u128 = 100_000_000;
const CENTS_PER_USD: u128 = 100;
pub const MIN_CONFIRMATIONS: u32 = 6;

#[derive(Debug, PartialEq)]
pub enum Rejection {
    BadPayload(DecodeError),
    UnknownDenomination(Amount),
    DuplicateBlindNonce,
    BlindNonceAlreadyUsed,
    Overflow,
    OverRequest { requested: u64, allowed: u64 },
}

#[derive(Debug)]
pub enum SettleError {
    Processed(ProcessedError),
    Mint(String),
}

impl fmt::Display for SettleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettleError::Processed(e) => write!(f, "{e}"),
            SettleError::Mint(e) => write!(f, "mint store: {e}"),
        }
    }
}

impl Error for SettleError {}

impl From<ProcessedError> for SettleError {
    fn from(e: ProcessedError) -> Self {
        SettleError::Processed(e)
    }
}

#[derive(Debug, PartialEq)]
pub enum Settlement {
    Waiting { confirmations: u32 },
    AlreadyProcessed,
    NoPayload,
    Rejected(Rejection),
    /// No usable BTC/USD price this run. The payment is left unprocessed and
    /// is tried again on the next run.
    PriceUnavailable(String),
    Issued(Vec<Result<MintOutputOutcome, MintOutputError>>),
}

/// Whole PSP units a payment buys: floor(sats * usd_cents_per_btc / 10^10).
pub fn allowed_units(value_sats: u64, usd_cents_per_btc: u64) -> u64 {
    let units = u128::from(value_sats) * u128::from(usd_cents_per_btc)
        / (SATS_PER_BTC * CENTS_PER_USD);
    u64::try_from(units).unwrap_or(u64::MAX)
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

pub fn settle(
    logic: &MintLogic,
    processed: &mut Processed,
    received: &Received,
    price: &mut dyn FnMut() -> Result<u64, String>,
) -> Result<Settlement, SettleError> {
    if processed.contains(&received.txid) {
        return Ok(Settlement::AlreadyProcessed);
    }
    if received.confirmations < MIN_CONFIRMATIONS {
        return Ok(Settlement::Waiting {
            confirmations: received.confirmations,
        });
    }
    let Some(bytes) = &received.payload else {
        return Ok(Settlement::NoPayload);
    };
    let payload = match payload::decode(bytes) {
        Ok(payload) => payload,
        Err(e) => return Ok(Settlement::Rejected(Rejection::BadPayload(e))),
    };
    let keys = logic.public_keys();
    let requested = match check(|amount| keys.contains_key(&amount), &payload.outputs) {
        Ok(requested) => requested,
        Err(why) => return Ok(Settlement::Rejected(why)),
    };
    for output in &payload.outputs {
        let used = logic
            .is_blind_nonce_used(&output.blind_nonce)
            .map_err(|e| SettleError::Mint(e.to_string()))?;
        if used {
            return Ok(Settlement::Rejected(Rejection::BlindNonceAlreadyUsed));
        }
    }
    // The price is asked for only now, once the payment is otherwise good, and
    // the caller fetches it at most once per run. It is the price at run time,
    // not at the 6th confirmation.
    let usd_cents_per_btc = match price() {
        Ok(cents) => cents,
        Err(why) => return Ok(Settlement::PriceUnavailable(why)),
    };
    let allowed = allowed_units(received.value_sats, usd_cents_per_btc);
    if requested > allowed {
        return Ok(Settlement::Rejected(Rejection::OverRequest {
            requested,
            allowed,
        }));
    }
    processed.insert(&received.txid)?;
    // PSP uses MintLogic::issue, not issue_with_key. Every PSP note of a given
    // amount is signed by the same key from the mint's fixed key table, and
    // issue looks that key up by amount. issue_with_key is for mints whose key
    // depends on more than the amount, such as the ESP, where it also depends
    // on the note's terms. See the docs on MintLogic::issue in mint-core.
    let results = payload
        .outputs
        .iter()
        .map(|output| logic.issue(output))
        .collect();
    Ok(Settlement::Issued(results))
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
    /// 1,000 sats is exactly 1 unit.
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
    fn request_total_overflow_is_rejected() {
        assert_eq!(
            check(any, &[out(u64::MAX, 1), out(1, 2)]),
            Err(Rejection::Overflow)
        );
    }

    fn fixture(name: &str) -> (MintLogic, Processed, PathBuf) {
        let dir = std::env::temp_dir().join(format!("psp-mint-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let logic = crate::mint::open(&dir, Amount::from_units(1024)).unwrap();
        let processed = Processed::open(&dir.join("processed_txids.json")).unwrap();
        (logic, processed, dir)
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

    fn run(
        logic: &MintLogic,
        processed: &mut Processed,
        r: &Received,
        price: &mut Price,
    ) -> Settlement {
        settle(logic, processed, r, &mut || price.ask()).unwrap()
    }

    #[test]
    fn issues_once_and_records_the_txid() {
        let (logic, mut processed, dir) = fixture("issue");
        let mut price = Price::at(BTC_100K);
        let r = received("aa", 6, 5_000, Some(PAYLOAD_HEX));
        match run(&logic, &mut processed, &r, &mut price) {
            Settlement::Issued(results) => {
                assert_eq!(results.len(), 2);
                assert!(results.iter().all(|x| x.is_ok()));
            }
            other => panic!("{other:?}"),
        }
        assert!(processed.contains("aa"));
        assert_eq!(logic.audit_totals().unwrap().0, Amount::from_units(5));
        assert_eq!(
            run(&logic, &mut processed, &r, &mut price),
            Settlement::AlreadyProcessed
        );
        assert_eq!(logic.audit_totals().unwrap().0, Amount::from_units(5));
        assert_eq!(price.asked, 1);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn waits_for_six_confirmations_without_asking_for_a_price() {
        let (logic, mut processed, dir) = fixture("wait");
        let mut price = Price::at(BTC_100K);
        let r = received("aa", 5, 5_000, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut processed, &r, &mut price),
            Settlement::Waiting { confirmations: 5 }
        );
        assert!(!processed.contains("aa"));
        assert_eq!(logic.audit_totals().unwrap().0, Amount::ZERO);
        assert_eq!(price.asked, 0);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_price_decides_how_much_a_payment_buys() {
        let (logic, mut processed, dir) = fixture("rate");
        // 5,000 sats buys 5 units at $100,000 but only 2 at $40,000.
        let mut low = Price::at(4_000_000);
        let r = received("aa", 6, 5_000, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut processed, &r, &mut low),
            Settlement::Rejected(Rejection::OverRequest {
                requested: 5,
                allowed: 2
            })
        );
        assert!(!processed.contains("aa"));
        let mut high = Price::at(BTC_100K);
        assert!(matches!(
            run(&logic, &mut processed, &r, &mut high),
            Settlement::Issued(_)
        ));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejected_payments_are_not_recorded() {
        let (logic, mut processed, dir) = fixture("reject");
        let mut price = Price::at(BTC_100K);
        // 4,999 sats buys 4 units, one short of the 5 requested.
        let over = received("aa", 6, 4_999, Some(PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut processed, &over, &mut price),
            Settlement::Rejected(Rejection::OverRequest {
                requested: 5,
                allowed: 4
            })
        );
        let none = received("bb", 6, 5_000, None);
        assert_eq!(
            run(&logic, &mut processed, &none, &mut price),
            Settlement::NoPayload
        );
        let garbage = received("cc", 6, 5_000, Some("00"));
        assert!(matches!(
            run(&logic, &mut processed, &garbage, &mut price),
            Settlement::Rejected(Rejection::BadPayload(_))
        ));
        assert!(!processed.contains("aa"));
        assert!(!processed.contains("bb"));
        assert!(!processed.contains("cc"));
        assert_eq!(logic.audit_totals().unwrap().0, Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reply_that_is_not_a_did_is_rejected_without_asking_for_a_price() {
        let (logic, mut processed, dir) = fixture("didlen");
        let mut price = Price::at(BTC_100K);
        let r = received("aa", 6, 5_000, Some(OLD_REPLY_PAYLOAD_HEX));
        assert_eq!(
            run(&logic, &mut processed, &r, &mut price),
            Settlement::Rejected(Rejection::BadPayload(DecodeError::WrongReplyLength))
        );
        assert!(!processed.contains("aa"));
        assert_eq!(price.asked, 0);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_usable_price_leaves_the_payment_for_the_next_run() {
        let (logic, mut processed, dir) = fixture("noprice");
        let r = received("aa", 6, 5_000, Some(PAYLOAD_HEX));
        let mut down = Price::down();
        assert_eq!(
            run(&logic, &mut processed, &r, &mut down),
            Settlement::PriceUnavailable("only 1 usable price".to_string())
        );
        assert!(!processed.contains("aa"));
        assert_eq!(logic.audit_totals().unwrap().0, Amount::ZERO);
        // next run, the price is back
        let mut up = Price::at(BTC_100K);
        assert!(matches!(
            run(&logic, &mut processed, &r, &mut up),
            Settlement::Issued(_)
        ));
        assert!(processed.contains("aa"));
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
    fn a_reused_blind_nonce_rejects_the_whole_payment_before_anything_is_issued() {
        let (logic, mut processed, dir) = fixture("reuse");
        let mut price = Price::at(BTC_100K);
        let first = received("aa", 6, 5_000, Some(&payload_bytes(&[(4, 1), (1, 2)])));
        assert!(matches!(
            run(&logic, &mut processed, &first, &mut price),
            Settlement::Issued(_)
        ));
        let asked = price.asked;
        let second = received("bb", 6, 5_000, Some(&payload_bytes(&[(2, 3), (1, 2)])));
        assert_eq!(
            run(&logic, &mut processed, &second, &mut price),
            Settlement::Rejected(Rejection::BlindNonceAlreadyUsed)
        );
        assert_eq!(price.asked, asked);
        assert!(!processed.contains("bb"));
        assert!(!logic.is_blind_nonce_used(&point(3)).unwrap());
        assert_eq!(logic.audit_totals().unwrap().0, Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }
}
