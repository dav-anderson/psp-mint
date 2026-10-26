//! Key rotation: generation 0 is the first set of keys, the first issue starts the
//! clock, notes of the previous set are spendable and can be rolled over until
//! the next rotation, then they are lost.
//!
//! The wallet's calls (rotation, keysets, rollover, check-state) go over AIR,
//! through the mint's real listener on a real `air_server` (see `common`). The
//! calls that stay on HTTP, because they are local to the SPI (`/melt`,
//! `/audit`), go to the HTTP server.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use mint_core::state::AppState;
use mint_types::{
    Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash, transaction_sighash,
};
use psp_mint::rotation::{
    PspSpendRequest, RolloverRequest, RolloverResponse, Rollovers, RotationResponse, Rotor,
};
use psp_mint::server::{
    AuditResponse, Burn, KeysetsResponse, PspMeltRequest, SharedMelts, SharedRotating,
};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1, SecretKey};
use sp_wire::keys::{CheckStateRequest, CheckStateResponse};

const T: u64 = 1_800_000_000;
const H: u64 = 3600;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("psp-mint-rot-{}-{name}", {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

struct Mint {
    base: String,
    state: Arc<AppState>,
    rot: SharedRotating,
    melts: SharedMelts,
    /// The wallet side of the mint's AIR listener. Holds the turn on the shared
    /// AIR server until the mint drops.
    air: common::Air,
}

async fn start(dir: &Path, clock: u64) -> Mint {
    start_with(dir, clock, Default::default()).await
}

async fn start_with(dir: &Path, clock: u64, limits: psp_mint::server::Limits) -> Mint {
    let mint = common::PspMint::open_in(dir, 1, 1, clock, limits);
    let air = mint.air();
    let base = mint.http().await;
    Mint {
        base,
        state: Arc::clone(&mint.state),
        rot: Arc::clone(&mint.rot),
        melts: Arc::clone(&mint.melts),
        air,
    }
}

impl Mint {
    fn set_clock(&self, t: u64) {
        self.rot
            .rotor
            .lock()
            .unwrap()
            .clock
            .store(t, Ordering::SeqCst);
    }
    fn issue(&self, amount: Amount) -> (Note, Keypair) {
        let kp = Keypair::new(SECP256K1, &mut OsRng);
        let nonce = Nonce(kp.public_key());
        let (b, r) = bdhke::blind_message(&nonce.as_hash_preimage(), None);
        let logic = self.state.logic.lock().unwrap();
        let out = logic
            .issue(&MintOutput {
                amount,
                blind_nonce: BlindNonce(b),
            })
            .unwrap();
        let signature = bdhke::unblind_message(&out.0, &r, &logic.public_keys()[&amount]);
        (Note { nonce, signature }, kp)
    }
    /// Ask for the rotation, which records the first issue and so starts the
    /// clock, and check that it and the keysets name the same current set.
    async fn records_first_issue(&self) {
        let rotation: RotationResponse = self.get("/rotation").await;
        let keysets: KeysetsResponse = self.get("/keysets").await;
        assert_eq!(rotation.current.id, keysets.current);
    }

    /// A read: `/audit` is the SPI's, over HTTP; the rest are the wallet's, over AIR.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        if path != "/audit" {
            return self.air.get(path);
        }
        reqwest::get(format!("{}{path}", self.base))
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    /// A call with a body: `/melt` is the SPI's, over HTTP; the rest are the
    /// wallet's, over AIR (a refusal reads as `400`, see `common`).
    async fn post(&self, path: &str, body: &impl serde::Serialize) -> (u16, serde_json::Value) {
        if path != "/melt" {
            return self.air.post_json(path, body);
        }
        let r = reqwest::Client::new()
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap())
    }
}

fn zero_sig() -> secp256k1::schnorr::Signature {
    secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap()
}

fn melt(notes: &[(Note, Keypair)], amount: Amount, memo: &str, keysets: &[&str]) -> PspMeltRequest {
    let unsigned: Vec<MintInput> = notes
        .iter()
        .map(|(n, _)| MintInput {
            amount,
            note: *n,
            spend_signature: zero_sig(),
        })
        .collect();
    let h = melt_sighash(&unsigned, memo.as_bytes());
    PspMeltRequest {
        melt: sp_wire::melt::MeltRequest {
            inputs: unsigned
                .into_iter()
                .zip(notes)
                .map(|(i, (_, k))| MintInput {
                    spend_signature: SECP256K1.sign_schnorr(&h, k),
                    ..i
                })
                .collect(),
            memo: memo.into(),
        },
        keysets: keysets.iter().map(|k| k.to_string()).collect(),
    }
}

/// A rollover of one note, with its blinding factor.
fn rollover(
    note: &(Note, Keypair),
    amount: Amount,
    input_keyset: &str,
    keyset: &str,
) -> (RolloverRequest, Keypair, SecretKey) {
    let kp = Keypair::new(SECP256K1, &mut OsRng);
    let (b, r) = bdhke::blind_message(&Nonce(kp.public_key()).as_hash_preimage(), None);
    let outputs = vec![MintOutput {
        amount,
        blind_nonce: BlindNonce(b),
    }];
    let input = MintInput {
        amount,
        note: note.0,
        spend_signature: zero_sig(),
    };
    let h = transaction_sighash(std::slice::from_ref(&input), &outputs);
    let input = MintInput {
        spend_signature: SECP256K1.sign_schnorr(&h, &note.1),
        ..input
    };
    (
        RolloverRequest {
            input_keysets: vec![input_keyset.to_string()],
            inputs: vec![input],
            keyset: keyset.to_string(),
            outputs,
        },
        kp,
        r,
    )
}

#[tokio::test]
async fn rotation_keeps_previous_notes_for_one_period_then_they_are_lost() {
    let dir = scratch("cycle");
    let one = Amount::from_units(1);
    // The keys of generation 0 (epoch 0), opened on their own first: the mint
    // started next in the same folder serves these same keys.
    let keys0 = Rotor::open_at(&dir, 1, 1, T)
        .unwrap()
        .1
        .public_keys()
        .clone();
    let m = start(&dir, T).await;
    let w: RotationResponse = m.get("/rotation").await;
    assert_eq!(
        (w.first_issue, w.current.epoch, w.active),
        (None, 0, false),
        "nothing issued yet"
    );
    assert_eq!(m.state.logic.lock().unwrap().public_keys(), &keys0);

    let a = m.issue(one);
    let b = m.issue(one);
    let c = m.issue(one);
    let w: RotationResponse = m.get("/rotation").await;
    assert_eq!(w.first_issue, Some(T), "the first issue starts the clock");
    assert_eq!(w.activation, Some(T + H));
    assert_eq!(w.next_rotation, Some(T + H));

    // The first rotation: generation 0 is previous and still spendable.
    m.set_clock(T + H);
    let w: RotationResponse = m.get("/rotation").await;
    assert!(w.active);
    assert_eq!(
        (w.current.epoch, w.previous.as_ref().unwrap().epoch),
        (1, 0)
    );
    assert_eq!(w.previous.unwrap().retires, Some(T + 2 * H));
    let id0 = psp_mint::rotation::keyset_id(&keys0);
    let id1 = w.current.id.clone();
    let ks: KeysetsResponse = m.get("/keysets").await;
    assert_eq!(
        (ks.current.as_str(), ks.previous.as_deref()),
        (id1.as_str(), Some(id0.as_str()))
    );
    assert_eq!(ks.keysets.len(), 2);
    let (code, _) = m.post("/melt", &melt(&[a], one, "x", &[&id0])).await;
    assert_eq!(code, 200, "a previous-set note can still be burned");

    // Rollover: same amounts, signed with the current set; a retry gets the
    // stored answer.
    let (req, kp, r) = rollover(&b, one, &id0, &id1);
    let (code, body) = m.post("/rollover", &req).await;
    assert_eq!(code, 200, "{body}");
    let ans: RolloverResponse = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(ans.epoch, 1);
    let (code2, again) = m.post("/rollover", &req).await;
    assert_eq!((code2, again), (200, body));
    let sig = bdhke::unblind_message(&ans.outcomes[0].0, &r, &ans.keys[0]);
    let fresh = (
        Note {
            nonce: Nonce(kp.public_key()),
            signature: sig,
        },
        kp,
    );
    // Splitting is not a rollover.
    let (mut bad, _, _) = rollover(&c, one, &id0, &id1);
    bad.outputs[0].amount = Amount::from_units(2);
    assert_eq!(m.post("/rollover", &bad).await.0, 400);
    // The spent check spans both sets.
    let cs: CheckStateResponse = serde_json::from_value(
        m.post(
            "/check-state",
            &CheckStateRequest {
                nonces: vec![a.0.nonce, b.0.nonce, c.0.nonce],
            },
        )
        .await
        .1,
    )
    .unwrap();
    assert_eq!(cs.spent, vec![true, true, false]);

    // The next rotation deletes generation 0 with its spent list: `c`, never
    // rolled over, is lost; the rolled-over note is fine.
    m.set_clock(T + 2 * H);
    let w: RotationResponse = m.get("/rotation").await;
    assert_eq!((w.current.epoch, w.previous.unwrap().epoch), (2, 1));
    assert!(!dir.join("db").exists() && !dir.join("mint_keys.json").exists());
    let (code, body) = m.post("/melt", &melt(&[c], one, "y", &[&id0])).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("keyset_retired")),
        "lost after a full cycle"
    );
    let (code, body) = m
        .post("/melt", &melt(&[c], one, "y", &["00".repeat(8).as_str()]))
        .await;
    assert_eq!((code, body["code"].as_str()), (400, Some("unknown_keyset")));
    // A rollover naming the retired set is refused before anything burns.
    let id2 = w.current.id.clone();
    let (late, _, _) = rollover(&c, one, &id0, &id2);
    let (code, body) = m.post("/rollover", &late).await;
    assert_eq!((code, body["code"].as_str()), (400, Some("keyset_retired")));
    // Outputs asked of a retired set are refused, and of an unknown one, before
    // anything is burned.
    let (gone_out, _, _) = rollover(&fresh, one, &id1, &id0);
    let (code, body) = m.post("/rollover", &gone_out).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("keyset_retired")),
        "{body}"
    );
    let (unk_out, _, _) = rollover(&fresh, one, &id1, &"00".repeat(8));
    let (code, body) = m.post("/rollover", &unk_out).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("unknown_keyset")),
        "{body}"
    );
    // Outputs asked of the previous set (now `id1`) are signed by it, and the
    // answer names it.
    let (prev_out, _, _) = rollover(&fresh, one, &id1, &id1);
    let (code, body) = m.post("/rollover", &prev_out).await;
    assert_eq!(code, 200, "{body}");
    let signed: RolloverResponse = serde_json::from_value(body).unwrap();
    assert_eq!((signed.keyset.as_str(), signed.epoch), (id1.as_str(), 1));
    let ks: KeysetsResponse = m.get("/keysets").await;
    assert!(
        !ks.keysets.contains_key(&id0),
        "the retired set's keys are gone"
    );
    // The audit still counts the deleted generation: 3 issued + 2 rolled over
    // (b's output, and fresh's output signed by the previous set); a (burned),
    // b and fresh (rolled) redeemed.
    let audit: AuditResponse = m.get("/audit").await;
    assert_eq!((audit.issued, audit.redeemed), (5, 3));
}

#[tokio::test]
async fn a_rollover_with_one_spent_input_among_several_burns_none() {
    let dir = scratch("rollover-partial");
    let one = Amount::from_units(1);
    let m = start(&dir, T).await;
    let a = m.issue(one);
    let b = m.issue(one);
    let c = m.issue(one);
    // The mint records the first issue when it next looks (a /rotation or any
    // keyset read), so look once before the clock moves, as the other
    // rotation tests do. Otherwise the first issue is stamped at T + H and
    // nothing has rotated yet.
    m.records_first_issue().await;
    m.set_clock(T + H);
    let w: RotationResponse = m.get("/rotation").await;
    let id0 = w.previous.as_ref().unwrap().id.clone();
    let id1 = w.current.id.clone();
    // `a` is burned first, alone.
    let (code, _) = m.post("/melt", &melt(&[a], one, "x", &[&id0])).await;
    assert_eq!(code, 200);
    // Then a rollover of three notes, `a` among them: refused whole.
    let outputs: Vec<MintOutput> = (0..3)
        .map(|_| {
            let kp = Keypair::new(SECP256K1, &mut OsRng);
            let (point, _r) =
                bdhke::blind_message(&Nonce(kp.public_key()).as_hash_preimage(), None);
            MintOutput {
                amount: one,
                blind_nonce: BlindNonce(point),
            }
        })
        .collect();
    let notes = [b, a, c];
    let unsigned: Vec<MintInput> = notes
        .iter()
        .map(|(n, _)| MintInput {
            amount: one,
            note: *n,
            spend_signature: zero_sig(),
        })
        .collect();
    let h = transaction_sighash(&unsigned, &outputs);
    let inputs: Vec<MintInput> = unsigned
        .into_iter()
        .zip(notes.iter())
        .map(|(i, (_, k))| MintInput {
            spend_signature: SECP256K1.sign_schnorr(&h, k),
            ..i
        })
        .collect();
    let req = RolloverRequest {
        input_keysets: vec![id0.clone(); 3],
        inputs,
        keyset: id1.clone(),
        outputs,
    };
    let (code, body) = m.post("/rollover", &req).await;
    assert_eq!(code, 400, "{body}");
    assert!(body.to_string().contains("already spent"), "{body}");
    // the body names the spent input, for the wallet
    assert_eq!(body["code"], "note_spent_elsewhere", "{body}");
    assert_eq!(body["spent"], serde_json::json!([1]), "{body}");
    let cs: CheckStateResponse = serde_json::from_value(
        m.post(
            "/check-state",
            &CheckStateRequest {
                nonces: vec![a.0.nonce, b.0.nonce, c.0.nonce],
            },
        )
        .await
        .1,
    )
    .unwrap();
    assert_eq!(
        cs.spent,
        vec![true, false, false],
        "nothing else was burned"
    );
}

#[tokio::test]
async fn a_rollover_interrupted_after_the_burn_is_finished_by_the_retry() {
    let dir = scratch("crash");
    let one = Amount::from_units(1);
    let m = start(&dir, T).await;
    let n = m.issue(one);
    m.records_first_issue().await;
    m.set_clock(T + H);
    let ids: KeysetsResponse = m.get("/keysets").await;
    let (req, _, _) = rollover(&n, one, ids.previous.as_ref().unwrap(), &ids.current);
    // Simulate the crash: the pending entry is written and the note burned,
    // but no answer stored.
    {
        let mut logic = m.state.logic.lock().unwrap();
        let mut rotor = m.rot.rotor.lock().unwrap();
        rotor.refresh(&mut logic).unwrap();
        let path = dir.join(psp_mint::rotation::ROLLOVERS_FILE);
        std::fs::write(
            &path,
            common::rollovers_file(serde_json::json!({"pending": {req.hash(): 1}})),
        )
        .unwrap();
        let (k, l) = rotor.previous.as_ref().unwrap();
        let h = transaction_sighash(&req.inputs, &req.outputs);
        l.redeem_with_key(&k.sk[&one], &req.inputs[0], h).unwrap();
    }
    // The restart: the rollover answers are read back from disk.
    *m.rot.rollovers.lock().unwrap() = Rollovers::open(&dir).unwrap();
    let (code, body) = m.post("/rollover", &req).await;
    assert_eq!(code, 200, "{body}");
    let (code2, again) = m.post("/rollover", &req).await;
    assert_eq!((code2, &again), (200, &body));
}

#[tokio::test]
async fn one_melt_may_mix_the_current_and_previous_sets() {
    let dir = scratch("mixed");
    let one = Amount::from_units(1);
    let m = start(&dir, T).await;
    let old = m.issue(one);
    m.records_first_issue().await;
    m.set_clock(T + H);
    m.records_first_issue().await;
    let new = m.issue(one);
    let nonces = vec![old.0.nonce, new.0.nonce];
    let ids: KeysetsResponse = m.get("/keysets").await;
    let req = melt(
        &[old, new],
        one,
        "both",
        &[ids.previous.as_ref().unwrap(), &ids.current],
    );
    let (code, body) = m.post("/melt", &req).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["burned"], serde_json::json!(2));
    assert_eq!(m.post("/melt", &req).await.0, 400, "spent in both sets");
    let cs: CheckStateResponse = serde_json::from_value(
        m.post("/check-state", &CheckStateRequest { nonces })
            .await
            .1,
    )
    .unwrap();
    assert_eq!(cs.spent, vec![true, true]);
}

#[tokio::test]
async fn a_refused_melt_records_no_memo() {
    // Fails if `/melt` records its memo before it has checked the notes: a refused
    // melt then leaves its memo on notes that are still unspent (the stale memo of
    // P1-01). Read from the log itself, since `/burns` shows no memo for an
    // unspent note.
    let (one, two) = (Amount::from_units(1), Amount::from_units(2));
    let m = start(&scratch("memo-refused"), T).await;
    let old = m.issue(one);
    m.records_first_issue().await;
    m.set_clock(T + H);
    m.records_first_issue().await;
    let new = m.issue(one);
    let ids: KeysetsResponse = m.get("/keysets").await;
    let (prev, cur) = (ids.previous.clone().unwrap(), ids.current);
    // A note worth 1 claimed as 2: refused on the current keyset...
    let (code, body) = m.post("/melt", &melt(&[new], two, "claim", &[&cur])).await;
    assert_eq!(code, 400, "{body}");
    // ...and in a melt of a previous-keyset note and that same current one.
    let (code, body) = m
        .post("/melt", &melt(&[old, new], two, "claim", &[&prev, &cur]))
        .await;
    assert_eq!(code, 400, "{body}");
    let log = m.melts.lock().unwrap();
    assert_eq!((log.get(&old.0.nonce), log.get(&new.0.nonce)), (None, None));
}

#[tokio::test]
async fn a_rollover_clears_the_stale_memo_of_a_note_and_keeps_a_real_burns() {
    // Fails if a rollover leaves the memo of an earlier `/melt` on an unspent note
    // it burns (the note is then reported burned under that memo), or clears the
    // memo of a note that was already spent by a melt.
    let one = Amount::from_units(1);
    let m = start(&scratch("memo-rollover"), T).await;
    let (stale, real) = (m.issue(one), m.issue(one));
    m.records_first_issue().await;
    let id = m.get::<KeysetsResponse>("/keysets").await.current;
    // What a refused melt, or a crash between a melt's record and its burn, left.
    m.melts
        .lock()
        .unwrap()
        .record(&[stale.0.nonce], "stale")
        .unwrap();
    assert_eq!(
        m.post("/melt", &melt(&[real], one, "real", &[&id])).await.0,
        200
    );
    let (req, ..) = rollover(&stale, one, &id, &id);
    assert_eq!(m.post("/rollover", &req).await.0, 200);
    let (req, ..) = rollover(&real, one, &id, &id);
    assert_eq!(m.post("/rollover", &req).await.0, 400, "already spent");
    let spent = |memo: Option<&str>| Burn {
        spent: true,
        memo: memo.map(String::from),
    };
    assert_eq!(
        common::burns(&m.base, vec![stale.0.nonce, real.0.nonce]).await,
        [spent(None), spent(Some("real"))]
    );
}

/// The state a crash leaves in a rollover: recorded, holding its note, and
/// (`burn`) the note burned in the input set, with (`flag`) the burn also
/// recorded under the request hash. Returns the mint, its folder, the note and
/// the request, before the input set is retired.
async fn stuck_rollover(
    name: &str,
    burn: bool,
    flag: bool,
) -> (Mint, PathBuf, (Note, Keypair), RolloverRequest) {
    let dir = scratch(name);
    let one = Amount::from_units(1);
    let m = start(&dir, T).await;
    let n = m.issue(one);
    m.records_first_issue().await;
    m.set_clock(T + H);
    let ids: KeysetsResponse = m.get("/keysets").await;
    let (req, _, _) = rollover(&n, one, ids.previous.as_ref().unwrap(), &ids.current);
    {
        let mut logic = m.state.logic.lock().unwrap();
        let mut rotor = m.rot.rotor.lock().unwrap();
        rotor.refresh(&mut logic).unwrap();
        let burned: Vec<String> = if flag { vec![req.hash()] } else { vec![] };
        let nonce = hex::encode(n.0.nonce.0.serialize());
        std::fs::write(
            dir.join(psp_mint::rotation::ROLLOVERS_FILE),
            common::rollovers_file(
                serde_json::json!({"pending": {req.hash(): 1}, "burned": burned,
                "holds": {req.hash(): [nonce]}}),
            ),
        )
        .unwrap();
        if burn {
            let (k, l) = rotor.previous.as_ref().unwrap();
            let h = transaction_sighash(&req.inputs, &req.outputs);
            l.redeem_with_key(&k.sk[&one], &req.inputs[0], h).unwrap();
        }
    }
    *m.rot.rollovers.lock().unwrap() = Rollovers::open(&dir).unwrap();
    (m, dir, n, req)
}

/// A rollover burned before its keyset retired, its answer lost, retried after
/// the retirement. `burn`: the note was burned before the crash; `flag`: the
/// burn was also recorded under the request hash; `later`: the retry comes
/// after the set the outputs name has been deleted too. Returns the status, the
/// body and the ID of the set that should have signed: the set the request
/// names while that is still live, else the current one.
async fn rollover_after_retirement(
    name: &str,
    burn: bool,
    flag: bool,
    later: bool,
) -> (u16, serde_json::Value, String) {
    let (m, _, _, req) = stuck_rollover(name, burn, flag).await;
    m.set_clock(T + 2 * H);
    let after_one: RotationResponse = m.get("/rotation").await;
    assert_eq!(after_one.previous.unwrap().id, req.keyset, "still live");
    let expected = if later {
        m.set_clock(T + 3 * H);
        let after_two: RotationResponse = m.get("/rotation").await;
        assert_ne!(after_two.previous.unwrap().id, req.keyset, "deleted");
        after_two.current.id
    } else {
        req.keyset.clone()
    };
    let (code, body) = m.post("/rollover", &req).await;
    (code, body, expected)
}

#[tokio::test]
async fn a_resumed_rollover_after_retirement_finishes_only_with_stored_proof_of_its_burn() {
    // The input set is deleted, the set the outputs name is still live (it is
    // the previous set now): the outputs are signed by the set they name.
    let (code, body, signer) = rollover_after_retirement("proof-yes", true, true, false).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["keyset"], signer.as_str(), "signed by the named set");
    // Crash injected after the burn, before the flag: the proof taken from
    // the spent list at the retirement (saved with the retired ID) finishes it.
    let (code, body, signer) = rollover_after_retirement("proof-crash", true, false, false).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["keyset"], signer.as_str());
    // Crash before the burn: nothing proves it.
    let (code, body, _) = rollover_after_retirement("proof-no", false, false, false).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("burn_not_proven")),
        "{body}"
    );
    // The set the outputs name is deleted meanwhile too: the outputs are signed
    // by the current set.
    let (code, body, signer) = rollover_after_retirement("proof-late", true, true, true).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["keyset"], signer.as_str(), "signed by the current set");
}

/// The request with these outputs, every input signed again by its note's key.
fn resign(mut req: RolloverRequest, keys: &[&Keypair], outs: Vec<MintOutput>) -> RolloverRequest {
    let h = transaction_sighash(&req.inputs, &outs);
    for (input, key) in req.inputs.iter_mut().zip(keys) {
        input.spend_signature = SECP256K1.sign_schnorr(&h, key);
    }
    req.outputs = outs;
    req
}

/// A rollover and a spend (kind 14) of `n` that name a keyset the mint does not
/// know are refused `unknown_keyset`, and nothing is issued for them.
async fn forged_requests_are_refused(m: &Mint, n: &(Note, Keypair), current: &str) {
    let issued = m.get::<AuditResponse>("/audit").await.issued;
    let (forged, ..) = rollover(n, Amount::from_units(1024), &"ab".repeat(8), current);
    let (code, body) = m.post("/rollover", &forged).await;
    assert_eq!((code, body["code"].as_str()), (400, Some("unknown_keyset")));
    // The same note in a spend (kind 14): 1024 = 512 of change + 512 paid.
    let mut spend = PspSpendRequest {
        input_keysets: forged.input_keysets,
        inputs: forged.inputs,
        keyset: current.to_string(),
        outputs: vec![MintOutput {
            amount: Amount::from_units(512),
            ..forged.outputs[0].clone()
        }],
        pay: 512,
        memo: String::new(),
    };
    spend.inputs[0].spend_signature = SECP256K1.sign_schnorr(&spend.digest(), &n.1);
    let (code, body) = m.post("/spend", &spend).await;
    assert_eq!((code, body["code"].as_str()), (400, Some("unknown_keyset")));
    assert_eq!(m.get::<AuditResponse>("/audit").await.issued, issued);
}

#[tokio::test]
async fn a_burn_proof_left_on_file_serves_no_other_request() {
    // Fails if the guard lets a request that is not resuming use a proof
    // (P2-01), whatever the drop does: a crash between a request's finish and
    // the drop of its proof leaves the proof behind.
    let (m, dir, n, req) = stuck_rollover("proof-left", true, false).await;
    m.set_clock(T + 2 * H);
    let current = m.get::<RotationResponse>("/rotation").await.current.id;
    // The request is gone from the store; its proof is still on file.
    *m.rot.rollovers.lock().unwrap() = Rollovers::open(&scratch("proof-left-store")).unwrap();
    let nonce = hex::encode(n.0.nonce.0.serialize());
    let file = std::fs::read(dir.join(psp_mint::rotation::RETIRED_FILE)).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&file).unwrap();
    assert_eq!(
        saved["burn_proofs"],
        serde_json::json!({ req.hash(): [nonce] })
    );
    forged_requests_are_refused(&m, &n, &current).await;
}

#[tokio::test]
async fn a_burn_proof_is_kept_for_its_own_request_and_dropped_when_it_finishes() {
    // Fails if retirement saves the proof where any request can use it (P2-01):
    // after the stuck request is finished, a new rollover or spend naming the
    // proven note with a keyset the mint does not know is signed without
    // burning anything.
    let (m, dir, n, req) = stuck_rollover("proof-forged", true, false).await;
    m.set_clock(T + 2 * H);
    let current = m.get::<RotationResponse>("/rotation").await.current.id;
    let proofs = || -> serde_json::Value {
        let file = std::fs::read(dir.join(psp_mint::rotation::RETIRED_FILE)).unwrap();
        serde_json::from_slice::<serde_json::Value>(&file).unwrap()["burn_proofs"].clone()
    };
    let nonce = hex::encode(n.0.nonce.0.serialize());
    assert_eq!(proofs(), serde_json::json!({ req.hash(): [nonce] }));
    assert_eq!(m.post("/rollover", &req).await.0, 200);
    assert_eq!(proofs(), serde_json::json!({}), "its request is finished");
    forged_requests_are_refused(&m, &n, &current).await;
}

#[tokio::test]
async fn a_rollover_with_a_repeated_or_used_output_blind_nonce_is_refused_before_any_burn() {
    // Fails if the outputs are checked after the burn (P1-07): the notes are then
    // burned and the request stays pending, holding them.
    let one = Amount::from_units(1);
    let m = start(&scratch("outputs"), T).await;
    let [a, b, c, d] = [(); 4].map(|_| m.issue(one));
    let id = m.get::<KeysetsResponse>("/keysets").await.current;
    // Two notes of one unit into two outputs that share a blind nonce.
    let (mut req, ..) = rollover(&a, one, &id, &id);
    let (second, ..) = rollover(&b, one, &id, &id);
    req.inputs.extend(second.inputs);
    req.input_keysets.extend(second.input_keysets);
    let out = req.outputs[0].clone();
    let twice = resign(req, &[&a.1, &b.1], vec![out.clone(), out]);
    let (code, body) = m.post("/rollover", &twice).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("duplicate_blind_nonce"))
    );
    // An output blind nonce the set already signed.
    let (done, ..) = rollover(&c, one, &id, &id);
    assert_eq!(m.post("/rollover", &done).await.0, 200);
    let (again, ..) = rollover(&d, one, &id, &id);
    let (code, body) = m
        .post("/rollover", &resign(again, &[&d.1], done.outputs))
        .await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("blind_nonce_used"))
    );
    let burned = common::burns(&m.base, vec![a.0.nonce, b.0.nonce, d.0.nonce]).await;
    assert!(burned.iter().all(|n| !n.spent), "nothing burned");
    assert_eq!(
        m.rot.rollovers.lock().unwrap().holds().count(),
        0,
        "nothing pending"
    );
}

#[tokio::test]
async fn a_note_held_by_a_pending_rollover_cannot_be_melted_or_rolled_over_by_another_request() {
    let dir = scratch("holds");
    let one = Amount::from_units(1);
    let m = start(&dir, T).await;
    let n = m.issue(one);
    let ids: KeysetsResponse = m.get("/keysets").await;
    let (req, _, _) = rollover(&n, one, &ids.current, &ids.current);
    // Recorded and holding the note, then a crash before its burn.
    std::fs::write(
        dir.join(psp_mint::rotation::ROLLOVERS_FILE),
        common::rollovers_file(serde_json::json!({"pending": {req.hash(): 0},
            "holds": {req.hash(): [hex::encode(n.0.nonce.0.serialize())]}})),
    )
    .unwrap();
    *m.rot.rollovers.lock().unwrap() = Rollovers::open(&dir).unwrap();
    let (code, body) = m
        .post("/melt", &melt(&[n], one, "steal", &[&ids.current]))
        .await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("note_held")),
        "{body}"
    );
    let (other, _, _) = rollover(&n, one, &ids.current, &ids.current);
    let (code, body) = m.post("/rollover", &other).await;
    assert_eq!(code, 400, "{body}");
    assert!(body.to_string().contains("held"), "{body}");
    // The holder finishes.
    let (code, body) = m.post("/rollover", &req).await;
    assert_eq!(code, 200, "{body}");
}

#[tokio::test]
async fn the_rollover_has_its_own_body_ceiling_and_note_cap() {
    let dir = scratch("rollover-limits");
    let one = Amount::from_units(1);
    let m = start_with(
        &dir,
        T,
        psp_mint::server::Limits {
            rollover_bytes: 4096,
            rollover_notes: 1,
            ..Default::default()
        },
    )
    .await;
    let a = m.issue(one);
    let b = m.issue(one);
    m.records_first_issue().await;
    m.set_clock(T + H);
    let ids: KeysetsResponse = m.get("/keysets").await;
    let prev = ids.previous.clone().unwrap();

    // a body over the ceiling is refused before it is parsed, with the 413
    // body (over AIR, a signed refusal)
    let (code, body) = m.air.post_bytes("/rollover", &vec![b' '; 4097]);
    assert_eq!(code, 400, "{body}");
    assert_eq!(body["code"], "request_too_large", "{body}");
    assert_eq!(
        (body["limit_bytes"].as_u64(), body["size_bytes"].as_u64()),
        (Some(4096), Some(4097))
    );

    // two notes with the cap at one: refused, and nothing is burned
    let (r1, _, _) = rollover(&a, one, &prev, &ids.current);
    let (r2, _, _) = rollover(&b, one, &prev, &ids.current);
    let outputs = [r1.outputs.clone(), r2.outputs.clone()].concat();
    let mut inputs = [r1.inputs.clone(), r2.inputs.clone()].concat();
    let h = transaction_sighash(&inputs, &outputs);
    inputs[0].spend_signature = SECP256K1.sign_schnorr(&h, &a.1);
    inputs[1].spend_signature = SECP256K1.sign_schnorr(&h, &b.1);
    let both = RolloverRequest {
        input_keysets: vec![prev.clone(), prev.clone()],
        inputs,
        keyset: ids.current.clone(),
        outputs,
    };
    let (code, body) = m.post("/rollover", &both).await;
    assert_eq!(code, 400, "{body}");
    assert!(body.to_string().contains("1 to 1 notes"), "{body}");

    // one note at a time is within the cap
    let (code, body) = m.post("/rollover", &r1).await;
    assert_eq!(code, 200, "{body}");
    let (code, body) = m.post("/rollover", &r2).await;
    assert_eq!(code, 200, "{body}");
}
