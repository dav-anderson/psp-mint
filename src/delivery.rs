//! Delivering a purchase to its payer: one record per purchase, on the SPI's AIR
//! server, at an address only the payer and this mint can compute
//! (`sp_wire::air_request::delivery`; REQUEST-DESIGN.md section 18). It carries
//! the signatures, or the rejection, and the keysets to unblind with.
//!
//! **What is kept and when.** When a purchase is issued or rejected, `settle`
//! stores what the payer is owed in the purchase's record (`records::Delivery`:
//! the reply DID, the purchase id and the outcome, with `delivered` false). That
//! is the only copy of the signatures; `describe` just logs them. This module
//! writes it.
//!
//! **Retry.** [`Deliverer::pass`] runs after every scan (and so also on every
//! pass of `serve`, whether or not the scan worked) and writes every record that
//! is not stored yet. A write that fails is tried again after a wait that doubles
//! from [`FIRST_RETRY_SECS`] to [`LONGEST_RETRY_SECS`]; the waits are kept in
//! memory, so a restart tries everything at once. What is retried is stored: the
//! sealed record is made once, on the first attempt, and saved in the purchase's
//! record *before* it is written, so every retry (and every restart) writes the
//! same bytes. The AIR server answers a write to a taken cell with what it holds,
//! so a write that got through but was not acknowledged is found on the next try
//! to have stored exactly these bytes, and counts as delivered. Exactly one record
//! is ever stored per purchase: the cell is write-once.
//!
//! **Delivery never blocks or undoes issuance.** Nothing here can fail a scan: a
//! pass reports what it could not do and leaves the purchase as it was.
//!
//! **What cannot be delivered.** A reply DID that is not a key (the payload is
//! the payer's to fill in) cannot be sealed to; that delivery is marked
//! `abandoned` with the reason, and not tried again. So is one whose address holds
//! a different record, which is reported and left alone, never overwritten. A
//! payment whose payload cannot be read has no reply DID and no delivery at all.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sp_wire::ServiceDid;
use sp_wire::air_request::AirError;
use sp_wire::air_request::addr;
use sp_wire::air_request::client::{self, ServerRef, block_on_new_runtime};
use sp_wire::air_request::delivery::{self as wire, Written};
use sp_wire::seal::SecretKey;

use crate::records::{Delivery, Records};

/// The wait after the first failed write; each failure after it doubles the wait.
pub const FIRST_RETRY_SECS: u64 = 10;
/// The longest wait between two tries of one record.
pub const LONGEST_RETRY_SECS: u64 = 900;

/// The wait after the `failures`th failure in a row (1 is the first).
pub fn retry_after(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    Duration::from_secs((FIRST_RETRY_SECS << doublings).min(LONGEST_RETRY_SECS))
}

struct Wait {
    failures: u32,
    next: Instant,
}

/// What one [`Deliverer::pass`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Records now stored (written, or found stored already).
    pub delivered: usize,
    /// Records tried and not stored; they are tried again after their wait.
    pub failed: usize,
    /// Records given up on (see the module docs).
    pub abandoned: usize,
    /// Records left alone because their wait is not over.
    pub waiting: usize,
}

/// Writes the delivery records of a mint to its AIR server.
pub struct Deliverer {
    server: ServerRef,
    timeout_secs: u64,
    key: SecretKey,
    waits: BTreeMap<String, Wait>,
}

impl Deliverer {
    /// `key` is the mint's DID key, which signs every record. `timeout_secs` is
    /// how long the AIR server may take to answer one call (`air.timeout_secs`),
    /// and also how long a connection may take.
    pub fn new(server: ServerRef, timeout_secs: u64, key: SecretKey) -> Self {
        Deliverer {
            server,
            timeout_secs,
            key,
            waits: BTreeMap::new(),
        }
    }

    /// Write every record that is not stored yet and whose wait is over, at time
    /// `now`. Never fails: what went wrong is printed and counted.
    pub fn pass(&mut self, records: &mut Records, now: Instant) -> Report {
        let (jobs, mut report) = self.plan(records, now);
        if jobs.is_empty() {
            return report;
        }
        let outcomes = self.write(&jobs);
        self.apply(records, &jobs, outcomes, now, &mut report);
        report
    }

    /// [`Deliverer::pass`] for the records the AIR listener shares. The records
    /// lock is held to prepare the records (a local file) and again to mark them
    /// stored, and **not while they are written to the AIR server**, so a
    /// reconciliation request never waits for the network.
    pub fn pass_shared(&mut self, records: &Mutex<Records>, now: Instant) -> Report {
        let Some(mut guard) = lock(records) else {
            return Report::default();
        };
        let (jobs, mut report) = self.plan(&mut guard, now);
        drop(guard);
        if jobs.is_empty() {
            return report;
        }
        let outcomes = self.write(&jobs);
        // Written but not marked: the next pass finds them undelivered and
        // writes the same bytes, which the AIR server answers as already stored.
        let Some(mut guard) = lock(records) else {
            return report;
        };
        self.apply(&mut guard, &jobs, outcomes, now, &mut report);
        report
    }

    /// The records to write now, sealed and saved (so every retry writes the same
    /// bytes), with what was left alone or given up on. Touches the records file
    /// only.
    fn plan(&mut self, records: &mut Records, now: Instant) -> (Vec<Job>, Report) {
        let mut report = Report::default();
        let psp = addr::key_to_did(&self.key.public_key());
        let mut jobs: Vec<Job> = Vec::new();
        for (txid, delivery) in records.undelivered() {
            if self.waits.get(&txid).is_some_and(|w| w.next > now) {
                report.waiting += 1;
                continue;
            }
            match self.prepare(records, &psp, &txid, delivery) {
                Prepared::Ready(cell, sealed) => jobs.push((txid, cell, sealed)),
                Prepared::Abandoned => report.abandoned += 1,
                Prepared::NotStored(why) => {
                    eprintln!("delivery {txid}: not written ({why}); will retry");
                    self.fail(&txid, now);
                    report.failed += 1;
                }
            }
        }
        (jobs, report)
    }

    /// Write the records to the AIR server. Touches no records: this is the
    /// network.
    fn write(&self, jobs: &[Job]) -> Outcomes {
        let server = self.server.clone();
        let secs = self.timeout_secs;
        let to_write: Vec<Job> = jobs.to_vec();
        block_on_new_runtime(move || async move {
            let connect = tokio::time::timeout(Duration::from_secs(secs), client::connect(&server));
            let client = match connect.await {
                Ok(Ok(client)) => client,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(AirError(format!(
                        "no connection to the AIR server within {secs} s"
                    )));
                }
            };
            let mut out = Vec::with_capacity(to_write.len());
            for (txid, cell, sealed) in to_write {
                let result = wire::write_record(&client, &cell, &sealed, secs)
                    .await
                    .map_err(|e| e.0);
                out.push((txid, result));
            }
            Ok(out)
        })
        .and_then(|r| r)
        .map_err(|e| e.to_string())
    }

    /// Mark what the writes found.
    fn apply(
        &mut self,
        records: &mut Records,
        jobs: &[Job],
        outcomes: Outcomes,
        now: Instant,
        report: &mut Report,
    ) {
        match outcomes {
            Err(e) => {
                eprintln!("delivery: the AIR server could not be used ({e}); will retry");
                for (txid, _, _) in jobs {
                    self.fail(txid, now);
                    report.failed += 1;
                }
            }
            Ok(outcomes) => {
                for (txid, result) in outcomes {
                    match result {
                        Ok(Written::Created) => {
                            println!("delivery {txid}: record written");
                            self.stored(records, &txid, report);
                        }
                        Ok(Written::AlreadyStored) => {
                            println!("delivery {txid}: record was already stored");
                            self.stored(records, &txid, report);
                        }
                        Ok(Written::Different) => {
                            eprintln!(
                                "delivery {txid}: the delivery address already holds a different \
                                 record. It was not overwritten and will not be tried again"
                            );
                            self.abandon(
                                records,
                                &txid,
                                "the delivery address holds a different record",
                            );
                            report.abandoned += 1;
                        }
                        Err(why) => {
                            eprintln!("delivery {txid}: not written ({why}); will retry");
                            self.fail(&txid, now);
                            report.failed += 1;
                        }
                    }
                }
            }
        }
    }

    /// The sealed record of one purchase, made and saved if this is the first
    /// try. A delivery that cannot be made is abandoned; one that cannot be saved
    /// is not written (a restart would make other bytes).
    fn prepare(
        &self,
        records: &mut Records,
        psp: &ServiceDid,
        txid: &str,
        mut delivery: Delivery,
    ) -> Prepared {
        let reply = match ServiceDid::parse(&delivery.reply_did) {
            Ok(reply) => reply,
            Err(e) => {
                return self.give_up(records, txid, delivery, format!("the reply DID: {e}"));
            }
        };
        let purchase: [u8; 32] = match hex::decode(&delivery.purchase)
            .ok()
            .and_then(|b| b.try_into().ok())
        {
            Some(purchase) => purchase,
            None => {
                return self.give_up(
                    records,
                    txid,
                    delivery,
                    "the purchase id is not 32 bytes of hex".to_string(),
                );
            }
        };
        let cell = wire::delivery_secret(psp, &reply, &purchase);
        if let Some(text) = &delivery.sealed {
            return match hex::decode(text) {
                Ok(sealed) => Prepared::Ready(cell, sealed),
                Err(_) => self.give_up(
                    records,
                    txid,
                    delivery,
                    "the saved record is not hex".into(),
                ),
            };
        }
        let built = match wire::build_record(
            &self.key,
            &reply,
            &purchase,
            &delivery.outcome,
            addr::now_secs(),
        ) {
            Ok(built) => built,
            Err(e) => {
                return self.give_up(records, txid, delivery, format!("sealing the record: {e}"));
            }
        };
        delivery.sealed = Some(hex::encode(&built.sealed));
        match records.put_delivery(txid, delivery) {
            Ok(()) => Prepared::Ready(cell, built.sealed),
            Err(e) => Prepared::NotStored(format!("the record could not be saved: {e}")),
        }
    }

    fn give_up(
        &self,
        records: &mut Records,
        txid: &str,
        mut delivery: Delivery,
        why: String,
    ) -> Prepared {
        eprintln!("delivery {txid}: cannot be delivered ({why}); not tried again");
        delivery.abandoned = Some(why);
        if let Err(e) = records.put_delivery(txid, delivery) {
            eprintln!("delivery {txid}: that could not be saved either: {e}");
        }
        Prepared::Abandoned
    }

    fn abandon(&mut self, records: &mut Records, txid: &str, why: &str) {
        self.waits.remove(txid);
        if let Some(mut delivery) = records.get(txid).and_then(|r| r.delivery.clone()) {
            delivery.abandoned = Some(why.to_string());
            if let Err(e) = records.put_delivery(txid, delivery) {
                eprintln!("delivery {txid}: that could not be saved: {e}");
            }
        }
    }

    /// The record is stored: say so in the purchase's record. If that cannot be
    /// saved it is written again next time, and found already stored.
    fn stored(&mut self, records: &mut Records, txid: &str, report: &mut Report) {
        self.waits.remove(txid);
        if let Some(mut delivery) = records.get(txid).and_then(|r| r.delivery.clone()) {
            delivery.delivered = true;
            if let Err(e) = records.put_delivery(txid, delivery) {
                eprintln!(
                    "delivery {txid}: stored, but that could not be saved ({e}); it will be \
                     found stored on the next pass"
                );
                report.failed += 1;
                return;
            }
        }
        report.delivered += 1;
    }

    fn fail(&mut self, txid: &str, now: Instant) {
        let failures = self.waits.get(txid).map_or(0, |w| w.failures) + 1;
        self.waits.insert(
            txid.to_string(),
            Wait {
                failures,
                next: now + retry_after(failures),
            },
        );
    }
}

/// A record to write: the purchase's txid, its cell and the sealed bytes.
type Job = (String, SecretKey, Vec<u8>);

/// What writing a pass to the AIR server found, by txid; an error when the
/// server could not be used at all.
type Outcomes = Result<Vec<(String, Result<Written, String>)>, String>;

/// The records lock, or `None` (after saying so) if a thread panicked holding it.
/// A poisoned lock is not recovered: see `service::lock_records`. The pass does
/// nothing, and the watcher in `serve` exits the process.
fn lock(records: &Mutex<Records>) -> Option<MutexGuard<'_, Records>> {
    match records.lock() {
        Ok(guard) => Some(guard),
        Err(_) => {
            eprintln!("delivery: the settlement records lock is poisoned; nothing written");
            None
        }
    }
}

enum Prepared {
    Ready(SecretKey, Vec<u8>),
    Abandoned,
    NotStored(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_doubles_from_ten_seconds_to_fifteen_minutes_and_stops() {
        let secs: Vec<u64> = (1..=10).map(|n| retry_after(n).as_secs()).collect();
        assert_eq!(secs, vec![10, 20, 40, 80, 160, 320, 640, 900, 900, 900]);
        // and a long run of failures does not overflow
        assert_eq!(retry_after(u32::MAX).as_secs(), LONGEST_RETRY_SECS);
        assert_eq!(retry_after(0).as_secs(), FIRST_RETRY_SECS);
    }

    #[test]
    fn a_poisoned_records_lock_writes_nothing_and_does_not_recover() {
        let dir = std::env::temp_dir().join(format!("psp-mint-poison-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let records = std::sync::Arc::new(Mutex::new(
            Records::open(&dir.join("settlements.json")).unwrap(),
        ));
        let held = std::sync::Arc::clone(&records);
        let died = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("a thread dies holding the records");
        })
        .join();
        assert!(died.is_err());
        assert!(records.is_poisoned());
        // the server is never reached: the pass stops at the lock
        let server = ServerRef {
            name: "00".repeat(33),
            host: "127.0.0.1".to_string(),
        };
        let mut deliverer = Deliverer::new(server, 1, SecretKey::new());
        assert_eq!(
            deliverer.pass_shared(&records, Instant::now()),
            Report::default()
        );
        assert!(records.is_poisoned(), "the lock was not recovered");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
