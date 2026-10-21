//! A PSP spend with change over AIR (kind 14): notes in, change out, an amount
//! paid that the mint burns and never reissues.
//!
//! The spend goes through the mint's real listener on a real `air_server` (see
//! `common`), as a wallet's request does. The calls that stay on HTTP because
//! they are local to the SPI (`/melt`, `/audit`) go to the HTTP server.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use mint_server::api::{CheckStateRequest, CheckStateResponse};
use mint_server::state::AppState;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash};
use psp_mint::melts::MeltLog;
use psp_mint::rotation::{
    PspSpendRequest, PspSpendResponse, ROLLOVERS_FILE, Rollovers, RotationResponse, Rotor,
};
use psp_mint::server::{Health, KeysetsResponse, Limits, PspMeltRequest, Rotating, SharedRotating};
use rand::rngs::OsRng;
use secp256k1::{Keypair, SECP256K1, SecretKey};
use serde_json::Value;

const T: u64 = 1_800_000_000;
const H: u64 = 3600;

/// A note and the key that spends it.
type Held = (Note, Keypair);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("psp-mint-spend-{}-{name}", {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

struct Mint {
    base: String,
    state: Arc<AppState>,
    rot: SharedRotating,
    /// The wallet side of the mint's AIR listener. Holds the turn on the shared
    /// AIR server until the mint drops.
    air: common::Air,
}

async fn start(dir: &Path, clock: u64) -> Mint {
    start_with(dir, clock, Default::default()).await
}

async fn start_with(dir: &Path, clock: u64, limits: Limits) -> Mint {
    let (rotor, logic) = Rotor::open_at(dir, Amount::from_units(1024), 1, 1, clock).unwrap();
    let rot = Rotating::new(rotor, Rollovers::open(dir).unwrap());
    let state = psp_mint::server::app_state(logic);
    let melts = Arc::new(Mutex::new(
        MeltLog::open(&dir.join("melt_memos.json")).unwrap(),
    ));
    let air = common::Air::start(&state, &rot, limits);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = psp_mint::server::router_full(
        Arc::clone(&state),
        Health::new(100),
        melts,
        limits,
        Some(Arc::clone(&rot)),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mint {
        base: format!("http://{addr}"),
        state,
        rot,
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

    /// A note of `amount`, issued by the mint now.
    fn issue(&self, amount: u64) -> Held {
        let amount = Amount::from_units(amount);
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

    async fn keysets(&self) -> KeysetsResponse {
        self.air.get("/keysets")
    }

    async fn rotation(&self) -> RotationResponse {
        self.air.get("/rotation")
    }

    /// The current keyset's ID.
    async fn current(&self) -> String {
        self.keysets().await.current
    }

    /// `/audit`, as the SPI reads it.
    async fn audit(&self) -> Value {
        reqwest::get(format!("{}/audit", self.base))
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Issued, redeemed, outstanding and paid, in PSP units.
    async fn totals(&self) -> [u64; 4] {
        let a = self.audit().await;
        ["issued", "redeemed", "outstanding", "paid"].map(|k| a[k].as_u64().expect(k))
    }

    async fn spend(&self, req: &PspSpendRequest) -> (u16, Value) {
        self.air.post_json("/spend", req)
    }

    async fn melt(&self, note: &Held, units: u64, memo: &str, keyset: &str) -> u16 {
        let unsigned = MintInput {
            amount: Amount::from_units(units),
            note: note.0,
            spend_signature: zero_sig(),
        };
        let h = melt_sighash(std::slice::from_ref(&unsigned), memo.as_bytes());
        let req = PspMeltRequest {
            melt: sp_wire::psp::MeltRequest {
                inputs: vec![MintInput {
                    spend_signature: SECP256K1.sign_schnorr(&h, &note.1),
                    ..unsigned
                }],
                memo: memo.into(),
            },
            keysets: vec![keyset.to_string()],
        };
        reqwest::Client::new()
            .post(format!("{}/melt", self.base))
            .json(&req)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn spent(&self, notes: &[&Held]) -> Vec<bool> {
        let (code, body) = self.air.post_json(
            "/check-state",
            &CheckStateRequest {
                nonces: notes.iter().map(|n| n.0.nonce).collect(),
            },
        );
        assert_eq!(code, 200, "{body}");
        serde_json::from_value::<CheckStateResponse>(body)
            .unwrap()
            .spent
    }
}

fn zero_sig() -> secp256k1::schnorr::Signature {
    secp256k1::schnorr::Signature::from_slice(&[0u8; 64]).unwrap()
}

/// What the wallet keeps to unblind one change output.
struct Change {
    key: Keypair,
    blind: SecretKey,
    amount: u64,
}

struct Built {
    req: PspSpendRequest,
    change: Vec<Change>,
}

/// Sign every input over the request's digest.
fn sign_all(req: &mut PspSpendRequest, notes: &[&Held]) {
    let digest = req.digest();
    for (input, note) in req.inputs.iter_mut().zip(notes) {
        input.spend_signature = SECP256K1.sign_schnorr(&digest, &note.1);
    }
}

/// A spend of `notes` (each with its amount and keyset), with change of the
/// amounts `change` signed by `out_keyset`, paying `pay`.
fn build(
    notes: &[(&Held, u64, &str)],
    out_keyset: &str,
    change: &[u64],
    pay: u64,
    memo: &str,
) -> Built {
    let mut kept = Vec::new();
    let mut outputs = Vec::new();
    for &amount in change {
        let key = Keypair::new(SECP256K1, &mut OsRng);
        let (b, blind) = bdhke::blind_message(&Nonce(key.public_key()).as_hash_preimage(), None);
        outputs.push(MintOutput {
            amount: Amount::from_units(amount),
            blind_nonce: BlindNonce(b),
        });
        kept.push(Change { key, blind, amount });
    }
    let mut req = PspSpendRequest {
        input_keysets: notes.iter().map(|(_, _, k)| k.to_string()).collect(),
        inputs: notes
            .iter()
            .map(|(held, amount, _)| MintInput {
                amount: Amount::from_units(*amount),
                note: held.0,
                spend_signature: zero_sig(),
            })
            .collect(),
        keyset: out_keyset.to_string(),
        outputs,
        pay,
        memo: memo.to_string(),
    };
    let held: Vec<&Held> = notes.iter().map(|(h, _, _)| *h).collect();
    sign_all(&mut req, &held);
    Built { req, change: kept }
}

fn answer(body: Value) -> PspSpendResponse {
    serde_json::from_value(body).expect("a spend answer")
}

impl Built {
    /// The change notes the wallet gets from `answer`, each checked genuine by
    /// the mint that signed them.
    fn notes(&self, mint: &Mint, answer: &PspSpendResponse) -> Vec<Held> {
        assert_eq!(answer.outcomes.len(), self.change.len());
        let logic = mint.state.logic.lock().unwrap();
        self.change
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let signature =
                    bdhke::unblind_message(&answer.outcomes[i].0, &c.blind, &answer.keys[i]);
                let note = Note {
                    nonce: Nonce(c.key.public_key()),
                    signature,
                };
                logic
                    .verify_note(&MintInput {
                        amount: Amount::from_units(c.amount),
                        note,
                        spend_signature: zero_sig(),
                    })
                    .expect("the change is genuine");
                (note, c.key)
            })
            .collect()
    }
}

#[tokio::test]
async fn a_spend_with_change_burns_the_payment_and_signs_the_change() {
    let m = start(&scratch("change"), T).await;
    let (a, b) = (m.issue(4), m.issue(2));
    let cur = m.current().await;
    let built = build(&[(&a, 4, &cur), (&b, 2, &cur)], &cur, &[4], 2, "request 1");
    let (code, body) = m.spend(&built.req).await;
    assert_eq!(code, 200, "{body}");
    let got = answer(body.clone());
    assert_eq!((got.keyset.as_str(), got.paid), (cur.as_str(), 2));
    let change = built.notes(&m, &got);
    assert_eq!(change.len(), 1);
    assert_eq!(
        m.spent(&[&a, &b, &change[0]]).await,
        vec![true, true, false]
    );
    // 6 issued and burned whole, 4 issued as change: the 2 paid is gone for good.
    assert_eq!(m.totals().await, [10, 6, 4, 2]);

    // A retry gets the stored answer and burns nothing more.
    let (code, again) = m.spend(&built.req).await;
    assert_eq!((code, again), (200, body));
    assert_eq!(m.totals().await, [10, 6, 4, 2]);
}

#[tokio::test]
async fn an_exact_spend_needs_no_change_and_no_change_keyset() {
    let m = start(&scratch("exact"), T).await;
    let a = m.issue(4);
    let cur = m.current().await;
    let built = build(&[(&a, 4, &cur)], "", &[], 4, "");
    let (code, body) = m.spend(&built.req).await;
    assert_eq!(code, 200, "{body}");
    let got = answer(body);
    assert!(got.outcomes.is_empty() && got.keys.is_empty());
    assert_eq!((got.keyset.as_str(), got.paid), (cur.as_str(), 4));
    assert_eq!(m.spent(&[&a]).await, vec![true]);
    assert_eq!(m.totals().await, [4, 4, 0, 4]);
}

#[tokio::test]
async fn inputs_that_are_not_change_plus_pay_are_refused_and_nothing_is_burned() {
    let m = start(&scratch("not-exact"), T).await;
    let a = m.issue(4);
    let cur = m.current().await;
    let cases = [
        // 4 in, 2 + 1 out
        build(&[(&a, 4, &cur)], &cur, &[2], 1, ""),
        // 4 in, 4 + 1 out
        build(&[(&a, 4, &cur)], &cur, &[4], 1, ""),
        // The sum is taken in u128: a huge pay cannot wrap around to 4.
        build(&[(&a, 4, &cur)], &cur, &[4], u64::MAX, ""),
    ];
    for built in cases {
        let (code, body) = m.spend(&built.req).await;
        assert_eq!(
            (code, body["code"].as_str()),
            (400, Some("not_exact")),
            "{body}"
        );
    }
    assert_eq!(m.spent(&[&a]).await, vec![false]);
    assert_eq!(m.totals().await, [4, 0, 4, 0]);
}

#[tokio::test]
async fn a_spend_that_pays_nothing_is_refused_as_a_plain_rollover() {
    let m = start(&scratch("pay-zero"), T).await;
    let a = m.issue(4);
    let cur = m.current().await;
    let built = build(&[(&a, 4, &cur)], &cur, &[4], 0, "");
    let (code, body) = m.spend(&built.req).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("nothing_to_pay")),
        "{body}"
    );
    assert_eq!(m.spent(&[&a]).await, vec![false]);
    assert_eq!(m.totals().await, [4, 0, 4, 0]);
}

#[tokio::test]
async fn a_spent_note_is_refused_and_the_body_says_which() {
    let m = start(&scratch("spent"), T).await;
    let (a, b) = (m.issue(4), m.issue(2));
    let cur = m.current().await;
    // `b` is burned first (an exchange's melt, say), then spent in a spend with `a`.
    assert_eq!(m.melt(&b, 2, "exchange", &cur).await, 200);
    let built = build(&[(&a, 4, &cur), (&b, 2, &cur)], &cur, &[4], 2, "");
    let (code, body) = m.spend(&built.req).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("note_spent_elsewhere")),
        "{body}"
    );
    assert_eq!(body["spent"], serde_json::json!([1]), "{body}");
    assert_eq!(
        m.spent(&[&a, &b]).await,
        vec![false, true],
        "`a` was not burned"
    );
    assert_eq!(m.totals().await, [6, 2, 4, 0]);
}

#[tokio::test]
async fn the_same_note_in_two_spends_is_spent_by_the_first_only() {
    let m = start(&scratch("twice"), T).await;
    let a = m.issue(4);
    let cur = m.current().await;
    let first = build(&[(&a, 4, &cur)], &cur, &[2], 2, "first");
    assert_eq!(m.spend(&first.req).await.0, 200);
    let second = build(&[(&a, 4, &cur)], &cur, &[2], 2, "second");
    let (code, body) = m.spend(&second.req).await;
    assert_eq!(
        (code, body["code"].as_str(), &body["spent"]),
        (400, Some("note_spent_elsewhere"), &serde_json::json!([0])),
        "{body}"
    );
    assert_eq!(m.totals().await, [6, 4, 2, 2], "paid once");
}

#[tokio::test]
async fn keysets_are_live_the_way_a_rollover_needs_them() {
    let dir = scratch("keysets");
    let m = start(&dir, T).await;
    let (a, b, c) = (m.issue(4), m.issue(4), m.issue(4));
    let _ = m.rotation().await; // records the first issue
    m.set_clock(T + H);
    let ks = m.keysets().await;
    let (id0, id1) = (ks.previous.clone().unwrap(), ks.current.clone());

    // A note of the previous set, change signed by the current one.
    let built = build(&[(&a, 4, &id0)], &id1, &[2], 2, "to current");
    let (code, body) = m.spend(&built.req).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(answer(body).keyset, id1);
    // The change may be signed by the previous set, if the request names it.
    let built = build(&[(&b, 4, &id0)], &id0, &[2], 2, "to previous");
    let (code, body) = m.spend(&built.req).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(answer(body).keyset, id0);

    // After a full cycle the first set is deleted with its spent list.
    m.set_clock(T + 2 * H);
    let ks = m.keysets().await;
    let id2 = ks.current.clone();
    let d = m.issue(4);
    let retired = build(&[(&c, 4, &id0)], &id2, &[2], 2, "late");
    let (code, body) = m.spend(&retired.req).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("keyset_retired")),
        "{body}"
    );
    let unknown = build(&[(&d, 4, &"00".repeat(8))], &id2, &[2], 2, "");
    let (code, body) = m.spend(&unknown.req).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("unknown_keyset")),
        "{body}"
    );
    // The change cannot be asked of a set that is not live either.
    let gone_out = build(&[(&d, 4, &id2)], &id0, &[2], 2, "");
    let (code, body) = m.spend(&gone_out.req).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("keyset_retired")),
        "{body}"
    );
    let unknown_out = build(&[(&d, 4, &id2)], &"00".repeat(8), &[2], 2, "");
    let (code, body) = m.spend(&unknown_out.req).await;
    assert_eq!(
        (code, body["code"].as_str()),
        (400, Some("unknown_keyset")),
        "{body}"
    );
    assert_eq!(m.spent(&[&d]).await, vec![false], "`d` was never burned");
    // The audit still counts the deleted set: 4 notes of 4 and two changes of 2
    // issued; the two spent notes redeemed whole; 2 + 2 paid.
    assert_eq!(m.totals().await, [20, 8, 12, 4]);
}

#[tokio::test]
async fn the_payment_is_counted_apart_from_what_exchanges_burn() {
    let m = start(&scratch("apart"), T).await;
    let (a, b) = (m.issue(2), m.issue(4));
    let cur = m.current().await;
    // The ESP mint's exchange burns through `/melt`: it is redeemed, not paid.
    assert_eq!(m.melt(&a, 2, "exchange", &cur).await, 200);
    assert_eq!(m.totals().await, [6, 2, 4, 0]);
    let built = build(&[(&b, 4, &cur)], "", &[], 4, "service fee");
    assert_eq!(m.spend(&built.req).await.0, 200);
    assert_eq!(m.totals().await, [6, 6, 0, 4]);
}

#[tokio::test]
async fn a_spend_cut_short_after_the_burn_is_finished_by_the_retry() {
    // `claimed`: a change output was also claimed before the crash (the crash
    // came while signing); otherwise it came right after the burn.
    for claimed in [false, true] {
        let dir = scratch(if claimed {
            "crash-signing"
        } else {
            "crash-burn"
        });
        let m = start(&dir, T).await;
        let a = m.issue(4);
        let cur = m.current().await;
        let built = build(&[(&a, 4, &cur)], &cur, &[1, 1], 2, "crash");
        let hash = built.req.hash();
        // The state a crash leaves: pending and holding the note, the note
        // burned, no answer stored.
        std::fs::write(
            dir.join(ROLLOVERS_FILE),
            serde_json::json!({"answers": {}, "pending": {hash.clone(): 0},
                "holds": {hash.clone(): [hex::encode(a.0.nonce.0.serialize())]}})
            .to_string(),
        )
        .unwrap();
        {
            let logic = m.state.logic.lock().unwrap();
            let rotor = m.rot.rotor.lock().unwrap();
            let sk = |u: u64| rotor.current.sk[&Amount::from_units(u)];
            logic
                .redeem_with_key(&sk(4), &built.req.inputs[0], built.req.digest())
                .unwrap();
            if claimed {
                logic.issue_with_key(&sk(1), &built.req.outputs[0]).unwrap();
            }
        }
        // The restart: the state is read back from disk.
        *m.rot.rollovers.lock().unwrap() = Rollovers::open(&dir).unwrap();

        // Until the holder finishes, no one else can take the note.
        let other = build(&[(&a, 4, &cur)], &cur, &[1, 1], 2, "someone else");
        let (code, body) = m.spend(&other.req).await;
        assert_eq!(
            (code, body["code"].as_str()),
            (400, Some("note_held")),
            "{body}"
        );

        let (code, body) = m.spend(&built.req).await;
        assert_eq!(code, 200, "claimed={claimed}: {body}");
        let got = answer(body.clone());
        assert_eq!(built.notes(&m, &got).len(), 2, "both outputs signed");
        // Counted once: 4 issued and burned, 2 issued as change, 2 paid.
        assert_eq!(m.totals().await, [6, 4, 2, 2], "claimed={claimed}");
        let (code, again) = m.spend(&built.req).await;
        assert_eq!((code, again), (200, body));
        assert_eq!(m.totals().await, [6, 4, 2, 2]);
        assert_eq!(
            Rollovers::open(&dir).unwrap().paid(),
            2,
            "the payment is on disk once"
        );
    }
}

#[tokio::test]
async fn the_shape_of_a_spend_is_checked_before_anything_is_burned() {
    let limits = Limits {
        rollover_bytes: 4096,
        rollover_notes: 2,
        ..Default::default()
    };
    let m = start_with(&scratch("shape"), T, limits).await;
    let (a, b, c) = (m.issue(2), m.issue(2), m.issue(2));
    let cur = m.current().await;
    let code_of = |body: &Value| body["code"].as_str().map(str::to_string);

    // No notes.
    let none = build(&[], &cur, &[2], 2, "");
    let (code, body) = m.spend(&none.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("no_inputs".into())),
        "{body}"
    );
    // More notes than the cap (2).
    let three = build(
        &[(&a, 2, &cur), (&b, 2, &cur), (&c, 2, &cur)],
        "",
        &[],
        6,
        "",
    );
    let (code, body) = m.spend(&three.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("too_many_items".into())),
        "{body}"
    );
    // A memo over 256 bytes.
    let long = build(&[(&a, 2, &cur)], "", &[], 2, &"x".repeat(257));
    let (code, body) = m.spend(&long.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("memo_too_long".into())),
        "{body}"
    );
    // Change with no keyset to sign it, and a keyset missing for a note.
    let no_keyset = build(&[(&a, 2, &cur)], "", &[1], 1, "");
    let (code, body) = m.spend(&no_keyset.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("missing_keyset".into())),
        "{body}"
    );
    let mut no_input_keyset = build(&[(&a, 2, &cur)], &cur, &[1], 1, "");
    no_input_keyset.req.input_keysets.clear();
    sign_all(&mut no_input_keyset.req, &[&a]);
    let (code, body) = m.spend(&no_input_keyset.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("missing_keyset".into())),
        "{body}"
    );
    // The same note twice, and the same blind nonce twice.
    let twice = build(&[(&a, 2, &cur), (&a, 2, &cur)], "", &[], 4, "");
    let (code, body) = m.spend(&twice.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("duplicate_input".into())),
        "{body}"
    );
    let mut repeat = build(&[(&a, 2, &cur), (&b, 2, &cur)], &cur, &[1, 1], 2, "");
    repeat.req.outputs[1] = repeat.req.outputs[0].clone();
    sign_all(&mut repeat.req, &[&a, &b]);
    let (code, body) = m.spend(&repeat.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("duplicate_blind_nonce".into())),
        "{body}"
    );
    // 3 is not a denomination.
    let odd = build(&[(&a, 4, &cur)], &cur, &[3], 1, "");
    let (code, body) = m.spend(&odd.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("bad_denomination".into())),
        "{body}"
    );
    // A spend signature that covers another request, and a note that is no note.
    let mut tampered = build(&[(&a, 2, &cur)], "", &[], 2, "as signed");
    tampered.req.memo = "as sent".into();
    let (code, body) = m.spend(&tampered.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("bad_spend_signature".into())),
        "{body}"
    );
    let mut forged = build(&[(&a, 2, &cur)], "", &[], 2, "");
    forged.req.inputs[0].note.signature = b.0.signature;
    sign_all(&mut forged.req, &[&a]);
    let (code, body) = m.spend(&forged.req).await;
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("bad_note".into())),
        "{body}"
    );
    // A body over the ceiling is refused before it is parsed.
    let (code, body) = m.air.post_bytes("/spend", &vec![b' '; 4097]);
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("request_too_large".into())),
        "{body}"
    );
    let (code, body) = m.air.post_bytes("/spend", b"not json");
    assert_eq!(
        (code, code_of(&body)),
        (400, Some("malformed_request".into())),
        "{body}"
    );

    assert_eq!(
        m.spent(&[&a, &b, &c]).await,
        vec![false; 3],
        "nothing was burned"
    );
    assert_eq!(m.totals().await, [6, 0, 6, 0]);
}

#[tokio::test]
async fn a_rollovers_file_from_before_spends_still_reads() {
    let dir = scratch("old-file");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(ROLLOVERS_FILE),
        serde_json::json!({"answers": {}, "pending": {}}).to_string(),
    )
    .unwrap();
    let m = start(&dir, T).await;
    let a = m.issue(4);
    let cur = m.current().await;
    assert_eq!(
        m.totals().await,
        [4, 0, 4, 0],
        "no paid yet, and the audit has the field"
    );
    let built = build(&[(&a, 4, &cur)], "", &[], 4, "");
    assert_eq!(m.spend(&built.req).await.0, 200);
    assert_eq!(m.totals().await, [4, 4, 0, 4]);
    assert_eq!(Rollovers::open(&dir).unwrap().paid(), 4);
}
