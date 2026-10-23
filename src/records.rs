//! One settlement record per txid, kept in a single JSON file.
//!
//! This is a placeholder store. The file is a JSON object from txid to record.
//! It is rewritten in full on every change: written to a temporary file,
//! fsynced, renamed over the old file, and the directory fsynced. A missing file
//! means no records yet. An unreadable file is an error, never an empty store.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sp_wire::air_request::delivery::{Outcome, ReconcileRules};
use sp_wire::psp::ReconcileResponse;

/// The records, shared by the scan thread (settle, deliver, forfeit) and the AIR
/// listener (reconcile). Lock order, everywhere: this, then the mint, then the
/// rotor. Nothing holds it across bitcoind, a price fetch or a network write.
pub type SharedRecords = std::sync::Arc<std::sync::Mutex<Records>>;

#[derive(Debug)]
pub enum RecordsError {
    Io(io::Error),
    Corrupt(serde_json::Error),
}

impl fmt::Display for RecordsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordsError::Io(e) => write!(f, "settlement records: {e}"),
            RecordsError::Corrupt(e) => {
                write!(f, "settlement records are not valid json: {e}")
            }
        }
    }
}

impl Error for RecordsError {}

impl From<io::Error> for RecordsError {
    fn from(e: io::Error) -> Self {
        RecordsError::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Price locked, waiting for the payment to reach 6 confirmations.
    Locked,
    /// Settled and at least one note was signed.
    Issued,
    /// Settled and nothing was signed.
    Rejected,
    /// Paid, but short of the request by more than the reconciliation target
    /// allows, so nothing was signed and the BTC is held. Kept under the
    /// payment and the payer's DID until the payer answers with a corrected
    /// request over AIR (kind 15, `crate::reconcile`), which settles it (the status
    /// then becomes `Issued`). Until then the record waits, for the reconciliation
    /// window (`Record::open_until`); when the window closes unsettled the payment
    /// is forfeited (`Record::forfeited_at`) and the BTC stays with the mint. See
    /// the README, "Underpayment".
    #[serde(rename = "paid_unsettled")]
    PaidUnsettled,
}

/// What the payer is owed over AIR for one purchase, and whether it has been
/// written yet (`sp_wire::air_request::delivery`). Made when the purchase is
/// issued or rejected, from what was signed, and kept in the purchase's record so
/// that a restart, or an AIR server that was down, loses nothing. The signatures
/// are not kept anywhere else: the scan log prints only counts and units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// The payer's reply DID from the payload (66 hex characters): the one-off key
    /// the record is sealed to.
    pub reply_did: String,
    /// The purchase id (64 hex characters): the hash of the unsealed payload.
    pub purchase: String,
    /// What the record says: the signatures and keysets, or the rejection.
    pub outcome: Outcome,
    /// The sealed record (hex), made on the first attempt to write it and kept, so
    /// that a retry writes the same bytes. The AIR server answers a write to a
    /// taken cell with what it holds, and the same bytes are how a write that
    /// got through but was not acknowledged is told from another record.
    pub sealed: Option<String>,
    /// True once the record is stored on the AIR server.
    pub delivered: bool,
    /// Why the record can never be written, when that is so: the reply DID in the
    /// payload is not a key, so nothing can be sealed to it. Retrying would
    /// not help, so the delivery is left alone and the reason is kept here.
    pub abandoned: Option<String>,
}

/// One counted attempt at reconciling a payment that was refused for its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// `ReconcileRequest::hash()` of the refused request.
    pub hash: String,
    /// The code it was refused with (`sp_wire::codes`).
    pub code: String,
    /// Unix seconds.
    pub at: u64,
}

/// A reconciliation that was accepted and is being done: saved before anything is
/// signed, so a crash is finished by the identical resend (see `crate::reconcile`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReconcile {
    /// `ReconcileRequest::hash()`.
    pub hash: String,
    /// The current epoch when it was accepted.
    pub epoch: u64,
    /// The keyset the request named.
    pub keyset: String,
    /// The blind nonces of the request (hex), so a record shows what is in flight.
    pub nonces: Vec<String>,
}

/// The answer to the accepted reconciliation, kept by request hash: a resend of
/// the same request gets it again and signs nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciled {
    pub hash: String,
    pub response: ReconcileResponse,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// USD per BTC in whole cents, taken when the payment was first seen and
    /// never refreshed. Every record is made with one (`Record::locked`).
    pub locked_price_cents: Option<u64>,
    /// True once at least one note was signed.
    pub issued: bool,
    /// Basis points of the requested total: positive means overpaid, negative
    /// underpaid (+1250 is 12.50% over, -80 is 0.80% under). Null when there is
    /// no valid request to compare against.
    pub over_under_bps: Option<i64>,
    pub status: Status,
    /// True if the payment was first seen with more than 1 confirmation, so the
    /// lock was taken later than intended.
    pub late: bool,
    /// Requested PSP units that were not signed because the payment was short
    /// (the smallest outputs are dropped first). For `paid_unsettled` it is the
    /// whole request.
    pub skipped_units: u64,
    /// Overpayment beyond `settle.payment_reconciliation_target`, in USD cents,
    /// owed back to the payer's DID. An overpayment within the target is kept by
    /// the mint and leaves this at 0.
    pub credit_cents: u64,
    /// Why nothing was signed, when status is rejected or paid_unsettled.
    pub reason: Option<String>,
    /// The payer's AIR DID from the payload (66 hex characters), once the
    /// payload has been read. Together with the txid it keys a credit or an
    /// unsettled payment. Absent on payments whose payload could not be read.
    pub payer_did: Option<String>,
    /// PSP units the payload asked for. 0 when there was no readable payload.
    pub requested_units: u64,
    /// What the payment was worth in sats. Kept so a new request over AIR can
    /// later be checked against what was actually paid.
    pub value_sats: u64,
    /// The record owed to the payer over AIR, when there is one: for an issued
    /// purchase, for a payment held as `paid_unsettled`, and for a rejected one
    /// whose payload could be read. Absent on a payment with no readable payload
    /// (no reply DID to address) and on a duplicate.
    pub delivery: Option<Delivery>,
    /// Unix seconds at which the reconciliation window closes, for a payment that
    /// is open: held (`paid_unsettled`), rejected with a readable reply DID, or
    /// issued with a credit owed. `None` for a payment that is closed (or was never
    /// open, such as a rejection with no reply DID, which the payer takes the loss
    /// of). Whatever settles an open payment later must clear this.
    pub open_until: Option<u64>,
    /// Unix seconds at which the window closed with the payment still open: the
    /// payment is forfeited, the mint keeps the BTC (and owes no credit), and
    /// `/audit` shows it apart.
    pub forfeited_at: Option<u64>,
    /// The rules of the reconciliation, fixed when the window opened (the same
    /// ones the first delivery carries): a request is checked against these, never
    /// against the current settings. `None` for a payment that was never open.
    /// Left out of the file when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<ReconcileRules>,
    /// The PSP units of the first delivery's read, prepaid by the PSP and taken
    /// from the payment before it was assessed (`delivery.read_fee` at the time).
    /// 0 when there is no delivery.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub read_prepaid_units: u64,
    /// For a credit: the remainder the PSP keeps when it rounds down to whole PSP
    /// units (`reconciliation.round_up_from`), in cents. Counted in `/audit` when
    /// the credit is claimed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub kept_cents: u64,
    /// For a credit: the cents the PSP gives away by rounding up.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub given_cents: u64,
    /// For a payment that carried a payload already used by another payment (one
    /// payload, two bitcoin payments): the txid of the first. It has no delivery
    /// and no window, and is never reconcilable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
    /// Requests refused for their content, counted against `rules.max_attempts`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attempts: u32,
    /// Which requests those were.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<Refusal>,
    /// A reconciliation accepted and not finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconcile_pending: Option<PendingReconcile>,
    /// The accepted reconciliation's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciled: Option<Reconciled>,
    /// Unix seconds at which a reconciliation settled the payment. `open_until` is
    /// kept, so the history shows the window the payment had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<u64>,
    /// For a credit claim: the credit (USD cents) that was claimed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub credit_claimed_cents: u64,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// A count of payments with what they were worth, for `/audit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub payments: u64,
    /// Sats of payments that got no notes (held, or rejected): all of the payment.
    pub sats: u64,
    /// USD cents of credit on payments that did get notes: the overpayment.
    pub credit_cents: u64,
}

/// The payments the mint is holding open, has forfeited or has settled, and what
/// reconciliation earned, for `/audit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Reconciliation {
    /// Open, inside their window.
    pub open: Tally,
    /// Their window closed unsettled.
    pub forfeited: Tally,
    /// Settled by a reconciliation (kind 15). `sats` is the held or rejected
    /// payments settled, `credit_cents` the credits claimed.
    pub settled: Tally,
    /// A payment that carried a payload already used by another payment. Kept at
    /// once, never open, never reconcilable; apart from `forfeited`.
    pub duplicates: Tally,
    /// PSP units taken as `reconciliation.fee` from settled payments. PSP revenue,
    /// never issued as notes.
    pub fees_units: u64,
    /// PSP units taken as `delivery.read_fee`: the first record of every purchase
    /// that has one, and the follow-up of every settled reconciliation. Revenue.
    pub read_prepaid_units: u64,
    /// PSP units taken as `reconciliation.write_fee`, once per settled payment.
    /// Revenue.
    pub write_prepaid_units: u64,
    /// Cents kept by rounding credits down, over the credits claimed.
    pub rounding_kept_cents: u64,
    /// Cents given away by rounding credits up, over the credits claimed.
    pub rounding_given_cents: u64,
    /// Requests refused for their content, counted against the cap, over every
    /// payment: so an operator can answer a support request.
    pub refused_attempts: u64,
}

impl Record {
    /// True while the payment is inside its reconciliation window and no
    /// reconciliation has settled it. `open_until` stays on a settled or
    /// forfeited record, as history.
    pub fn is_open(&self) -> bool {
        self.open_until.is_some() && self.forfeited_at.is_none() && self.settled_at.is_none()
    }
}

impl Record {
    pub fn locked(price_cents: u64, late: bool) -> Self {
        Record {
            locked_price_cents: Some(price_cents),
            issued: false,
            over_under_bps: None,
            status: Status::Locked,
            late,
            skipped_units: 0,
            credit_cents: 0,
            reason: None,
            payer_did: None,
            requested_units: 0,
            value_sats: 0,
            delivery: None,
            open_until: None,
            forfeited_at: None,
            rules: None,
            read_prepaid_units: 0,
            kept_cents: 0,
            given_cents: 0,
            duplicate_of: None,
            attempts: 0,
            refusals: Vec::new(),
            reconcile_pending: None,
            reconciled: None,
            settled_at: None,
            credit_claimed_cents: 0,
        }
    }
}

pub struct Records {
    path: PathBuf,
    rows: BTreeMap<String, Record>,
}

impl Records {
    pub fn open(path: &Path) -> Result<Self, RecordsError> {
        let rows = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(RecordsError::Corrupt)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path: path.to_path_buf(),
            rows,
        })
    }

    pub fn get(&self, txid: &str) -> Option<&Record> {
        self.rows.get(txid)
    }

    /// The txids of payments whose price is locked and that are not settled
    /// yet. The scan looks at these again every time.
    pub fn unsettled(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|(_, r)| r.status == Status::Locked && !r.issued)
            .map(|(txid, _)| txid.clone())
            .collect()
    }

    /// The txids of the payments that carry this purchase: `payer_did` is the
    /// reply key and the delivery's `purchase` is the purchase id (both hex), by
    /// txid. The lookup a reconciliation request is matched with. A duplicate payment
    /// has neither, so it never matches.
    pub fn matching(&self, reply_did: &str, purchase: &str) -> Vec<String> {
        self.rows
            .iter()
            .filter(|(_, r)| {
                r.payer_did.as_deref() == Some(reply_did)
                    && r.delivery.as_ref().is_some_and(|d| d.purchase == purchase)
            })
            .map(|(txid, _)| txid.clone())
            .collect()
    }

    /// The txid of an earlier payment of the same purchase, other than `except`:
    /// what makes a payment a duplicate.
    pub fn first_of_purchase(
        &self,
        reply_did: &str,
        purchase: &str,
        except: &str,
    ) -> Option<String> {
        self.rows
            .iter()
            .find(|(txid, r)| {
                txid.as_str() != except
                    && r.payer_did.as_deref() == Some(reply_did)
                    && r.delivery.as_ref().is_some_and(|d| d.purchase == purchase)
            })
            .map(|(txid, _)| txid.clone())
    }

    /// The purchases whose record has not been written to the AIR server yet and
    /// still can be, as (txid, delivery), by txid.
    pub fn undelivered(&self) -> Vec<(String, Delivery)> {
        self.rows
            .iter()
            .filter_map(|(txid, r)| match &r.delivery {
                Some(d) if !d.delivered && d.abandoned.is_none() => Some((txid.clone(), d.clone())),
                _ => None,
            })
            .collect()
    }

    /// Replace the delivery state of one purchase and write the file. An error if
    /// there is no such record or it has no delivery.
    pub fn put_delivery(&mut self, txid: &str, delivery: Delivery) -> Result<(), RecordsError> {
        let mut record = match self.rows.get(txid) {
            Some(r) if r.delivery.is_some() => r.clone(),
            _ => {
                return Err(RecordsError::Io(io::Error::other(format!(
                    "{txid} has no delivery to update"
                ))));
            }
        };
        record.delivery = Some(delivery);
        self.put(txid, record)
    }

    /// Mark forfeited every payment that is open and whose window closed at or
    /// before `now`: the mint keeps the BTC and owes nothing. Returns the txids
    /// marked, by txid. A payment a reconciliation settled is not open, and one
    /// with a reconciliation accepted and not finished (`reconcile_pending`) is
    /// skipped, so a request accepted before the close is finished.
    pub fn forfeit_expired(&mut self, now: u64) -> Result<Vec<String>, RecordsError> {
        let due: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, r)| {
                r.is_open()
                    && r.reconcile_pending.is_none()
                    && r.open_until.is_some_and(|until| until <= now)
            })
            .map(|(txid, _)| txid.clone())
            .collect();
        let mut next = self.rows.clone();
        for txid in &due {
            if let Some(record) = next.get_mut(txid) {
                record.forfeited_at = Some(now);
            }
        }
        if !due.is_empty() {
            self.write(&next)?;
            self.rows = next;
        }
        Ok(due)
    }

    /// What is held open, forfeited and settled, what reconciliation earned and
    /// what was a duplicate, for `/audit`.
    pub fn reconciliation(&self) -> Reconciliation {
        let mut out = Reconciliation::default();
        for record in self.rows.values() {
            out.read_prepaid_units = out
                .read_prepaid_units
                .saturating_add(record.read_prepaid_units);
            out.refused_attempts = out
                .refused_attempts
                .saturating_add(u64::from(record.attempts));
            if record.duplicate_of.is_some() {
                out.duplicates.payments += 1;
                out.duplicates.sats = out.duplicates.sats.saturating_add(record.value_sats);
                continue;
            }
            if let Some(done) = &record.reconciled {
                let r = &done.response;
                out.settled.payments += 1;
                if record.credit_claimed_cents > 0 {
                    out.settled.credit_cents = out
                        .settled
                        .credit_cents
                        .saturating_add(record.credit_claimed_cents);
                    out.rounding_kept_cents = out.rounding_kept_cents.saturating_add(r.kept_cents);
                    out.rounding_given_cents =
                        out.rounding_given_cents.saturating_add(r.given_cents);
                } else {
                    out.settled.sats = out.settled.sats.saturating_add(record.value_sats);
                }
                out.fees_units = out.fees_units.saturating_add(r.fee_units);
                out.read_prepaid_units =
                    out.read_prepaid_units.saturating_add(r.read_prepaid_units);
                out.write_prepaid_units = out
                    .write_prepaid_units
                    .saturating_add(r.write_prepaid_units);
                continue;
            }
            if record.open_until.is_none() {
                continue;
            }
            let tally = if record.forfeited_at.is_some() {
                &mut out.forfeited
            } else {
                &mut out.open
            };
            tally.payments += 1;
            if record.status == Status::Issued {
                tally.credit_cents = tally.credit_cents.saturating_add(record.credit_cents);
            } else {
                tally.sats = tally.sats.saturating_add(record.value_sats);
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Insert or replace a record and write the whole file.
    pub fn put(&mut self, txid: &str, record: Record) -> Result<(), RecordsError> {
        let mut next = self.rows.clone();
        next.insert(txid.to_string(), record);
        self.write(&next)?;
        self.rows = next;
        Ok(())
    }

    fn write(&self, rows: &BTreeMap<String, Record>) -> Result<(), RecordsError> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(rows).map_err(io::Error::other)?;
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        // Make the rename itself durable. Not every platform allows opening a
        // directory, so a failure here is ignored.
        if let Some(dir) = self.path.parent()
            && let Ok(d) = File::open(dir)
        {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psp-mint-rec-{}-{name}", {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        }));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn put_persists_and_replaces() {
        let dir = scratch("persist");
        let path = dir.join("settlements.json");
        let mut r = Records::open(&path).unwrap();
        r.put("bb", Record::locked(8_000_000, false)).unwrap();
        let mut later = Record::locked(8_000_000, false);
        later.status = Status::Issued;
        later.issued = true;
        later.over_under_bps = Some(-80);
        r.put("bb", later.clone()).unwrap();
        r.put("aa", Record::locked(9, true)).unwrap();
        let reopened = Records::open(&path).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.get("bb"), Some(&later));
        assert!(reopened.get("aa").unwrap().late);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_has_the_documented_columns_and_no_temp_is_left() {
        let dir = scratch("format");
        let path = dir.join("settlements.json");
        let mut r = Records::open(&path).unwrap();
        let mut rec = Record::locked(8_167_355, false);
        rec.over_under_bps = Some(1250);
        r.put("aa", rec).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let row = &value["aa"];
        assert_eq!(row["locked_price_cents"], 8_167_355);
        assert_eq!(row["issued"], false);
        assert_eq!(row["over_under_bps"], 1250);
        assert_eq!(row["status"], "locked");
        assert!(!dir.join("settlements.json.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn delivery(purchase: &str) -> Delivery {
        Delivery {
            reply_did: "02".to_string() + &"cc".repeat(32),
            purchase: purchase.to_string(),
            outcome: Outcome::Refused(sp_wire::air_request::delivery::Refused {
                reason: "Overflow".to_string(),
                ..Default::default()
            }),
            sealed: None,
            delivered: false,
            abandoned: None,
        }
    }

    #[test]
    fn undelivered_lists_the_records_not_yet_written_and_a_delivery_update_persists() {
        let dir = scratch("undelivered");
        let path = dir.join("settlements.json");
        let mut r = Records::open(&path).unwrap();
        let mut a = Record::locked(1, false);
        a.status = Status::Rejected;
        a.delivery = Some(delivery("p1"));
        r.put("t1", a).unwrap();
        let mut done = Record::locked(1, false);
        done.status = Status::Rejected;
        let mut d = delivery("p2");
        d.delivered = true;
        done.delivery = Some(d);
        r.put("t2", done).unwrap();
        r.put("t3", Record::locked(1, false)).unwrap();
        let listed = r.undelivered();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "t1");

        let mut sealed = delivery("p1");
        sealed.sealed = Some("abcd".to_string());
        r.put_delivery("t1", sealed.clone()).unwrap();
        let reopened = Records::open(&path).unwrap();
        assert_eq!(reopened.get("t1").unwrap().delivery, Some(sealed.clone()));
        let mut delivered = sealed;
        delivered.delivered = true;
        let mut reopened = reopened;
        reopened.put_delivery("t1", delivered).unwrap();
        assert!(reopened.undelivered().is_empty());
        // a record with no delivery cannot be given one this way, and a record
        // that does not exist cannot either: the error says so, and nothing is
        // written
        let before = fs::read_to_string(&path).unwrap();
        for txid in ["t3", "missing"] {
            match reopened.put_delivery(txid, delivery("p3")) {
                Err(RecordsError::Io(e)) => {
                    assert_eq!(e.to_string(), format!("{txid} has no delivery to update"));
                }
                other => panic!("{txid}: {other:?}"),
            }
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert!(reopened.get("t3").unwrap().delivery.is_none());
        assert!(reopened.get("missing").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn held(sats: u64, until: u64) -> Record {
        let mut r = Record::locked(1, false);
        r.status = Status::PaidUnsettled;
        r.value_sats = sats;
        r.open_until = Some(until);
        r
    }

    #[test]
    fn a_payment_whose_window_closes_unsettled_is_forfeited_once_and_shown_apart() {
        let dir = scratch("forfeit");
        let path = dir.join("settlements.json");
        let mut r = Records::open(&path).unwrap();
        r.put("held-soon", held(300, 100)).unwrap();
        r.put("held-late", held(700, 200)).unwrap();
        let mut rejected = held(500, 100);
        rejected.status = Status::Rejected;
        r.put("rejected", rejected).unwrap();
        let mut credit = Record::locked(1, false);
        credit.status = Status::Issued;
        credit.issued = true;
        credit.credit_cents = 250;
        credit.value_sats = 9_000;
        credit.open_until = Some(100);
        r.put("credit", credit).unwrap();
        // closed for good: no window
        let mut closed = Record::locked(1, false);
        closed.status = Status::Issued;
        closed.value_sats = 1_000;
        r.put("closed", closed).unwrap();

        // inside the window nothing is forfeited, and all four are open
        assert!(r.forfeit_expired(99).unwrap().is_empty());
        let open = r.reconciliation();
        assert_eq!(
            (open.open.payments, open.open.sats, open.open.credit_cents),
            (4, 300 + 700 + 500, 250)
        );
        assert_eq!(open.forfeited, Tally::default());

        // the boundary is inclusive; the later window is still open
        let gone = r.forfeit_expired(100).unwrap();
        assert_eq!(gone, vec!["credit", "held-soon", "rejected"]);
        let tally = r.reconciliation();
        assert_eq!(
            (
                tally.forfeited.payments,
                tally.forfeited.sats,
                tally.forfeited.credit_cents
            ),
            (3, 300 + 500, 250)
        );
        assert_eq!((tally.open.payments, tally.open.sats), (1, 700));
        assert!(r.get("held-late").unwrap().is_open());
        assert_eq!(r.get("held-soon").unwrap().forfeited_at, Some(100));

        // forfeited once: asking again later changes nothing, and it persists
        assert_eq!(
            r.forfeit_expired(10_000).unwrap(),
            vec!["held-late".to_string()]
        );
        assert!(r.forfeit_expired(20_000).unwrap().is_empty());
        let reopened = Records::open(&path).unwrap();
        assert_eq!(reopened.get("held-soon").unwrap().forfeited_at, Some(100));
        assert_eq!(
            reopened.get("held-late").unwrap().forfeited_at,
            Some(10_000)
        );
        assert_eq!(reopened.reconciliation().open, Tally::default());
        assert_eq!(reopened.reconciliation().forfeited.payments, 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_empty_and_a_damaged_one_is_an_error() {
        let dir = scratch("corrupt");
        // a missing file means no records yet
        let none = Records::open(&dir.join("settlements.json")).unwrap();
        assert!(none.is_empty());
        assert!(none.get("aa").is_none());

        let path = dir.join("settlements.json");
        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            Records::open(&path),
            Err(RecordsError::Corrupt(_))
        ));
        // the file is left as it was, never replaced by an empty store
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        // a file that cannot be read at all is an error too, and not "no records"
        let unreadable = dir.join("is-a-folder.json");
        fs::create_dir(&unreadable).unwrap();
        assert!(matches!(
            Records::open(&unreadable),
            Err(RecordsError::Io(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    // ---- reconciliation fields ----

    /// A held payment of `payer` and `purchase`, as the matching looks at it.
    fn of_purchase(payer: &str, purchase: &str, sats: u64, until: u64) -> Record {
        let mut r = held(sats, until);
        r.payer_did = Some(payer.to_string());
        let mut d = delivery(purchase);
        d.reply_did = payer.to_string();
        r.delivery = Some(d);
        r
    }

    #[test]
    fn matching_finds_a_purchase_by_reply_key_and_purchase_id_and_never_a_duplicate() {
        let dir = scratch("matching");
        let mut r = Records::open(&dir.join("settlements.json")).unwrap();
        r.put("a", of_purchase("02aa", "p1", 100, 50)).unwrap();
        r.put("b", of_purchase("02aa", "p2", 100, 50)).unwrap();
        r.put("c", of_purchase("02bb", "p1", 100, 50)).unwrap();
        // a duplicate has neither a payer nor a delivery
        let mut dup = Record::locked(1, false);
        dup.status = Status::Rejected;
        dup.duplicate_of = Some("a".to_string());
        r.put("d", dup).unwrap();
        assert_eq!(r.matching("02aa", "p1"), vec!["a".to_string()]);
        assert_eq!(r.matching("02bb", "p1"), vec!["c".to_string()]);
        assert!(r.matching("02aa", "p3").is_empty());
        assert_eq!(
            r.first_of_purchase("02aa", "p1", "z"),
            Some("a".to_string())
        );
        assert_eq!(r.first_of_purchase("02aa", "p1", "a"), None);
        // two payments of one purchase are both found
        r.put("e", of_purchase("02aa", "p1", 100, 50)).unwrap();
        assert_eq!(r.matching("02aa", "p1").len(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_settled_payment_is_not_open_and_not_forfeited() {
        let dir = scratch("settled");
        let mut r = Records::open(&dir.join("settlements.json")).unwrap();
        let mut done = of_purchase("02aa", "p1", 100, 50);
        done.settled_at = Some(10);
        assert!(!done.is_open());
        r.put("done", done).unwrap();
        assert!(r.forfeit_expired(1_000).unwrap().is_empty());
        assert_eq!(r.get("done").unwrap().forfeited_at, None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_payment_with_a_reconciliation_pending_is_not_forfeited() {
        let dir = scratch("pending");
        let mut r = Records::open(&dir.join("settlements.json")).unwrap();
        let mut pending = of_purchase("02aa", "p1", 100, 50);
        pending.reconcile_pending = Some(PendingReconcile {
            hash: "h".to_string(),
            epoch: 0,
            keyset: "k".to_string(),
            nonces: Vec::new(),
        });
        r.put("pending", pending).unwrap();
        r.put("plain", of_purchase("02bb", "p1", 100, 50)).unwrap();
        assert_eq!(r.forfeit_expired(50).unwrap(), vec!["plain".to_string()]);
        // it survives a reload with the pending entry
        let again = Records::open(&dir.join("settlements.json")).unwrap();
        assert!(again.get("pending").unwrap().reconcile_pending.is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_report_counts_settled_duplicates_fees_rounding_and_refused_attempts() {
        let dir = scratch("report");
        let mut r = Records::open(&dir.join("settlements.json")).unwrap();
        let response = |fee, read, write, kept, given| ReconcileResponse {
            keyset: "k".to_string(),
            signatures: Vec::new(),
            keysets: sp_wire::psp::KeysetsResponse {
                current: "k".to_string(),
                previous: None,
                keysets: Default::default(),
            },
            issued_units: 3,
            fee_units: fee,
            read_prepaid_units: read,
            write_prepaid_units: write,
            kept_cents: kept,
            given_cents: given,
        };
        // a held payment settled: 4,000 sats, a fee of 2, the follow-up's read of
        // 1 and the one write of 1 (the first read of 1 is on the record)
        let mut a = of_purchase("02aa", "p1", 4_000, 50);
        a.settled_at = Some(10);
        a.status = Status::Issued;
        a.read_prepaid_units = 1;
        a.reconciled = Some(Reconciled {
            hash: "h1".to_string(),
            response: response(2, 1, 1, 0, 0),
        });
        r.put("a", a).unwrap();
        // a credit claimed: 29 cents, 1 given
        let mut b = of_purchase("02bb", "p1", 6_000, 50);
        b.settled_at = Some(10);
        b.status = Status::Issued;
        b.issued = true;
        b.credit_cents = 29;
        b.credit_claimed_cents = 29;
        b.reconciled = Some(Reconciled {
            hash: "h2".to_string(),
            response: response(0, 0, 0, 0, 1),
        });
        r.put("b", b).unwrap();
        // one open with two refused attempts, one duplicate
        let mut c = of_purchase("02cc", "p1", 700, 5_000);
        c.attempts = 2;
        r.put("c", c).unwrap();
        let mut d = Record::locked(1, false);
        d.status = Status::Rejected;
        d.value_sats = 900;
        d.duplicate_of = Some("c".to_string());
        r.put("d", d).unwrap();

        let report = r.reconciliation();
        assert_eq!((report.settled.payments, report.settled.sats), (2, 4_000));
        assert_eq!(report.settled.credit_cents, 29);
        assert_eq!((report.open.payments, report.open.sats), (1, 700));
        assert_eq!(report.forfeited, Tally::default());
        assert_eq!(
            (report.duplicates.payments, report.duplicates.sats),
            (1, 900)
        );
        assert_eq!(report.fees_units, 2);
        assert_eq!(report.read_prepaid_units, 2);
        assert_eq!(report.write_prepaid_units, 1);
        assert_eq!(
            (report.rounding_kept_cents, report.rounding_given_cents),
            (0, 1)
        );
        assert_eq!(report.refused_attempts, 2);
        fs::remove_dir_all(&dir).unwrap();
    }
}
