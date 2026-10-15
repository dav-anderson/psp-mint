//! One scan cycle, shared by the one-shot run and `serve`.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use mint_core::MintLogic;

use crate::price::{self, Aggregate, Config};
use crate::records::Records;
use crate::scan::Received;
use crate::settle::{
    self, LOCK_CONFIRMATIONS, MIN_CONFIRMATIONS, Rejection, SettleError, Settlement,
};

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
    let lookup = price::combine(&price::fetch_all(), now, config);
    for (source, why) in &lookup.excluded {
        println!("  price: excluded {source}: {why}");
    }
    match lookup.aggregate {
        Aggregate::Price { cents, sources } => {
            println!("  price: {} USD/BTC from {sources} sources", cents_text(cents));
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
    for received in found {
        let needs_price =
            received.confirmations >= LOCK_CONFIRMATIONS && records.get(&received.txid).is_none();
        let fetched = if needs_price { Some(price()) } else { None };
        let settlement = {
            let guard = logic
                .lock()
                .map_err(|_| SettleError::Mint("mint lock is poisoned".to_string()))?;
            settle::settle(&guard, records, received, &mut || {
                fetched
                    .clone()
                    .unwrap_or_else(|| Err("price was not fetched".to_string()))
            })?
        };
        report(received, &settlement);
    }
    Ok(())
}

/// The lines printed for a payment.
pub fn describe(received: &Received, settlement: &Settlement) -> Vec<String> {
    let mut lines = vec![format!(
        "{} confirmations={} value_sats={}",
        received.txid, received.confirmations, received.value_sats
    )];
    match settlement {
        Settlement::Waiting { confirmations } => lines.push(format!(
            "  waiting: {confirmations} of {LOCK_CONFIRMATIONS} confirmation to lock the price"
        )),
        Settlement::Locked {
            confirmations,
            price_cents,
        } => lines.push(format!(
            "  price locked at {} USD/BTC, waiting: {confirmations} of {MIN_CONFIRMATIONS} confirmations",
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
        Settlement::Issued(issue) => {
            lines.push(format!("  over/under: {} bps", issue.over_under_bps));
            if issue.skipped_units > 0 {
                lines.push(format!(
                    "  underpaid by more than 1%: {} units not signed, kept by the mint",
                    issue.skipped_units
                ));
            }
            if issue.credit_cents > 0 {
                // The AIR message asking the payer's DID for a new request
                // is not built yet. The credit is only recorded.
                lines.push(format!(
                    "  overpaid by 10% or more: credit {} USD recorded (AIR message to the payer not built)",
                    cents_text(issue.credit_cents)
                ));
            }
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
    /// $100,000 per BTC: 5,000 sats is exactly the 5 units requested.
    const BTC_100K: u64 = 10_000_000;

    fn fixture(name: &str) -> (Mutex<MintLogic>, Records, PathBuf) {
        let dir = std::env::temp_dir().join(format!("psp-mint-svc-{}-{name}", std::process::id()));
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
                Ok(BTC_100K)
            },
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(asked, 1);
        assert_eq!(seen_free, vec![true]);
        assert_eq!(
            records.get("aa").unwrap().locked_price_cents,
            Some(BTC_100K)
        );
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_price_is_only_asked_for_payments_with_no_record_and_a_confirmation() {
        let (logic, mut records, dir) = fixture("need");
        let mut asked = 0;
        let mut price = || {
            asked += 1;
            Ok(BTC_100K)
        };
        // not in a block yet: no price
        run_once(&logic, &mut records, &[received("zero", 0)], &mut price, &mut |_, _| {}).unwrap();
        // first sight at 1 confirmation: price
        run_once(&logic, &mut records, &[received("aa", 1)], &mut price, &mut |_, _| {}).unwrap();
        // already locked: no price, then settled: no price
        run_once(&logic, &mut records, &[received("aa", 3)], &mut price, &mut |_, _| {}).unwrap();
        run_once(&logic, &mut records, &[received("aa", 6)], &mut price, &mut |_, _| {}).unwrap();
        run_once(&logic, &mut records, &[received("aa", 7)], &mut price, &mut |_, _| {}).unwrap();
        assert_eq!(asked, 1);
        assert_eq!(records.get("aa").unwrap().status, Status::Issued);
        assert!(records.get("zero").is_none());
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
            &mut || Ok(BTC_100K),
            &mut |r, s| reports.push((r.txid.clone(), describe(r, s))),
        )
        .unwrap();
        assert!(reports[0].1[1].contains("price locked at 100000.00 USD/BTC"));
        assert!(records.get("aa").is_some() && records.get("bb").is_some());
        drop(logic);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sealed_payments_settle_and_payments_sealed_to_another_key_leave_no_trace() {
        use crate::seal;
        use air::names::secp256k1::SecretKey;

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
                Ok(BTC_100K)
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
            &mut || Ok(BTC_100K),
            &mut |r, s| lines = describe(r, s),
        )
        .unwrap();
        assert_eq!(lines[0], "aa confirmations=6 value_sats=5000");
        assert_eq!(lines[1], "  over/under: 0 bps");
        let signature_lines: Vec<&String> =
            lines.iter().filter(|l| l.starts_with("  signature ")).collect();
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
}
