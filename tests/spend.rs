//! A PSP spend with change over AIR (kind 14): notes in, change out, an amount
//! paid. The mint reissues what is paid to the SPI's own ecash-wallet (a file in
//! `revenue/`), or burns it when `burn.enabled` is on and the SPI's wallet already
//! holds `burn.threshold_psp` (the wallet's balance as `spi` last published it
//! in `wallet_balance.json`, plus what the mint has reissued since).
//!
//! The spend goes through the mint's real listener on a real `air_server` (see
//! `common`), as a wallet's request does. The calls that stay on HTTP because
//! they are local to the SPI (`/melt`, `/audit`) go to the HTTP server.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use mint_core::state::AppState;
use mint_types::{Amount, BlindNonce, MintInput, MintOutput, Nonce, Note, melt_sighash};
use psp_mint::balance::{self, Figure};
use psp_mint::rotation::{
    BurnPolicy, PspSpendRequest, PspSpendResponse, REVENUE_DIR, ROLLOVERS_FILE, Rollovers,
    RotationResponse,
};
use psp_mint::server::{
    Burn, KeysetsResponse, Limits, PspMeltRequest, SharedMelts, SharedRotating,
};
use rand::rngs::OsRng;
use secp256k1::{Keypair, PublicKey, SECP256K1, SecretKey};
use serde_json::Value;
use sp_wire::keys::{CheckStateRequest, CheckStateResponse};
use sp_wire::psp::PspHandover;

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
    /// The mint's data folder.
    dir: PathBuf,
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

async fn start_with(dir: &Path, clock: u64, limits: Limits) -> Mint {
    let mint = common::PspMint::open_in(dir, 1, 1, clock, limits);
    let air = mint.air();
    let base = mint.http().await;
    Mint {
        dir: dir.to_path_buf(),
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

    /// Ask for the rotation, which records the first issue and so starts the
    /// clock, and check that it and the keysets name the same current set.
    async fn records_first_issue(&self) {
        let rotation = self.rotation().await;
        assert_eq!(rotation.current.id, self.keysets().await.current);
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
        let total = |k: &str| a[k].as_str().expect(k).parse().expect(k);
        [
            total("issued"),
            total("redeemed"),
            total("outstanding"),
            a["paid"].as_u64().expect("paid"),
        ]
    }

    /// Of what was paid, the units burned and the units reissued, as `/audit`
    /// gives them.
    async fn paid_split(&self) -> [u64; 2] {
        let a = self.audit().await;
        ["burned", "reissued"].map(|k| a[k].as_u64().expect(k))
    }

    /// The mint's `reissued` total, as `/audit` gives it.
    async fn reissued(&self) -> u64 {
        self.audit().await["reissued"].as_u64().expect("reissued")
    }

    /// What `spi` does: the wallet holds `psp` now. It reads the wallet first and
    /// the mint's `reissued` second, and writes both with the time.
    async fn wallet_holds(&self, psp: u64) {
        let reissued = self.reissued().await;
        publish(&self.dir, psp, reissued, psp_mint::server::now_secs());
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
            melt: sp_wire::melt::MeltRequest {
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

/// The figure `spi` publishes for the mint, written to the mint's data folder.
fn publish(dir: &Path, psp: u64, reissued: u64, at: u64) {
    let text = serde_json::to_string(&Figure { psp, reissued, at }).unwrap();
    std::fs::write(dir.join(balance::FILE), text).unwrap();
}

/// The file the mint wrote in `revenue/` for the spend with request hash `hash`,
/// read as the SPI's wallet reads it.
fn revenue_file(dir: &Path, hash: &str) -> PspHandover {
    let path = dir.join(REVENUE_DIR).join(format!("{hash}.json"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    PspHandover::read(&bytes).unwrap()
}

/// Each note in `file` is genuine (signed by the mint) and its spend secret is
/// the secret of its nonce.
fn assert_genuine(m: &Mint, file: &PspHandover) {
    let logic = m.state.logic.lock().unwrap();
    for held in &file.notes {
        assert_eq!(
            held.note.nonce.0,
            PublicKey::from_secret_key(SECP256K1, &held.spend_secret),
            "the spend secret is the nonce's"
        );
        logic
            .verify_note(&MintInput {
                amount: Amount::from_units(held.amount),
                note: held.note,
                spend_signature: zero_sig(),
            })
            .expect("the reissued note is genuine");
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
async fn a_spend_with_change_reissues_the_payment_and_signs_the_change() {
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
    // 6 issued and burned whole; 4 issued as change and 2 reissued to the SPI's
    // wallet (burning is off): the 2 paid is outstanding again, in a file.
    assert_eq!(m.totals().await, [12, 6, 6, 2]);
    assert_eq!(m.paid_split().await, [0, 2]);
    let file = revenue_file(&m.dir, &built.req.hash());
    assert_eq!(file.units(), 2);
    assert_eq!(file.notes[0].keyset, cur);
    assert_genuine(&m, &file);

    // A retry gets the stored answer and reissues nothing more.
    let (code, again) = m.spend(&built.req).await;
    assert_eq!((code, again), (200, body));
    assert_eq!(m.totals().await, [12, 6, 6, 2]);
    assert_eq!(revenue_file(&m.dir, &built.req.hash()), file);
}

#[tokio::test]
async fn a_spend_clears_the_stale_memo_of_a_note_it_burns() {
    // Fails if a spend leaves the memo of an earlier `/melt` on an unspent note it
    // burns: the note is then reported burned under that memo (P1-01).
    let m = start(&scratch("memo"), T).await;
    let note = m.issue(2);
    let cur = m.current().await;
    m.melts
        .lock()
        .unwrap()
        .record(&[note.0.nonce], "stale")
        .unwrap();
    let built = build(&[(&note, 2, &cur)], &cur, &[1], 1, "pay");
    let (code, body) = m.spend(&built.req).await;
    assert_eq!(code, 200, "{body}");
    let burn = Burn {
        spent: true,
        memo: None,
    };
    assert_eq!(common::burns(&m.base, vec![note.0.nonce]).await, [burn]);
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
    assert_eq!(m.totals().await, [8, 4, 4, 4]);
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
    assert_eq!(m.totals().await, [8, 4, 4, 2], "paid once");
}

#[tokio::test]
async fn keysets_are_live_the_way_a_rollover_needs_them() {
    let dir = scratch("keysets");
    let m = start(&dir, T).await;
    let (a, b, c) = (m.issue(4), m.issue(4), m.issue(4));
    m.records_first_issue().await;
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
    // issued, and the 2 + 2 paid reissued; the two spent notes redeemed whole.
    assert_eq!(m.totals().await, [24, 8, 16, 4]);
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
    assert_eq!(m.totals().await, [10, 6, 4, 4]);
}

#[tokio::test]
async fn a_spend_cut_short_after_the_burn_is_finished_by_the_retry() {
    // `claimed`: a change output and the reissued note were also claimed before
    // the crash (the crash came while signing); otherwise it came right after
    // the burn.
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
        // burned, no answer stored, and the note planned for the SPI's wallet
        // (saved with `pending`, before anything was burned).
        let planned_nonce = SecretKey::from_slice(&[0x0b; 32]).unwrap();
        let planned_blind = SecretKey::from_slice(&[0x0c; 32]).unwrap();
        std::fs::write(
            dir.join(ROLLOVERS_FILE),
            common::rollovers_file(serde_json::json!({"pending": {hash.clone(): 0},
                "holds": {hash.clone(): [hex::encode(a.0.nonce.0.serialize())]},
                "plans": {hash.clone(): [{"amount": 2, "keyset": cur,
                    "nonce_secret": planned_nonce, "blinding_key": planned_blind}]}})),
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
                let (blinded, _) = bdhke::blind_message(
                    &Nonce(PublicKey::from_secret_key(SECP256K1, &planned_nonce))
                        .as_hash_preimage(),
                    Some(planned_blind),
                );
                logic
                    .issue_with_key(
                        &sk(2),
                        &MintOutput {
                            amount: Amount::from_units(2),
                            blind_nonce: BlindNonce(blinded),
                        },
                    )
                    .unwrap();
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
        // Counted once: 4 issued and burned, 2 issued as change, 2 paid and
        // reissued.
        assert_eq!(m.totals().await, [8, 4, 4, 2], "claimed={claimed}");
        let (code, again) = m.spend(&built.req).await;
        assert_eq!((code, again), (200, body));
        assert_eq!(m.totals().await, [8, 4, 4, 2]);
        let on_disk = Rollovers::open(&dir).unwrap();
        assert_eq!(
            (on_disk.paid(), on_disk.paid_kept(), on_disk.paid_burned()),
            (2, 2, 0),
            "the payment is on disk once"
        );
        // The note in the file is the planned one, made again after the crash
        // (and, when its blind nonce was already claimed, signed again).
        let file = revenue_file(&dir, &hash);
        assert_eq!(file.notes.len(), 1, "claimed={claimed}");
        assert_eq!(file.notes[0].amount, 2);
        assert_eq!(file.notes[0].spend_secret, planned_nonce);
        assert_eq!(file.notes[0].keyset, cur);
        assert_genuine(&m, &file);
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
async fn paid_psp_above_the_burn_threshold_is_burned_and_the_rest_reissued() {
    // Burning on, with the SPI's wallet to be kept at 3 PSP. The wallet is empty
    // when `spi` publishes its balance and takes in nothing more, so what it
    // holds is what the mint has reissued since: the first payment is reissued
    // whole, the second only up to the threshold, the third not at all.
    let limits = Limits {
        burn: BurnPolicy {
            enabled: true,
            threshold_psp: 3,
        },
        ..Default::default()
    };
    let dir = scratch("burn");
    let m = start_with(&dir, T, limits).await;
    m.wallet_holds(0).await;
    let (a, b, c) = (m.issue(4), m.issue(4), m.issue(4));
    let cur = m.current().await;

    let first = build(&[(&a, 4, &cur)], &cur, &[2], 2, "one");
    assert_eq!(m.spend(&first.req).await.0, 200);
    assert_eq!(m.paid_split().await, [0, 2]);
    assert_eq!(revenue_file(&dir, &first.req.hash()).units(), 2);

    let second = build(&[(&b, 4, &cur)], &cur, &[2], 2, "two");
    assert_eq!(m.spend(&second.req).await.0, 200);
    assert_eq!(m.paid_split().await, [1, 3], "1 burned, 1 reissued");
    let file = revenue_file(&dir, &second.req.hash());
    assert_eq!(file.units(), 1);
    assert_genuine(&m, &file);

    let third = build(&[(&c, 4, &cur)], &cur, &[2], 2, "three");
    assert_eq!(m.spend(&third.req).await.0, 200);
    assert_eq!(m.paid_split().await, [3, 3], "all of the third burned");
    assert!(
        !dir.join(REVENUE_DIR)
            .join(format!("{}.json", third.req.hash()))
            .exists(),
        "nothing reissued above the threshold"
    );
    // 12 issued + 3 changes of 2 + 3 reissued; 3 notes of 4 redeemed; 6 paid.
    assert_eq!(m.totals().await, [21, 12, 9, 6]);
}

#[tokio::test]
async fn burning_on_with_a_zero_threshold_burns_every_payment_when_the_figure_is_usable() {
    let limits = Limits {
        burn: BurnPolicy {
            enabled: true,
            threshold_psp: 0,
        },
        ..Default::default()
    };
    let dir = scratch("burn-all");
    let m = start_with(&dir, T, limits).await;
    m.wallet_holds(0).await;
    let a = m.issue(4);
    let cur = m.current().await;
    let built = build(&[(&a, 4, &cur)], &cur, &[2], 2, "all burned");
    assert_eq!(m.spend(&built.req).await.0, 200);
    assert_eq!(m.paid_split().await, [2, 0]);
    assert_eq!(m.totals().await, [6, 4, 2, 2]);
    assert!(!dir.join(REVENUE_DIR).exists(), "no file for nothing");
}

/// A mint with burning on, `threshold` PSP, and `n` notes of 4 to spend.
async fn burning(name: &str, threshold: u64, n: usize) -> (Mint, PathBuf, Vec<Held>, String) {
    let limits = Limits {
        burn: BurnPolicy {
            enabled: true,
            threshold_psp: threshold,
        },
        ..Default::default()
    };
    let dir = scratch(name);
    let m = start_with(&dir, T, limits).await;
    let notes = (0..n).map(|_| m.issue(4)).collect();
    let cur = m.current().await;
    (m, dir, notes, cur)
}

/// Pay 2 from a note of 4 (change 2) and check the spend went through.
async fn pay_two(m: &Mint, note: &Held, cur: &str, memo: &str) -> PspSpendRequest {
    let built = build(&[(note, 4, cur)], cur, &[2], 2, memo);
    let (code, body) = m.spend(&built.req).await;
    assert_eq!(code, 200, "{memo}: {body}");
    built.req
}

#[tokio::test]
async fn burning_follows_what_the_wallet_holds_up_to_the_threshold_and_back_down() {
    // Threshold 3, each payment 2. Burned and reissued totals after each spend:
    // under (2 held: room for 1), at (3 held: none), over (4 held: none), then
    // the wallet spends and holds 0 again: room for the whole payment.
    let (m, _dir, notes, cur) = burning("burn-follows", 3, 5).await;

    m.wallet_holds(1).await;
    pay_two(&m, &notes[0], &cur, "under").await;
    assert_eq!(m.paid_split().await, [0, 2], "1 held, room for 2");

    // The wallet took the 2 in: 3 held, which is the threshold.
    m.wallet_holds(3).await;
    pay_two(&m, &notes[1], &cur, "at").await;
    assert_eq!(m.paid_split().await, [2, 2], "at the threshold, none kept");

    m.wallet_holds(2).await;
    pay_two(&m, &notes[2], &cur, "one under").await;
    assert_eq!(m.paid_split().await, [3, 3], "2 held, room for 1");

    m.wallet_holds(4).await;
    pay_two(&m, &notes[3], &cur, "over").await;
    assert_eq!(
        m.paid_split().await,
        [5, 3],
        "over the threshold, none kept"
    );

    m.wallet_holds(0).await;
    pay_two(&m, &notes[4], &cur, "spent").await;
    assert_eq!(
        m.paid_split().await,
        [5, 5],
        "the wallet spent, so the payment is kept again"
    );
}

#[tokio::test]
async fn units_reissued_since_the_figure_count_as_held_and_a_figure_from_before_does_not() {
    // `spi` published 0 held and 0 reissued. Two payments of 2 later the wallet
    // has not yet taken them in, but they are the wallet's: the third payment
    // finds 4 held and the threshold of 5 leaves room for 1.
    let (m, _dir, notes, cur) = burning("burn-since", 5, 3).await;
    m.wallet_holds(0).await;
    pay_two(&m, &notes[0], &cur, "one").await;
    pay_two(&m, &notes[1], &cur, "two").await;
    assert_eq!(m.paid_split().await, [0, 4]);
    pay_two(&m, &notes[2], &cur, "three").await;
    assert_eq!(m.paid_split().await, [1, 5], "4 held, room for 1");
}

#[tokio::test]
async fn without_a_usable_figure_every_paid_unit_is_reissued_not_burned() {
    // A threshold of 0 burns every unit when there is a figure, so a payment
    // that is reissued whole shows that the mint did not guess.
    let (m, dir, notes, cur) = burning("burn-no-figure", 0, 4).await;
    let now = psp_mint::server::now_secs();
    let path = dir.join(balance::FILE);

    // No file at all.
    pay_two(&m, &notes[0], &cur, "missing").await;
    assert_eq!(m.paid_split().await, [0, 2]);

    // A file that is not a figure: `reissued` and `at` are missing, which the
    // real type cannot make.
    std::fs::write(&path, r#"{"psp": 0}"#).unwrap();
    pay_two(&m, &notes[1], &cur, "malformed").await;
    assert_eq!(m.paid_split().await, [0, 4]);

    // A figure older than `burn.balance_max_age_secs` (24 hours by default).
    publish(&dir, 0, 0, now - 48 * H);
    pay_two(&m, &notes[2], &cur, "stale").await;
    assert_eq!(m.paid_split().await, [0, 6]);

    // A figure dated ahead of the clock.
    publish(&dir, 0, 0, now + 7200);
    pay_two(&m, &notes[3], &cur, "from the future").await;
    assert_eq!(m.paid_split().await, [0, 8]);

    // And a fresh one is trusted: now the same mint burns.
    let extra = m.issue(4);
    m.wallet_holds(0).await;
    pay_two(&m, &extra, &cur, "fresh").await;
    assert_eq!(m.paid_split().await, [2, 8]);
}

#[tokio::test]
async fn with_burning_off_the_figure_is_not_read() {
    // Threshold 0 and a wallet that holds a great deal would burn everything if
    // burning were on.
    let limits = Limits {
        burn: BurnPolicy {
            enabled: false,
            threshold_psp: 0,
        },
        ..Default::default()
    };
    let dir = scratch("burn-off");
    let m = start_with(&dir, T, limits).await;
    m.wallet_holds(1_000_000).await;
    let a = m.issue(4);
    let cur = m.current().await;
    pay_two(&m, &a, &cur, "off").await;
    assert_eq!(m.paid_split().await, [0, 2]);
}

#[tokio::test]
async fn a_spend_resumed_after_a_crash_keeps_the_decision_it_was_planned_with() {
    // Burning on with room for everything (the wallet is empty, threshold 10),
    // but the request was planned earlier, when it was decided to keep 1 of the
    // 2 paid. The retry finishes that plan and does not decide again.
    let (m, dir, notes, cur) = burning("burn-resume", 10, 1).await;
    m.wallet_holds(0).await;
    let a = &notes[0];
    let built = build(&[(a, 4, &cur)], &cur, &[2], 2, "resume");
    let hash = built.req.hash();
    let planned_nonce = SecretKey::from_slice(&[0x0b; 32]).unwrap();
    let planned_blind = SecretKey::from_slice(&[0x0c; 32]).unwrap();
    std::fs::write(
        dir.join(ROLLOVERS_FILE),
        common::rollovers_file(serde_json::json!({"pending": {hash.clone(): 0},
            "holds": {hash.clone(): [hex::encode(a.0.nonce.0.serialize())]},
            "plans": {hash.clone(): [{"amount": 1, "keyset": cur,
                "nonce_secret": planned_nonce, "blinding_key": planned_blind}]}})),
    )
    .unwrap();
    *m.rot.rollovers.lock().unwrap() = Rollovers::open(&dir).unwrap();

    let (code, body) = m.spend(&built.req).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(m.paid_split().await, [1, 1], "1 burned, 1 kept, as planned");
    let file = revenue_file(&dir, &hash);
    assert_eq!(file.units(), 1);
    assert_genuine(&m, &file);
}

#[tokio::test]
async fn a_spend_with_a_used_or_held_output_blind_nonce_is_refused_before_any_burn() {
    // Fails if the outputs are checked after the burn (P1-07), or if a pending
    // rollover does not hold its output blind nonces (the same check serves a
    // rollover, see `rotation.rs`).
    let dir = scratch("outputs");
    let m = start(&dir, T).await;
    let (a, b, c, d) = (m.issue(4), m.issue(4), m.issue(4), m.issue(4));
    let cur = m.current().await;
    let first = build(&[(&a, 4, &cur)], &cur, &[2], 2, "first");
    assert_eq!(m.spend(&first.req).await.0, 200);
    let mut used = build(&[(&b, 4, &cur)], &cur, &[2], 2, "used");
    used.req.outputs = first.req.outputs.clone();
    sign_all(&mut used.req, &[&b]);
    let (_, body) = m.spend(&used.req).await;
    assert_eq!(body["code"], "blind_nonce_used", "{body}");
    // A rollover of `d`, recorded and holding its note and its output.
    let held = build(&[(&c, 4, &cur)], &cur, &[2], 2, "held");
    let blind = hex::encode(held.req.outputs[0].blind_nonce.0.serialize());
    std::fs::write(
        dir.join(ROLLOVERS_FILE),
        common::rollovers_file(serde_json::json!({"pending": {"holder": 0},
            "holds": {"holder": [hex::encode(d.0.nonce.0.serialize())]},
            "out_holds": {"holder": [blind]}})),
    )
    .unwrap();
    *m.rot.rollovers.lock().unwrap() = Rollovers::open(&dir).unwrap();
    let (_, body) = m.spend(&held.req).await;
    assert_eq!(body["code"], "note_held", "{body}");
    assert_eq!(m.spent(&[&b, &c]).await, vec![false, false]);
}
