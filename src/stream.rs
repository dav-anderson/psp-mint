//! The key set stream: the PSP mint publishes its live key sets on AIR so that
//! every wallet can see the same keys (ROADMAP item 31, finding P2-04, the mint
//! fingerprinting attack). The layout, the cells and the records are
//! `sp_wire::air_request::stream`; this module is the mint's side of it.
//!
//! **What is published.** One record for each key set the mint makes, in epoch
//! order, written to the stream of the mint's DID: the epoch and every
//! (amount, key) of the set. The wallet accepts a key only if it is the key for
//! its amount in the last two records, the current and the previous key set.
//!
//! **When.** `serve` runs a pass of `sp_wire::air_request::stream::KeysetPublisher`
//! every `TICK_SECS` (5) seconds on a thread of its own. A pass compares the key
//! sets that are live now (the previous, if any, and the current) with the ones it
//! last published, and does nothing while they are the same. At start, and after
//! a rotation, they differ: the pass reads the end of the stream and the last two
//! records and writes the key sets that are not among them
//! (`sp_wire::air_request::stream::publish_keysets`).
//!
//! **A restart** keeps nothing: the stream is the memory. The first pass finds
//! what is published and writes the rest, so a key set is neither lost nor
//! written twice, whether the mint stopped before, between or after the writes of
//! a rotation, and a write that was not acknowledged is repeated with the same
//! bytes.
//!
//! **A failure** (the AIR server is down, a write timed out) is printed and the
//! pass is tried again after `RETRY_SECS` (30) seconds. It never stops the mint: the keys
//! were already in use, and the wallet that finds a record missing waits for it
//! and then tells its user to run the command again.
//!
//! **The size.** A record is 2,636 bytes (64 keys); the stream grows by one
//! record for each rotation, and `rotation.period_hours` is 0 (no rotation) or at
//! least 720, so by at most about 12 records a year. AIR has no delete, so the
//! stream is never shortened.

use sp_wire::air_request::stream::{KeysetRecord, MAX_KEYSET_KEYS};

use crate::rotation::Rotor;

/// The live key sets of `rotor`, oldest first: the previous one (if there is
/// one), then the current one. `Err` names a key set that cannot be published
/// (none or more than 64 keys).
pub fn live_records(rotor: &Rotor) -> Result<Vec<KeysetRecord>, String> {
    let mut sets = Vec::new();
    if let Some((previous, _)) = &rotor.previous {
        sets.push((previous.epoch, &previous.id, &previous.pk));
    }
    sets.push((rotor.current.epoch, &rotor.current.id, &rotor.current.pk));
    sets.into_iter()
        .map(|(epoch, id, pk)| {
            if pk.is_empty() || pk.len() > MAX_KEYSET_KEYS {
                return Err(format!(
                    "key set {id} (epoch {epoch}) has {} keys: a stream record holds 1 to {MAX_KEYSET_KEYS}",
                    pk.len()
                ));
            }
            Ok(KeysetRecord {
                epoch,
                keys: pk.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::Rotor;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("psp-mint-stream-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Fails if the previous key set is not published before the current one, or
    /// if a key set that is not live is published.
    #[test]
    fn the_live_key_sets_are_the_previous_then_the_current() {
        let dir = scratch("live");
        let (mut rotor, mut logic) = Rotor::open_at(&dir, 1, 1, 1_800_000_000).unwrap();
        let first = live_records(&rotor).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].epoch, 0);
        assert_eq!(first[0].id(), rotor.current.id);
        assert_eq!(first[0].keys.len(), 64);
        // issue so the clock starts, then move two periods on
        rotor.first_issue = Some(1_800_000_000);
        rotor.clock.store(
            1_800_000_000 + 3600 + 3600,
            std::sync::atomic::Ordering::SeqCst,
        );
        rotor.refresh(&mut logic).unwrap();
        let live = live_records(&rotor).unwrap();
        assert_eq!(live.iter().map(|r| r.epoch).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(live[1].id(), rotor.current.id);
        std::fs::remove_dir_all(&dir).ok();
    }
}
