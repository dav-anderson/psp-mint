//! One scan cycle, shared by the one-shot run and `serve`.

use std::error::Error;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use mint_core::MintLogic;
use mint_core::state::AppState;
use sp_wire::seal::SecretKey;

use crate::delivery::Deliverer;
use crate::price::{self, Aggregate, Config};
use crate::records::Records;
use crate::scan::{self, Node, Received};
use crate::seal;
use crate::server::Rotating;
use crate::settle::{self, Policy, Rejection, SettleError, Settlement, Terms};

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

/// Move every payment as far as it can go, under the settlement `policy` (the
/// terms and the reconciliation fees), in the records the AIR listener shares (it
/// reconciles payments in them).
///
/// The mint is shared with the HTTP API behind `logic`, so it is locked once per
/// payment and never while waiting on the network. The price is fetched before
/// any lock is taken, and only for a payment that has no record yet, which is the
/// one case where `settle_ruled` asks for it. `price` is called at most once per
/// payment, and the caller makes it fetch at most once per cycle. A payment whose
/// payload names the previous key set is signed by it (see `settle::settle_ruled`).
///
/// **Locks.** The records lock is taken for one payment at a time and never while
/// the price is fetched, so a reconciliation request waits for at most one
/// settlement (a few file writes), never for bitcoind or a price source. The
/// order is fixed everywhere: the records, then the mint, then the rotor. A
/// poisoned records lock fails the pass with `SettleError::RecordsPoisoned`, as a
/// poisoned mint lock fails it with `SettleError::Mint`: a thread died in the
/// middle of a settlement or a reconciliation, and `serve` is about to exit.
pub fn run_once_shared(
    logic: &Mutex<MintLogic>,
    rotating: &Rotating,
    records: &Mutex<Records>,
    found: &[Received],
    price: &mut dyn FnMut() -> Result<u64, String>,
    report: &mut dyn FnMut(&Received, &Settlement),
    policy: &Policy,
) -> Result<(), SettleError> {
    for received in found {
        let needs_price = {
            let guard = lock_records(records)?;
            received.confirmations >= policy.terms.lock_confirmations
                && guard.get(&received.txid).is_none()
        };
        // No lock is held while the price is fetched.
        let fetched = if needs_price { Some(price()) } else { None };
        let settlement = {
            let mut guard = lock_records(records)?;
            settle_one(logic, rotating, &mut guard, received, fetched, policy)?
        };
        report(received, &settlement);
    }
    Ok(())
}

/// The records lock. If a thread panicked holding it the lock is **poisoned**
/// and this fails with `SettleError::RecordsPoisoned`, never recovering: a panic
/// means a thread died in the middle of a settlement or a reconciliation, and the
/// watcher in `serve` exits the process for a supervisor to restart. Every caller
/// treats it the same way: the scan pass fails (`run_once_shared`,
/// `Settings::cycle`), and a delivery or forfeit pass logs and skips. The AIR
/// listener answers nothing (`unavailable`, ignored).
pub fn lock_records(
    records: &Mutex<Records>,
) -> Result<std::sync::MutexGuard<'_, Records>, SettleError> {
    records.lock().map_err(|_| SettleError::RecordsPoisoned)
}

/// Settle one payment. The caller holds the records; the mint is locked first
/// and the rotor second, as everywhere else.
fn settle_one(
    logic: &Mutex<MintLogic>,
    rotating: &Rotating,
    records: &mut Records,
    received: &Received,
    fetched: Option<Result<u64, String>>,
    policy: &Policy,
) -> Result<Settlement, SettleError> {
    let guard = logic
        .lock()
        .map_err(|_| SettleError::Mint("mint lock is poisoned".to_string()))?;
    let rotor = rotating
        .rotor
        .lock()
        .map_err(|_| SettleError::Mint("rotor lock is poisoned".to_string()))?;
    let previous = rotor
        .previous
        .as_ref()
        .map(|(keys, prev)| (keys.id.as_str(), prev));
    settle::settle_ruled(
        &guard,
        previous,
        records,
        received,
        &mut || {
            fetched
                .clone()
                .unwrap_or_else(|| Err("price was not fetched".to_string()))
        },
        policy,
    )
}

/// What one scan pass takes from the settings.
pub struct Pass<'a> {
    /// The network's folder: the scan checkpoint is kept here.
    pub data_dir: &'a Path,
    /// The wallet's name, for the log.
    pub wallet: &'a str,
    /// `settle.min_confirmations`: how deep the checkpoint trails the tip.
    pub min_confirmations: u32,
    /// The terms and fees every payment is settled under.
    pub policy: &'a Policy,
    /// The mint's DID key: payloads are sealed to its public half.
    pub did: &'a SecretKey,
    /// Leave out payments that are already settled.
    pub quiet: bool,
}

/// Scan the wallet once and move every payment as far as it can go: read what is
/// new since the checkpoint (and what is still unsettled), open the payloads with
/// the mint's DID key, rotate to the current key set, settle each payment
/// (`run_once_shared`), print what happened, and only then save the checkpoint.
///
/// A payment whose data does not open with the DID key is not for this mint and
/// is ignored, not rejected. A failed pass leaves the checkpoint where it was, so
/// the same blocks are read again next time. The records are locked only to read
/// what is unsettled and, one payment at a time, to settle it; never while the
/// wallet is scanned or a price is fetched.
///
/// The callers follow it with [`deliver`] and [`forfeit`], whether or not it
/// worked: what was issued before is owed its record, and a window that closed
/// stays closed.
pub fn cycle(
    pass: &Pass,
    node: &dyn Node,
    state: &AppState,
    rot: &Rotating,
    records: &Mutex<Records>,
    price: &mut dyn FnMut() -> Result<u64, String>,
) -> Result<(), Box<dyn Error>> {
    let checkpoint = scan::Checkpoint::load(pass.data_dir)?;
    let unsettled = lock_records(records)?.unsettled();
    let since = scan::since(
        node,
        checkpoint.as_ref(),
        pass.min_confirmations,
        &unsettled,
    )?;
    let (found, ignored) = seal::open_payloads(pass.did, since.received);
    if !pass.quiet {
        if found.is_empty() && ignored == 0 {
            println!("no payments received by wallet {}", pass.wallet);
        }
        if ignored > 0 {
            println!("ignored {ignored} payment(s) whose data is not encrypted to this mint");
        }
    }
    // Rotate first, so a settlement signs with the current key set.
    {
        let mut logic = state
            .logic
            .lock()
            .map_err(|_| "the mint lock is poisoned")?;
        rot.rotor
            .lock()
            .map_err(|_| "the rotor lock is poisoned")?
            .refresh(&mut logic)?;
    }
    let terms = pass.policy.terms;
    run_once_shared(
        &state.logic,
        rot,
        records,
        &found,
        price,
        &mut |received, settlement| {
            if pass.quiet && matches!(settlement, Settlement::AlreadySettled(_)) {
                return;
            }
            for line in describe_with(received, settlement, &terms) {
                println!("{line}");
            }
        },
        pass.policy,
    )?;
    // Only after every payment was processed: a failed pass reads the same
    // blocks again next time.
    since.next.save(pass.data_dir)?;
    Ok(())
}

/// Write the records of every purchase that is owed one and has none stored.
/// Never fails a scan: what could not be written is printed and tried again on the
/// next pass. Without a deliverer (`air.server` and `air.host` not set) the
/// purchases stay in the records, and `quiet` leaves out saying so.
pub fn deliver(deliverer: &mut Option<Deliverer>, records: &Mutex<Records>, quiet: bool) {
    match deliverer {
        Some(deliverer) => {
            // The records are locked to plan and to apply, not while the
            // records are written to the AIR server.
            deliverer.pass_shared(records, Instant::now());
        }
        None => {
            let Ok(guard) = lock_records(records) else {
                eprintln!("delivery: the settlement records lock is poisoned; skipped");
                return;
            };
            let waiting = guard.undelivered().len();
            drop(guard);
            if waiting > 0 && !quiet {
                println!(
                    "{waiting} purchase(s) are waiting for their signatures to be delivered over \
                     AIR; air.server and air.host are not set, so they stay in the records"
                );
            }
        }
    }
}

/// Forfeit every payment whose reconciliation window closed at or before `now`
/// with the payment still open: the mint keeps the BTC and owes nothing, and
/// `/audit` shows it apart. Never fails a scan. A poisoned records lock is logged
/// and skipped (the watcher in `serve` exits the process).
pub fn forfeit(records: &Mutex<Records>, now: u64) {
    let Ok(mut guard) = lock_records(records) else {
        eprintln!("forfeit check skipped: the settlement records lock is poisoned");
        return;
    };
    match guard.forfeit_expired(now) {
        Ok(gone) => {
            for txid in gone {
                println!(
                    "forfeited {txid}: its reconciliation window closed with the payment \
                     unsettled; the mint keeps the BTC"
                );
            }
        }
        Err(e) => eprintln!("forfeit check failed, will retry: {e}"),
    }
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

/// The lines printed for a payment, with the numbers of the given settlement
/// rules.
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
            // The record is made, the BTC is held, and the payer's delivery
            // record (over AIR) carries the rules for a corrected request.
            lines.push(format!(
                "  paid but unsettled: {} units requested, {} paid; no smaller request gets within {} of the amount paid, so nothing was signed and the BTC is held for reply key {}",
                held.requested_units,
                held.paid_units,
                percent_text(terms.reconciliation_target_bps),
                hex::encode(held.payer_did)
            ));
            let days = terms.reconciliation_window_secs.div_ceil(86_400);
            lines.push(format!(
                "  the payer is told over AIR and can send a corrected request (kind 15); if it is not settled within {days} day(s) the payment is forfeited and the mint keeps the BTC"
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
                // The credit is recorded; the payer claims it over AIR (kind 15)
                // with a credit claim signed by the reply key.
                lines.push(format!(
                    "  overpaid by more than {}: credit {} USD recorded for reply key {}; the payer can claim it over AIR (kind 15)",
                    percent_text(terms.reconciliation_target_bps),
                    cents_text(issue.credit_cents),
                    hex::encode(issue.payer_did)
                ));
            }
            // Counts and amounts only. The signatures are not logged: they are
            // stored in the purchase's record and written to the AIR server as
            // its delivery record (see `delivery`).
            let signed = issue.results.iter().filter(|r| r.is_ok()).count();
            lines.push(format!(
                "  signed {signed} output(s), {} unit(s)",
                issue.signed_units
            ));
            let failed: Vec<String> = issue
                .results
                .iter()
                .filter_map(|r| r.as_ref().err().map(|e| e.to_string()))
                .collect();
            if let Some(first) = failed.first() {
                lines.push(format!(
                    "  not issued: {} output(s), the first because: {first}",
                    failed.len()
                ));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::Status;
    use crate::rotation::{Rollovers, Rotor};
    use crate::server::SharedRotating;
    use mint_types::Amount;
    use sp_wire::air_request::delivery::Outcome;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    // DID 0279be66..1798, then outputs 4 -> 3G and 1 -> 5G (5 units in all)
    const PAYLOAD_HEX: &str = "210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";
    /// $10,000 per BTC: a unit is $0.10, so 5,000 sats is exactly the 5 units
    /// requested.
    const BTC_10K: u64 = 1_000_000;

    /// The mint wired the way `Settings::open_mint` wires it: the rotor and its
    /// rollovers behind a `Rotating`, the mint behind its mutex, and the
    /// settlement records behind a mutex.
    struct Mint {
        logic: Mutex<MintLogic>,
        rot: SharedRotating,
        records: Mutex<Records>,
        dir: PathBuf,
    }

    impl Mint {
        fn open(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("psp-mint-svc-{}-{name}", {
                static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                format!(
                    "{}-{}",
                    std::process::id(),
                    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                )
            }));
            let _ = fs::remove_dir_all(&dir);
            let (rotor, logic) = Rotor::open(&dir, Amount::from_units(1024), 0, 0).unwrap();
            let rollovers = Rollovers::open(&dir).unwrap();
            Mint {
                logic: Mutex::new(logic),
                rot: Rotating::new(rotor, rollovers),
                records: Mutex::new(Records::open(&dir.join("settlements.json")).unwrap()),
                dir,
            }
        }

        /// One cycle under `policy`.
        fn run_under(
            &self,
            found: &[Received],
            price: &mut dyn FnMut() -> Result<u64, String>,
            report: &mut dyn FnMut(&Received, &Settlement),
            policy: &Policy,
        ) -> Result<(), SettleError> {
            run_once_shared(
                &self.logic,
                &self.rot,
                &self.records,
                found,
                price,
                report,
                policy,
            )
        }

        /// One cycle under the default policy.
        fn run(
            &self,
            found: &[Received],
            price: &mut dyn FnMut() -> Result<u64, String>,
            report: &mut dyn FnMut(&Received, &Settlement),
        ) {
            self.run_under(found, price, report, &Policy::default())
                .unwrap();
        }

        fn record(&self, txid: &str) -> Option<crate::records::Record> {
            self.records.lock().unwrap().get(txid).cloned()
        }

        fn issued_units(&self) -> u128 {
            self.logic.lock().unwrap().audit_totals().unwrap().0
        }
    }

    impl Drop for Mint {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
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
        let m = Mint::open("lock");
        let found = [received("aa", 1)];
        let mut asked = 0;
        let mut seen_free = Vec::new();
        m.run(
            &found,
            &mut || {
                asked += 1;
                // another thread (the HTTP API) could take the lock right now
                seen_free.push(m.logic.try_lock().is_ok());
                Ok(BTC_10K)
            },
            &mut |_, _| {},
        );
        assert_eq!(asked, 1);
        assert_eq!(seen_free, vec![true]);
        assert_eq!(m.record("aa").unwrap().locked_price_cents, Some(BTC_10K));
    }

    #[test]
    fn the_records_are_not_locked_while_the_price_is_fetched() {
        // A reconciliation request (kind 15) takes the records lock: it must not
        // wait for a price source.
        let m = Mint::open("records-free");
        let mut seen_free = Vec::new();
        m.run(
            &[received("aa", 1)],
            &mut || {
                seen_free.push(m.records.try_lock().is_ok());
                Ok(BTC_10K)
            },
            &mut |_, _| {},
        );
        assert_eq!(seen_free, vec![true]);
    }

    #[test]
    fn the_price_is_only_asked_for_payments_with_no_record_and_a_confirmation() {
        let m = Mint::open("need");
        let mut asked = 0;
        let mut price = || {
            asked += 1;
            Ok(BTC_10K)
        };
        // not in a block yet: no price
        m.run(&[received("zero", 0)], &mut price, &mut |_, _| {});
        // first sight at 1 confirmation: price
        m.run(&[received("aa", 1)], &mut price, &mut |_, _| {});
        // already locked: no price, then settled: no price
        m.run(&[received("aa", 3)], &mut price, &mut |_, _| {});
        m.run(&[received("aa", 6)], &mut price, &mut |_, _| {});
        m.run(&[received("aa", 7)], &mut price, &mut |_, _| {});
        assert_eq!(asked, 1);
        assert_eq!(m.record("aa").unwrap().status, Status::Issued);
        assert!(m.record("zero").is_none());
    }

    #[test]
    fn a_reorg_after_settlement_never_settles_twice_and_one_before_waits() {
        let m = Mint::open("reorg");
        let mut price = || Ok(BTC_10K);
        let mut issued = 0;
        let mut count = |_: &Received, s: &Settlement| {
            if matches!(s, Settlement::Issued(_)) {
                issued += 1;
            }
        };
        // locked at 1, then a reorg takes it back to 0 before it settles: it waits
        m.run(&[received("aa", 1)], &mut price, &mut count);
        m.run(&[received("aa", 0)], &mut price, &mut count);
        assert_eq!(m.record("aa").unwrap().status, Status::Locked);
        assert_eq!(
            m.records.lock().unwrap().unsettled(),
            vec!["aa".to_string()]
        );
        // mined again and settled at 6
        m.run(&[received("aa", 6)], &mut price, &mut count);
        assert_eq!(m.record("aa").unwrap().status, Status::Issued);
        assert!(m.records.lock().unwrap().unsettled().is_empty());
        // a later reorg and re-mining is seen again but never issues again
        m.run(&[received("aa", 0)], &mut price, &mut count);
        m.run(&[received("aa", 6)], &mut price, &mut count);
        assert_eq!(issued, 1);
        assert_eq!(m.record("aa").unwrap().status, Status::Issued);
        assert_eq!(m.issued_units(), 5);
    }

    #[test]
    fn every_payment_is_reported_and_a_failed_price_is_retried_next_cycle() {
        let m = Mint::open("report");
        let terms = Policy::default().terms;
        let found = [received("aa", 1), received("bb", 1)];
        let mut reports = Vec::new();
        m.run(&found, &mut || Err("down".to_string()), &mut |r, s| {
            reports.push((r.txid.clone(), describe_with(r, s, &terms)))
        });
        assert_eq!(reports.len(), 2);
        assert_eq!(
            reports[0].1[1],
            "  waiting: no price this run (down); will retry next run"
        );
        assert!(m.record("aa").is_none());

        let mut reports = Vec::new();
        m.run(&found, &mut || Ok(BTC_10K), &mut |r, s| {
            reports.push((r.txid.clone(), describe_with(r, s, &terms)))
        });
        assert_eq!(
            reports[0].1[1],
            "  price locked at 10000.00 USD/BTC, waiting: 1 of 6 confirmations"
        );
        assert!(m.record("aa").is_some() && m.record("bb").is_some());
    }

    #[test]
    fn sealed_payments_settle_and_payments_sealed_to_another_key_leave_no_trace() {
        use crate::seal;
        use sp_wire::seal::SecretKey;

        let m = Mint::open("sealed");
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

        let (found, _) = seal::open_payloads(&mint, vec![ours, theirs, cleartext]);

        let mut asked = 0;
        m.run(
            &found,
            &mut || {
                asked += 1;
                Ok(BTC_10K)
            },
            &mut |_, _| {},
        );

        // only the payment sealed to this mint was priced, recorded and issued
        assert_eq!(asked, 1);
        assert_eq!(m.record("ours").unwrap().status, Status::Issued);
        assert!(m.record("theirs").is_none());
        assert!(m.record("cleartext").is_none());
        assert_eq!(m.issued_units(), 5);
    }

    #[test]
    fn the_scan_log_has_counts_and_amounts_and_no_signature() {
        let m = Mint::open("describe");
        let terms = Policy::default().terms;
        let r = received("aa", 6);
        let mut lines = Vec::new();
        m.run(
            std::slice::from_ref(&r),
            &mut || Ok(BTC_10K),
            &mut |r, s| lines = describe_with(r, s, &terms),
        );
        assert_eq!(
            lines,
            [
                "aa confirmations=6 value_sats=5000",
                "  over/under: 0 bps",
                "  signed 2 output(s), 5 unit(s)",
            ]
        );
        // what the payer is owed is in the record, and none of it is in the log
        let record = m.record("aa").unwrap();
        let Outcome::Signed(signed) = record.delivery.unwrap().outcome else {
            panic!("an exact purchase is issued");
        };
        assert_eq!(signed.signatures.len(), 2);
        for owed in &signed.signatures {
            for line in &lines {
                assert!(!line.contains(&owed.blind_nonce), "{line}");
                assert!(!line.contains(&owed.signature), "{line}");
            }
        }
    }
    #[test]
    fn log_amounts_are_printed_as_dollars_and_percent() {
        assert_eq!(cents_text(8_167_355), "81673.55");
        assert_eq!(cents_text(5), "0.05");
        assert_eq!(cents_text(10_000_000), "100000.00");
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
            reconciliation_window_secs: 86_400,
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
            signed_units: 0,
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
        // a credit names the reply key and says how it is claimed (kind 15)
        let credit = format!(
            "  overpaid by more than 1.5%: credit 1.00 USD recorded for reply key {}; the payer can claim it over AIR (kind 15)",
            "07".repeat(33)
        );
        assert!(lines.contains(&credit), "{lines:?}");
        let held = settle::Unsettled {
            payer_did: [7; crate::payload::DID_LEN],
            requested_units: 100,
            paid_units: 60,
            value_sats: 60_000,
            over_under_bps: -4000,
        };
        let lines = describe_with(&r, &Settlement::PaidUnsettled(held), &terms);
        assert_eq!(lines[1], "  over/under: -4000 bps");
        assert_eq!(
            lines[2],
            format!(
                "  paid but unsettled: 100 units requested, 60 paid; no smaller request gets within 1.5% of the amount paid, so nothing was signed and the BTC is held for reply key {}",
                "07".repeat(33)
            )
        );
        // kind 15 is built: the payer can answer, and the window is the terms'
        assert_eq!(
            lines[3],
            "  the payer is told over AIR and can send a corrected request (kind 15); if it is not settled within 1 day(s) the payment is forfeited and the mint keeps the BTC"
        );
        // and nothing says the exchange is not built
        for line in &lines {
            assert!(!line.contains("not built"), "{line}");
            assert!(!line.contains("payer DID"), "{line}");
        }
    }

    #[test]
    fn a_poisoned_records_lock_fails_the_pass_and_settles_nothing() {
        let m = Mint::open("poison");
        let records = Arc::new(Mutex::new(
            Records::open(&m.dir.join("poisoned.json")).unwrap(),
        ));
        let held = Arc::clone(&records);
        let died = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("a thread dies holding the records");
        })
        .join();
        assert!(died.is_err());
        assert!(records.is_poisoned());
        assert!(matches!(
            lock_records(&records),
            Err(SettleError::RecordsPoisoned)
        ));
        let found = [received("aa", 6)];
        let mut reported = 0;
        let err = run_once_shared(
            &m.logic,
            &m.rot,
            &records,
            &found,
            &mut || Ok(BTC_10K),
            &mut |_, _| reported += 1,
            &Policy::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SettleError::RecordsPoisoned), "{err}");
        assert_eq!(reported, 0, "nothing was settled or reported");
        assert_eq!(m.issued_units(), 0);
        assert!(records.is_poisoned(), "the lock was not recovered");
    }

    #[test]
    fn a_poisoned_mint_or_rotor_lock_fails_the_pass_and_settles_nothing() {
        let m = Mint::open("poison-mint");
        let logic = Arc::new(Mutex::new(
            Rotor::open(&m.dir.join("other"), Amount::from_units(16), 0, 0)
                .unwrap()
                .1,
        ));
        let held = Arc::clone(&logic);
        let died = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("a thread dies holding the mint");
        })
        .join();
        assert!(died.is_err());
        let mut reported = 0;
        let err = run_once_shared(
            &logic,
            &m.rot,
            &m.records,
            &[received("aa", 6)],
            &mut || Ok(BTC_10K),
            &mut |_, _| reported += 1,
            &Policy::default(),
        )
        .unwrap_err();
        match err {
            SettleError::Mint(why) => assert_eq!(why, "mint lock is poisoned"),
            other => panic!("{other}"),
        }
        assert_eq!(reported, 0);
        // the mint is needed before the payment is looked at: no record, and
        // nothing was signed
        assert!(m.record("aa").is_none());
        assert_eq!(m.issued_units(), 0);

        // a poisoned rotor lock fails the pass the same way
        let m = Mint::open("poison-rotor");
        let rot = Arc::clone(&m.rot);
        let died = std::thread::spawn(move || {
            let _guard = rot.rotor.lock().unwrap();
            panic!("a thread dies holding the rotor");
        })
        .join();
        assert!(died.is_err());
        let err = m
            .run_under(
                &[received("aa", 6)],
                &mut || Ok(BTC_10K),
                &mut |_, _| {},
                &Policy::default(),
            )
            .unwrap_err();
        match err {
            SettleError::Mint(why) => assert_eq!(why, "rotor lock is poisoned"),
            other => panic!("{other}"),
        }
        assert!(m.record("aa").is_none());
        assert_eq!(m.issued_units(), 0);
    }

    #[test]
    fn a_payment_the_previous_keyset_names_is_signed_by_it_through_the_cycle() {
        // The rotor's previous key set reaches settlement: the cycle hands it to
        // `settle_ruled` as `previous`.
        let dir = std::env::temp_dir().join(format!("psp-mint-svc-{}-rotated", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let t = 10_000;
        let (mut rotor, mut logic) =
            Rotor::open_at(&dir, Amount::from_units(1024), 1, 1, t).unwrap();
        rotor.first_issue = Some(t);
        rotor
            .clock
            .store(t + 3600, std::sync::atomic::Ordering::SeqCst);
        rotor.refresh(&mut logic).unwrap();
        let previous_id = rotor.previous.as_ref().unwrap().0.id.clone();
        let current_id = crate::rotation::keyset_id(logic.public_keys());
        assert_ne!(previous_id, current_id);
        let rot = Rotating::new(rotor, Rollovers::open(&dir).unwrap());
        let logic = Mutex::new(logic);
        let records = Mutex::new(Records::open(&dir.join("settlements.json")).unwrap());

        let mut payload = crate::payload::decode(&hex::decode(PAYLOAD_HEX).unwrap()).unwrap();
        payload.keyset = Some(previous_id.clone());
        let mut r = received("aa", 6);
        r.payload = Some(crate::payload::encode(&payload).unwrap());
        let mut signed_by = None;
        run_once_shared(
            &logic,
            &rot,
            &records,
            &[r],
            &mut || Ok(BTC_10K),
            &mut |_, s| {
                if let Settlement::Issued(issue) = s {
                    signed_by = Some(issue.keyset.clone());
                }
            },
            &Policy::default(),
        )
        .unwrap();
        assert_eq!(signed_by, Some(previous_id));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A payload for the reply key `07..07` asking for these `(units, seed)`
    /// outputs, in the wire form `payload::decode` reads.
    fn payload_bytes(outputs: &[(u64, u8)]) -> Vec<u8> {
        use secp256k1::{PublicKey, SECP256K1, SecretKey};
        let mut bytes = vec![33u8];
        bytes.extend_from_slice(&[7u8; 33]);
        bytes.push(outputs.len() as u8);
        for &(units, seed) in outputs {
            bytes.extend_from_slice(&units.to_be_bytes());
            let key = SecretKey::from_slice(&[seed; 32]).unwrap();
            bytes.extend_from_slice(&PublicKey::from_secret_key(SECP256K1, &key).serialize());
        }
        bytes
    }

    #[test]
    fn fees_come_off_what_the_payment_buys_and_are_kept_in_the_records() {
        // delivery.read_fee 1, reconciliation.fee 2, reconciliation.write_fee 3
        // (PSP units, $0.10 each at this price)
        let m = Mint::open("fees");
        let policy = Policy {
            fees: crate::settle::ReconcileTerms {
                fee_units: 2,
                write_fee_units: 3,
                read_fee_units: 1,
                ..Default::default()
            },
            ..Policy::default()
        };
        // 6,000 sats are 6 units; the first read costs 1, which leaves exactly
        // the 5 units asked: issued in full, no credit
        let mut exact = received("exact", 6);
        exact.value_sats = 6_000;
        // 12,000 sats are 12 units, less the read 11 against the one note of 16
        // asked: held, with the rules for the corrected request
        let mut held = received("held", 6);
        held.value_sats = 12_000;
        held.payload = Some(payload_bytes(&[(16, 3)]));
        let mut kinds = Vec::new();
        m.run_under(
            &[exact, held],
            &mut || Ok(BTC_10K),
            &mut |r, s| {
                let kind = match s {
                    Settlement::Issued(_) => "issued",
                    Settlement::PaidUnsettled(_) => "held",
                    _ => "other",
                };
                kinds.push((r.txid.clone(), kind));
            },
            &policy,
        )
        .unwrap();
        assert_eq!(
            kinds,
            vec![
                ("exact".to_string(), "issued"),
                ("held".to_string(), "held")
            ]
        );
        let exact = m.record("exact").unwrap();
        assert_eq!(exact.read_prepaid_units, 1);
        assert_eq!(exact.credit_cents, 0);
        assert_eq!(m.issued_units(), 5);
        let held = m.record("held").unwrap();
        assert_eq!(held.read_prepaid_units, 1);
        let rules = held.rules.as_ref().unwrap();
        // 12 units paid, less the first read (1), the fee (2), the write (3) and
        // the follow-up's read (1)
        assert_eq!(rules.due_units, 5);
        assert_eq!(rules.reconciliation_fee_units, 2);
        assert_eq!(rules.write_prepaid_units, 3);
        assert_eq!(rules.read_prepaid_units, 2);
        // the first reads are revenue, counted in /audit's reconciliation
        // section; the fee and the write are counted when a reconciliation settles
        let report = m.records.lock().unwrap().reconciliation();
        assert_eq!(report.read_prepaid_units, 2);
        assert_eq!((report.fees_units, report.write_prepaid_units), (0, 0));
    }
}
