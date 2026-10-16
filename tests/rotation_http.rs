//! Key rotation over HTTP: generation 0 is the existing mint, the first issue
//! starts the clock, notes of the previous set are spendable and can be rolled
//! over until the next rotation, then they are lost.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use mint_server::api::{AuditResponse, CheckStateRequest, CheckStateResponse, MeltRequest};
use mint_server::state::AppState;
use mint_types::{
    Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash, transaction_sighash,
};
use psp_mint::melts::MeltLog;
use psp_mint::rotation::{RolloverRequest, RolloverResponse, Rollovers, RotationResponse, Rotor};
use psp_mint::server::{Health, KeysetsResponse, PspMeltRequest, Rotating, SharedRotating};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1, SecretKey};

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
}

async fn start(dir: &PathBuf, clock: u64) -> Mint {
    let (rotor, logic) = Rotor::open_at(dir, Amount::from_units(1024), 1, 1, clock).unwrap();
    let rot = Rotating::new(rotor, Rollovers::open(dir).unwrap());
    let state = psp_mint::server::app_state(logic);
    let melts = Arc::new(Mutex::new(
        MeltLog::open(&dir.join("melt_memos.json")).unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = psp_mint::server::router_full(
        Arc::clone(&state),
        Health::new(100),
        melts,
        Default::default(),
        Some(Arc::clone(&rot)),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mint {
        base: format!("http://{addr}"),
        state,
        rot,
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
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        reqwest::get(format!("{}{path}", self.base))
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn post(&self, path: &str, body: &impl serde::Serialize) -> (u16, serde_json::Value) {
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
        melt: MeltRequest {
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
    // An existing mint from before rotation: its keys are generation 0.
    let keys0 = psp_mint::mint::open(&dir, Amount::from_units(1024))
        .unwrap()
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
    // Outputs asked of the previous set (now `id1`) are refused: new signing
    // is by the current set only, and the error names it.
    let (prev_out, _, _) = rollover(&fresh, one, &id1, &id1);
    let (code, body) = m.post("/rollover", &prev_out).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("keyset_not_current")),
        "{body}"
    );
    assert!(body["error"].as_str().unwrap().contains(&id2));
    let ks: KeysetsResponse = m.get("/keysets").await;
    assert!(
        !ks.keysets.contains_key(&id0),
        "the retired set's keys are gone"
    );
    assert_eq!(
        m.post("/melt", &melt(&[fresh], one, "z", &[&id1])).await.0,
        200
    );
    // The audit still counts the deleted generation: 3 issued + 1 rolled over;
    // a, b (rolled), fresh redeemed.
    let audit: AuditResponse = m.get("/audit").await;
    assert_eq!((audit.issued.units, audit.redeemed.units), (4, 3));
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
    let _: RotationResponse = m.get("/rotation").await;
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
    let _: RotationResponse = m.get("/rotation").await;
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
            serde_json::json!({"answers": {}, "pending": {req.hash(): 1}}).to_string(),
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
    let _: RotationResponse = m.get("/rotation").await;
    m.set_clock(T + H);
    let _: RotationResponse = m.get("/rotation").await;
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

/// A rollover burned before its keyset retired, its answer lost, retried after
/// the retirement. `burn`: the note was burned before the crash; `flag`: the
/// burn was also recorded under the request hash.
async fn rollover_after_retirement(
    name: &str,
    burn: bool,
    flag: bool,
) -> (u16, serde_json::Value, String) {
    let dir = scratch(name);
    let one = Amount::from_units(1);
    let m = start(&dir, T).await;
    let n = m.issue(one);
    let _: RotationResponse = m.get("/rotation").await;
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
            serde_json::json!({"answers": {}, "pending": {req.hash(): 1}, "burned": burned,
                "holds": {req.hash(): [nonce]}})
            .to_string(),
        )
        .unwrap();
        if burn {
            let (k, l) = rotor.previous.as_ref().unwrap();
            let h = transaction_sighash(&req.inputs, &req.outputs);
            l.redeem_with_key(&k.sk[&one], &req.inputs[0], h).unwrap();
        }
    }
    *m.rot.rollovers.lock().unwrap() = Rollovers::open(&dir).unwrap();
    m.set_clock(T + 2 * H);
    let w: RotationResponse = m.get("/rotation").await;
    let (code, body) = m.post("/rollover", &req).await;
    (code, body, w.current.id)
}

#[tokio::test]
async fn a_resumed_rollover_after_retirement_finishes_only_with_stored_proof_of_its_burn() {
    let (code, body, current) = rollover_after_retirement("proof-yes", true, true).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        body["keyset"],
        current.as_str(),
        "signed by the current set"
    );
    // Crash injected after the burn, before the flag: the proof taken from
    // the spent list at the retirement (saved with the retired ID) finishes it.
    let (code, body, current) = rollover_after_retirement("proof-crash", true, false).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["keyset"], current.as_str());
    // Crash before the burn: nothing proves it.
    let (code, body, _) = rollover_after_retirement("proof-no", false, false).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("burn_not_proven")),
        "{body}"
    );
}

#[tokio::test]
async fn every_keyset_field_is_in_the_rollover_request_hash() {
    let dir = scratch("hash-keysets");
    let one = Amount::from_units(1);
    let m = start(&dir, T).await;
    let n = m.issue(one);
    let (req, _, _) = rollover(&n, one, &"ab".repeat(8), &"cd".repeat(8));
    let mut a = req.clone();
    a.input_keysets = vec!["ef".repeat(8)];
    let mut b = req.clone();
    b.keyset = "ef".repeat(8);
    assert!(req.hash() != a.hash() && req.hash() != b.hash() && a.hash() != b.hash());
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
        serde_json::json!({"answers": {}, "pending": {req.hash(): 0},
            "holds": {req.hash(): [hex::encode(n.0.nonce.0.serialize())]}})
        .to_string(),
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
