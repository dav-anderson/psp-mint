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
/// are not kept anywhere else: `describe` only logs them.
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
    #[serde(default)]
    pub sealed: Option<String>,
    /// True once the record is stored on the AIR server.
    #[serde(default)]
    pub delivered: bool,
    /// Why the record can never be written, when that is so: the reply DID in the
    /// payload is not a key, so nothing can be sealed to it. Retrying would
    /// not help, so the delivery is left alone and the reason is kept here.
    #[serde(default)]
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
    /// never refreshed. Null only for records imported from the old
    /// `processed_txids.json`, which did not store one.
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
    /// unsettled payment. Absent on records made before this field existed and
    /// on payments whose payload could not be read.
    #[serde(default)]
    pub payer_did: Option<String>,
    /// PSP units the payload asked for. 0 when there was no readable payload.
    #[serde(default)]
    pub requested_units: u64,
    /// What the payment was worth in sats. Kept so a new request over AIR can
    /// later be checked against what was actually paid. 0 on older records.
    #[serde(default)]
    pub value_sats: u64,
    /// The record owed to the payer over AIR, when there is one: for an issued
    /// purchase, for a payment held as `paid_unsettled`, and for a rejected one
    /// whose payload could be read. Absent on records made before delivery existed
    /// (they were never delivered, and their signatures were only logged), and on
    /// a payment with no readable payload (no reply DID to address).
    #[serde(default)]
    pub delivery: Option<Delivery>,
    /// Unix seconds at which the reconciliation window closes, for a payment that
    /// is open: held (`paid_unsettled`), rejected with a readable reply DID, or
    /// issued with a credit owed. `None` for a payment that is closed (or was never
    /// open, such as a rejection with no reply DID, which the payer takes the loss
    /// of). Whatever settles an open payment later must clear this.
    #[serde(default)]
    pub open_until: Option<u64>,
    /// Unix seconds at which the window closed with the payment still open: the
    /// payment is forfeited, the mint keeps the BTC (and owes no credit), and
    /// `/audit` shows it apart.
    #[serde(default)]
    pub forfeited_at: Option<u64>,
    /// The rules of the reconciliation, fixed when the window opened (the same
    /// ones the first delivery carries): a request is checked against these, never
    /// against the current settings. `None` for a payment that was never open, and
    /// for a record from before reconciliation existed.
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
/// reconciliation earned, for `/audit`. Old files read as 0.
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

    /// Open the store and bring in txids from the old `processed_txids.json`
    /// (a JSON array of txid strings), if that file exists. Each becomes an
    /// issued record with no price. Existing records are never overwritten. The
    /// old file is left in place.
    pub fn open_with_legacy(path: &Path, legacy: &Path) -> Result<Self, RecordsError> {
        let mut records = Self::open(path)?;
        let old: Vec<String> = match fs::read_to_string(legacy) {
            Ok(text) => serde_json::from_str(&text).map_err(RecordsError::Corrupt)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(records),
            Err(e) => return Err(e.into()),
        };
        let mut next = records.rows.clone();
        for txid in old {
            next.entry(txid).or_insert_with(|| Record {
                locked_price_cents: None,
                issued: true,
                status: Status::Issued,
                reason: Some("imported from processed_txids.json".to_string()),
                ..Record::locked(0, false)
            });
        }
        if next.len() != records.rows.len() {
            records.write(&next)?;
            records.rows = next;
        }
        Ok(records)
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

    /// What the mint still owes one payer DID: every payment of theirs that
    /// holds a credit or is paid but unsettled and has been neither forfeited nor
    /// settled, as (txid, record), by txid. Both kinds stay here until a
    /// reconciliation settles them (kind 15) or their window closes
    /// (`forfeit_expired`).
    pub fn owed_to(&self, payer_did: &str) -> Vec<(&str, &Record)> {
        self.rows
            .iter()
            .filter(|(_, r)| {
                r.payer_did.as_deref() == Some(payer_did)
                    && r.forfeited_at.is_none()
                    && r.settled_at.is_none()
                    && (r.credit_cents > 0 || r.status == Status::PaidUnsettled)
            })
            .map(|(txid, r)| (txid.as_str(), r))
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
        if let Some(dir) = self.path.parent() {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
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
    fn missing_file_is_empty() {
        let dir = scratch("missing");
        let r = Records::open(&dir.join("settlements.json")).unwrap();
        assert!(r.is_empty());
        assert!(r.get("aa").is_none());
        fs::remove_dir_all(&dir).unwrap();
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

    #[test]
    fn a_record_from_before_the_payer_fields_still_opens() {
        let dir = scratch("old-format");
        let path = dir.join("settlements.json");
        fs::write(
            &path,
            r#"{"aa": {"locked_price_cents": 9, "issued": true, "over_under_bps": 5,
                "status": "issued", "late": false, "skipped_units": 0,
                "credit_cents": 0, "reason": null}}"#,
        )
        .unwrap();
        let r = Records::open(&path).unwrap();
        let rec = r.get("aa").unwrap();
        assert_eq!(rec.payer_did, None);
        assert_eq!(rec.requested_units, 0);
        assert_eq!(rec.value_sats, 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn owed_to_lists_credits_and_unsettled_payments_of_one_did_only() {
        let dir = scratch("owed");
        let path = dir.join("settlements.json");
        let mut r = Records::open(&path).unwrap();
        let mut credit = Record::locked(1, false);
        credit.status = Status::Issued;
        credit.issued = true;
        credit.credit_cents = 50;
        credit.payer_did = Some("aa".repeat(33));
        r.put("t1", credit).unwrap();
        let mut held = Record::locked(1, false);
        held.status = Status::PaidUnsettled;
        held.payer_did = Some("aa".repeat(33));
        r.put("t2", held).unwrap();
        let mut plain = Record::locked(1, false);
        plain.status = Status::Issued;
        plain.issued = true;
        plain.payer_did = Some("aa".repeat(33));
        r.put("t3", plain).unwrap();
        let mut other = Record::locked(1, false);
        other.status = Status::PaidUnsettled;
        other.payer_did = Some("bb".repeat(33));
        r.put("t4", other).unwrap();
        let owed: Vec<&str> = r
            .owed_to(&"aa".repeat(33))
            .into_iter()
            .map(|(txid, _)| txid)
            .collect();
        assert_eq!(owed, vec!["t1", "t2"]);
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["t2"]["status"], "paid_unsettled");
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
    fn a_record_from_before_delivery_has_none_and_is_never_listed_as_undelivered() {
        let dir = scratch("no-delivery");
        let path = dir.join("settlements.json");
        fs::write(
            &path,
            r#"{"aa": {"locked_price_cents": 9, "issued": true, "over_under_bps": 5,
                "status": "issued", "late": false, "skipped_units": 0,
                "credit_cents": 0, "reason": null, "payer_did": null,
                "requested_units": 3, "value_sats": 5000}}"#,
        )
        .unwrap();
        let r = Records::open(&path).unwrap();
        assert_eq!(r.get("aa").unwrap().delivery, None);
        assert!(r.undelivered().is_empty());
        fs::remove_dir_all(&dir).unwrap();
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
        // a record with no delivery cannot be given one this way
        assert!(reopened.put_delivery("t3", delivery("p3")).is_err());
        assert!(reopened.put_delivery("missing", delivery("p3")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_from_before_the_window_has_none_and_is_never_open_or_forfeited() {
        let dir = scratch("no-window");
        let path = dir.join("settlements.json");
        fs::write(
            &path,
            r#"{"aa": {"locked_price_cents": 9, "issued": false, "over_under_bps": -5000,
                "status": "paid_unsettled", "late": false, "skipped_units": 5,
                "credit_cents": 0, "reason": "held", "payer_did": null,
                "requested_units": 5, "value_sats": 300}}"#,
        )
        .unwrap();
        let mut r = Records::open(&path).unwrap();
        let rec = r.get("aa").unwrap();
        assert_eq!((rec.open_until, rec.forfeited_at), (None, None));
        assert!(!rec.is_open());
        // an old held payment has no window, so nothing forfeits it
        assert!(r.forfeit_expired(u64::MAX).unwrap().is_empty());
        assert_eq!(r.reconciliation(), Reconciliation::default());
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
        assert!(r.forfeit_expired(10_000).unwrap() == vec!["held-late"]);
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
    fn a_forfeited_payment_is_no_longer_owed() {
        let dir = scratch("forfeit-owed");
        let mut r = Records::open(&dir.join("settlements.json")).unwrap();
        let mut a = held(300, 100);
        a.payer_did = Some("aa".repeat(33));
        r.put("t1", a).unwrap();
        assert_eq!(r.owed_to(&"aa".repeat(33)).len(), 1);
        r.forfeit_expired(100).unwrap();
        assert!(r.owed_to(&"aa".repeat(33)).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_file_is_an_error_not_an_empty_store() {
        let dir = scratch("corrupt");
        let path = dir.join("settlements.json");
        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            Records::open(&path),
            Err(RecordsError::Corrupt(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_processed_txids_become_issued_records() {
        let dir = scratch("legacy");
        let path = dir.join("settlements.json");
        let legacy = dir.join("processed_txids.json");
        fs::write(&legacy, r#"["aa","bb"]"#).unwrap();
        let r = Records::open_with_legacy(&path, &legacy).unwrap();
        for txid in ["aa", "bb"] {
            let rec = r.get(txid).unwrap();
            assert_eq!(rec.status, Status::Issued);
            assert!(rec.issued);
            assert_eq!(rec.locked_price_cents, None);
        }
        // saved, and the old file is untouched
        assert_eq!(Records::open(&path).unwrap().len(), 2);
        assert!(legacy.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn importing_never_overwrites_an_existing_record() {
        let dir = scratch("legacy-keep");
        let path = dir.join("settlements.json");
        let legacy = dir.join("processed_txids.json");
        let mut r = Records::open(&path).unwrap();
        r.put("aa", Record::locked(5, false)).unwrap();
        fs::write(&legacy, r#"["aa","bb"]"#).unwrap();
        let r = Records::open_with_legacy(&path, &legacy).unwrap();
        assert_eq!(r.get("aa").unwrap().status, Status::Locked);
        assert_eq!(r.get("bb").unwrap().status, Status::Issued);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_legacy_file_is_fine_and_a_corrupt_one_is_an_error() {
        let dir = scratch("legacy-none");
        let path = dir.join("settlements.json");
        let legacy = dir.join("processed_txids.json");
        assert!(
            Records::open_with_legacy(&path, &legacy)
                .unwrap()
                .is_empty()
        );
        assert!(!path.exists());
        fs::write(&legacy, "nope").unwrap();
        assert!(Records::open_with_legacy(&path, &legacy).is_err());
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
    fn a_plain_record_is_written_without_the_reconciliation_fields_and_old_files_read() {
        let dir = scratch("recon-fields");
        let path = dir.join("settlements.json");
        let mut r = Records::open(&path).unwrap();
        r.put("aa", Record::locked(9, false)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        for field in [
            "rules",
            "read_prepaid_units",
            "kept_cents",
            "given_cents",
            "duplicate_of",
            "attempts",
            "refusals",
            "reconcile_pending",
            "reconciled",
            "settled_at",
            "credit_claimed_cents",
        ] {
            assert!(
                !text.contains(field),
                "{field} is written for a plain record"
            );
        }
        // a file from before reconciliation reads, with the defaults
        fs::write(
            &path,
            r#"{"aa": {"locked_price_cents": 9, "issued": false, "over_under_bps": null,
                "status": "paid_unsettled", "late": false, "skipped_units": 5,
                "credit_cents": 0, "reason": null, "payer_did": null,
                "requested_units": 5, "value_sats": 100, "open_until": 50}}"#,
        )
        .unwrap();
        let old = Records::open(&path).unwrap();
        let rec = old.get("aa").unwrap();
        assert!(rec.rules.is_none() && rec.reconciled.is_none() && rec.reconcile_pending.is_none());
        assert_eq!(
            (rec.attempts, rec.settled_at, rec.duplicate_of.clone()),
            (0, None, None)
        );
        assert!(rec.is_open());
        fs::remove_dir_all(&dir).unwrap();
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
    fn a_settled_payment_is_not_open_not_owed_and_not_forfeited() {
        let dir = scratch("settled");
        let mut r = Records::open(&dir.join("settlements.json")).unwrap();
        let mut done = of_purchase("02aa", "p1", 100, 50);
        done.settled_at = Some(10);
        assert!(!done.is_open());
        r.put("done", done).unwrap();
        assert!(r.owed_to("02aa").is_empty());
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
