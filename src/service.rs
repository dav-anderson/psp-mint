//! One scan cycle, shared by the one-shot run and `serve`.

use std::error::Error;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use mint_core::MintLogic;
use mint_core::state::AppState;
use sp_wire::air_request::addr::now_secs;
use sp_wire::seal::SecretKey;

use crate::claim;
use crate::delivery::Deliverer;
use crate::price::{self, Aggregate, Config};
use crate::records::Records;
use crate::scan::{self, Node, Received};
use crate::seal;
use crate::server::Rotating;
use crate::settle::{self, Kept, Policy, SettleError, Settlement, Terms};

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
/// terms and the claim fees), in the records the AIR listener shares (it takes
/// claims against the owed records in them). `now` is Unix seconds: the time a
/// claim finished by [`recover`] in this run is stored at.
///
/// The mint is shared with the HTTP API behind `logic`, so it is locked once per
/// payment and never while waiting on the network. The price is fetched before
/// any lock is taken, and only for a payment that has no record yet, which is the
/// one case where `settle_ruled` asks for it. `price` is called at most once per
/// payment, and the caller makes it fetch at most once per cycle. Nothing is
/// signed here: a payment that reaches its tier is owed a record, and the notes
/// are signed when it is claimed (`claim`).
///
/// **Locks.** The records lock is taken for one payment at a time and never while
/// the price is fetched, so a claim waits for at most one settlement (a few file
/// writes), never for bitcoind or a price source. The order is fixed everywhere:
/// the records, then the mint, then the rotor (also in [`recover`], which runs
/// first). A poisoned records lock fails the pass with
/// `SettleError::RecordsPoisoned`. The mint and the rotor are locked only by
/// [`recover`], inside its loop over the claims to finish: a pass with a claim to
/// recover and a poisoned mint or rotor lock fails with `SettleError::Mint`
/// before any payment is settled, and a pass with none never takes either lock,
/// so settling (which signs nothing) does not depend on them. In every case a
/// thread died in the middle of a settlement or a claim, and `serve` is about to
/// exit.
#[allow(clippy::too_many_arguments)]
pub fn run_once_shared(
    logic: &Mutex<MintLogic>,
    rotating: &Rotating,
    records: &Mutex<Records>,
    found: &[Received],
    price: &mut dyn FnMut() -> Result<u64, String>,
    report: &mut dyn FnMut(&Received, &Settlement),
    policy: &Policy,
    now: u64,
) -> Result<(), SettleError> {
    // A claim that was accepted and not finished is finished before any payment
    // is settled, whoever calls this.
    recover(logic, rotating, records, now)?;
    for received in found {
        let needs_price = {
            let guard = lock_records(records)?;
            received.confirmations >= policy.terms.lock_confirmations
                && guard.get(&received.txid).is_none()
                && !guard.was_pruned(&received.txid)
                && !guard.is_beyond_cutoff(received.block_time)
        };
        // No lock is held while the price is fetched.
        let fetched = if needs_price { Some(price()) } else { None };
        let settlement = {
            let mut guard = lock_records(records)?;
            settle_one(&mut guard, received, fetched, policy, now)?
        };
        report(received, &settlement);
    }
    Ok(())
}

/// The records lock. If a thread panicked holding it the lock is **poisoned**
/// and this fails with `SettleError::RecordsPoisoned`, never recovering: a panic
/// means a thread died in the middle of a settlement or a claim, and the
/// watcher in `serve` exits the process for a supervisor to restart. Every caller
/// treats it the same way: the scan pass fails (`run_once_shared`,
/// `Settings::cycle`), and a delivery or expiry pass logs and skips. The AIR
/// listener answers nothing (`unavailable`, ignored).
pub fn lock_records(
    records: &Mutex<Records>,
) -> Result<std::sync::MutexGuard<'_, Records>, SettleError> {
    records.lock().map_err(|_| SettleError::RecordsPoisoned)
}

/// Settle one payment. The caller holds the records. Nothing here touches the
/// mint or the rotor: the owed record carries no keysets and no clock.
fn settle_one(
    records: &mut Records,
    received: &Received,
    fetched: Option<Result<u64, String>>,
    policy: &Policy,
    now: u64,
) -> Result<Settlement, SettleError> {
    settle::settle_ruled(
        records,
        received,
        &mut || {
            fetched
                .clone()
                .unwrap_or_else(|| Err("price was not fetched".to_string()))
        },
        policy,
        now,
    )
}

/// Finish every claim that was accepted and has no answer: the mint saved the
/// claim before signing and a crash, or a failed write, came before the answer
/// was stored (`claim::finish`). Each is signed with the keyset that is
/// **current** when this runs, and its answer is stored, so the wallet's identical
/// resend gets it; the wallet need not send the claim again for the mint to finish
/// it. Returns how many were finished.
///
/// Normally the current keyset is the one the claim was accepted for, and the
/// signatures are the same as the first time. If the mint rotated in between, the
/// answer names the new keyset (see `claim` for what that does to the issued
/// totals). A rotation never waits for an unfinished claim.
///
/// Runs at startup, before the AIR listener, and at the start of every scan pass,
/// before any new payment is settled. **Locks**, as everywhere: the records, then
/// the mint, then the rotor, taken for one claim at a time.
///
/// Every claim is tried. The first error is returned after the last, and names
/// the txid: the claim stays as it is and is tried again next time.
pub fn recover(
    logic: &Mutex<MintLogic>,
    rotating: &Rotating,
    records: &Mutex<Records>,
    now: u64,
) -> Result<usize, SettleError> {
    let mut guard = lock_records(records)?;
    let mut finished = 0;
    let mut first_error = None;
    for txid in guard.pending_claims() {
        let mint = logic
            .lock()
            .map_err(|_| SettleError::Mint("mint lock is poisoned".to_string()))?;
        let rotor = rotating
            .rotor
            .lock()
            .map_err(|_| SettleError::Mint("rotor lock is poisoned".to_string()))?;
        match claim::finish(&mut guard, &mint, &rotor, &txid, now) {
            Ok(done) => {
                finished += 1;
                println!(
                    "{txid}: finished an accepted claim ({} units signed by keyset {})",
                    done.issued_units, done.keyset
                );
            }
            Err(e) => {
                eprintln!(
                    "{txid}: could not finish an accepted claim: {} ({})",
                    e.why, e.code
                );
                first_error.get_or_insert(SettleError::Mint(format!("{txid}: {}", e.why)));
            }
        }
    }
    first_error.map_or(Ok(finished), Err)
}

/// What one scan pass takes from the settings.
pub struct Pass<'a> {
    /// The network's folder: the scan checkpoint is kept here.
    pub data_dir: &'a Path,
    /// The wallet's name, for the log.
    pub wallet: &'a str,
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
/// is ignored. A failed pass leaves the checkpoint where it was, so
/// the same blocks are read again next time. The records are locked only to read
/// what is unsettled and, one payment at a time, to settle it; never while the
/// wallet is scanned or a price is fetched.
///
/// The callers follow it with [`deliver`] and [`expire`], whether or not it
/// worked: what is owed is owed its record, and a window that closed stays closed.
pub fn cycle(
    pass: &Pass,
    node: &dyn Node,
    state: &AppState,
    rot: &Rotating,
    records: &Mutex<Records>,
    price: &mut dyn FnMut() -> Result<u64, String>,
) -> Result<(), Box<dyn Error>> {
    // Claims the mint accepted and did not finish storing come first, before
    // anything new is settled and before the key sets rotate; this does not need
    // bitcoind, so a pass that cannot reach it still finishes them.
    recover(&state.logic, rot, records, now_secs())?;
    let checkpoint = scan::Checkpoint::load(pass.data_dir)?;
    let unsettled = lock_records(records)?.unsettled();
    let since = scan::since(
        node,
        checkpoint.as_ref(),
        pass.policy.terms.highest(),
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
    let terms = &pass.policy.terms;
    let mut beyond_horizon = 0usize;
    let settled = run_once_shared(
        &state.logic,
        rot,
        records,
        &found,
        price,
        &mut |received, settlement| {
            if matches!(settlement, Settlement::BeyondHorizon) {
                // Counted and said once per pass, not once per payment.
                beyond_horizon += 1;
                return;
            }
            if pass.quiet
                && matches!(
                    settlement,
                    Settlement::AlreadySettled(_) | Settlement::Pruned
                )
            {
                return;
            }
            for line in describe_with(received, settlement, terms) {
                println!("{line}");
            }
        },
        pass.policy,
        now_secs(),
    );
    if beyond_horizon > 0 {
        println!(
            "ignored {beyond_horizon} payment(s) with no record from before the prune cutoff \
             (settle.prune_horizon_days): too old to settle"
        );
    }
    settled?;
    // Only after every payment was processed: a failed pass reads the same
    // blocks again next time.
    since.next.save(pass.data_dir)?;
    Ok(())
}

/// Write the owed record of every payment that has one and has none stored.
/// Never fails a scan: what could not be written is printed and tried again on the
/// next pass. Without a deliverer (`air.server` and `air.host` not set) the
/// records stay in the payments' records, and `quiet` leaves out saying so.
pub fn deliver(
    deliverer: &mut Option<Deliverer>,
    records: &Mutex<Records>,
    quiet: bool,
    wall_now: u64,
) {
    match deliverer {
        Some(deliverer) => {
            // The records are locked to plan and to apply, not while the
            // records are written to the AIR server. `wall_now` (Unix seconds)
            // opens the claim window of a record that lands in this pass.
            deliverer.pass_shared(records, Instant::now(), wall_now);
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
                    "{waiting} owed record(s) are waiting to be written over AIR; air.server \
                     and air.host are not set, so they stay in the records"
                );
            }
        }
    }
}

/// Close the claim window of every owed payment whose `open_until` (its record's
/// landing plus the window) is at or before `now` and that was not claimed: the
/// mint keeps the BTC and owes nothing, and `/audit` shows it apart as expired. A
/// claim accepted before the window closed is not expired (it is finished), and a
/// payment whose owed record is not stored on AIR has no window and does not
/// expire. Expiry does not depend on the retention setting: with
/// `settle.retention_days` 0 a payment still expires on time, and the expired
/// record stays.
///
/// Then, in the same step, prune (`Records::prune`): the finished payments
/// (claimed, expired, kept, or given up on) `retention_secs`
/// (`settle.retention_days`) after they finished, and the markers older than
/// `horizon_secs` (`settle.prune_horizon_days`), and say how many of each. 0 for
/// either keeps everything it governs. Never fails a scan. A poisoned records lock
/// is logged and skipped (the watcher in `serve` exits the process).
pub fn expire(records: &Mutex<Records>, now: u64, retention_secs: u64, horizon_secs: u64) {
    let Ok(mut guard) = lock_records(records) else {
        eprintln!("expiry check skipped: the settlement records lock is poisoned");
        return;
    };
    match guard.expire_unclaimed(now) {
        Ok(gone) => {
            for txid in gone {
                println!(
                    "expired {txid}: its claim window closed with the payment unclaimed; the \
                     mint keeps the BTC"
                );
            }
        }
        Err(e) => eprintln!("expiry check failed, will retry: {e}"),
    }
    match guard.prune(now, retention_secs, horizon_secs) {
        Ok(done) => {
            if done.removed > 0 {
                println!(
                    "pruned {} finished payment(s) (claimed, expired, kept or given up on) {} \
                     day(s) ago or more from the records",
                    done.removed,
                    retention_secs / 86_400
                );
            }
            if done.markers_dropped > 0 || done.counters_dropped > 0 {
                println!(
                    "dropped {} pruned-payment marker(s) and {} claim-key counter(s) older than \
                     the {}-day prune horizon",
                    done.markers_dropped,
                    done.counters_dropped,
                    horizon_secs / 86_400
                );
            }
        }
        Err(e) => eprintln!("pruning the records failed, will retry: {e}"),
    }
}

/// The lines printed for a payment, with the numbers of the given settlement
/// rules.
pub fn describe_with(received: &Received, settlement: &Settlement, terms: &Terms) -> Vec<String> {
    let lock = terms.lock_confirmations;
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
            tier_confirmations,
            due_units,
        } => lines.push(format!(
            "  price locked at {} USD/BTC, {due_units} unit(s) owed once it has \
             {tier_confirmations} confirmations (it has {confirmations})",
            cents_text(*price_cents)
        )),
        Settlement::AlreadySettled(status) => lines.push(format!("  already settled ({status:?})")),
        Settlement::Pruned => lines.push(
            "  already settled and since pruned from the records; nothing more to do".to_string(),
        ),
        Settlement::BeyondHorizon => lines.push(
            "  too old to settle: no record, and its block is before the prune cutoff".to_string(),
        ),
        Settlement::PriceUnavailable(why) => lines.push(format!(
            "  waiting: no price this run ({why}); will retry next run"
        )),
        Settlement::Kept(Kept::NoDataOutput) => {
            lines.push("  kept, nothing owed: no single data output".to_string())
        }
        Settlement::Kept(why) => lines.push(format!("  kept, nothing owed: {why}")),
        Settlement::Owed(owed) => lines.push(format!(
            "  owed {} unit(s) to claim key {} (record {}); once the record is stored over AIR \
             the wallet can claim them, once (kind 15), within the claim window",
            owed.due_units,
            owed.claim_did.to_hex(),
            owed.index
        )),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::{PendingClaim, Status};
    use crate::rotation::{Rollovers, Rotor};
    use crate::server::SharedRotating;
    use mint_types::{Amount, BlindNonce, MintOutput};
    use secp256k1::{PublicKey, SECP256K1, SecretKey};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// $100,000 per BTC: 100 sats are exactly 1 unit ($0.10), so 500 sats are the
    /// 5 units the payments below are owed, and worth 50 cents (the base tier, 3).
    const BTC_100K: u64 = 10_000_000;
    const NOW: u64 = 1_800_000_000;

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
            let (rotor, logic) = Rotor::open(&dir, 0, 0).unwrap();
            let rollovers = Rollovers::open(&dir).unwrap();
            Mint {
                logic: Mutex::new(logic),
                rot: Rotating::new(rotor, rollovers),
                records: Mutex::new(Records::open(&dir.join(crate::records::FILE)).unwrap()),
                dir,
            }
        }

        /// One cycle under the default policy.
        fn run(
            &self,
            found: &[Received],
            price: &mut dyn FnMut() -> Result<u64, String>,
            report: &mut dyn FnMut(&Received, &Settlement),
        ) {
            run_once_shared(
                &self.logic,
                &self.rot,
                &self.records,
                found,
                price,
                report,
                &Policy::default(),
                NOW,
            )
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

    /// A payment of 500 sats to the claim key `[7; 32]`.
    fn received(txid: &str, confirmations: u32) -> Received {
        let key = SecretKey::from_slice(&[7; 32]).unwrap();
        Received {
            txid: txid.to_string(),
            confirmations,
            value_sats: 500,
            block_time: Some(NOW),
            payload: Some(
                PublicKey::from_secret_key(SECP256K1, &key)
                    .serialize()
                    .to_vec(),
            ),
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
                Ok(BTC_100K)
            },
            &mut |_, _| {},
        );
        assert_eq!(asked, 1);
        assert_eq!(seen_free, vec![true]);
        assert_eq!(m.record("aa").unwrap().locked_price_cents, BTC_100K);
    }

    #[test]
    fn the_records_are_not_locked_while_the_price_is_fetched() {
        // A claim (kind 15) takes the records lock: it must not wait for a price
        // source.
        let m = Mint::open("records-free");
        let mut seen_free = Vec::new();
        m.run(
            &[received("aa", 1)],
            &mut || {
                seen_free.push(m.records.try_lock().is_ok());
                Ok(BTC_100K)
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
            Ok(BTC_100K)
        };
        // not in a block yet: no price
        m.run(&[received("zero", 0)], &mut price, &mut |_, _| {});
        // first sight at 1 confirmation: price
        m.run(&[received("aa", 1)], &mut price, &mut |_, _| {});
        // already locked: no price, then owed: no price
        m.run(&[received("aa", 2)], &mut price, &mut |_, _| {});
        m.run(&[received("aa", 3)], &mut price, &mut |_, _| {});
        m.run(&[received("aa", 7)], &mut price, &mut |_, _| {});
        assert_eq!(asked, 1);
        assert_eq!(m.record("aa").unwrap().status, Status::Owed);
        assert!(m.record("zero").is_none());
    }

    #[test]
    fn a_reorg_after_the_owed_record_never_makes_a_second_one_and_one_before_waits() {
        let m = Mint::open("reorg");
        let mut price = || Ok(BTC_100K);
        let mut owed = 0;
        let mut count = |_: &Received, s: &Settlement| {
            if matches!(s, Settlement::Owed(_)) {
                owed += 1;
            }
        };
        // locked at 1, then a reorg takes it back to 0 before its tier: it waits
        m.run(&[received("aa", 1)], &mut price, &mut count);
        m.run(&[received("aa", 0)], &mut price, &mut count);
        assert_eq!(m.record("aa").unwrap().status, Status::Locked);
        assert_eq!(
            m.records.lock().unwrap().unsettled(),
            vec!["aa".to_string()]
        );
        // mined again and at its tier of 3
        m.run(&[received("aa", 3)], &mut price, &mut count);
        assert_eq!(m.record("aa").unwrap().status, Status::Owed);
        assert!(m.records.lock().unwrap().unsettled().is_empty());
        // a later reorg and re-mining is seen again but never owes again
        m.run(&[received("aa", 0)], &mut price, &mut count);
        m.run(&[received("aa", 6)], &mut price, &mut count);
        assert_eq!(owed, 1);
        // and nothing was signed: the notes are made when the record is claimed
        assert_eq!(m.issued_units(), 0);
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
        m.run(&found, &mut || Ok(BTC_100K), &mut |r, s| {
            reports.push((r.txid.clone(), describe_with(r, s, &terms)))
        });
        assert_eq!(
            reports[0].1[1],
            "  price locked at 100000.00 USD/BTC, 5 unit(s) owed once it has 3 confirmations (it has 1)"
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
        let plain = received("x", 6).payload.unwrap();
        // first seen with 6 confirmations, so it locks and is owed in one run
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
                Ok(BTC_100K)
            },
            &mut |_, _| {},
        );

        // only the payment sealed to this mint was priced and recorded
        assert_eq!(asked, 1);
        assert_eq!(m.record("ours").unwrap().status, Status::Owed);
        assert!(m.record("theirs").is_none());
        assert!(m.record("cleartext").is_none());
    }

    #[test]
    fn the_scan_log_names_the_claim_key_and_the_units_and_the_record_holds_the_rest() {
        let m = Mint::open("describe");
        let terms = Policy::default().terms;
        let r = received("aa", 6);
        let mut lines = Vec::new();
        m.run(
            std::slice::from_ref(&r),
            &mut || Ok(BTC_100K),
            &mut |r, s| lines = describe_with(r, s, &terms),
        );
        let claim = r.payload.as_ref().map(hex::encode).unwrap();
        assert_eq!(
            lines,
            [
                "aa confirmations=6 value_sats=500".to_string(),
                format!(
                    "  owed 5 unit(s) to claim key {claim} (record 0); once the record is stored over AIR the wallet can claim them, once (kind 15), within the claim window"
                ),
            ]
        );
    }

    #[test]
    fn every_settlement_is_described() {
        use crate::settle::{Kept, OwedNow};
        let terms = Terms::default();
        let r = received("dd", 1);
        let claim = sp_wire::ServiceDid::from_slice(r.payload.as_ref().unwrap()).unwrap();
        let owed_line = format!(
            "  owed 7 unit(s) to claim key {} (record 1); once the record is stored over AIR the wallet can claim them, once (kind 15), within the claim window",
            hex::encode(r.payload.as_ref().unwrap())
        );
        let cases: Vec<(Settlement, &str)> = vec![
            (
                Settlement::Waiting { confirmations: 0 },
                "  waiting: 0 of 1 confirmation to lock the price",
            ),
            (
                Settlement::AlreadySettled(Status::Kept),
                "  already settled (Kept)",
            ),
            (
                Settlement::Kept(Kept::NoDataOutput),
                "  kept, nothing owed: no single data output",
            ),
            (
                Settlement::Kept(Kept::NothingOwed {
                    value_units: 3,
                    costs: 4,
                }),
                "  kept, nothing owed: the payment buys 3 unit(s) at the locked price and the prepaid costs (4) leave nothing to claim",
            ),
            (
                Settlement::Locked {
                    confirmations: 2,
                    price_cents: 10_000_000,
                    tier_confirmations: 4,
                    due_units: 120_000,
                },
                "  price locked at 100000.00 USD/BTC, 120000 unit(s) owed once it has 4 confirmations (it has 2)",
            ),
            (
                Settlement::Owed(OwedNow {
                    claim_did: claim,
                    index: 1,
                    due_units: 7,
                }),
                &owed_line,
            ),
        ];
        for (settlement, line) in cases {
            assert_eq!(describe_with(&r, &settlement, &terms)[1], line);
        }
        assert_eq!(cents_text(8_167_355), "81673.55");
        assert_eq!(cents_text(5), "0.05");
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
            &mut || Ok(BTC_100K),
            &mut |_, _| reported += 1,
            &Policy::default(),
            NOW,
        )
        .unwrap_err();
        assert!(matches!(err, SettleError::RecordsPoisoned), "{err}");
        assert_eq!(reported, 0, "nothing was settled or reported");
        assert!(records.is_poisoned(), "the lock was not recovered");
    }

    /// "bb" is owed and has a claim accepted and not finished (what a crash
    /// leaves), saved in full from the real types.
    fn with_a_claim_to_recover(m: &Mint) {
        m.run(&[received("bb", 6)], &mut || Ok(BTC_100K), &mut |_, _| {});
        let mut record = m.record("bb").unwrap();
        assert_eq!(record.status, Status::Owed);
        let (epoch, keyset) = {
            let rotor = m.rot.rotor.lock().unwrap();
            (rotor.current.epoch, rotor.current.id.clone())
        };
        let blind = SecretKey::from_slice(&[9; 32]).unwrap();
        record.claim_pending = Some(PendingClaim {
            hash: "ab".repeat(32),
            epoch,
            keyset,
            outputs: vec![MintOutput {
                amount: Amount::from_units(4),
                blind_nonce: BlindNonce(PublicKey::from_secret_key(SECP256K1, &blind)),
            }],
        });
        m.records.lock().unwrap().put("bb", record).unwrap();
    }

    fn run_aa(m: &Mint, logic: &Mutex<MintLogic>, rot: &Rotating) -> Result<(), SettleError> {
        run_once_shared(
            logic,
            rot,
            &m.records,
            &[received("aa", 6)],
            &mut || Ok(BTC_100K),
            &mut |_, _| {},
            &Policy::default(),
            NOW,
        )
    }

    /// Fails if a pass takes the mint or the rotor lock without a claim to
    /// recover (the last assert), or if a pass that cannot take them settles a
    /// payment, finishes the claim, or signs anything (the first four).
    #[test]
    fn a_poisoned_mint_or_rotor_lock_fails_a_pass_with_a_claim_to_recover() {
        // the mint lock, poisoned
        let m = Mint::open("poison-mint");
        with_a_claim_to_recover(&m);
        let logic = Arc::new(Mutex::new(
            Rotor::open(&m.dir.join("other"), 0, 0).unwrap().1,
        ));
        let held = Arc::clone(&logic);
        let died = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("a thread dies holding the mint");
        })
        .join();
        assert!(died.is_err());
        match run_aa(&m, &logic, &m.rot).unwrap_err() {
            SettleError::Mint(why) => assert_eq!(why, "mint lock is poisoned"),
            other => panic!("{other}"),
        }
        assert!(m.record("aa").is_none());
        let pending = m.record("bb").unwrap();
        assert!(pending.claim_pending.is_some() && pending.claimed.is_none());
        assert_eq!(m.issued_units(), 0);

        // the rotor lock, poisoned, the same way
        let m = Mint::open("poison-rotor");
        with_a_claim_to_recover(&m);
        let rot = Arc::clone(&m.rot);
        let died = std::thread::spawn(move || {
            let _guard = rot.rotor.lock().unwrap();
            panic!("a thread dies holding the rotor");
        })
        .join();
        assert!(died.is_err());
        match run_aa(&m, &m.logic, &m.rot).unwrap_err() {
            SettleError::Mint(why) => assert_eq!(why, "rotor lock is poisoned"),
            other => panic!("{other}"),
        }
        assert!(m.record("aa").is_none());
        let pending = m.record("bb").unwrap();
        assert!(pending.claim_pending.is_some() && pending.claimed.is_none());
        assert_eq!(m.issued_units(), 0);

        // with no claim to recover the same poisoned mint lock does not matter:
        // settling an owed record signs nothing
        let m = Mint::open("poison-mint-idle");
        let logic = Arc::new(Mutex::new(
            Rotor::open(&m.dir.join("other"), 0, 0).unwrap().1,
        ));
        let held = Arc::clone(&logic);
        assert!(
            std::thread::spawn(move || {
                let _guard = held.lock().unwrap();
                panic!("a thread dies holding the mint");
            })
            .join()
            .is_err()
        );
        run_aa(&m, &logic, &m.rot).unwrap();
        assert_eq!(m.record("aa").unwrap().status, Status::Owed);
    }

    /// Fails if the window is counted from the record's creation (`NOW`) and not
    /// from the landing, if an unstored record can expire, or if the pass does not
    /// prune a payment `retention` after it expired.
    #[test]
    fn the_expiry_pass_closes_a_window_that_passed_and_only_that_then_prunes() {
        const WINDOW: u64 = 2_592_000;
        const RETENTION: u64 = 90 * 86_400;
        const HORIZON: u64 = 210 * 86_400;
        let m = Mint::open("expire");
        m.run(&[received("aa", 3)], &mut || Ok(BTC_100K), &mut |_, _| {});
        // made at NOW, not stored on AIR yet: no window, nothing to close
        assert_eq!(m.record("aa").unwrap().open_until, None);
        expire(&m.records, NOW + 100 * 86_400, RETENTION, HORIZON);
        assert!(m.record("aa").unwrap().expired_at.is_none());
        // stored five days later
        let landed = NOW + 5 * 86_400;
        m.records.lock().unwrap().land("aa", landed).unwrap();
        let until = m.record("aa").unwrap().open_until.unwrap();
        assert_eq!(until, landed + WINDOW);
        expire(&m.records, until - 1, RETENTION, HORIZON);
        assert!(m.record("aa").unwrap().expired_at.is_none());
        expire(&m.records, until, RETENTION, HORIZON);
        assert_eq!(m.record("aa").unwrap().expired_at, Some(until));
        let claims = m.records.lock().unwrap().claims();
        let shown = (
            claims.expired.payments,
            claims.expired.sats,
            claims.expired.units,
        );
        assert_eq!(shown, (1, 500, 5));
        assert_eq!(claims.unclaimed.payments, 0);
        // retention after it expired it is pruned, and the totals stay
        expire(&m.records, until + RETENTION - 1, RETENTION, HORIZON);
        assert!(m.record("aa").is_some());
        expire(&m.records, until + RETENTION, RETENTION, HORIZON);
        assert!(m.record("aa").is_none());
        assert_eq!(m.records.lock().unwrap().claims(), claims);
        // its marker stays until the payment's block (NOW) is before the cutoff
        assert!(m.records.lock().unwrap().was_pruned("aa"));
    }
}
