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
use sp_wire::air_request::delivery::Outcome;

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
    /// payment and the payer's DID until the payer answers with a new request
    /// over AIR. That AIR exchange is not built yet, so today the record just
    /// waits, for the reconciliation window (`Record::open_until`); when the window
    /// closes unsettled the payment is forfeited (`Record::forfeited_at`) and the
    /// BTC stays with the mint. See the README, "Underpayment".
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

/// The payments the mint is holding open or has forfeited, for `/audit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Reconciliation {
    /// Open, inside their window.
    pub open: Tally,
    /// Their window closed unsettled.
    pub forfeited: Tally,
}

impl Record {
    /// True while the payment is inside its reconciliation window.
    pub fn is_open(&self) -> bool {
        self.open_until.is_some() && self.forfeited_at.is_none()
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
                over_under_bps: None,
                status: Status::Issued,
                late: false,
                skipped_units: 0,
                credit_cents: 0,
                reason: Some("imported from processed_txids.json".to_string()),
                payer_did: None,
                requested_units: 0,
                value_sats: 0,
                delivery: None,
                open_until: None,
                forfeited_at: None,
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
    /// holds a credit or is paid but unsettled and has not been forfeited, as
    /// (txid, record), by txid. Both kinds stay here until the AIR exchange that
    /// settles them exists, or their window closes (`forfeit_expired`).
    pub fn owed_to(&self, payer_did: &str) -> Vec<(&str, &Record)> {
        self.rows
            .iter()
            .filter(|(_, r)| {
                r.payer_did.as_deref() == Some(payer_did)
                    && r.forfeited_at.is_none()
                    && (r.credit_cents > 0 || r.status == Status::PaidUnsettled)
            })
            .map(|(txid, r)| (txid.as_str(), r))
            .collect()
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
    /// marked, by txid. Nothing settles an open payment yet, so every one ends
    /// here; the exchange that settles one will clear `open_until` first.
    pub fn forfeit_expired(&mut self, now: u64) -> Result<Vec<String>, RecordsError> {
        let due: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, r)| r.is_open() && r.open_until.is_some_and(|until| until <= now))
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

    /// What is held open and what has been forfeited, for `/audit`.
    pub fn reconciliation(&self) -> Reconciliation {
        let mut out = Reconciliation::default();
        for record in self.rows.values() {
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
}
