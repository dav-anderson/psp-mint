//! The PSP's HTTP API, which is for this machine only, and the calls the AIR
//! listener answers.
//!
//! Wallets reach the PSP over AIR and only over AIR (`air_served_with` is what the
//! listener calls: mint info, keys, keysets, rotation, rollover, check-state,
//! spend and reconcile, which settles an open payment against the shared
//! settlement records and answers only an accepted request). What is left on HTTP is local to the SPI and the operator:
//! the ESP mint's `/melt` and `/burns`, `/audit` and `/health`. The audit and
//! the melt handlers are mint-server's, the same ones `mintd` uses, mounted
//! over the PSP's own `MintLogic` in the same process. One process owns the
//! database, so the API, the AIR listener and the payment scanner share the
//! mint behind one mutex.
//!
//! Routes (on `server.bind`, which must be a loopback address):
//!
//! - `POST /melt`: burn notes, bound to a memo that every input signs. The ESP
//!   mint uses this to burn PSP in an exchange. This is the PSP's own handler,
//!   which records the memo of each burn (see `melts`) and otherwise does what
//!   mint-server's does.
//! - `POST /burns`: for each nonce, whether it is spent and the memo it was burned
//!   under. The ESP mint uses this to tell, on a retry, a note it burned itself
//!   from a note that was burned by someone else.
//! - `GET /audit`: issued, redeemed and outstanding totals, `paid` (the PSP units
//!   burned as payments by PSP spends, kept apart from every other burn), and (when `serve`
//!   gives the router the settlement records) `reconciliation`: the payments
//!   held open and the payments forfeited, each with a count, the sats of the
//!   ones that got no notes and the USD cents of credit on the ones that did.
//! - `GET /health`: `200` while the scanner is keeping up and the mint lock is
//!   sound, `503` otherwise, with the time of the last successful scan.
//!
//! Every call that takes a body has a ceiling (`server.*_max_request_bytes`,
//! 1 MiB by default), on the HTTP routes and over AIR alike. A body over it is
//! refused with `413` before it is parsed, with `error`,
//! `code: "request_too_large"`, `size_bytes` (when the request said its size)
//! and `limit_bytes`. Nothing is burned.
//!
//! Not mounted, on purpose:
//!
//! - `/admin/issue`: the PSP issues only from confirmed BTC payments.
//! - `/swap`: PSP to FSP is out of scope, and Service Points are not exchanged peer to
//!   peer.
//!
//! There is no authentication and no TLS, which is why `server.bind` must be a
//! loopback address. A note authorizes its own spend, so a caller can only burn
//! notes they hold.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get, post};
use axum::{Json, Router};
use mint_core::MintLogic;
use mint_server::api::{self, MeltRequest, MeltResponse};
use mint_server::state::AppState;
use mint_types::{Amount, MintInput, Nonce};

use crate::melts::MeltLog;
use crate::records::{Records, SharedRecords};
use crate::rotation::{
    PspSpendRequest, RolloverError, RolloverRequest, Rollovers, Rotor, SpendError,
};

// The wire types live in sp-wire; they are re-exported so the paths are unchanged.
pub use sp_wire::psp::{Burn, BurnsRequest, BurnsResponse, KeysetsResponse, PspMeltRequest};

/// Key rotation, when the mint has it: the previous generation and the
/// rollover answers. Always locked while holding the mint lock, so they
/// cannot deadlock with each other or the burn memos.
pub struct Rotating {
    pub rotor: Mutex<Rotor>,
    pub rollovers: Mutex<Rollovers>,
}
pub type SharedRotating = Arc<Rotating>;

impl Rotating {
    pub fn new(rotor: Rotor, rollovers: Rollovers) -> SharedRotating {
        Arc::new(Rotating {
            rotor: Mutex::new(rotor),
            rollovers: Mutex::new(rollovers),
        })
    }
}

/// The burn memos, shared by the handlers. Lock order: the mint first, then this.
pub type SharedMelts = Arc<Mutex<MeltLog>>;

/// Wrap a mint for the API and the scanner to share.
pub fn app_state(logic: MintLogic) -> Arc<AppState> {
    Arc::new(AppState {
        logic: Mutex::new(logic),
        // Only `/admin/issue` reads this, and that route is not mounted. It is a
        // random value nobody is given, so even a mistaken mount would not open
        // unilateral issuance.
        admin_token: hex::encode(rand::random::<[u8; 32]>()),
    })
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// When the scanner last finished a pass, for `/health`.
///
/// A pass counts as successful when the wallet was read and every payment was
/// processed as far as it could go. A payment waiting on confirmations or on a
/// price is not a failed pass.
pub struct Health {
    started: u64,
    last_scan: AtomicU64,
    stale_after_secs: u64,
    network: Option<String>,
}

impl Health {
    /// `stale_after_secs`: how long without a successful pass before `/health`
    /// reports a failure.
    pub fn new(stale_after_secs: u64) -> Arc<Self> {
        Arc::new(Health {
            started: now_secs(),
            last_scan: AtomicU64::new(0),
            stale_after_secs,
            network: None,
        })
    }

    /// As [`Health::new`], for a mint on `network`. `/health` and `/audit` then
    /// report it as `"network"`.
    pub fn for_network(stale_after_secs: u64, network: &str) -> Arc<Self> {
        Arc::new(Health {
            started: now_secs(),
            last_scan: AtomicU64::new(0),
            stale_after_secs,
            network: Some(network.to_string()),
        })
    }

    pub fn network(&self) -> Option<&str> {
        self.network.as_deref()
    }

    pub fn record_scan(&self) {
        self.record_scan_at(now_secs());
    }

    pub fn record_scan_at(&self, unix_secs: u64) {
        self.last_scan.store(unix_secs, Ordering::Relaxed);
    }

    /// Unix time of the last successful pass, or `None` if there has not been one.
    pub fn last_scan(&self) -> Option<u64> {
        match self.last_scan.load(Ordering::Relaxed) {
            0 => None,
            t => Some(t),
        }
    }

    /// What `/health` reports, as the body of a mint-info reply over AIR
    /// (`status`, `wire_version`, `network`): the same values as
    /// [`Health::report`] at time `now`, and no status code. A mint with no
    /// network (a test mint) reports an empty one.
    pub fn mint_info(&self, lock_poisoned: bool, now: u64) -> sp_wire::air_request::MintInfo {
        let (_, body) = self.report(lock_poisoned, now);
        sp_wire::air_request::MintInfo {
            status: body["status"].as_str().unwrap_or("unknown").to_string(),
            wire_version: sp_wire::WIRE_VERSION,
            network: self.network.clone().unwrap_or_default(),
        }
    }

    /// The status code and body for `/health` at time `now`.
    pub fn report(&self, lock_poisoned: bool, now: u64) -> (StatusCode, serde_json::Value) {
        let last = self.last_scan();
        let age = last.map(|t| now.saturating_sub(t));
        let since_start = now.saturating_sub(self.started);
        let status = if lock_poisoned {
            "mint_lock_poisoned"
        } else {
            match age {
                Some(a) if a > self.stale_after_secs => "scan_stale",
                Some(_) => "ok",
                // no pass yet: give the first one the same allowance
                None if since_start > self.stale_after_secs => "scan_stale",
                None => "starting",
            }
        };
        let code = match status {
            "ok" | "starting" => StatusCode::OK,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        let mut body = serde_json::json!({
            "status": status,
            "last_scan_unix": last,
            "seconds_since_last_scan": age,
            "stale_after_secs": self.stale_after_secs,
            "wire_version": sp_wire::WIRE_VERSION,
        });
        if let Some(n) = &self.network {
            body["network"] = serde_json::Value::String(n.clone());
        }
        (code, body)
    }
}

/// Why the process should exit, if it should. A poisoned lock means a thread
/// panicked while holding the mint, so every later request would panic too. A
/// finished scan thread means payments are no longer being settled. Either way
/// the process is no good and a supervisor should start a fresh one.
pub fn fatal(state: &AppState, scan_finished: bool) -> Option<&'static str> {
    if state.logic.is_poisoned() {
        Some("the mint lock is poisoned")
    } else if scan_finished {
        Some("the scan thread has stopped")
    } else {
        None
    }
}

async fn health_handler(
    State((state, health)): State<(Arc<AppState>, Arc<Health>)>,
) -> (StatusCode, Json<serde_json::Value>) {
    // `is_poisoned` does not take the lock, so this answers while a long
    // request or scan holds the mint.
    let (code, body) = health.report(state.logic.is_poisoned(), now_secs());
    (code, Json(body))
}

/// Add `"reconciliation"` to a JSON object reply (`/audit`), when the router was
/// given the settlement records: the payments held open (inside their window) and
/// the payments forfeited (the window closed unsettled; the mint keeps the BTC),
/// kept apart from the issued, redeemed and outstanding totals. The records are
/// read when the report is asked for, so it is as current as the last write. A
/// file that cannot be read leaves the section out and says so on the log.
async fn add_reconciliation(
    State(records): State<Option<Arc<PathBuf>>>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    let Some(path) = records else {
        return response;
    };
    if !response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1 << 20).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(serde_json::Value::Object(mut map)) => {
            match Records::open(&path) {
                Ok(records) => {
                    if let Ok(section) = serde_json::to_value(records.reconciliation()) {
                        map.insert("reconciliation".into(), section);
                    }
                }
                Err(e) => eprintln!("/audit: the reconciliation section was left out: {e}"),
            }
            let text = serde_json::Value::Object(map).to_string();
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(text))
        }
        _ => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// Add `"network"` to a JSON object reply (`/audit`), when the mint has one.
async fn add_network(
    State(network): State<Option<String>>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    let Some(network) = network else {
        return response;
    };
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1 << 20).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(serde_json::Value::Object(mut map)) => {
            map.insert("network".into(), serde_json::Value::String(network));
            let text = serde_json::Value::Object(map).to_string();
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(text))
        }
        _ => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// Why a burn did not happen.
#[derive(Debug, PartialEq, Eq)]
pub enum MeltFailure {
    /// The mint refused the burn (`400`). Nothing was burned.
    Rejected(String),
    /// A fault, such as a failed write (`500`). A retry is safe.
    Internal(String),
}

/// Burn `inputs` bound to `memo`, and keep the memo of the burn.
///
/// The memo is written, and synced, for every input that is not yet spent,
/// before the mint burns anything. The caller holds the mint lock across the
/// whole call, so no other burn can come between the record and the burn.
pub fn melt_recording(
    logic: &MintLogic,
    log: &mut MeltLog,
    inputs: &[MintInput],
    memo: &str,
) -> Result<Amount, MeltFailure> {
    melt_recording_gens(logic, None, log, inputs, memo, &vec![false; inputs.len()])
}

/// [`melt_recording`] over the current generation and, when the mint rotates,
/// the previous one. A melt whose notes are all current is mint-core's
/// `melt`. One with notes of the previous set is checked whole first (each note
/// against its own set and spent list, every spend signature over the one
/// `melt_sighash`), then each note is burned in its own set.
pub fn melt_recording_gens(
    logic: &MintLogic,
    previous: Option<(
        &crate::rotation::GenKeys,
        &crate::rotation::GenKeys,
        &MintLogic,
    )>,
    log: &mut MeltLog,
    inputs: &[MintInput],
    memo: &str,
    on_previous: &[bool],
) -> Result<Amount, MeltFailure> {
    let internal = |e: String| MeltFailure::Internal(e);
    // Each input is on the generation its keyset names.
    let in_prev: Vec<bool> = on_previous.to_vec();
    let gen_of = |k: usize| {
        if in_prev[k] {
            previous.expect("checked").2
        } else {
            logic
        }
    };
    let mut unspent = Vec::with_capacity(inputs.len());
    for (k, input) in inputs.iter().enumerate() {
        let nonce = input.note.nonce;
        if !gen_of(k)
            .is_spent(nonce)
            .map_err(|e| internal(e.to_string()))?
        {
            unspent.push(nonce);
        }
    }
    if !in_prev.iter().any(|p| *p) {
        log.record(&unspent, memo)
            .map_err(|e| internal(e.to_string()))?;
        return logic
            .melt(inputs, memo.as_bytes())
            .map_err(|e| MeltFailure::Rejected(e.to_string()));
    }
    let (ckeys, pkeys, _) = previous.expect("some input is on it");
    let sighash = mint_types::melt_sighash(inputs, memo.as_bytes());
    let mut seen = std::collections::HashSet::new();
    let mut total = Amount::ZERO;
    for (k, input) in inputs.iter().enumerate() {
        let g = gen_of(k);
        g.verify_note(input)
            .map_err(|e| MeltFailure::Rejected(e.to_string()))?;
        g.verify_spend_authorization(input, sighash)
            .map_err(|e| MeltFailure::Rejected(e.to_string()))?;
        if !seen.insert(input.note.nonce) || !unspent.contains(&input.note.nonce) {
            return Err(MeltFailure::Rejected(
                mint_types::MintInputError::SpentCoin.to_string(),
            ));
        }
        total += input.amount;
    }
    log.record(&unspent, memo)
        .map_err(|e| internal(e.to_string()))?;
    for (k, input) in inputs.iter().enumerate() {
        let (g, keys) = if in_prev[k] {
            (gen_of(k), pkeys)
        } else {
            (logic, ckeys)
        };
        let sk = keys.sk.get(&input.amount).ok_or_else(|| {
            MeltFailure::Rejected(
                mint_types::MintInputError::InvalidAmountTier(input.amount).to_string(),
            )
        })?;
        g.redeem_with_key(sk, input, sighash)
            .map_err(|e| MeltFailure::Rejected(e.to_string()))?;
    }
    Ok(total)
}

/// The state of each note, for `POST /burns`. The caller holds the mint lock.
pub fn burns(logic: &MintLogic, log: &MeltLog, nonces: &[Nonce]) -> Result<Vec<Burn>, String> {
    burns_gens(logic, None, log, nonces)
}

/// Spent in the current or the previous set.
fn spent_in(logic: &MintLogic, prev: Option<&MintLogic>, nonce: Nonce) -> Result<bool, String> {
    Ok(logic.is_spent(nonce).map_err(|e| e.to_string())?
        || match prev {
            Some(p) => p.is_spent(nonce).map_err(|e| e.to_string())?,
            None => false,
        })
}

/// [`burns`] over both live generations.
pub fn burns_gens(
    logic: &MintLogic,
    prev: Option<&MintLogic>,
    log: &MeltLog,
    nonces: &[Nonce],
) -> Result<Vec<Burn>, String> {
    nonces
        .iter()
        .map(|nonce| {
            let spent = spent_in(logic, prev, *nonce)?;
            let memo = if spent {
                log.get(nonce).map(str::to_string)
            } else {
                None
            };
            Ok(Burn { spent, memo })
        })
        .collect()
}

type ApiFailure = (StatusCode, Json<serde_json::Value>);

fn failure(status: StatusCode, message: impl ToString) -> ApiFailure {
    (
        status,
        Json(serde_json::json!({ "error": message.to_string() })),
    )
}

fn coded(status: StatusCode, code: &str, message: impl ToString) -> ApiFailure {
    (
        status,
        Json(serde_json::json!({ "error": message.to_string(), "code": code })),
    )
}

type MeltState = (Arc<AppState>, SharedMelts, Option<SharedRotating>);

fn poisoned() -> ApiFailure {
    failure(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the mint lock is poisoned",
    )
}

/// Lock the rotor (after the mint) and rotate as far as the clock says.
fn rotor_of<'a>(
    rot: &'a Option<SharedRotating>,
    logic: &mut MintLogic,
) -> Result<Option<std::sync::MutexGuard<'a, Rotor>>, ApiFailure> {
    let Some(rot) = rot else { return Ok(None) };
    let mut g = rot.rotor.lock().map_err(|_| poisoned())?;
    g.refresh(logic).map_err(|e| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("key rotation failed: {e}"),
        )
    })?;
    Ok(Some(g))
}

async fn melt_handler(
    State((state, melts, rot)): State<MeltState>,
    Json(req): Json<PspMeltRequest>,
) -> Result<Json<MeltResponse>, ApiFailure> {
    let PspMeltRequest { melt, keysets } = req;
    let req = MeltRequest {
        inputs: melt.inputs,
        memo: melt.memo,
    };
    let mut logic = state.logic.lock().map_err(|_| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the mint lock is poisoned",
        )
    })?;
    let mut log = melts.lock().map_err(|_| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the burn memos lock is poisoned",
        )
    })?;
    let rotor = rotor_of(&rot, &mut logic)?;
    if keysets.len() != req.inputs.len() {
        return Err(coded(
            StatusCode::BAD_REQUEST,
            sp_wire::codes::MISSING_KEYSET,
            "every input needs its keyset (keysets, in order)",
        ));
    }
    let mut on_previous = Vec::with_capacity(keysets.len());
    for (i, k) in keysets.iter().enumerate() {
        let side = match &rotor {
            Some(r) => r.resolve(k).map_err(|e| {
                coded(
                    StatusCode::BAD_REQUEST,
                    e.code(),
                    e.why(&format!("input {i}")),
                )
            })?,
            None if *k == crate::rotation::keyset_id(logic.public_keys()) => {
                crate::rotation::Side::Current
            }
            None => {
                let e = crate::rotation::KeysetError::Unknown(k.clone());
                return Err(coded(
                    StatusCode::BAD_REQUEST,
                    e.code(),
                    e.why(&format!("input {i}")),
                ));
            }
        };
        on_previous.push(side == crate::rotation::Side::Previous);
    }
    // A note a pending rollover holds is that rollover's to burn.
    if let Some(r) = &rot {
        let held = r.rollovers.lock().map_err(|_| poisoned())?.held_nonces("");
        if let Some(i) = req
            .inputs
            .iter()
            .position(|n| held.contains(&hex::encode(n.note.nonce.0.serialize())))
        {
            return Err(coded(
                StatusCode::BAD_REQUEST,
                "note_held",
                format!("input {i} is held by an unfinished rollover. Nothing was burned"),
            ));
        }
    }
    let prev = rotor
        .as_ref()
        .and_then(|r| r.previous.as_ref().map(|(k, l)| (&r.current, k, l)));
    match melt_recording_gens(&logic, prev, &mut log, &req.inputs, &req.memo, &on_previous) {
        Ok(burned) => Ok(Json(MeltResponse { burned })),
        Err(MeltFailure::Rejected(e)) => Err(failure(StatusCode::BAD_REQUEST, e)),
        Err(MeltFailure::Internal(e)) => Err(failure(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

async fn burns_handler(
    State((state, melts, rot)): State<MeltState>,
    Json(req): Json<BurnsRequest>,
) -> Result<Json<BurnsResponse>, ApiFailure> {
    let mut logic = state.logic.lock().map_err(|_| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the mint lock is poisoned",
        )
    })?;
    let log = melts.lock().map_err(|_| {
        failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the burn memos lock is poisoned",
        )
    })?;
    let rotor = rotor_of(&rot, &mut logic)?;
    let prev = rotor
        .as_ref()
        .and_then(|r| r.previous.as_ref().map(|(_, l)| l));
    burns_gens(&logic, prev, &log, &req.nonces)
        .map(|burns| Json(BurnsResponse { burns }))
        .map_err(|e| failure(StatusCode::INTERNAL_SERVER_ERROR, e))
}

/// The ceilings on request bodies, one for each route that takes a body
/// (`server.*_max_request_bytes` in the config), and the most notes in one
/// rollover (`rollover.max_items`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub melt_bytes: usize,
    pub burns_bytes: usize,
    pub rollover_bytes: usize,
    pub rollover_notes: usize,
    pub check_state_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        let bytes = crate::config::DEFAULT_MAX_REQUEST_BYTES;
        Limits {
            melt_bytes: bytes,
            burns_bytes: bytes,
            rollover_bytes: bytes,
            rollover_notes: sp_wire::psp::MAX_ROLLOVER_NOTES,
            check_state_bytes: bytes,
        }
    }
}

/// The body of a `413`. Like every other error here it has `error`; it also has
/// a `code` and the sizes, as the ESP mint's `413` does.
fn too_large(size: Option<u64>, limit: usize) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(too_large_body(size, limit)),
    )
        .into_response()
}

/// The JSON of a `413`, shared by the HTTP route and the AIR listener (where it
/// is the body of a signed refusal), so that the two say the same thing.
fn too_large_body(size: Option<u64>, limit: usize) -> serde_json::Value {
    let said = match size {
        Some(size) => format!("the request is {size} bytes"),
        None => "the request is larger than the ceiling".to_string(),
    };
    serde_json::json!({
        "error": format!(
            "{said}, and this route accepts at most {limit} bytes. Nothing was parsed or burned"
        ),
        "code": sp_wire::codes::REQUEST_TOO_LARGE,
        "size_bytes": size,
        "limit_bytes": limit as u64,
    })
}

/// Read a request body, but only up to `limit` bytes, before the handler parses
/// it. A body that says it is too large is refused without reading any of it,
/// and one that does not say is read only as far as the limit.
async fn limit_body(State(limit): State<usize>, request: Request, next: Next) -> Response {
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > limit as u64) {
        return too_large(declared, limit);
    }
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => bytes,
        Err(e) if e.to_string().contains("length limit") => return too_large(None, limit),
        Err(e) => {
            return failure(
                StatusCode::BAD_REQUEST,
                format!("the body could not be read: {e}"),
            )
            .into_response();
        }
    };
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// A route with its body ceiling in front of it. The ceiling also replaces
/// axum's own default (2 MB) for the extractor behind it.
fn limited<S: Clone + Send + Sync + 'static>(
    route: MethodRouter<S>,
    limit: usize,
) -> MethodRouter<S> {
    route
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::from_fn_with_state(limit, limit_body))
}

type RotState = (Arc<AppState>, SharedRotating);

/// The keys call: the public key for each denomination now in force.
fn keys_response(state: &AppState, rot: &SharedRotating) -> Result<api::KeysResponse, ApiFailure> {
    let mut logic = state.logic.lock().map_err(|_| poisoned())?;
    let rot = Some(Arc::clone(rot));
    let _r = rotor_of(&rot, &mut logic)?;
    Ok(api::KeysResponse {
        keys: logic.public_keys().clone(),
    })
}

/// The check-state call: for each
/// nonce, whether it is spent in either live key set.
fn check_state_response(
    state: &AppState,
    rot: &SharedRotating,
    req: api::CheckStateRequest,
) -> Result<api::CheckStateResponse, ApiFailure> {
    let mut logic = state.logic.lock().map_err(|_| poisoned())?;
    let rot = Some(Arc::clone(rot));
    let rotor = rotor_of(&rot, &mut logic)?;
    let prev = rotor
        .as_ref()
        .and_then(|r| r.previous.as_ref().map(|(_, l)| l));
    let mut spent = Vec::with_capacity(req.nonces.len());
    for n in req.nonces {
        spent.push(
            spent_in(&logic, prev, n).map_err(|e| failure(StatusCode::INTERNAL_SERVER_ERROR, e))?,
        );
    }
    Ok(api::CheckStateResponse { spent })
}

/// Issued and redeemed over every generation, the deleted ones included, and
/// `paid`: the PSP units burned as payments by PSP spends (kind 14), which is the
/// SPI's revenue. `paid` is part of `redeemed` (the notes of a spend are
/// redeemed whole, and the change is issued) and is counted apart from every
/// other burn, such as what the ESP mint burns through `/melt` for exchanges.
async fn audit_handler(
    State((state, rot)): State<RotState>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    let mut logic = state.logic.lock().map_err(|_| poisoned())?;
    let shared = Arc::clone(&rot);
    let rot = Some(rot);
    let rotor = rotor_of(&rot, &mut logic)?.expect("rotating");
    let int = |e| failure(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"));
    let (mut i, mut d) = logic.audit_totals().map_err(int)?;
    if let Some((_, p)) = &rotor.previous {
        let (pi, pd) = p.audit_totals().map_err(int)?;
        i += pi;
        d += pd;
    }
    let (ri, rd) = rotor.retired_totals();
    i += Amount::from_units(ri);
    d += Amount::from_units(rd);
    // Lock order: the mint, the rotor, then the rollovers (as a rollover does).
    let paid = shared.rollovers.lock().map_err(|_| poisoned())?.paid();
    let mut body = serde_json::to_value(api::AuditResponse {
        issued: i,
        redeemed: d,
        outstanding: Amount::from_units(i.units.saturating_sub(d.units)),
    })
    .map_err(|e| failure(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    body["paid"] = serde_json::json!(paid);
    Ok(Json(body))
}

/// The keysets call.
fn keysets_response(state: &AppState, rot: &SharedRotating) -> Result<KeysetsResponse, ApiFailure> {
    let mut logic = state.logic.lock().map_err(|_| poisoned())?;
    let rot = Some(Arc::clone(rot));
    let rotor = rotor_of(&rot, &mut logic)?.expect("rotating");
    let units = |pk: &std::collections::BTreeMap<Amount, secp256k1::PublicKey>| {
        pk.iter().map(|(a, k)| (a.units, *k)).collect()
    };
    let mut keysets = std::collections::BTreeMap::new();
    keysets.insert(rotor.current.id.clone(), units(&rotor.current.pk));
    if let Some((k, _)) = &rotor.previous {
        keysets.insert(k.id.clone(), units(&k.pk));
    }
    Ok(KeysetsResponse {
        current: rotor.current.id.clone(),
        previous: rotor.previous.as_ref().map(|(k, _)| k.id.clone()),
        keysets,
    })
}

/// The rotation call.
fn rotation_response(
    state: &AppState,
    rot: &SharedRotating,
) -> Result<crate::rotation::RotationResponse, ApiFailure> {
    let mut logic = state.logic.lock().map_err(|_| poisoned())?;
    let rot = Some(Arc::clone(rot));
    let rotor = rotor_of(&rot, &mut logic)?.expect("rotating");
    Ok(rotor.window())
}

/// A call with a body, over AIR: the ceiling, then the parse, then the call.
///
/// - A body over `limit` bytes is refused with the `413` JSON (`request_too_large`,
///   `size_bytes`, `limit_bytes`), before it is parsed.
/// - A body that does not parse as `Q` is refused with `malformed_request`.
/// - What `call` refuses with a 4xx is a signed refusal with its JSON (`error`,
///   `code`, and anything else such as `spent`).
/// - A 5xx, or a reply that cannot be encoded, is "cannot answer now": nothing
///   of the mint's state is told to the wallet, and a retry is safe, because
///   what changes state is idempotent by request hash.
fn post_served<Q, R>(
    what: &str,
    limit: usize,
    body: &[u8],
    call: impl FnOnce(Q) -> Result<R, ApiFailure>,
) -> sp_wire::air_request::Served
where
    Q: serde::de::DeserializeOwned,
    R: serde::Serialize,
{
    use sp_wire::air_request::Served;
    let refused = |why: &serde_json::Value| {
        Served::Refused(serde_json::to_vec(why).expect("a JSON value encodes"))
    };
    if body.len() > limit {
        return refused(&too_large_body(Some(body.len() as u64), limit));
    }
    let req: Q = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => {
            return refused(&serde_json::json!({
                "error": format!("the {what} request could not be read: {e}"),
                "code": sp_wire::codes::MALFORMED_REQUEST,
            }));
        }
    };
    match call(req) {
        Ok(value) => match serde_json::to_vec(&value) {
            Ok(bytes) => Served::Body(bytes),
            Err(e) => {
                eprintln!("AIR: {what} could not be encoded: {e}");
                Served::Unavailable
            }
        },
        Err((status, Json(why))) if status.is_client_error() => refused(&why),
        Err((_, Json(why))) => {
            eprintln!("AIR: {what} cannot be answered now: {why}");
            Served::Unavailable
        }
    }
}

/// What the AIR listener says to one kind of request: the JSON body of the
/// call, or a signed "cannot answer now" where the call fails (the mint lock is
/// poisoned, or a rotation step failed), or, for the calls with a body, a
/// signed refusal for a 4xx outcome. `None` for a kind this mint does not
/// serve, which the listener skips.
///
/// `body` is the request body (empty for the reads); `limits` are the ceilings
/// (`server.*_max_request_bytes`, the same ones `/melt` and `/burns` have).
pub fn air_served(
    state: &AppState,
    rot: &SharedRotating,
    health: &Health,
    limits: &Limits,
    kind: u8,
    body: &[u8],
) -> Option<sp_wire::air_request::Served> {
    air_served_with(state, rot, None, health, limits, kind, body)
}

/// [`air_served`] for a mint that also reconciles payments (kind 15): `records`
/// is the settlement records, shared with the scan thread. Without them kind 15 is
/// not answered. Lock order: the records, then the mint, then the rotor.
pub fn air_served_with(
    state: &AppState,
    rot: &SharedRotating,
    records: Option<&SharedRecords>,
    health: &Health,
    limits: &Limits,
    kind: u8,
    body: &[u8],
) -> Option<sp_wire::air_request::Served> {
    use sp_wire::air_request::{
        KIND_CHECK_STATE, KIND_KEYS, KIND_KEYSETS, KIND_MINT_INFO, KIND_PSP_RECONCILE,
        KIND_PSP_SPEND, KIND_ROLLOVER, KIND_ROTATION, Served,
    };
    fn read<T: serde::Serialize>(what: &str, result: Result<T, ApiFailure>) -> Served {
        match result {
            Ok(value) => match serde_json::to_vec(&value) {
                Ok(bytes) => Served::Body(bytes),
                Err(e) => {
                    eprintln!("AIR: {what} could not be encoded: {e}");
                    Served::Unavailable
                }
            },
            Err((_, Json(why))) => {
                eprintln!("AIR: {what} cannot be answered now: {why}");
                Served::Unavailable
            }
        }
    }
    Some(match kind {
        KIND_MINT_INFO => {
            let info = health.mint_info(state.logic.is_poisoned(), now_secs());
            read("mint info", Ok(info))
        }
        KIND_KEYS => read("keys", keys_response(state, rot)),
        KIND_KEYSETS => read("keysets", keysets_response(state, rot)),
        KIND_ROTATION => read("rotation", rotation_response(state, rot)),
        KIND_ROLLOVER => post_served(
            "rollover",
            limits.rollover_bytes,
            body,
            |req: RolloverRequest| rollover_response(state, rot, &req, limits.rollover_notes),
        ),
        // A spend shares the rollover's two ceilings: the request's bytes
        // (`server.rollover_max_request_bytes`) and the most notes
        // (`rollover.max_items`), for the notes in and for the change out.
        KIND_PSP_SPEND => post_served(
            "spend",
            limits.rollover_bytes,
            body,
            |req: PspSpendRequest| spend_response(state, rot, &req, limits.rollover_notes),
        ),
        // The corrected request or the credit claim of an open payment. It shares
        // the rollover's two ceilings too. Only an accepted request is answered.
        KIND_PSP_RECONCILE => reconcile_served(state, rot, records, limits, body),
        KIND_CHECK_STATE => post_served(
            "check-state",
            limits.check_state_bytes,
            body,
            |req: api::CheckStateRequest| check_state_response(state, rot, req),
        ),
        _ => return None,
    })
}

/// The reconciliation call (kind 15): the corrected request or the credit claim
/// of an open payment. **Only an accepted request is answered.** Every other
/// outcome is `Served::Ignored`: nothing is written to the reply cell, and the
/// code and reason are only logged here. That includes a transient failure of the
/// mint (a poisoned lock, a failed save), which the wallet meets by sending the
/// identical request again.
///
/// Checked without any lock: the size, the shape and the signature of the
/// reply key (not counted). Then the records, the mint and the rotor are locked,
/// in that order, and `reconcile::reconcile` does the rest.
fn reconcile_served(
    state: &AppState,
    rot: &SharedRotating,
    records: Option<&SharedRecords>,
    limits: &Limits,
    body: &[u8],
) -> sp_wire::air_request::Served {
    use sp_wire::air_request::Served;
    let ignored = |code: &str, why: &str, counted: bool| {
        println!(
            "AIR: reconciliation ignored ({code}{}): {why}",
            if counted { ", counted" } else { "" }
        );
        Served::Ignored(format!("{code}: {why}"))
    };
    let Some(records) = records else {
        return ignored(
            sp_wire::codes::UNAVAILABLE,
            "this mint was started without its settlement records",
            false,
        );
    };
    let early = |e: crate::reconcile::Ignore| ignored(&e.code, &e.why, e.counted);
    if body.len() > limits.rollover_bytes {
        return early(crate::reconcile::Ignore {
            code: sp_wire::codes::REQUEST_TOO_LARGE.to_string(),
            why: format!("{} bytes, at most {}", body.len(), limits.rollover_bytes),
            counted: false,
        });
    }
    let req: sp_wire::psp::ReconcileRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => {
            return ignored(
                sp_wire::codes::MALFORMED_REQUEST,
                &format!("the request could not be read: {e}"),
                false,
            );
        }
    };
    if let Err(e) = crate::reconcile::check_shape(&req, limits.rollover_notes) {
        return early(e);
    }
    if let Err(e) = crate::reconcile::verify_signature(&req) {
        return early(e);
    }
    let Ok(mut store) = records.lock() else {
        return ignored(
            sp_wire::codes::UNAVAILABLE,
            "the records lock is poisoned",
            false,
        );
    };
    let Ok(mut logic) = state.logic.lock() else {
        return ignored(
            sp_wire::codes::UNAVAILABLE,
            "the mint lock is poisoned",
            false,
        );
    };
    let r2 = Some(Arc::clone(rot));
    let rotor = match rotor_of(&r2, &mut logic) {
        Ok(rotor) => rotor.expect("rotating"),
        Err((_, Json(why))) => {
            return ignored(sp_wire::codes::UNAVAILABLE, &why.to_string(), false);
        }
    };
    match crate::reconcile::reconcile(&mut store, &logic, &rotor, &req, now_secs()) {
        Ok(answer) => match serde_json::to_vec(&answer) {
            Ok(bytes) => Served::Body(bytes),
            Err(e) => ignored(
                sp_wire::codes::UNAVAILABLE,
                &format!("the answer could not be encoded: {e}"),
                false,
            ),
        },
        Err(e) => early(e),
    }
}

/// The rollover call: move notes of the previous (or current) keyset to fresh
/// notes of the same amounts on the current keyset. The answer is stored by
/// request hash before any note is marked spent, so a resend returns the same
/// stored answer and a crash in the middle is finished by the next resend.
fn rollover_response(
    state: &AppState,
    rot: &SharedRotating,
    req: &RolloverRequest,
    max_notes: usize,
) -> Result<crate::rotation::RolloverResponse, ApiFailure> {
    let mut logic = state.logic.lock().map_err(|_| poisoned())?;
    let r2 = Some(Arc::clone(rot));
    let rotor = rotor_of(&r2, &mut logic)?.expect("rotating");
    let mut store = rot.rollovers.lock().map_err(|_| poisoned())?;
    match crate::rotation::rollover(&logic, &rotor, &mut store, req, max_notes) {
        Ok(a) => Ok(a),
        Err(RolloverError::Refused(e)) => Err(coded(StatusCode::BAD_REQUEST, "refused", e)),
        Err(RolloverError::Keyset(code, e)) => Err(coded(StatusCode::BAD_REQUEST, code, e)),
        Err(RolloverError::Spent(i)) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("input {i}: the note is already spent"),
                "code": sp_wire::codes::NOTE_SPENT_ELSEWHERE,
                "spent": [i],
            })),
        )),
        Err(RolloverError::Internal(e)) => Err(failure(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

/// The spend call (kind 14): notes in, change out, an amount paid and burned.
/// The answer is stored by request hash before the reply is made, so a resend
/// returns the same stored answer and a crash in the middle is finished by the
/// next resend (see `rotation::spend`).
fn spend_response(
    state: &AppState,
    rot: &SharedRotating,
    req: &PspSpendRequest,
    max_items: usize,
) -> Result<crate::rotation::PspSpendResponse, ApiFailure> {
    // Cheap checks first, before the mint lock is taken.
    if let Err(e) = crate::rotation::check_spend_shape(req, max_items) {
        return Err(spend_failure(e));
    }
    let mut logic = state.logic.lock().map_err(|_| poisoned())?;
    let r2 = Some(Arc::clone(rot));
    let rotor = rotor_of(&r2, &mut logic)?.expect("rotating");
    let mut store = rot.rollovers.lock().map_err(|_| poisoned())?;
    crate::rotation::spend(&logic, &rotor, &mut store, req, max_items).map_err(spend_failure)
}

fn spend_failure(e: SpendError) -> ApiFailure {
    match e {
        SpendError::Refused(code, why) => coded(StatusCode::BAD_REQUEST, code, why),
        SpendError::Spent(i) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("input {i}: the note is already spent"),
                "code": sp_wire::codes::NOTE_SPENT_ELSEWHERE,
                "spent": [i],
            })),
        ),
        SpendError::Internal(e) => failure(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// The API with the default ceilings.
pub fn router(state: Arc<AppState>, health: Arc<Health>, melts: SharedMelts) -> Router {
    router_with(state, health, melts, Limits::default())
}

pub fn router_with(
    state: Arc<AppState>,
    health: Arc<Health>,
    melts: SharedMelts,
    limits: Limits,
) -> Router {
    router_full(state, health, melts, limits, None)
}

/// The API, with key rotation when `rot` is given: then `/audit` spans both
/// live key sets. The wallet-facing calls (keys, keysets, rotation, rollover,
/// check-state) are not routes: they are answered over AIR (`air_served`).
pub fn router_full(
    state: Arc<AppState>,
    health: Arc<Health>,
    melts: SharedMelts,
    limits: Limits,
    rot: Option<SharedRotating>,
) -> Router {
    router_audited(state, health, melts, limits, rot, None)
}

/// [`router_full`] with the path of `settlements.json`: `/audit` then also
/// reports the payments held open and the payments forfeited (see
/// `add_reconciliation`).
pub fn router_audited(
    state: Arc<AppState>,
    health: Arc<Health>,
    melts: SharedMelts,
    limits: Limits,
    rot: Option<SharedRotating>,
    records: Option<PathBuf>,
) -> Router {
    let audit_network = health.network().map(str::to_string);
    let audit_records = records.map(Arc::new);
    let health_routes = Router::new()
        .route("/health", get(health_handler))
        .with_state((Arc::clone(&state), health));
    let melt_routes = Router::new()
        .route("/melt", limited(post(melt_handler), limits.melt_bytes))
        .route("/burns", limited(post(burns_handler), limits.burns_bytes))
        .with_state((Arc::clone(&state), melts, rot.clone()));
    if let Some(rot) = rot {
        return Router::new()
            .route(
                "/audit",
                get(audit_handler)
                    .layer(middleware::from_fn_with_state(
                        audit_records.clone(),
                        add_reconciliation,
                    ))
                    .layer(middleware::from_fn_with_state(audit_network, add_network)),
            )
            .with_state((Arc::clone(&state), rot))
            .merge(melt_routes)
            .merge(health_routes);
    }
    Router::new()
        .route(
            "/audit",
            get(api::audit)
                .layer(middleware::from_fn_with_state(
                    audit_records,
                    add_reconciliation,
                ))
                .layer(middleware::from_fn_with_state(audit_network, add_network)),
        )
        .with_state(state)
        .merge(melt_routes)
        .merge(health_routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(h: &Health, poisoned: bool, now: u64) -> (u16, String) {
        let (code, body) = h.report(poisoned, now);
        (code.as_u16(), body["status"].as_str().unwrap().to_string())
    }

    #[test]
    fn starting_until_the_first_scan_then_ok() {
        let h = Health::new(100);
        let t = h.started;
        assert_eq!(status(&h, false, t + 5), (200, "starting".into()));
        h.record_scan_at(t + 10);
        assert_eq!(status(&h, false, t + 20), (200, "ok".into()));
        assert_eq!(h.last_scan(), Some(t + 10));
    }

    #[test]
    fn no_scan_for_too_long_is_a_failure() {
        let h = Health::new(100);
        let t = h.started;
        assert_eq!(status(&h, false, t + 101), (503, "scan_stale".into()));
    }

    #[test]
    fn an_old_scan_is_a_failure_and_the_boundary_is_ok() {
        let h = Health::new(100);
        h.record_scan_at(1_000);
        assert_eq!(status(&h, false, 1_100), (200, "ok".into()));
        assert_eq!(status(&h, false, 1_101), (503, "scan_stale".into()));
    }

    #[test]
    fn a_poisoned_lock_is_a_failure_even_after_a_fresh_scan() {
        let h = Health::new(100);
        h.record_scan_at(1_000);
        assert_eq!(status(&h, true, 1_001), (503, "mint_lock_poisoned".into()));
    }

    #[test]
    fn the_body_reports_the_last_scan_time() {
        let h = Health::new(100);
        h.record_scan_at(1_000);
        let (_, body) = h.report(false, 1_030);
        assert_eq!(body["last_scan_unix"], 1_000);
        assert_eq!(body["seconds_since_last_scan"], 30);
        let (_, none) = Health::new(100).report(false, now_secs());
        assert!(none["last_scan_unix"].is_null());
        assert!(none["seconds_since_last_scan"].is_null());
    }

    #[test]
    fn the_air_mint_info_carries_the_same_status_as_health() {
        let h = Health::for_network(100, "regtest");
        let t = h.started;
        let info = h.mint_info(false, t + 5);
        assert_eq!(info.status, "starting");
        assert_eq!(info.wire_version, sp_wire::WIRE_VERSION);
        assert_eq!(info.network, "regtest");
        h.record_scan_at(t + 10);
        assert_eq!(h.mint_info(false, t + 20).status, "ok");
        assert_eq!(h.mint_info(false, t + 111).status, "scan_stale");
        assert_eq!(h.mint_info(true, t + 20).status, "mint_lock_poisoned");
        assert_eq!(Health::new(100).mint_info(false, now_secs()).network, "");
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_underflow() {
        let h = Health::new(100);
        h.record_scan_at(2_000);
        assert_eq!(status(&h, false, 1_000), (200, "ok".into()));
    }
}
