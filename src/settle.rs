use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use mint_core::MintLogic;
use mint_types::{Amount, MintOutput, MintOutputError, MintOutputOutcome};

use crate::payload::{self, DecodeError};
use crate::processed::{Processed, ProcessedError};
use crate::scan::Received;

pub const UNITS_PER_SAT: u64 = 100;
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
    Issued(Vec<Result<MintOutputOutcome, MintOutputError>>),
}

pub fn check(
    is_denomination: impl Fn(Amount) -> bool,
    value_sats: u64,
    outputs: &[MintOutput],
) -> Option<Rejection> {
    let mut seen = HashSet::new();
    let mut requested: u64 = 0;
    for output in outputs {
        if !is_denomination(output.amount) {
            return Some(Rejection::UnknownDenomination(output.amount));
        }
        if !seen.insert(&output.blind_nonce) {
            return Some(Rejection::DuplicateBlindNonce);
        }
        requested = match requested.checked_add(output.amount.units) {
            Some(total) => total,
            None => return Some(Rejection::Overflow),
        };
    }
    let allowed = value_sats.saturating_mul(UNITS_PER_SAT);
    if requested > allowed {
        return Some(Rejection::OverRequest { requested, allowed });
    }
    None
}

pub fn settle(
    logic: &MintLogic,
    processed: &mut Processed,
    received: &Received,
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
    if let Some(why) = check(
        |amount| keys.contains_key(&amount),
        received.value_sats,
        &payload.outputs,
    ) {
        return Ok(Settlement::Rejected(why));
    }
    for output in &payload.outputs {
        let used = logic
            .is_blind_nonce_used(&output.blind_nonce)
            .map_err(|e| SettleError::Mint(e.to_string()))?;
        if used {
            return Ok(Settlement::Rejected(Rejection::BlindNonceAlreadyUsed));
        }
    }
    processed.insert(&received.txid)?;
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

    const PAYLOAD_HEX: &str = "0a7265706c792d7465737402000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";

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
    fn within_allowed_passes() {
        assert_eq!(check(any, 1, &[out(4, 1), out(1, 2)]), None);
    }

    #[test]
    fn exactly_the_allowed_amount_passes() {
        assert_eq!(check(any, 1, &[out(64, 1), out(32, 2), out(4, 3)]), None);
    }

    #[test]
    fn one_unit_over_is_rejected() {
        assert_eq!(
            check(any, 1, &[out(64, 1), out(32, 2), out(4, 3), out(1, 4)]),
            Some(Rejection::OverRequest {
                requested: 101,
                allowed: 100
            })
        );
    }

    #[test]
    fn nothing_paid_allows_nothing() {
        assert_eq!(
            check(any, 0, &[out(1, 1)]),
            Some(Rejection::OverRequest {
                requested: 1,
                allowed: 0
            })
        );
    }

    #[test]
    fn unknown_denomination_is_rejected() {
        assert_eq!(
            check(|a| a.units != 3, 10, &[out(4, 1), out(3, 2)]),
            Some(Rejection::UnknownDenomination(Amount::from_units(3)))
        );
    }

    #[test]
    fn duplicate_blind_nonce_is_rejected() {
        assert_eq!(
            check(any, 10, &[out(4, 1), out(1, 1)]),
            Some(Rejection::DuplicateBlindNonce)
        );
    }

    #[test]
    fn request_total_overflow_is_rejected() {
        assert_eq!(
            check(any, u64::MAX, &[out(u64::MAX, 1), out(1, 2)]),
            Some(Rejection::Overflow)
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

    #[test]
    fn issues_once_and_records_the_txid() {
        let (logic, mut processed, dir) = fixture("issue");
        let r = received("aa", 6, 1, Some(PAYLOAD_HEX));
        match settle(&logic, &mut processed, &r).unwrap() {
            Settlement::Issued(results) => {
                assert_eq!(results.len(), 2);
                assert!(results.iter().all(|x| x.is_ok()));
            }
            other => panic!("{other:?}"),
        }
        assert!(processed.contains("aa"));
        assert_eq!(logic.audit_totals().unwrap().0, Amount::from_units(5));
        assert_eq!(
            settle(&logic, &mut processed, &r).unwrap(),
            Settlement::AlreadyProcessed
        );
        assert_eq!(logic.audit_totals().unwrap().0, Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn waits_for_six_confirmations() {
        let (logic, mut processed, dir) = fixture("wait");
        let r = received("aa", 5, 1, Some(PAYLOAD_HEX));
        assert_eq!(
            settle(&logic, &mut processed, &r).unwrap(),
            Settlement::Waiting { confirmations: 5 }
        );
        assert!(!processed.contains("aa"));
        assert_eq!(logic.audit_totals().unwrap().0, Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejected_payments_are_not_recorded() {
        let (logic, mut processed, dir) = fixture("reject");
        let over = received("aa", 6, 0, Some(PAYLOAD_HEX));
        assert_eq!(
            settle(&logic, &mut processed, &over).unwrap(),
            Settlement::Rejected(Rejection::OverRequest {
                requested: 5,
                allowed: 0
            })
        );
        let none = received("bb", 6, 1, None);
        assert_eq!(
            settle(&logic, &mut processed, &none).unwrap(),
            Settlement::NoPayload
        );
        let garbage = received("cc", 6, 1, Some("00"));
        assert!(matches!(
            settle(&logic, &mut processed, &garbage).unwrap(),
            Settlement::Rejected(Rejection::BadPayload(_))
        ));
        assert!(!processed.contains("aa"));
        assert!(!processed.contains("bb"));
        assert!(!processed.contains("cc"));
        assert_eq!(logic.audit_totals().unwrap().0, Amount::ZERO);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn payload_bytes(outputs: &[(u64, u8)]) -> String {
        let mut bytes = vec![0u8, outputs.len() as u8];
        for &(units, i) in outputs {
            bytes.extend_from_slice(&units.to_be_bytes());
            bytes.extend_from_slice(&point(i).0.serialize());
        }
        hex::encode(bytes)
    }

    #[test]
    fn a_reused_blind_nonce_rejects_the_whole_payment_before_anything_is_issued() {
        let (logic, mut processed, dir) = fixture("reuse");
        let first = received("aa", 6, 1, Some(&payload_bytes(&[(4, 1), (1, 2)])));
        assert!(matches!(
            settle(&logic, &mut processed, &first).unwrap(),
            Settlement::Issued(_)
        ));
        let second = received("bb", 6, 1, Some(&payload_bytes(&[(2, 3), (1, 2)])));
        assert_eq!(
            settle(&logic, &mut processed, &second).unwrap(),
            Settlement::Rejected(Rejection::BlindNonceAlreadyUsed)
        );
        assert!(!processed.contains("bb"));
        assert!(!logic.is_blind_nonce_used(&point(3)).unwrap());
        assert_eq!(logic.audit_totals().unwrap().0, Amount::from_units(5));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }
}
