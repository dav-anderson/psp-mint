//! One scan cycle, shared by the one-shot run and `serve`.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use mint_core::MintLogic;

use crate::price::{self, Aggregate, Config};
use crate::records::Records;
use crate::scan::Received;
use crate::settle::{self, Rejection, SettleError, Settlement, Terms};

pub fn cents_text(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// Fetch the three tickers and combine them. Returns whole cents per BTC, or
/// why no price could be taken.
pub fn btc_usd_cents(config: Config) -> Result<u64, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock: {e}"))?
        .as_secs();
    let lookup = price::combine(&price::fetch_all_with(config.timeout_secs), now, config);
    for (source, why) in &lookup.excluded {
        println!("  price: excluded {source}: {why}");
    }
    match lookup.aggregate {
        Aggregate::Price { cents, sources } => {
            println!(
                "  price: {} USD/BTC from {sources} sources",
                cents_text(cents)
            );
            Ok(cents)
        }
        Aggregate::TooFewSources { usable } => Err(format!(
            "only {usable} usable price source(s), need at least 2"
        )),
        Aggregate::Disagree { low, high } => Err(format!(
            "two usable prices disagree: {} and {} USD/BTC",
            cents_text(low),
            cents_text(high)
        )),
    }
}

/// Move every payment as far as it can go.
///
/// The mint is shared with the HTTP API behind `logic`, so it is locked once
/// per payment and never while waiting on the network. The price is fetched
/// before the lock is taken, and only for a payment that has no record yet,
/// which is the one case where `settle` asks for it. `price` is called at most
/// once per payment, and the caller makes it fetch at most once per cycle.
pub fn run_once(
    logic: &Mutex<MintLogic>,
    records: &mut Records,
    found: &[Received],
    price: &mut dyn FnMut() -> Result<u64, String>,
    report: &mut dyn FnMut(&Received, &Settlement),
) -> Result<(), SettleError> {
    run_once_with(logic, records, found, price, report, &Terms::default())
}

/// `run_once` under the given settlement rules.
pub fn run_once_with(
    logic: &Mutex<MintLogic>,
    records: &mut Records,
    found: &[Received],
    price: &mut dyn FnMut() -> Result<u64, String>,
    report: &mut dyn FnMut(&Received, &Settlement),
    terms: &Terms,
) -> Result<(), SettleError> {
    run_once_rotating(logic, None, records, found, price, report, terms)
}

/// `run_once_with` for a mint that rotates. A payment whose payload names the
/// previous key set is signed by it (see `settle::settle_with_previous`). The
/// mint is locked first and the rotor second, as everywhere else.
pub fn run_once_rotating(
    logic: &Mutex<MintLogic>,
    rotating: Option<&crate::server::Rotating>,
    records: &mut Records,
    found: &[Received],
    price: &mut dyn FnMut() -> Result<u64, String>,
    report: &mut dyn FnMut(&Received, &Settlement),
    terms: &Terms,
) -> Result<(), SettleError> {
    for received in found {
        let needs_price = received.confirmations >= terms.lock_confirmations
            && records.get(&received.txid).is_none();
        let fetched = if needs_price { Some(price()) } else { None };
        let settlement = {
            let guard = logic
                .lock()
                .map_err(|_| SettleError::Mint("mint lock is poisoned".to_string()))?;
            let rotor = match rotating {
                Some(r) => Some(
                    r.rotor
                        .lock()
                        .map_err(|_| SettleError::Mint("rotor lock is poisoned".to_string()))?,
                ),
                None => None,
            };
            let previous = rotor
                .as_ref()
                .and_then(|r| r.previous.as_ref())
                .map(|(keys, prev)| (keys.id.as_str(), prev));
            settle::settle_with_previous(
                &guard,
                previous,
                records,
                received,
                &mut || {
                    fetched
                        .clone()
                        .unwrap_or_else(|| Err("price was not fetched".to_string()))
                },
                terms,
            )?
        };
        report(received, &settlement);
    }
    Ok(())
}

/// The lines printed for a payment.
pub fn describe(received: &Received, settlement: &Settlement) -> Vec<String> {
    describe_with(received, settlement, &Terms::default())
}

/// Basis points as a percentage: 100 is "1%", 150 is "1.5%".
pub fn percent_text(bps: u128) -> String {
    let (whole, rest) = (bps / 100, bps % 100);
    if rest == 0 {
        format!("{whole}%")
    } else if rest % 10 == 0 {
        format!("{whole}.{}%", rest / 10)
    } else {
        format!("{whole}.{rest:02}%")
    }
}

/// `describe` with the numbers of the given settlement rules.
pub fn describe_with(received: &Received, settlement: &Settlement, terms: &Terms) -> Vec<String> {
    let (lock, min) = (terms.lock_confirmations, terms.min_confirmations);
    let mut lines = vec![format!(
        "{} confirmations={} value_sats={}",
        received.txid, received.confirmations, received.value_sats
    )];
    match settlement {
        Settlement::Waiting { confirmations } => lines.push(format!(
            "  waiting: {confirmations} of {lock} confirmation to lock the price"
        )),
        Settlement::Locked {
            confirmations,
            price_cents,
        } => lines.push(format!(
            "  price locked at {} USD/BTC, waiting: {confirmations} of {min} confirmations",
            cents_text(*price_cents)
        )),
        Settlement::AlreadySettled(status) => lines.push(format!("  already settled ({status:?})")),
        Settlement::PriceUnavailable(why) => lines.push(format!(
            "  waiting: no price this run ({why}); will retry next run"
        )),
        Settlement::Rejected(Rejection::NoDataOutput) => {
            lines.push("  rejected: no single data output".to_string())
        }
        Settlement::Rejected(why) => lines.push(format!("  rejected: {why:?}")),
        Settlement::PaidUnsettled(held) => {
            lines.push(format!("  over/under: {} bps", held.over_under_bps));
            // The AIR exchange that settles this (asking the payer's DID for a
            // new request that matches what was paid) is not built yet. Only the
            // record is made, and the BTC is held.
            lines.push(format!(
                "  paid but unsettled: {} units requested, {} paid; no smaller request gets within {} of the amount paid, so nothing was signed and the BTC is held for payer DID {} (AIR request for a new matching request not built)",
                held.requested_units,
                held.paid_units,
                percent_text(terms.reconciliation_target_bps),
                hex::encode(held.payer_did)
            ));
        }
        Settlement::Issued(issue) => {
            lines.push(format!("  over/under: {} bps", issue.over_under_bps));
            if issue.within_tolerance && issue.over_under_bps != 0 {
                lines.push(format!(
                    "  within the price tolerance ({}): settled in full as if exact",
                    percent_text(terms.price_tolerance_bps)
                ));
            }
            if issue.skipped_units > 0 {
                lines.push(format!(
                    "  underpaid: the smallest outputs were dropped ({} units not signed); what is signed is within {} of the amount paid and the mint keeps the remainder",
                    issue.skipped_units,
                    percent_text(terms.reconciliation_target_bps)
                ));
            }
            if issue.credit_cents > 0 {
                // The AIR message asking the payer's DID for a new request
                // is not built yet. The credit is only recorded.
                lines.push(format!(
                    "  overpaid by more than {}: credit {} USD recorded for payer DID {} (AIR message to the payer not built)",
                    percent_text(terms.reconciliation_target_bps),
                    cents_text(issue.credit_cents),
                    hex::encode(issue.payer_did)
                ));
            }
            // "keyset <id>": the keyset that signed, so the payer unblinds with
            // its keys (GET /keysets) even after a rotation.
            lines.push(format!("  keyset {}", issue.keyset));
            // "signature <blind nonce> <signature>", so the payer can pair each
            // signature with the output it blinded.
            for (blind_nonce, result) in issue.blind_nonces.iter().zip(&issue.results) {
                match result {
                    Ok(signature) => {
                        lines.push(format!("  signature {} {}", blind_nonce.0, signature.0))
                    }
                    Err(e) => lines.push(format!("  not issued: {blind_nonce:?}: {e}")),
                }
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::Status;
    use mint_types::Amount;
    use std::fs;
    use std::path::PathBuf;

    // DID 0279be66..1798, then outputs 4 -> 3G and 1 -> 5G (5 units in all)
    const PAYLOAD_HEX: &str = "210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";
    /// $10,000 per BTC: a unit is $0.10, so 5,000 sats is exactly the 5 units
    /// requested.
    const BTC_10K: u64 = 1_000_000;

    fn fixture(name: &str) -> (Mutex<MintLogic>, Records, PathBuf) {
        let dir = std::env::temp_dir().join(format!("psp-mint-svc-{}-{name}", {
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
        (Mutex::new(logic), records, dir)
    }

    fn received(txid: &str, confirmations: u32) -> Received {
        Received {
            txid: txid.to_string(),
            confirmations,
            value_sats: 5_000,
            payload: Some(hex::decode(PAYLOAD_HEX).unwrap()),
        }
    }

    #[test]
    fn the_mint_is_not_locked_while_the_price_is_fetched() {
        let (logic, mut records, dir) = fixture("lock");
        let found = [received("aa", 1)];
        let mut asked = 0;
        let mut seen_free = Vec::new();
        run_once(
            &logic,
            &mut records,
            &found,
            &mut || {
                asked += 1;
                // another thread (the HTTP API) could take the lock right now
                seen_free.push(logic.try_lock().is_ok());
                Ok(BTC_10K)
            },
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(asked, 1);
        assert_eq!(seen_free, vec![true]);
        assert_eq!(records.get("aa").unwrap().locked_price_cents, Some(BTC_10K));
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_price_is_only_asked_for_payments_with_no_record_and_a_confirmation() {
        let (logic, mut records, dir) = fixture("need");
        let mut asked = 0;
        let mut price = || {
            asked += 1;
            Ok(BTC_10K)
        };
        // not in a block yet: no price
        run_once(
            &logic,
            &mut records,
            &[received("zero", 0)],
            &mut price,
            &mut |_, _| {},
        )
        .unwrap();
        // first sight at 1 confirmation: price
        run_once(
            &logic,
            &mut records,
            &[received("aa", 1)],
            &mut price,
            &mut |_, _| {},
        )
        .unwrap();
        // already locked: no price, then settled: no price
        run_once(
            &logic,
            &mut records,
            &[received("aa", 3)],
            &mut price,
            &mut |_, _| {},
        )
        .unwrap();
        run_once(
            &logic,
            &mut records,
            &[received("aa", 6)],
            &mut price,
            &mut |_, _| {},
        )
        .unwrap();
        run_once(
            &logic,
            &mut records,
            &[received("aa", 7)],
            &mut price,
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(asked, 1);
        assert_eq!(records.get("aa").unwrap().status, Status::Issued);
        assert!(records.get("zero").is_none());
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reorg_after_settlement_never_settles_twice_and_one_before_waits() {
        let (logic, mut records, dir) = fixture("reorg");
        let mut price = || Ok(BTC_10K);
        let mut issued = 0;
        let mut count = |_: &Received, s: &Settlement| {
            if matches!(s, Settlement::Issued(_)) {
                issued += 1;
            }
        };
        // locked at 1, then a reorg takes it back to 0 before it settles: it waits
        run_once(
            &logic,
            &mut records,
            &[received("aa", 1)],
            &mut price,
            &mut count,
        )
        .unwrap();
        run_once(
            &logic,
            &mut records,
            &[received("aa", 0)],
            &mut price,
            &mut count,
        )
        .unwrap();
        assert_eq!(records.get("aa").unwrap().status, Status::Locked);
        assert_eq!(records.unsettled(), vec!["aa".to_string()]);
        // mined again and settled at 6
        run_once(
            &logic,
            &mut records,
            &[received("aa", 6)],
            &mut price,
            &mut count,
        )
        .unwrap();
        assert_eq!(records.get("aa").unwrap().status, Status::Issued);
        assert!(records.unsettled().is_empty());
        // a later reorg and re-mining is seen again but never issues again
        run_once(
            &logic,
            &mut records,
            &[received("aa", 0)],
            &mut price,
            &mut count,
        )
        .unwrap();
        run_once(
            &logic,
            &mut records,
            &[received("aa", 6)],
            &mut price,
            &mut count,
        )
        .unwrap();
        assert_eq!(issued, 1);
        assert_eq!(records.get("aa").unwrap().status, Status::Issued);
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_payment_is_reported_and_a_failed_price_is_retried_next_cycle() {
        let (logic, mut records, dir) = fixture("report");
        let found = [received("aa", 1), received("bb", 1)];
        let mut reports = Vec::new();
        run_once(
            &logic,
            &mut records,
            &found,
            &mut || Err("down".to_string()),
            &mut |r, s| reports.push((r.txid.clone(), describe(r, s))),
        )
        .unwrap();
        assert_eq!(reports.len(), 2);
        assert!(reports[0].1[1].contains("no price this run (down)"));
        assert!(records.get("aa").is_none());

        let mut reports = Vec::new();
        run_once(
            &logic,
            &mut records,
            &found,
            &mut || Ok(BTC_10K),
            &mut |r, s| reports.push((r.txid.clone(), describe(r, s))),
        )
        .unwrap();
        assert!(reports[0].1[1].contains("price locked at 10000.00 USD/BTC"));
        assert!(records.get("aa").is_some() && records.get("bb").is_some());
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sealed_payments_settle_and_payments_sealed_to_another_key_leave_no_trace() {
        use crate::seal;
        use sp_wire::seal::SecretKey;

        let (logic, mut records, dir) = fixture("sealed");
        let mint = SecretKey::new();
        let other = SecretKey::new();
        let plain = hex::decode(PAYLOAD_HEX).unwrap();
        // first seen with 6 confirmations, so it locks and settles in one run
        let mut ours = received("ours", 6);
        ours.payload = Some(seal::encrypt(&mint.public_key(), &plain).unwrap());
        let mut theirs = received("theirs", 6);
        theirs.payload = Some(seal::encrypt(&other.public_key(), &plain).unwrap());
        let mut cleartext = received("cleartext", 6);
        cleartext.payload = Some(plain.clone());

        let (found, dropped) = seal::open_payloads(&mint, vec![ours, theirs, cleartext]);
        assert_eq!(dropped, 2);

        let mut asked = 0;
        run_once(
            &logic,
            &mut records,
            &found,
            &mut || {
                asked += 1;
                Ok(BTC_10K)
            },
            &mut |_, _| {},
        )
        .unwrap();

        // only the payment sealed to this mint was priced, recorded and issued
        assert_eq!(asked, 1);
        assert_eq!(records.get("ours").unwrap().status, Status::Issued);
        assert!(records.get("theirs").is_none());
        assert!(records.get("cleartext").is_none());
        assert_eq!(
            logic.lock().unwrap().audit_totals().unwrap().0,
            Amount::from_units(5)
        );
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn describe_lists_signatures_and_the_over_under() {
        let (logic, mut records, dir) = fixture("describe");
        let r = received("aa", 6);
        let mut lines = Vec::new();
        run_once(
            &logic,
            &mut records,
            std::slice::from_ref(&r),
            &mut || Ok(BTC_10K),
            &mut |r, s| lines = describe(r, s),
        )
        .unwrap();
        assert_eq!(lines[0], "aa confirmations=6 value_sats=5000");
        assert_eq!(lines[1], "  over/under: 0 bps");
        let signature_lines: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with("  signature "))
            .collect();
        assert_eq!(signature_lines.len(), 2);
        // each line is the blind nonce then the signature, both 33-byte points
        for line in signature_lines {
            let parts: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(parts.len(), 3);
            assert_eq!(parts[1].len(), 66);
            assert_eq!(parts[2].len(), 66);
        }
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cents_are_printed_as_dollars() {
        assert_eq!(cents_text(8_167_355), "81673.55");
        assert_eq!(cents_text(5), "0.05");
        assert_eq!(cents_text(10_000_000), "100000.00");
    }

    #[test]
    fn percentages_are_printed_from_basis_points() {
        assert_eq!(percent_text(100), "1%");
        assert_eq!(percent_text(1000), "10%");
        assert_eq!(percent_text(150), "1.5%");
        assert_eq!(percent_text(125), "1.25%");
        assert_eq!(percent_text(5), "0.05%");
    }

    #[test]
    fn describe_uses_the_numbers_in_force() {
        let terms = Terms {
            lock_confirmations: 2,
            min_confirmations: 3,
            price_tolerance_bps: 50,
            reconciliation_target_bps: 150,
        };
        let r = received("dd", 1);
        let waiting = describe_with(&r, &Settlement::Waiting { confirmations: 1 }, &terms);
        assert!(waiting[1].contains("1 of 2 confirmation"), "{waiting:?}");
        let locked = describe_with(
            &r,
            &Settlement::Locked {
                confirmations: 2,
                price_cents: 10_000_000,
            },
            &terms,
        );
        assert!(locked[1].contains("2 of 3 confirmations"), "{locked:?}");
        let issue = settle::Issue {
            keyset: "00".repeat(8),
            results: vec![],
            blind_nonces: vec![],
            skipped_units: 2,
            credit_cents: 100,
            over_under_bps: 0,
            within_tolerance: false,
            payer_did: [7; crate::payload::DID_LEN],
        };
        let lines = describe_with(&r, &Settlement::Issued(issue), &terms);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("within 1.5% of the amount paid")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("overpaid by more than 1.5%")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains(&"07".repeat(33))),
            "{lines:?}"
        );
        let held = settle::Unsettled {
            payer_did: [7; crate::payload::DID_LEN],
            requested_units: 100,
            paid_units: 60,
            value_sats: 60_000,
            over_under_bps: -4000,
        };
        let lines = describe_with(&r, &Settlement::PaidUnsettled(held), &terms);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("paid but unsettled: 100 units requested, 60 paid")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("AIR request for a new matching request not built")),
            "{lines:?}"
        );
    }
}
