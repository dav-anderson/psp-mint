//! One settlement record per txid, kept in a single JSON file.
//!
//! This is a placeholder store. The file is a JSON object with two members:
//! `records`, from txid to record, and `pruned`, what is left of the records
//! that were pruned (below). It is rewritten in full on every change: written to
//! a temporary file, fsynced, renamed over the old file, and the directory
//! fsynced. A missing file means no records yet. An unreadable file is an error,
//! never an empty store.
//!
//! A record is a payment: its price lock, what it is owed (`due_units`), the
//! confirmation tier it must reach, and, once it has reached it, the owed record
//! written to its claim key (`delivery`) and what became of the claim. The
//! claim window belongs to the record only from the moment the owed record is
//! stored on AIR (`landed_at`, `open_until`; [`Records::land`]). A file from
//! psp-mint 0.7.x, or from an earlier build of 0.8.0 (a bare object from txid to
//! record, or a `pruned` list of bare txids), does not read and the mint does not
//! start on it; it is not migrated (see the CHANGELOG).
//!
//! **Retention.** The file is rewritten whole on every change, so it must not
//! grow without limit. [`Records::prune`] removes, `settle.retention_days` after
//! the time that ends them, every payment that is finished: a claimed one (from
//! `claimed_at`), an expired one (`expired_at`), a kept one (`kept_at`) and one
//! whose owed record the mint gave up on (`abandoned_at`). A payment that is
//! locked, open, landed and unclaimed, not stored and not given up on, or has a
//! claim pending is never removed. What the mint still needs from a removed
//! payment stays in [`Pruned`]: its txid with its block time (so the scan can
//! never owe it again), the count of owed records it took from its claim key
//! (so an index is never reused while the key may still be read: the cell is
//! write-once), and its share of the `/audit` totals (so the totals never
//! shrink). The first two are themselves dropped once the payment is older than
//! `settle.prune_horizon_days`: [`Pruned::cutoff`] then stands in for them, and
//! a payment older than the cutoff that has no record is ignored. So the file is
//! bounded by the payments of the last horizon, plus the live ones, plus the
//! fixed-size totals.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use mint_types::MintOutput;
use serde::{Deserialize, Serialize};
use sp_wire::ServiceDid;
use sp_wire::air_request::delivery::{Outcome, record_id};
use sp_wire::psp::{CLAIM_WINDOW_SECS, ClaimResponse};

/// The file the records are kept in, in the network's data folder.
pub const FILE: &str = "settlements.json";

/// The records, shared by the scan thread (settle, deliver, expire) and the AIR
/// listener (claim). Lock order, everywhere: this, then the mint, then the
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
            RecordsError::Corrupt(e) => write!(
                f,
                "settlement records are not valid for this version: {e}. A settlements.json \
                 written by psp-mint 0.7.x, or by an earlier build of 0.8.0, is not read (see \
                 the CHANGELOG); move it aside"
            ),
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
    /// Price locked, waiting for the payment to reach its confirmation tier.
    Locked,
    /// The tier was reached and the owed record was made (`Record::delivery`):
    /// the payment is owed `due_units`, claimable once. The window opens when
    /// the record is stored on AIR and closes at `open_until`; a record that is
    /// not stored (yet) cannot be claimed and does not expire.
    Owed,
    /// The payment is kept and nothing is owed: it had no payload the mint can
    /// use, or it buys nothing after the prepaid costs. `Record::reason` says
    /// which. There is no owed record.
    Kept,
}

/// What the payer is owed over AIR for one payment, and whether it has been
/// written yet (`sp_wire::air_request::delivery`). Made when the payment reaches
/// its tier, and kept in the payment's record so that a restart, or an AIR server
/// that was down, loses nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// The payer's claim DID from the payload (66 hex characters): the one-off
    /// key the record is sealed to.
    pub claim_did: String,
    /// Which owed record of this claim DID this is, counted from 0 in the order
    /// the mint made them: it sets the record's address and its id.
    pub index: u32,
    /// What the record says: the units owed and the terms.
    pub outcome: Outcome,
    /// The sealed record (hex), made on the first attempt to write it and kept, so
    /// that a retry writes the same bytes. The AIR server answers a write to a
    /// taken cell with what it holds, and the same bytes are how a write that
    /// got through but was not acknowledged is told from another record.
    pub sealed: Option<String>,
    /// True once the record is stored on the AIR server. Set only by
    /// [`Records::land`], in the same write that opens the claim window.
    pub delivered: bool,
    /// Why the record can never be written, when that is so: its cell holds
    /// another record. Retrying would not help, so the delivery is left alone and
    /// the reason is kept here.
    pub abandoned: Option<String>,
    /// Unix seconds (the mint's clock) at which the delivery was given up on:
    /// set in the same write as `abandoned`. Retention (`settle.retention_days`)
    /// counts from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_at: Option<u64>,
}

/// One counted attempt at claiming a payment that was refused for its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// `ClaimRequest::hash()` of the refused claim.
    pub hash: String,
    /// The code it was refused with (`sp_wire::codes`).
    pub code: String,
    /// Unix seconds.
    pub at: u64,
}

/// A claim that was accepted and is being done: saved before anything is signed,
/// with everything that is signed, so a crash is finished without the claim being
/// sent again (see `crate::claim`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingClaim {
    /// `ClaimRequest::hash()`.
    pub hash: String,
    /// The current epoch when it was accepted.
    pub epoch: u64,
    /// The keyset the claim named, which was the current one when it was
    /// accepted. It is a record of what was accepted: [`crate::claim::finish`]
    /// signs with whichever keyset is current when it runs.
    pub keyset: String,
    /// The outputs to sign, in the order of the claim.
    pub outputs: Vec<MintOutput>,
}

/// The answer to the accepted claim, kept by request hash: a resend of the same
/// claim gets it again and signs nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claimed {
    pub hash: String,
    pub response: ClaimResponse,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub status: Status,
    /// USD per BTC in whole cents, taken when the payment was first seen and
    /// never refreshed.
    pub locked_price_cents: u64,
    /// True if the payment was first seen with more confirmations than
    /// `settle.lock_confirmations`, so the lock was taken later than intended.
    pub late: bool,
    /// What the payment was worth in sats.
    pub value_sats: u64,
    /// The payer's claim DID from the payload (66 hex characters), once the
    /// payload has been read. Absent on a payment with no readable payload.
    pub claim_did: Option<String>,
    /// The confirmations at which the owed record is written, picked once at the
    /// lock from the payment's value at the locked price (`settle::Terms`).
    pub tier_confirmations: u32,
    /// The whole PSP units the payment buys at the locked price.
    pub value_units: u64,
    /// The units the payment is owed: `value_units` less the prepaid costs,
    /// floored at 0. Exactly what the one claim must carry.
    pub due_units: u64,
    /// The PSP units of the two prepaid reads, taken from `value_units`
    /// (`delivery.read_fee` twice).
    pub read_prepaid_units: u64,
    /// The PSP units of the one prepaid write, taken from `value_units`
    /// (`claim.write_fee`).
    pub write_prepaid_units: u64,
    /// The sub-unit remainder the mint keeps when `value_units` rounds down, in
    /// USD cents at the locked price (0 to 9).
    pub kept_cents: u64,
    /// Why the payment is kept, when status is `kept`.
    pub reason: Option<String>,
    /// Unix seconds at which the payment was kept with nothing owed (set when it
    /// became `kept`). Retention counts from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_at: Option<u64>,
    /// Unix seconds: the time of the block the payment is in, as the scan read it
    /// (`Received::block_time`), or the time it was first settled if the node gave
    /// none. It dates the payment for the prune horizon.
    #[serde(default)]
    pub block_time: u64,
    /// The record owed to the payer over AIR, once the payment reached its tier.
    pub delivery: Option<Delivery>,
    /// Unix seconds at which the claim window closes: `landed_at` plus
    /// `sp_wire::psp::CLAIM_WINDOW_SECS`. Absent until the owed record is stored
    /// on AIR; a payment without it is not claimable and never expires.
    pub open_until: Option<u64>,
    /// Unix seconds at which the owed record was first found stored on AIR (the
    /// mint's clock): the start of the claim window. Set with `open_until` and
    /// `delivery.delivered`, in one write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_at: Option<u64>,
    /// Unix seconds at which the window closed with the payment unclaimed: the
    /// mint keeps the payment, and `/audit` shows it apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_at: Option<u64>,
    /// Claims refused for their content, counted against `max_attempts`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attempts: u32,
    /// Which claims those were.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<Refusal>,
    /// A claim accepted and not finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_pending: Option<PendingClaim>,
    /// The accepted claim's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed: Option<Claimed>,
    /// Unix seconds at which the claim was finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<u64>,
}

/// A count of payments with what they were worth, for `/audit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub payments: u64,
    /// What the payments were worth, in sats.
    pub sats: u64,
    /// The PSP units owed (`due_units`): issued for a claimed payment, still
    /// claimable for an open one, kept for an expired one. 0 for a kept payment.
    pub units: u64,
}

/// What became of the payments that are owed, and what the mint kept, for
/// `/audit`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Claimed: the notes were signed and the answer stored.
    pub claimed: Tally,
    /// Unclaimed and not expired: owed and claimable, or claimed and being
    /// finished.
    pub unclaimed: Tally,
    /// Their claim window closed unclaimed; the mint keeps the payment.
    pub expired: Tally,
    /// Kept with nothing owed: no readable payload, or nothing left after the
    /// prepaid costs. Apart from `expired`.
    pub kept: Tally,
    /// Cents kept by rounding the units owed down, over every owed payment.
    pub subunit_kept_cents: u64,
    /// PSP units of the prepaid reads, over the claimed payments. Revenue.
    pub read_prepaid_units: u64,
    /// PSP units of the prepaid write, over the claimed payments. Revenue.
    pub write_prepaid_units: u64,
    /// Claims refused for their content, counted against the cap, over every
    /// payment: so an operator can answer a support request.
    pub refused_attempts: u64,
}

impl Tally {
    fn plus(self, other: Tally) -> Tally {
        Tally {
            payments: self.payments.saturating_add(other.payments),
            sats: self.sats.saturating_add(other.sats),
            units: self.units.saturating_add(other.units),
        }
    }
}

impl Claims {
    /// Add one payment's share to the totals: the one tally it belongs to, the
    /// sub-unit cents, the prepaid costs of a claimed payment, and its refused
    /// attempts. A locked payment counts for nothing yet.
    fn add(&mut self, record: &Record) {
        self.refused_attempts = self
            .refused_attempts
            .saturating_add(u64::from(record.attempts));
        let tally = match record.status {
            Status::Locked => return,
            Status::Kept => &mut self.kept,
            Status::Owed => {
                self.subunit_kept_cents = self.subunit_kept_cents.saturating_add(record.kept_cents);
                if let Some(done) = &record.claimed {
                    self.read_prepaid_units = self
                        .read_prepaid_units
                        .saturating_add(done.response.read_prepaid_units);
                    self.write_prepaid_units = self
                        .write_prepaid_units
                        .saturating_add(done.response.write_prepaid_units);
                    &mut self.claimed
                } else if record.expired_at.is_some() {
                    &mut self.expired
                } else {
                    &mut self.unclaimed
                }
            }
        };
        tally.payments += 1;
        tally.sats = tally.sats.saturating_add(record.value_sats);
        if record.status == Status::Owed {
            tally.units = tally.units.saturating_add(record.due_units);
        }
    }

    /// These totals and `other`'s, added.
    pub fn plus(self, other: Claims) -> Claims {
        Claims {
            claimed: self.claimed.plus(other.claimed),
            unclaimed: self.unclaimed.plus(other.unclaimed),
            expired: self.expired.plus(other.expired),
            kept: self.kept.plus(other.kept),
            subunit_kept_cents: self
                .subunit_kept_cents
                .saturating_add(other.subunit_kept_cents),
            read_prepaid_units: self
                .read_prepaid_units
                .saturating_add(other.read_prepaid_units),
            write_prepaid_units: self
                .write_prepaid_units
                .saturating_add(other.write_prepaid_units),
            refused_attempts: self.refused_attempts.saturating_add(other.refused_attempts),
        }
    }
}

/// How many owed records of one claim DID were pruned, and how recent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwedCount {
    /// How many of the claim DID's owed records were pruned.
    pub count: u32,
    /// The block time (Unix seconds) of the newest payment counted.
    pub last_block_time: u64,
}

/// What the mint keeps of the payments it pruned ([`Records::prune`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pruned {
    /// The pruned payments (txid) with their block time (Unix seconds): a payment
    /// on this list is never locked or owed again, however often the scan lists
    /// it. A payment is dropped from it once its block time is before `cutoff`.
    #[serde(default)]
    pub txids: BTreeMap<String, u64>,
    /// For each claim DID (hex) that had owed records pruned, how many and how
    /// recent. Indexes are never reused while the claim key may still be read, so
    /// the next owed index of a claim DID is the number of its records still in
    /// the file plus this count. Dropped once its newest payment is before
    /// `cutoff` and the file holds no record of the claim DID.
    #[serde(default)]
    pub owed: BTreeMap<String, OwedCount>,
    /// The pruned payments' share of the `/audit` totals, as they were when
    /// pruned. A fixed-size aggregate: never dropped, so the totals never shrink.
    #[serde(default)]
    pub totals: Claims,
    /// Unix seconds, only ever moving forward: at each prune pass it becomes the
    /// larger of itself and `now` less the horizon. A payment with no record and
    /// a block time before it is ignored (it is too old to be settled), which is
    /// what lets its marker above be dropped. Raising the horizon does not move
    /// it back, so a dropped marker is never revived.
    #[serde(default)]
    pub cutoff: u64,
}

/// What one prune pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pruning {
    /// Payments removed from the file.
    pub removed: usize,
    /// Pruned txids dropped because they are older than the horizon.
    pub markers_dropped: usize,
    /// Claim DID counters dropped for the same reason.
    pub counters_dropped: usize,
}

/// The file as it is written.
#[derive(Serialize, Deserialize)]
struct Stored {
    records: BTreeMap<String, Record>,
    #[serde(default)]
    pruned: Pruned,
}

impl Record {
    /// True while the payment is owed, not claimed and not expired. It says
    /// nothing about whether the window has opened: see [`Record::is_claimable`].
    pub fn is_open(&self) -> bool {
        self.status == Status::Owed && self.claimed.is_none() && self.expired_at.is_none()
    }

    /// True while a claim for this payment can be accepted: it is open, and its
    /// owed record is stored on AIR (`landed_at`). A record the wallet cannot
    /// have read has no window, and a claim for it is `unknown_owed`.
    pub fn is_claimable(&self) -> bool {
        self.is_open() && self.landed_at.is_some()
    }

    /// The time (Unix seconds) from which retention counts, once the payment is
    /// finished: `claimed_at` for a claimed one, `expired_at` for an expired one,
    /// `kept_at` for a kept one, `abandoned_at` for one whose owed record was
    /// given up on. `None` for a payment that is not finished (locked, open,
    /// landed and unclaimed, not stored yet) or has a claim being finished, and
    /// for a finished one that carries no such time.
    pub fn finished_at(&self) -> Option<u64> {
        if self.claim_pending.is_some() {
            return None;
        }
        match self.status {
            Status::Locked => None,
            Status::Kept => self.kept_at,
            Status::Owed => match (&self.claimed, self.claimed_at, self.expired_at) {
                (Some(_), Some(at), _) | (None, _, Some(at)) => Some(at),
                (None, None, None) => self
                    .delivery
                    .as_ref()
                    .filter(|d| !d.delivered)
                    .and_then(|d| d.abandoned.as_ref().and(d.abandoned_at)),
                _ => None,
            },
        }
    }

    /// A payment whose price was just locked. `value_units`, `due_units` and the
    /// tier are filled in by `settle`.
    pub fn locked(price_cents: u64, late: bool, value_sats: u64) -> Self {
        Record {
            status: Status::Locked,
            locked_price_cents: price_cents,
            late,
            value_sats,
            claim_did: None,
            tier_confirmations: 0,
            value_units: 0,
            due_units: 0,
            read_prepaid_units: 0,
            write_prepaid_units: 0,
            kept_cents: 0,
            reason: None,
            kept_at: None,
            block_time: 0,
            delivery: None,
            open_until: None,
            landed_at: None,
            expired_at: None,
            attempts: 0,
            refusals: Vec::new(),
            claim_pending: None,
            claimed: None,
            claimed_at: None,
        }
    }
}

pub struct Records {
    path: PathBuf,
    rows: BTreeMap<String, Record>,
    pruned: Pruned,
}

impl Records {
    pub fn open(path: &Path) -> Result<Self, RecordsError> {
        let stored = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(RecordsError::Corrupt)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Stored {
                records: BTreeMap::new(),
                pruned: Pruned::default(),
            },
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path: path.to_path_buf(),
            rows: stored.records,
            pruned: stored.pruned,
        })
    }

    pub fn get(&self, txid: &str) -> Option<&Record> {
        self.rows.get(txid)
    }

    /// True if this payment was settled and then pruned
    /// ([`Records::prune`]) and its marker is still on the list. The scan may list it again (a lost checkpoint, a
    /// rescan); it must not be locked and owed a second time.
    pub fn was_pruned(&self, txid: &str) -> bool {
        self.pruned.txids.contains_key(txid)
    }

    /// True if a payment with this block time is older than the prune cutoff
    /// ([`Pruned::cutoff`]), so a payment with no record is too old to be
    /// settled. A payment with no block time (not in a block) is never.
    pub fn is_beyond_cutoff(&self, block_time: Option<u64>) -> bool {
        block_time.is_some_and(|t| t < self.pruned.cutoff)
    }

    /// The prune cutoff, Unix seconds ([`Pruned::cutoff`]).
    pub fn cutoff(&self) -> u64 {
        self.pruned.cutoff
    }

    /// The txids of payments whose price is locked and that have not reached
    /// their tier yet. The scan looks at these again every time.
    pub fn unsettled(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|(_, r)| r.status == Status::Locked)
            .map(|(txid, _)| txid.clone())
            .collect()
    }

    /// How many owed records the mint has made for this claim DID (hex): the
    /// index the next one gets. Indexes are never reused, so the records of one
    /// claim DID are 0, 1, 2 and so on without a gap. Counts the records still
    /// in the file and the ones pruned from it ([`Pruned::owed`]): a pruned
    /// record's cell is still written on AIR, and the cell is write-once.
    pub fn next_owed_index(&self, claim_did: &str) -> u32 {
        let live = self
            .rows
            .values()
            .filter(|r| r.claim_did.as_deref() == Some(claim_did) && r.delivery.is_some())
            .count();
        let live = u32::try_from(live).unwrap_or(u32::MAX);
        live.saturating_add(self.pruned.owed.get(claim_did).map_or(0, |c| c.count))
    }

    /// The txid of the payment whose owed record is `id` for this claim DID, if
    /// there is one. The lookup a claim is matched with.
    pub fn owed_for(&self, claim: &ServiceDid, id: &[u8; 32]) -> Option<String> {
        let hex = claim.to_hex();
        self.rows
            .iter()
            .find(|(_, r)| {
                r.claim_did.as_deref() == Some(hex.as_str())
                    && r.delivery
                        .as_ref()
                        .is_some_and(|d| record_id(claim, d.index) == *id)
            })
            .map(|(txid, _)| txid.clone())
    }

    /// The txids of the claims that are accepted and not finished: the mint
    /// recorded the claim and did not store the answer (a crash, or a failed
    /// write). `claim::finish` finishes each.
    pub fn pending_claims(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|(_, r)| r.claim_pending.is_some() && r.claimed.is_none())
            .map(|(txid, _)| txid.clone())
            .collect()
    }

    /// The owed records that have not been written to the AIR server yet and
    /// still can be, as (txid, delivery), by txid: not stored, not given up on
    /// (`abandoned`). A record whose window closed unclaimed is no longer written;
    /// no record that is not stored can have one, so that filter changes nothing
    /// today and is kept for safety.
    pub fn undelivered(&self) -> Vec<(String, Delivery)> {
        self.rows
            .iter()
            .filter_map(|(txid, r)| match &r.delivery {
                Some(d) if !d.delivered && d.abandoned.is_none() && r.expired_at.is_none() => {
                    Some((txid.clone(), d.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// The records to write next: of [`Records::undelivered`], for each claim DID
    /// only the one with the lowest index. A wallet reads a claim key's cells 0,
    /// 1, 2 and so on and stops at the first empty one, so a record written
    /// behind a missing one cannot be found: the later index waits for the
    /// earlier one, however long that takes (a failed write waiting out its
    /// retry holds it back too). A record given up on (`abandoned`) is not
    /// undelivered and does not hold the later ones back: a wallet reads past a
    /// cell it cannot open and stops only at an empty one.
    pub fn next_to_deliver(&self) -> Vec<(String, Delivery)> {
        let mut first: BTreeMap<String, (String, Delivery)> = BTreeMap::new();
        for (txid, delivery) in self.undelivered() {
            match first.get(&delivery.claim_did) {
                Some((_, held)) if held.index <= delivery.index => {}
                _ => {
                    first.insert(delivery.claim_did.clone(), (txid, delivery));
                }
            }
        }
        let mut next: Vec<(String, Delivery)> = first.into_values().collect();
        next.sort_by(|a, b| a.0.cmp(&b.0));
        next
    }

    /// Replace the delivery state of one payment and write the file. An error if
    /// there is no such record or it has no delivery, or if `delivery` says
    /// stored for a payment whose window has not been opened: that is
    /// [`Records::land`].
    pub fn put_delivery(&mut self, txid: &str, delivery: Delivery) -> Result<(), RecordsError> {
        let mut record = match self.rows.get(txid) {
            Some(r) if r.delivery.is_some() => r.clone(),
            _ => {
                return Err(RecordsError::Io(io::Error::other(format!(
                    "{txid} has no delivery to update"
                ))));
            }
        };
        if delivery.delivered && record.landed_at.is_none() {
            return Err(RecordsError::Io(io::Error::other(format!(
                "{txid}: a record is marked stored with `land`, which opens its claim window"
            ))));
        }
        record.delivery = Some(delivery);
        self.put(txid, record)
    }

    /// The owed record of this payment is stored on AIR (written, or found
    /// stored) at `now`, Unix seconds on the mint's clock. In one write: mark it
    /// delivered, set `landed_at` to `now` and open the claim window,
    /// `open_until = now + CLAIM_WINDOW_SECS`. Nothing happens, and no error, for
    /// a payment already landed: its window does not move. An error if there is
    /// no such record or it has no delivery.
    pub fn land(&mut self, txid: &str, now: u64) -> Result<(), RecordsError> {
        let mut record = match self.rows.get(txid) {
            Some(r) if r.delivery.is_some() => r.clone(),
            _ => {
                return Err(RecordsError::Io(io::Error::other(format!(
                    "{txid} has no delivery to land"
                ))));
            }
        };
        if record.landed_at.is_some() {
            return Ok(());
        }
        if let Some(delivery) = record.delivery.as_mut() {
            delivery.delivered = true;
        }
        record.landed_at = Some(now);
        record.open_until = Some(now.saturating_add(CLAIM_WINDOW_SECS));
        self.put(txid, record)
    }

    /// Mark expired every payment that is open, has landed and whose window
    /// closed at or before `now`: the mint keeps the payment. Returns the txids
    /// marked. A payment that was claimed is not open, one whose record is not
    /// stored on AIR has no window and is skipped, and one with a claim accepted
    /// and not finished (`claim_pending`) is skipped, so a claim accepted before
    /// the close is finished after it.
    pub fn expire_unclaimed(&mut self, now: u64) -> Result<Vec<String>, RecordsError> {
        let due: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, r)| {
                r.is_claimable()
                    && r.claim_pending.is_none()
                    && r.open_until.is_some_and(|until| until <= now)
            })
            .map(|(txid, _)| txid.clone())
            .collect();
        let mut next = self.rows.clone();
        for txid in &due {
            if let Some(record) = next.get_mut(txid) {
                record.expired_at = Some(now);
            }
        }
        if !due.is_empty() {
            self.write(&next, &self.pruned)?;
            self.rows = next;
        }
        Ok(due)
    }

    /// One prune pass at `now` (Unix seconds), in one write.
    ///
    /// `retention_secs` 0 keeps every record: the pass does nothing at all (no
    /// payment is removed, so there is no marker to cap, and the cutoff does not
    /// move). `horizon_secs` 0 keeps every marker: the cutoff never moves and
    /// nothing is dropped.
    ///
    /// Removes every finished payment `retention_secs` after the time that ended
    /// it ([`Record::finished_at`]): claimed, expired, kept, or given up on. A
    /// payment that is locked, open, landed and unclaimed, not stored and not
    /// given up on, or has a claim being finished is never removed. Each removed
    /// payment's txid (with its block time), its owed record's place in its claim
    /// DID's count and its share of the `/audit` totals go to [`Pruned`] in the
    /// same write.
    ///
    /// Then the horizon: `cutoff` becomes the larger of itself and `now` less
    /// `horizon_secs`, every pruned txid with a block time before it is dropped,
    /// and so is every claim DID counter whose newest payment is before it and
    /// that has no record left in the file. The cutoff is kept in memory at once
    /// and written with the next write of the file (at the latest when something
    /// is dropped, in the same write as the drop), so a dropped marker is always
    /// covered by a cutoff that is on disk.
    pub fn prune(
        &mut self,
        now: u64,
        retention_secs: u64,
        horizon_secs: u64,
    ) -> Result<Pruning, RecordsError> {
        if retention_secs == 0 {
            return Ok(Pruning::default());
        }
        let cutoff = if horizon_secs == 0 {
            self.pruned.cutoff
        } else {
            self.pruned.cutoff.max(now.saturating_sub(horizon_secs))
        };
        let due: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, r)| {
                r.finished_at()
                    .is_some_and(|at| at.saturating_add(retention_secs) <= now)
            })
            .map(|(txid, _)| txid.clone())
            .collect();
        let mut rows = self.rows.clone();
        let mut pruned = self.pruned.clone();
        pruned.cutoff = cutoff;
        for txid in &due {
            if let Some(record) = rows.remove(txid) {
                pruned.txids.insert(txid.clone(), record.block_time);
                if record.delivery.is_some()
                    && let Some(claim) = &record.claim_did
                {
                    let entry = pruned.owed.entry(claim.clone()).or_default();
                    entry.count = entry.count.saturating_add(1);
                    entry.last_block_time = entry.last_block_time.max(record.block_time);
                }
                let mut share = Claims::default();
                share.add(&record);
                pruned.totals = pruned.totals.plus(share);
            }
        }
        let markers = pruned.txids.len();
        if horizon_secs != 0 {
            pruned.txids.retain(|_, block_time| *block_time >= cutoff);
        }
        let markers_dropped = markers - pruned.txids.len();
        let held: BTreeSet<&str> = rows
            .values()
            .filter_map(|r| r.claim_did.as_deref())
            .collect();
        let counters = pruned.owed.len();
        if horizon_secs != 0 {
            pruned
                .owed
                .retain(|claim, c| c.last_block_time >= cutoff || held.contains(claim.as_str()));
        }
        let counters_dropped = counters - pruned.owed.len();
        let done = Pruning {
            removed: due.len(),
            markers_dropped,
            counters_dropped,
        };
        if done == Pruning::default() {
            self.pruned.cutoff = cutoff;
            return Ok(done);
        }
        self.write(&rows, &pruned)?;
        self.rows = rows;
        self.pruned = pruned;
        Ok(done)
    }

    /// What was claimed, is still claimable, expired and was kept, for `/audit`:
    /// the payments in the file and the pruned ones, so the totals never shrink.
    pub fn claims(&self) -> Claims {
        let mut out = Claims::default();
        for record in self.rows.values() {
            out.add(record);
        }
        out.plus(self.pruned.totals)
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
        self.write(&next, &self.pruned)?;
        self.rows = next;
        Ok(())
    }

    fn write(&self, rows: &BTreeMap<String, Record>, pruned: &Pruned) -> Result<(), RecordsError> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&serde_json::json!({
            "records": rows,
            "pruned": pruned,
        }))
        .map_err(io::Error::other)?;
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
    use sp_wire::air_request::delivery::{ClaimTerms, EXPIRY_NOTICE, Owed, SEND_PURCHASE_CLAIM};
    use sp_wire::psp::KeysetsResponse;

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

    const CLAIM: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    /// 2G, a second valid key.
    const OTHER: &str = "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";

    fn delivery(index: u32) -> Delivery {
        Delivery {
            claim_did: CLAIM.to_string(),
            index,
            outcome: Outcome::Owed(Owed {
                txid: "aa".repeat(32),
                paid_sats: 5_000,
                value_units: 5,
                terms: ClaimTerms {
                    send: SEND_PURCHASE_CLAIM.to_string(),
                    kind: 15,
                    due_units: 5,
                    read_prepaid_units: 0,
                    write_prepaid_units: 0,
                    window_secs: CLAIM_WINDOW_SECS,
                    max_attempts: 2,
                    on_expiry: EXPIRY_NOTICE.to_string(),
                },
            }),
            sealed: None,
            delivered: false,
            abandoned: None,
            abandoned_at: None,
        }
    }

    /// An owed payment whose record is not stored on AIR yet: 5,000 sats, 5 units
    /// owed, no window.
    fn owed(index: u32) -> Record {
        let mut r = Record::locked(1_000_000, false, 5_000);
        r.status = Status::Owed;
        r.claim_did = Some(CLAIM.to_string());
        r.tier_confirmations = 3;
        r.value_units = 5;
        r.due_units = 5;
        r.kept_cents = 4;
        r.delivery = Some(delivery(index));
        r
    }

    /// The same payment with its record stored on AIR at `landed`: the window
    /// closes `CLAIM_WINDOW_SECS` later.
    fn landed(index: u32, landed: u64) -> Record {
        let mut r = owed(index);
        r.delivery.as_mut().unwrap().delivered = true;
        r.landed_at = Some(landed);
        r.open_until = Some(landed + CLAIM_WINDOW_SECS);
        r
    }

    /// The claim window, in seconds, as a literal (30 days).
    const WINDOW: u64 = 2_592_000;
    const DAY: u64 = 86_400;
    const RETENTION: u64 = 90 * DAY;
    const HORIZON: u64 = 210 * DAY;

    /// A payment whose record landed at `landed_at` and that was claimed at
    /// `claimed_at`: the answer is stored.
    fn claimed(index: u32, landed_at: u64, claimed_at: u64) -> Record {
        let mut r = landed(index, landed_at);
        r.claimed = Some(Claimed {
            hash: "h".into(),
            response: ClaimResponse {
                keyset: "k".into(),
                signatures: Vec::new(),
                keysets: KeysetsResponse {
                    current: "k".into(),
                    previous: None,
                    keysets: BTreeMap::new(),
                },
                issued_units: 5,
                read_prepaid_units: 1,
                write_prepaid_units: 2,
            },
        });
        r.claimed_at = Some(claimed_at);
        r
    }

    /// A payment whose record landed at `landed_at` and whose window closed
    /// unclaimed at `expired_at`.
    fn expired(index: u32, landed_at: u64, expired_at: u64) -> Record {
        let mut r = landed(index, landed_at);
        r.expired_at = Some(expired_at);
        r
    }

    #[test]
    fn put_persists_and_replaces() {
        let dir = scratch("persist");
        let path = dir.join(FILE);
        let mut r = Records::open(&path).unwrap();
        r.put("bb", Record::locked(8_000_000, false, 10)).unwrap();
        let later = owed(0);
        r.put("bb", later.clone()).unwrap();
        r.put("aa", Record::locked(9, true, 1)).unwrap();
        let reopened = Records::open(&path).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.get("bb"), Some(&later));
        assert!(reopened.get("aa").unwrap().late);
        assert_eq!(reopened.unsettled(), vec!["aa".to_string()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Fails if the window is written at the record's creation instead of when it
    /// lands, or if the file loses its `records` / `pruned` members.
    #[test]
    fn the_file_has_the_documented_columns_and_no_temp_is_left() {
        let dir = scratch("format");
        let path = dir.join(FILE);
        let mut r = Records::open(&path).unwrap();
        r.put("aa", landed(0, 10)).unwrap();
        r.put("bb", owed(1)).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let row = &value["records"]["aa"];
        assert_eq!(row["status"], "owed");
        assert_eq!(row["locked_price_cents"], 1_000_000);
        assert_eq!(row["due_units"], 5);
        assert_eq!(row["landed_at"], 10);
        assert_eq!(row["open_until"], 2_592_010);
        assert_eq!(row["delivery"]["index"], 0);
        assert_eq!(row["delivery"]["delivered"], true);
        assert!(
            row.get("claimed").is_none(),
            "an unclaimed record writes no claim"
        );
        // a record that is not stored yet has no window
        let waiting = &value["records"]["bb"];
        assert!(waiting["open_until"].is_null());
        assert!(waiting.get("landed_at").is_none());
        assert_eq!(value["pruned"]["txids"], serde_json::json!({}));
        assert_eq!(value["pruned"]["owed"], serde_json::json!({}));
        assert_eq!(value["pruned"]["cutoff"], 0);
        assert_eq!(value["pruned"]["totals"]["claimed"]["payments"], 0);
        assert!(!dir.join("settlements.json.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_empty_and_a_damaged_or_old_one_is_an_error() {
        let dir = scratch("damaged");
        let path = dir.join(FILE);
        assert!(Records::open(&path).unwrap().is_empty());
        fs::write(&path, "{ not json").unwrap();
        assert!(matches!(
            Records::open(&path),
            Err(RecordsError::Corrupt(_))
        ));
        // A 0.7.x record: a status and fields this version does not have.
        fs::write(
            &path,
            r#"{"aa":{"locked_price_cents":1,"issued":true,"status":"issued","late":false}}"#,
        )
        .unwrap();
        let why = Records::open(&path).err().unwrap().to_string();
        assert!(why.contains("0.7.x"), "{why}");
        // The first build of 0.8.0 wrote a bare object from txid to record.
        let mut bare = BTreeMap::new();
        bare.insert("aa".to_string(), owed(0));
        fs::write(&path, serde_json::to_string(&bare).unwrap()).unwrap();
        let why = Records::open(&path).err().unwrap().to_string();
        assert!(why.contains("earlier build of 0.8.0"), "{why}");
        // So did the second: the pruned txids were a list, without block times.
        fs::write(&path, r#"{"records":{},"pruned":{"txids":["aa"]}}"#).unwrap();
        assert!(matches!(
            Records::open(&path),
            Err(RecordsError::Corrupt(_))
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn owed_records_of_one_claim_key_are_counted_and_found_by_id() {
        let dir = scratch("owed");
        let mut r = Records::open(&dir.join(FILE)).unwrap();
        let claim = ServiceDid::parse(CLAIM).unwrap();
        assert_eq!(r.next_owed_index(CLAIM), 0);
        r.put("aa", owed(0)).unwrap();
        r.put("bb", owed(1)).unwrap();
        // a locked payment of the same claim key has no owed record yet
        let mut waiting = Record::locked(1, false, 1);
        waiting.claim_did = Some(CLAIM.to_string());
        r.put("cc", waiting).unwrap();
        assert_eq!(r.next_owed_index(CLAIM), 2);
        assert_eq!(r.owed_for(&claim, &record_id(&claim, 0)), Some("aa".into()));
        assert_eq!(r.owed_for(&claim, &record_id(&claim, 1)), Some("bb".into()));
        assert_eq!(r.owed_for(&claim, &record_id(&claim, 2)), None);
        let other = ServiceDid::parse(OTHER).unwrap();
        assert_eq!(r.owed_for(&other, &record_id(&claim, 0)), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn undelivered_lists_the_records_not_yet_written_and_landing_opens_the_window() {
        let dir = scratch("undelivered");
        let path = dir.join(FILE);
        let mut r = Records::open(&path).unwrap();
        r.put("aa", owed(0)).unwrap();
        r.put("bb", owed(1)).unwrap();
        r.put("cc", Record::locked(1, false, 1)).unwrap();
        assert_eq!(r.undelivered().len(), 2);
        // stored: delivered, landed and the window open in one write
        r.land("aa", 1_000).unwrap();
        let aa = r.get("aa").unwrap();
        assert!(aa.delivery.as_ref().unwrap().delivered);
        assert_eq!(aa.landed_at, Some(1_000));
        assert_eq!(aa.open_until, Some(1_000 + WINDOW));
        // landing again does not move the window
        r.land("aa", 5_000).unwrap();
        assert_eq!(r.get("aa").unwrap().landed_at, Some(1_000));
        assert_eq!(r.get("aa").unwrap().open_until, Some(1_000 + WINDOW));
        let mut gone = delivery(1);
        gone.abandoned = Some("the cell holds another record".into());
        r.put_delivery("bb", gone).unwrap();
        assert!(r.undelivered().is_empty());
        // never a window for the abandoned one
        assert!(r.get("bb").unwrap().open_until.is_none());
        let reopened = Records::open(&path).unwrap();
        assert_eq!(reopened.get("aa").unwrap().open_until, Some(1_000 + WINDOW));
        // a payment without a delivery has nothing to update or land
        assert!(r.put_delivery("cc", delivery(0)).is_err());
        assert!(r.land("cc", 1).is_err());
        // and delivered is only ever set by `land`
        let mut stored = delivery(1);
        stored.delivered = true;
        assert!(r.put_delivery("bb", stored).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Fails without the per-claim-key ordering of `next_to_deliver`, or if an
    /// abandoned record holds the later ones back.
    #[test]
    fn only_the_lowest_undelivered_index_of_each_claim_key_is_next_to_deliver() {
        let dir = scratch("order");
        let mut r = Records::open(&dir.join(FILE)).unwrap();
        // txids sort the other way round from the indexes
        r.put("zz", owed(0)).unwrap();
        r.put("mm", owed(1)).unwrap();
        r.put("aa", owed(2)).unwrap();
        // another claim key is independent
        let mut other = owed(0);
        other.claim_did = Some(OTHER.to_string());
        other.delivery.as_mut().unwrap().claim_did = OTHER.to_string();
        r.put("bb", other).unwrap();
        let next: Vec<String> = r.next_to_deliver().into_iter().map(|(t, _)| t).collect();
        assert_eq!(next, vec!["bb".to_string(), "zz".to_string()]);
        // 0 is stored: 1 is next, 2 still waits
        r.land("zz", 10).unwrap();
        let next: Vec<String> = r.next_to_deliver().into_iter().map(|(t, _)| t).collect();
        assert_eq!(next, vec!["bb".to_string(), "mm".to_string()]);
        // 1 is given up on: it does not hold 2 back
        let mut gone = delivery(1);
        gone.abandoned = Some("the cell holds another record".into());
        r.put_delivery("mm", gone).unwrap();
        let next: Vec<String> = r.next_to_deliver().into_iter().map(|(t, _)| t).collect();
        assert_eq!(next, vec!["aa".to_string(), "bb".to_string()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Fails if the window starts when the record is made, or if a record that is
    /// not stored on AIR can expire.
    #[test]
    fn a_window_that_closes_unclaimed_expires_once_and_an_unstored_record_never_does() {
        let dir = scratch("expire");
        let mut r = Records::open(&dir.join(FILE)).unwrap();
        r.put("open", landed(0, 100)).unwrap();
        r.put("later", landed(1, 200)).unwrap();
        // not stored yet: no window, however late it is
        r.put("unlanded", owed(3)).unwrap();
        // a claim accepted before the close and not finished is not expired
        let mut pending = landed(2, 100);
        pending.claim_pending = Some(PendingClaim {
            hash: "h".into(),
            epoch: 0,
            keyset: "k".into(),
            outputs: Vec::new(),
        });
        r.put("pending", pending).unwrap();
        assert!(r.expire_unclaimed(100 + WINDOW - 1).unwrap().is_empty());
        assert_eq!(
            r.expire_unclaimed(100 + WINDOW).unwrap(),
            vec!["open".to_string()]
        );
        assert!(
            r.expire_unclaimed(100 + WINDOW).unwrap().is_empty(),
            "expired once"
        );
        assert_eq!(r.get("open").unwrap().expired_at, Some(100 + WINDOW));
        assert!(r.get("unlanded").unwrap().expired_at.is_none());
        // an unstored record and a pending claim are never expired
        assert_eq!(
            r.expire_unclaimed(u64::MAX).unwrap(),
            vec!["later".to_string()]
        );
        assert_eq!(r.pending_claims(), vec!["pending".to_string()]);
        let claims = r.claims();
        assert_eq!(
            claims.expired,
            Tally {
                payments: 2,
                sats: 10_000,
                units: 10
            }
        );
        assert_eq!(
            claims.unclaimed,
            Tally {
                payments: 2,
                sats: 10_000,
                units: 10
            }
        );
        assert_eq!(claims.subunit_kept_cents, 16);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `r` as the payment of the block time `t`.
    fn at_block(mut r: Record, t: u64) -> Record {
        r.block_time = t;
        r
    }

    /// `r` as a payment to the claim key `key` (hex).
    fn for_key(mut r: Record, key: &str) -> Record {
        r.claim_did = Some(key.to_string());
        if let Some(d) = r.delivery.as_mut() {
            d.claim_did = key.to_string();
        }
        r
    }

    /// A payment kept with nothing owed, kept at `at`.
    fn kept(at: u64) -> Record {
        let mut r = Record::locked(1_000_000, false, 5_000);
        r.status = Status::Kept;
        r.reason = Some("no single data output".into());
        r.kept_at = Some(at);
        r
    }

    /// An owed payment whose owed record was given up on at `at`.
    fn abandoned(index: u32, at: u64) -> Record {
        let mut r = owed(index);
        let d = r.delivery.as_mut().unwrap();
        d.abandoned = Some("the cell holds another record".into());
        d.abandoned_at = Some(at);
        r
    }

    /// The first moment a claimed or expired payment is pruned is exactly
    /// retention after it was claimed or expired. Fails if the cut is off by one,
    /// or measured from when the record landed.
    #[test]
    fn a_claimed_or_expired_payment_is_pruned_exactly_at_the_retention() {
        let dir = scratch("prune-edge");
        let mut r = Records::open(&dir.join(FILE)).unwrap();
        // landed long before: only claimed_at / expired_at count
        r.put("claimed", claimed(0, 1_000, 5_000_000)).unwrap();
        r.put("expired", expired(1, 1_000, 5_000_000)).unwrap();
        let early = r
            .prune(5_000_000 + RETENTION - 1, RETENTION, HORIZON)
            .unwrap();
        assert_eq!(early.removed, 0);
        assert_eq!(r.len(), 2);
        let on_time = r.prune(5_000_000 + RETENTION, RETENTION, HORIZON).unwrap();
        assert_eq!(on_time.removed, 2);
        assert_eq!(r.len(), 0);
        assert!(r.was_pruned("claimed") && r.was_pruned("expired"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A kept payment is pruned `kept_at` + retention, and an abandoned one
    /// `abandoned_at` + retention, to the second; the `/audit` totals are the same
    /// before and after. Fails if kept or abandoned payments are never pruned (the
    /// file grows without limit), if the cut is off by one, or if the share of
    /// either is lost.
    #[test]
    fn kept_and_abandoned_payments_are_pruned_exactly_at_the_retention() {
        let dir = scratch("prune-kept");
        let path = dir.join(FILE);
        let mut r = Records::open(&path).unwrap();
        r.put("kept", kept(5_000_000)).unwrap();
        r.put("gave-up", abandoned(0, 6_000_000)).unwrap();
        let totals = |r: &Records| {
            let c = r.claims();
            (c.kept, c.unclaimed, c.subunit_kept_cents)
        };
        let before = (
            Tally {
                payments: 1,
                sats: 5_000,
                units: 0,
            },
            Tally {
                payments: 1,
                sats: 5_000,
                units: 5,
            },
            4,
        );
        assert_eq!(totals(&r), before);
        // 90 days is 7,776,000 s: kept at 5,000,000 is due at 12,776,000
        assert_eq!(RETENTION, 7_776_000);
        let early = r.prune(12_776_000 - 1, RETENTION, HORIZON).unwrap();
        assert_eq!(early.removed, 0);
        let kept_due = r.prune(12_776_000, RETENTION, HORIZON).unwrap();
        assert_eq!(kept_due.removed, 1);
        assert!(r.get("kept").is_none() && r.was_pruned("kept"));
        assert!(r.get("gave-up").is_some());
        // abandoned at 6,000,000 is due at 13,776,000
        let early = r.prune(13_776_000 - 1, RETENTION, HORIZON).unwrap();
        assert_eq!(early.removed, 0);
        let gave_up_due = r.prune(13_776_000, RETENTION, HORIZON).unwrap();
        assert_eq!(gave_up_due.removed, 1);
        assert!(r.is_empty() && r.was_pruned("gave-up"));
        assert_eq!(totals(&r), before);
        // the abandoned record's place in its claim key's count is kept
        assert_eq!(r.next_owed_index(CLAIM), 1);
        assert_eq!(totals(&Records::open(&path).unwrap()), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Fails if prune removes a payment that is still owed anything or is not
    /// settled: locked, open, not stored, being claimed; or a finished one that
    /// carries no time to count from (it is left alone, not guessed at).
    #[test]
    fn nothing_that_is_still_live_is_pruned() {
        let dir = scratch("prune-live");
        let mut r = Records::open(&dir.join(FILE)).unwrap();
        r.put("locked", Record::locked(1_000_000, false, 5_000))
            .unwrap();
        r.put("open", landed(0, 1_000)).unwrap();
        r.put("unlanded", owed(1)).unwrap();
        let mut pending = landed(2, 1_000);
        pending.claim_pending = Some(PendingClaim {
            hash: "h".into(),
            epoch: 0,
            keyset: "k".into(),
            outputs: Vec::new(),
        });
        r.put("pending", pending).unwrap();
        // given up on and kept payments with no time
        let mut no_kept_at = kept(1_000);
        no_kept_at.kept_at = None;
        r.put("kept-no-time", no_kept_at).unwrap();
        let mut no_abandoned_at = abandoned(3, 1_000);
        no_abandoned_at.delivery.as_mut().unwrap().abandoned_at = None;
        r.put("gave-up-no-time", no_abandoned_at).unwrap();
        let mut odd = claimed(4, 1_000, 1_000);
        odd.claimed_at = None;
        r.put("odd", odd).unwrap();
        // and a delivered record is never "given up on", whatever else it says
        let mut stored = abandoned(5, 1_000);
        stored.delivery.as_mut().unwrap().delivered = true;
        r.put("stored", stored).unwrap();
        let done = r.prune(u64::MAX, RETENTION, HORIZON).unwrap();
        assert_eq!(done, Pruning::default());
        assert_eq!(r.len(), 8);
        assert!(!r.was_pruned("open"));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Fails without the aggregate (the totals shrink), the tombstones (the txid
    /// can be owed again), or the per-claim count (an index is reused).
    #[test]
    fn pruning_keeps_the_audit_totals_the_txids_and_the_owed_index() {
        let dir = scratch("prune-keep");
        let path = dir.join(FILE);
        let mut r = Records::open(&path).unwrap();
        r.put("a1", claimed(0, 1_000, 2_000)).unwrap();
        r.put("a2", expired(1, 1_000, 3_000)).unwrap();
        r.put("a3", landed(2, 4_000)).unwrap();
        let mut kept_no_time = kept(0);
        kept_no_time.kept_at = None;
        r.put("a4", kept_no_time).unwrap();
        let before = r.claims();
        assert_eq!(
            before.claimed,
            Tally {
                payments: 1,
                sats: 5_000,
                units: 5
            }
        );
        assert_eq!(before.read_prepaid_units, 1);
        assert_eq!(before.write_prepaid_units, 2);
        assert_eq!(r.next_owed_index(CLAIM), 3);
        assert_eq!(
            r.prune(3_000 + RETENTION, RETENTION, HORIZON)
                .unwrap()
                .removed,
            2
        );
        // the totals are the same literals, from the aggregate
        assert_eq!(r.claims(), before);
        assert_eq!(r.len(), 2);
        // the next index is 3, not 1
        assert_eq!(r.next_owed_index(CLAIM), 3);
        assert!(r.was_pruned("a1") && r.was_pruned("a2") && !r.was_pruned("a3"));
        // all of it survives a restart
        let reopened = Records::open(&path).unwrap();
        assert_eq!(reopened.claims(), before);
        assert_eq!(reopened.next_owed_index(CLAIM), 3);
        assert!(reopened.was_pruned("a1"));
        // and pruning the rest keeps adding to it
        let mut again = reopened;
        let now = 4_000 + WINDOW + RETENTION;
        assert_eq!(again.prune(now, RETENTION, HORIZON).unwrap().removed, 0);
        again.expire_unclaimed(4_000 + WINDOW).unwrap();
        assert_eq!(again.prune(now, RETENTION, HORIZON).unwrap().removed, 1);
        assert_eq!(again.next_owed_index(CLAIM), 3);
        assert_eq!(again.claims().expired.payments, 2);
        assert_eq!(again.claims().unclaimed.payments, 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Two claim keys, each with a payment pruned at the block time 100,000,000;
    /// the second key still has a live payment. Retention 90 days, horizon 210.
    /// Returns the records, with the pass that pruned them already run.
    fn two_keys_pruned(path: &Path) -> Records {
        let mut r = Records::open(path).unwrap();
        r.put(
            "old",
            at_block(claimed(0, 100_000_000, 100_086_400), 100_000_000),
        )
        .unwrap();
        r.put(
            "old2",
            for_key(
                at_block(claimed(0, 100_000_000, 100_086_400), 100_000_000),
                OTHER,
            ),
        )
        .unwrap();
        r.put(
            "live",
            for_key(at_block(landed(1, 100_000_000), 100_000_000), OTHER),
        )
        .unwrap();
        // claimed at 100,086,400: due at 107,862,400, cutoff 107,862,400 - 18,144,000
        let done = r.prune(107_862_400, RETENTION, HORIZON).unwrap();
        assert_eq!(
            done,
            Pruning {
                removed: 2,
                markers_dropped: 0,
                counters_dropped: 0
            }
        );
        assert_eq!(r.cutoff(), 89_718_400);
        r
    }

    /// The horizon drops a pruned payment's txid once its block time is before
    /// the cutoff (not at it), and its claim key's counter once the file holds no
    /// record of the key; a payment with a record is not touched; and raising the
    /// horizon does not bring the cutoff back. Fails without the cutoff, if the
    /// counter is dropped while the key has a record, or if a raised horizon
    /// revives a dropped marker.
    #[test]
    fn the_horizon_drops_old_markers_and_a_raised_horizon_does_not_revive_them() {
        let dir = scratch("horizon");
        let path = dir.join(FILE);
        let mut r = two_keys_pruned(&path);
        assert!(r.was_pruned("old") && r.was_pruned("old2"));
        assert_eq!(r.next_owed_index(CLAIM), 1);
        assert_eq!(r.next_owed_index(OTHER), 2);
        // 100,000,000 + 18,144,000: the cutoff is the block time, not after it
        let at = r.prune(118_144_000, RETENTION, HORIZON).unwrap();
        assert_eq!(at, Pruning::default());
        assert!(r.was_pruned("old"));
        // one second later it is before the cutoff: both txids go; the counter of
        // the key with no record goes, the other stays
        let past = r.prune(118_144_001, RETENTION, HORIZON).unwrap();
        assert_eq!(
            past,
            Pruning {
                removed: 0,
                markers_dropped: 2,
                counters_dropped: 1
            }
        );
        assert!(!r.was_pruned("old") && !r.was_pruned("old2"));
        assert_eq!(r.cutoff(), 100_000_001);
        assert_eq!(r.next_owed_index(CLAIM), 0);
        assert_eq!(r.next_owed_index(OTHER), 2);
        assert!(r.is_beyond_cutoff(Some(100_000_000)));
        assert!(!r.is_beyond_cutoff(Some(100_000_001)));
        assert!(!r.is_beyond_cutoff(None));
        // the payment with a record is untouched
        assert!(r.get("live").is_some());
        // the drop and the cutoff are on disk together
        assert_eq!(Records::open(&path).unwrap().cutoff(), 100_000_001);
        assert!(!Records::open(&path).unwrap().was_pruned("old"));
        // the horizon is raised to 400 days: the cutoff stays where it was
        let raised = r.prune(118_230_401, RETENTION, 400 * DAY).unwrap();
        assert_eq!(raised, Pruning::default());
        assert_eq!(r.cutoff(), 100_000_001);
        assert!(r.is_beyond_cutoff(Some(100_000_000)));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The cutoff moves in memory on every pass and is written with the next
    /// write, at the latest with a drop. Fails if a pass that changed nothing
    /// rewrites the file, or if the cutoff moves back.
    #[test]
    fn the_cutoff_only_moves_forward_and_a_pass_that_changes_nothing_writes_nothing() {
        let dir = scratch("cutoff");
        let path = dir.join(FILE);
        let mut r = Records::open(&path).unwrap();
        assert_eq!(r.cutoff(), 0);
        let done = r.prune(18_144_005, RETENTION, HORIZON).unwrap();
        assert_eq!(done, Pruning::default());
        assert_eq!(r.cutoff(), 5);
        // nothing was written: there is no file yet
        assert!(!path.exists());
        // an earlier `now` (a clock set back) does not move it back
        r.prune(18_144_003, RETENTION, HORIZON).unwrap();
        assert_eq!(r.cutoff(), 5);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 0 for the retention keeps every record and makes no marker, however old
    /// they are; 0 for the horizon keeps every marker and counter and never moves
    /// the cutoff. Fails if either 0 prunes or drops anything.
    #[test]
    fn a_retention_of_0_prunes_nothing_and_a_horizon_of_0_drops_nothing() {
        let dir = scratch("zero");
        let path = dir.join(FILE);
        let mut r = Records::open(&path).unwrap();
        r.put("c", claimed(0, 1_000, 2_000)).unwrap();
        r.put("k", kept(3_000)).unwrap();
        r.put("g", abandoned(1, 4_000)).unwrap();
        let none = r.prune(u64::MAX, 0, HORIZON).unwrap();
        assert_eq!(none, Pruning::default());
        assert_eq!(r.len(), 3);
        assert!(!r.was_pruned("c") && r.cutoff() == 0);
        assert_eq!(Records::open(&path).unwrap().len(), 3);
        fs::remove_dir_all(&dir).unwrap();

        let dir = scratch("zero-horizon");
        let path = dir.join(FILE);
        let mut r = two_keys_pruned(&path);
        assert_eq!(r.cutoff(), 89_718_400);
        // a very late pass with horizon 0: nothing dropped, the cutoff stays
        let late = r.prune(u64::MAX, RETENTION, 0).unwrap();
        assert_eq!(late, Pruning::default());
        assert_eq!(r.cutoff(), 89_718_400);
        assert!(r.was_pruned("old") && r.was_pruned("old2"));
        assert_eq!(r.next_owed_index(CLAIM), 1);
        assert!(!r.is_beyond_cutoff(Some(89_718_400)));
        assert!(r.is_beyond_cutoff(Some(89_718_399)));
        // and the same after a restart
        let reopened = Records::open(&path).unwrap();
        assert!(reopened.was_pruned("old2") && reopened.cutoff() == 89_718_400);
        fs::remove_dir_all(&dir).unwrap();
    }
}
