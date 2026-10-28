//! The key set stream against a real `air_server` started in this process, with
//! the mint built the way `serve` builds it (see `common`): every key set the mint
//! makes is published once, in epoch order, and a wallet reads the last two as the
//! current and the previous key set (ROADMAP item 31, finding P2-04).
//!
//! The AIR client always dials port 5702, so these tests need that port free.

mod common;

use std::sync::atomic::Ordering;

use psp_mint::stream::live_records;
use sp_wire::air_request::client::{self, block_on_new_runtime};
use sp_wire::air_request::stream::{
    Cell, KeysetPublisher, KeysetRecord, read_cell, read_keyset_tail,
};

const T: u64 = 1_800_000_000;
const H: u64 = 3600;

/// The wallet's view of the mint's stream: the end and the last two records.
fn wallet_view(
    server: &client::ServerRef,
    mint: &sp_wire::ServiceDid,
) -> sp_wire::air_request::stream::KeysetTail {
    block_on_new_runtime(|| async {
        let wallet = client::connect(server).await.unwrap();
        read_keyset_tail(&wallet, mint, 10).await.unwrap()
    })
    .unwrap()
}

/// Rotate the mint to the epoch the clock `t` is in.
fn rotate_to(mint: &common::PspMint, t: u64) {
    // The mint's lock order: the mint, then the rotor.
    let mut logic = mint.state.logic.lock().unwrap();
    let mut rotor = mint.rot.rotor.lock().unwrap();
    rotor.clock.store(t, Ordering::SeqCst);
    rotor.refresh(&mut logic).unwrap();
}

/// Fails if a key set is published twice or out of order, if a restarted mint
/// (a new `KeysetPublisher`, which keeps nothing) writes again what is there, or if a
/// wallet cannot tell the current and the previous key set from the last two
/// records. Epoch 2 publishes after epoch 1; epoch 0 leaves the last two.
#[test]
fn every_key_set_is_published_once_in_order_and_the_last_two_are_current_and_previous() {
    let server = common::server();
    let mint = common::PspMint::open("stream", T, Default::default());
    mint.rot.rotor.lock().unwrap().first_issue = Some(T);
    let mut streamer = KeysetPublisher::new(server.clone(), 10, &mint.did_key);

    // start: epoch 0 is the only key set; the pass writes record 0
    let live = live_records(&mint.rot.rotor.lock().unwrap()).unwrap();
    let id0 = live[0].id();
    let done = streamer.pass(&live).unwrap().unwrap();
    assert_eq!((done.end, done.written), (1, vec![0]));
    // nothing changed: nothing is done
    assert_eq!(streamer.pass(&live).unwrap(), None);
    let view = wallet_view(&server, &mint.did);
    assert_eq!(view.end, 1);
    assert_eq!(view.last.len(), 1);
    assert_eq!(view.last[0].1.epoch, 0);
    assert_eq!(view.last[0].1.id(), id0);
    // the record holds the 64 keys of the mint's key set
    assert_eq!(view.last[0].1.keys.len(), 64);
    assert_eq!(
        view.last[0].1.keys,
        mint.rot.rotor.lock().unwrap().current.pk
    );

    // the first rotation: epoch 1 is current, epoch 0 previous
    rotate_to(&mint, T + H);
    let live = live_records(&mint.rot.rotor.lock().unwrap()).unwrap();
    assert_eq!(live.iter().map(|r| r.epoch).collect::<Vec<_>>(), vec![0, 1]);
    let id1 = live[1].id();
    let done = streamer.pass(&live).unwrap().unwrap();
    assert_eq!((done.end, done.written), (2, vec![1]));

    // a restart: a KeysetPublisher that remembers nothing finds both and writes nothing
    let mut restarted = KeysetPublisher::new(server.clone(), 10, &mint.did_key);
    let done = restarted.pass(&live).unwrap().unwrap();
    assert_eq!((done.end, done.written), (2, vec![]));

    // the second rotation: epoch 2 is current, epoch 1 previous, epoch 0 is deleted
    rotate_to(&mint, T + 2 * H);
    let live = live_records(&mint.rot.rotor.lock().unwrap()).unwrap();
    assert_eq!(live.iter().map(|r| r.epoch).collect::<Vec<_>>(), vec![1, 2]);
    let id2 = live[1].id();
    let done = streamer.pass(&live).unwrap().unwrap();
    assert_eq!((done.end, done.written), (3, vec![2]));

    // the wallet's last two are epochs 1 and 2: epoch 0, two rotations old, is not
    let view = wallet_view(&server, &mint.did);
    assert_eq!(view.end, 3);
    let last: Vec<(u32, u64, String)> = view
        .last
        .iter()
        .map(|(i, r)| (*i, r.epoch, r.id()))
        .collect();
    assert_eq!(last, vec![(1, 1, id1), (2, 2, id2)]);
    assert_ne!(id0, last[0].2);
    assert_ne!(id0, last[1].2);
}

/// Fails if the stream is written under the mint's DID key itself, or in the
/// mailbox, where anyone can write: the cell is the mint's key tweaked by the
/// cell's name, and nothing else is at index 0.
#[test]
fn the_record_is_in_the_tweaked_cell_and_nowhere_else() {
    let server = common::server();
    let mint = common::PspMint::open("stream-cell", T, Default::default());
    let mut streamer = KeysetPublisher::new(server.clone(), 10, &mint.did_key);
    let live = live_records(&mint.rot.rotor.lock().unwrap()).unwrap();
    streamer.pass(&live).unwrap();
    let (stored, control): (Option<Vec<u8>>, Option<Vec<u8>>) = block_on_new_runtime(|| async {
        let wallet = client::connect(&server).await.unwrap();
        (
            read_cell(&wallet, &mint.did, &Cell::Keyset { index: 0 }, 10)
                .await
                .unwrap(),
            read_cell(&wallet, &mint.did, &Cell::Control { index: 0 }, 10)
                .await
                .unwrap(),
        )
    })
    .unwrap();
    assert_eq!(KeysetRecord::decode(&stored.unwrap()).unwrap(), live[0]);
    assert_eq!(control, None);
}
