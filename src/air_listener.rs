//! The AIR listener: answers a wallet's requests over AIR, the only way wallets
//! reach this mint. It always runs under `serve`. The calls are **mint info**
//! (what `GET /health` reports), the keys calls, **keys**, **keysets** and
//! **rotation**, **rollover**, **check-state**, **spend** and **reconcile** (kind 15,
//! which is answered only when it is accepted: `Served::Ignored` writes no reply);
//! the reply body is built by `server::air_served_with`. What is left on HTTP is local to the SPI and the
//! operator: `/melt`, `/burns`, `/audit` and `/health`.
//!
//! The design (REQUEST-DESIGN.md in sandbox-mint) and the wire (envelopes,
//! addresses, signing) are in `sp_wire::air_request`; this module is the mint's
//! loop around them:
//!
//! - The mint holds reads on the next `air.held_reads` mailbox slots of its DID
//!   (`mailbox_secret(DID, j)`; anyone can write a slot, only the mint can open
//!   it). A slot that fills is opened with the mint DID key
//!   (`mint_did_key.json`), answered with the call's result (for mint info:
//!   `status`, `wire_version`, `network`), signed with the same key, sealed to
//!   the wallet's one-off key `R`, and `Create`d in the reply cell only the
//!   wallet and the mint can compute.
//! - **Create first, no read-then-write.** The reply cell is write-once, so the
//!   first `Create` wins and a second comes back as the stored record, which
//!   means "already answered". A retry that reached the mint twice, a copied
//!   cell and two mint processes all end with one reply.
//! - **The counter `next`** (the lowest slot not yet finished) is a text file,
//!   `air_mailbox_next`, in the network's data folder with the rest of the
//!   mint's local state. It is not under the storage policy: it is only an
//!   optimisation. Without it the mint starts at slot 0, and an answered
//!   request finds its reply cell taken and is skipped, so it is rebuilt by
//!   probing from 0. It is written (temp file, fsync, rename) after each slot is
//!   finished, so a crash between answering and writing it repeats a slot,
//!   which is harmless.
//! - **Garbage is skipped.** A cell that does not open, is malformed, is of an
//!   unknown kind or names a key that is not one is logged and skipped. Every
//!   call that changes state is idempotent by request hash, and junk changes no
//!   state; the answer to a valid request costs the mint one short lock.
//! - **A call the mint cannot answer now** (the mint lock is poisoned, or a
//!   rotation step failed) gets a signed reply
//!   with status `unavailable` and no body, so the wallet fails at once and says
//!   why, instead of waiting out its tries.
//! - **If the AIR client gives up** (the session was refused or killed: more than
//!   [`LOST_LIMIT`] held reads end within [`LOST_WINDOW_SECS`] seconds, or a
//!   `Create` fails three times) the listener returns an error and ends, and
//!   psp-mint exits with status 1 (a mint no wallet can reach is not running),
//!   so that a supervisor starts it again from `next`. A restart of the AIR server needs nothing: the client
//!   reconnects and its held reads resume (shown by the prototype).
//!
//! Held reads left behind by a stopped listener are never cleaned up by the AIR
//! server (upstream item 17); with the default of 16 a handful of restarts costs
//! little.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sp_wire::ServiceDid;
use sp_wire::air_request::addr::{self, mailbox_secret};
use sp_wire::air_request::client::{self, Client, Event, Put, ServerRef};
use sp_wire::air_request::{KIND_MINT_INFO, MintInfo, Served, answer_request};
use sp_wire::seal::SecretKey;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The file, in the network's data folder, that holds the counter `next`.
pub const NEXT_FILE: &str = "air_mailbox_next";

/// More than this many held reads ending within [`LOST_WINDOW_SECS`] seconds
/// means the AIR session is gone (a rejected session ends all of them at once).
/// Supervision constants of the listener, not operator settings.
pub const LOST_LIMIT: usize = 5;
/// See [`LOST_LIMIT`].
pub const LOST_WINDOW_SECS: u64 = 10;

/// How many times a failed `Create` of a reply is tried before the listener gives up.
const CREATE_TRIES: u32 = 3;

/// What the listener needs. Built from the settings (`air.*`) by
/// [`crate::config::Config::air_listener`].
#[derive(Clone, Debug)]
pub struct Settings {
    /// The AIR server to hold reads on and `Create` replies at.
    pub server: ServerRef,
    /// Mailbox slots with a held read (`air.held_reads`).
    pub held_reads: u32,
    /// How long the server may take to answer one `Create` or `Read` (`air.timeout_secs`).
    pub timeout_secs: u64,
    /// The network's data folder: where `air_mailbox_next` is.
    pub state_dir: PathBuf,
}

fn next_path(dir: &Path) -> PathBuf {
    dir.join(NEXT_FILE)
}

/// The stored counter, or 0 when there is none.
pub fn read_next(dir: &Path) -> Result<u32, String> {
    match std::fs::read_to_string(next_path(dir)) {
        Ok(text) => text
            .trim()
            .parse()
            .map_err(|e| format!("{}: {e}", next_path(dir).display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(format!("read {}: {e}", next_path(dir).display())),
    }
}

/// Write the counter durably: temp file, fsync, rename.
pub fn write_next(dir: &Path, next: u32) -> Result<(), String> {
    let tmp = dir.join(format!("{NEXT_FILE}.tmp"));
    let mut file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    file.write_all(next.to_string().as_bytes())
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, next_path(dir)).map_err(|e| e.to_string())
}

/// The held reads on the next K slots.
struct Window<'a> {
    client: &'a Client,
    did: ServiceDid,
    held: BTreeMap<u32, JoinHandle<()>>,
    /// First slot that has no held read yet.
    requested: u32,
    k: u32,
    tx: mpsc::UnboundedSender<Event>,
}

impl Window<'_> {
    /// Keep held reads on the K slots from `next`.
    async fn top_up(&mut self, next: u32) {
        let from = self.requested;
        while self.requested < next.saturating_add(self.k) {
            self.hold(self.requested).await;
            self.requested += 1;
        }
        if self.requested > from {
            println!(
                "AIR listener: holding mailbox slots {from}..{} ({} held)",
                self.requested,
                self.held.len()
            );
        }
    }

    async fn hold(&mut self, slot: u32) {
        let key = mailbox_secret(&self.did, slot).public_key();
        let handle = client::hold(self.client, key, slot, self.tx.clone()).await;
        self.held.insert(slot, handle);
    }
}

/// What the listener does with a request: given its kind and its body, the
/// reply to it, or `None` for a kind not served (the cell is skipped). The
/// mint's is [`crate::server::air_served_with`].
pub type Serve = dyn Fn(u8, &[u8]) -> Option<Served> + Send + Sync;

/// A [`Serve`] that answers mint info with `info` (what the mint reports now,
/// asked for at each request) and nothing else. For tests of the listener that
/// do not need a mint.
pub fn mint_info_only(
    info: impl Fn() -> MintInfo + Send + Sync + 'static,
) -> impl Fn(u8, &[u8]) -> Option<Served> + Send + Sync + 'static {
    move |kind, _| {
        (kind == KIND_MINT_INFO)
            .then(|| Served::Body(serde_json::to_vec(&info()).expect("mint info is JSON")))
    }
}

/// Run the listener until it fails, or until `stop` resolves (then `Ok`). It
/// never ends on its own otherwise. `serve` is called for each cell that is read
/// and opens to a request, and says what the mint answers now.
pub async fn run(
    settings: &Settings,
    key: &SecretKey,
    serve: &Serve,
    stop: impl Future<Output = ()>,
) -> Result<(), String> {
    tokio::pin!(stop);
    let did = addr::key_to_did(&key.public_key());
    // A stop that comes while the connection is still being made ends the
    // listener too: a stop does not wait for an AIR server that is slow.
    let client = tokio::select! {
        connected = client::connect(&settings.server) => connected.map_err(|e| e.0)?,
        _ = &mut stop => return Ok(()),
    };
    let mut next = read_next(&settings.state_dir)?;
    println!(
        "AIR listener: mint {did} on AIR server {} ({}), next slot {next}, {} held reads",
        settings.server.name.trim(),
        settings.server.host.trim(),
        settings.held_reads
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut window = Window {
        client: &client,
        did,
        held: BTreeMap::new(),
        requested: next,
        k: settings.held_reads,
        tx,
    };
    window.top_up(next).await;

    let mut pending: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut losses: VecDeque<Instant> = VecDeque::new();
    loop {
        while let Some(bytes) = pending.remove(&next) {
            window.held.remove(&next);
            handle_fill(&client, key, serve, settings.timeout_secs, next, &bytes).await?;
            next += 1;
            write_next(&settings.state_dir, next)?;
            window.top_up(next).await;
        }
        let event = tokio::select! {
            event = rx.recv() => event,
            _ = &mut stop => return Ok(()),
        };
        match event {
            Some(Event::Filled(slot, bytes)) => {
                pending.insert(slot, bytes);
            }
            Some(Event::Lost(slot)) => {
                let now = Instant::now();
                losses.push_back(now);
                while losses
                    .front()
                    .map(|t| now.duration_since(*t) > Duration::from_secs(LOST_WINDOW_SECS))
                    .unwrap_or(false)
                {
                    losses.pop_front();
                }
                println!(
                    "AIR listener: held read on slot {slot} ended ({} in the last {LOST_WINDOW_SECS} s)",
                    losses.len()
                );
                if losses.len() > LOST_LIMIT {
                    return Err(format!(
                        "the AIR session is gone (more than {LOST_LIMIT} held reads ended in \
                         {LOST_WINDOW_SECS} s). psp-mint exits so that a supervisor can start it \
                         again, and the listener resumes from slot {next}"
                    ));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                if slot >= next {
                    window.hold(slot).await;
                }
            }
            None => return Err("the listener's event channel closed".into()),
        }
    }
}

/// Answer one filled slot, or skip it. `Err` only when the reply could not be
/// written after several tries (the AIR server is not taking `Create`s).
async fn handle_fill(
    client: &Client,
    key: &SecretKey,
    serve: &Serve,
    timeout_secs: u64,
    slot: u32,
    sealed: &[u8],
) -> Result<(), String> {
    let answer = match answer_request(key, sealed, addr::now_secs(), serve) {
        Ok(answer) => answer,
        Err(reason) => {
            println!("AIR listener: slot {slot} skipped: {reason}");
            return Ok(());
        }
    };
    let id = hex::encode(answer.request.request_id);
    // Create first, no read-then-write: the first Create wins, a later one comes
    // back as `Taken` (already answered).
    let mut last = String::new();
    for attempt in 1..=CREATE_TRIES {
        match client::create(
            client,
            &answer.reply_cell,
            answer.sealed_reply.clone(),
            timeout_secs,
        )
        .await
        {
            Ok(Put::Created(_)) => {
                println!("AIR listener: slot {slot} answered request {id}");
                return Ok(());
            }
            Ok(Put::Taken) => {
                println!("AIR listener: slot {slot} request {id} was already answered");
                return Ok(());
            }
            Err(e) => {
                last = e.0;
                eprintln!(
                    "AIR listener: reply to request {id} not written (try {attempt}): {last}"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Err(format!(
        "could not write the reply to request {id} after {CREATE_TRIES} tries: {last}"
    ))
}

/// Run the listener on the calling thread, with a runtime of its own, until it
/// fails or until `stop` is sent (or its sender is dropped): then it finishes
/// the slot it is answering and returns `Ok`, which is how a stop and the
/// snapshot quiet the listener. The slots it has not read stay in the AIR
/// server, and the next listener resumes from the stored counter. For a thread
/// of its own: `serve` starts one so that the HTTP API's runtime and the
/// listener never share a thread. A panic in `serve` ends the thread, like a
/// crash of the process would (the tests use that).
pub fn run_blocking(
    settings: &Settings,
    key: &SecretKey,
    serve: &Serve,
    stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start a runtime: {e}"))?;
    runtime.block_on(run(settings, key, serve, async move {
        // Sent, or the sender was dropped: either way the listener is to stop.
        stop.await.ok();
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "psp-mint-air-{}-{name}-{}",
            std::process::id(),
            addr::now_secs()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_counter_starts_at_zero_and_survives_a_write() {
        let dir = scratch("next");
        assert_eq!(read_next(&dir).unwrap(), 0);
        write_next(&dir, 7).unwrap();
        assert_eq!(read_next(&dir).unwrap(), 7);
        write_next(&dir, 8).unwrap();
        assert_eq!(read_next(&dir).unwrap(), 8);
        assert!(!dir.join(format!("{NEXT_FILE}.tmp")).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
