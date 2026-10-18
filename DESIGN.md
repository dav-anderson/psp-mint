# psp-mint: design notes

How psp-mint is built, and why. The README has the build, the settings, the commands and the HTTP API; REGTEST.md has the regtest setup; this file has the reasoning. The sections were moved here from the README as they were, except where noted in [CHANGELOG.md](CHANGELOG.md).

## The scanner interface (silent payments deferred to the AIR)

The scan finds payments with `listsinceblock` from a checkpoint (`scan_checkpoint.json` in the network's data folder, the `lastblock` of the last successful scan), plus every payment already on record that is not settled yet, looked at again each scan. *Why:* the old scan read a fixed number of recent wallet transactions and could miss payments in a burst; `listsinceblock` sees every one since the checkpoint, and a reorg is handled because Bitcoin Core reports from the fork point.

Silent payments are not supported and are deferred to the AIR. To let them plug in later without touching settlement, the scanner exposes one narrow interface (`src/scan.rs`):

- `trait Node { fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>; }`: the only way the scanner talks to a node. `Rpc` implements it; tests use a fake.
- `fn since(node: &dyn Node, checkpoint: Option<&Checkpoint>, target_confirmations: u32, pending: &[String]) -> Result<Since, RpcError>`: every payment since the checkpoint, plus the pending txids.
- `struct Since { received: Vec<Received>, next: Checkpoint }`: what was found, and the checkpoint to save once it has been processed.
- `struct Received { txid: String, confirmations: u32, value_sats: u64, payload: Option<Vec<u8>> }`: one payment to the PSP wallet, with its OP_RETURN payload if any.
- `Checkpoint::load` / `save` in the data folder.

Settlement reads only `Received` values. A silent-payments scanner (which must find outputs by scanning every transaction with the PSP's scan key, not by wallet address) would be another source of `Received` values with its own checkpoint, and settlement would not change.

## Key rotation

psp-mint rotates its keys with the same scheme as fsp-mint (see [fsp-mint DESIGN.md](../fsp-mint/DESIGN.md), "Key rotation", for the full how and why): `rotation.activation_hours` (`PSP_ROTATION_ACTIVATION_HOURS`, default 8760) after the first issue, then every `rotation.period_hours` (`PSP_ROTATION_PERIOD_HOURS`, default **87600**, 10 years, because PSP is held as savings). Both are whole hours. **Either one set to 0 turns rotation off.** Restart-only settings. **Minimum period, a hard limit:** with rotation on, `psp-mint` refuses to start if the period is under 24 hours, whether it comes from `psp-mint.toml` or `PSP_ROTATION_PERIOD_HOURS`: "the PSP rotation period must be at least 24 hours, because BTC claims need time to settle before a rotation". A BTC->PSP claim takes about an hour to reach 6 confirmations, longer in slow blocks, and its outputs are blinded to the key set that was current when it was prepared. PSP only: fsp-mint and esp-mint have no policy minimum (their period is whole hours, at least 1, and in them only `activation_hours = 0` turns rotation off).

- **Two key sets, two spent lists.** Each key set (generation) is its own mint-core mint with its own sled store. Generation 0 is the existing `mint_keys.json` and `db/`, so existing data is unchanged; generation N lives in `gen-N/`. At each rotation the oldest generation is deleted with its spent list; its issued/redeemed totals move to `retired.json` so `/audit` still adds up.
- **First issue.** `first_issue.json` is written once, at the first check that sees anything issued.
- **Keyset-addressed, as fsp-mint.** Every request names keysets: a melt gives `keysets` (one per input, in order) and a rollover gives `input_keysets` and `keyset` (the set to sign the outputs with: the live set the request names, current or previous; a set that is not live is refused `keyset_retired` or `unknown_keyset`, nothing burned, and a stored answer is still replayed). The rollover request hash covers the whole request, every keyset field included. A pending rollover holds its inputs (`holds` in `rollovers.json`, saved with the pending entry before burning): another rollover naming one is refused, and a melt naming one gets `400 note_held`, nothing burned. A rollover resumed after a retirement skips a retired set's inputs only if it is proven burned under that request hash: by its `burned` flag, or by the retirement proof, taken when the set is deleted (each held note checked against the retiring spent list, the spent ones saved as `burn_proofs` in the same `retired.json` write as the retired ID, before deletion). Otherwise `400 burn_not_proven`. Each note is verified, and its spent list chosen, by the ID it names; a retired ID is refused with `keyset_retired`, any other non-live ID with `unknown_keyset`, a missing list with `missing_keyset`, all before anything is burned. One melt can mix both live sets. `/check-state` looks in both lists. `GET /keysets` lists each live keyset's keys by ID (current and previous); `/keys` is the current set only.
- **BTC claims.** A claim is signed with the live set its payload names (current or previous), and with the current set when the payload names none, names an unknown set, or names one that has been deleted since (the BTC is already paid and blinding does not depend on the key, so any live set can sign). The output prints `keyset <id>` before the signatures, so a wallet unblinds with that keyset's keys (`GET /keysets`) even if it claims after a rotation. The payload may carry the keyset it was blinded for (8 bytes after the outputs; optional, old payloads still decode), and the wallet names the set it last saw. Settlement uses it as above, and the printed `keyset <id>` line is what tells the wallet which keys to unblind with. The cost of honouring the name: a note signed by the previous set expires at the next rotation, sooner than one signed by the current set. The blind-nonce check runs against the signing set's logic only, so the same blind nonce could be used once in each of two generations; that is not value-inflating (each note is still paid for) and is listed in [ROADMAP.md](../sandbox-mint/ROADMAP.md).
- **Retired keyset list (fixed cap, not a setting).** When a rotation deletes a key set, its 8-byte keyset ID is appended to `retired.json`, written and synced *before* anything is deleted (a crash in between leaves the set listed and still on disk; the next start deletes it without listing it twice). A request naming a listed ID gets `keyset_retired` (those notes expired); an ID that is neither live nor listed gets `unknown_keyset` (junk, or never this mint's). That lets the mint tell genuinely expired notes from junk, so a wallet can explain a loss. The list is a cache capped at 1 MB of 8-byte IDs, **131,072 entries**, a constant in code and the same in every mint (`RETIRED_KEYSETS_CAP`); when full, the oldest ID is dropped. That covers hourly rotation for about 15 years, and a yearly mint never comes close; dropping only turns a very old `keyset_retired` answer into `unknown_keyset`. It is deliberately not configurable, an exception to "nothing hard-coded": it is too cheap to be worth tuning. In the file each ID is 16 hex characters, so a full list is about 2.5 to 3.5 MB of JSON on disk (quotes, commas and indentation included).
- **Deletion is crash-safe.** The retiring set's spent list and keys are renamed into `retiring-gen-N/` (keys last) and that folder is removed; a crash leaves the folder and the next start removes it. Stored rollover answers for a retired set are pruned (their notes are dead); burn memos are kept, because esp-mint's exchange crash recovery matches on them.

- **`GET /rotation`.** Public and read-only, the same shape as fsp-mint's: active flag, `first_issue`, `activation`, `rotation_period_hours`, current and previous keyset (ID, epoch, `starts`, `current_until`, `retires`), `next_rotation`, and the mint's clock `now`. The wallet uses it to time its staggered swap. `config` prints the same schedule for the operator.
- **`POST /rollover`.** Swaps notes for new notes of exactly the same amounts on the set the request names, current or previous (inputs sign `transaction_sighash`; no split). Crash-safe: the request is recorded in `rollovers.json` before the burn, and a retry gets the stored answer, or is finished (already-burned inputs skipped, outputs re-signed). A request resumed after a rotation is finished too: inputs of a set that retired meanwhile were already burned, and outputs whose named set retired are signed with the current set (the reply names the keyset).
- **It is also the PSP swap for handing PSP from wallet to wallet** (a developer to its app's user; see the ecash-wallet README). It already fits: exact amounts, the same denominations, no fee, the usual 1 MiB size limit (`server.melt_max_request_bytes`), the cheap checks first (count, amounts, denominations, then keys, signatures and the spent list), and crash-safe by request hash. If any input is already spent the whole swap is refused (`400`) and nothing is burned. That check lives in this mint; mint-core's own `swap` does not make it, a known limit that only matters to code calling mint-core directly (see the top-level `~/sandbox-mint/README.md`). No new route and no fee setting were added: a per-byte fee would make it a different route, and the hand-over swap is meant to cost nothing.
- **Lost.** PSP left untouched for a full cycle (10 to 20 years) is lost.
- **Claims left a full cycle.** Signatures printed for a keyset that has since retired cannot be used: the wallet reports the outputs as expired. With a 10-year period this needs a claim left unclaimed for years.
- mint-core is unchanged: the generation is picked in psp-mint before mint-core's primitives are called.

## Fees and junk requests

Fees are charged on requests that carry value. A request that carries no value is refused before anything can be charged, so fees do not stop junk requests; the body ceiling and cheap checks first bound their cost. Per-DID and per-connection rate limits at the AIR layer are a possible further mitigation, not yet explored.

## Wire format and versions

The request and response bodies, the payload and its sealed form, the key-set ID, the request hashes and the error codes are defined in the `sp-wire` crate, not here. `psp-mint` re-exports them (`psp_mint::payload`, `psp_mint::rotation::RolloverRequest`, `psp_mint::server::KeysetsResponse`, and so on). The ESP and FSP mints and the wallet build against the same crate, so they cannot drift from this mint by a byte.

`GET /health` returns `wire_version`, the crate's `WIRE_VERSION`. **Changing any encoded byte or JSON field in `sp-wire` means bumping `WIRE_VERSION`**, and a wallet refuses a mint that reports another number. Golden-bytes and size tests in `sp-wire` fail if a byte moves.

### Burn memos

An exchange at the ESP mint burns PSP notes and then issues ESP. If the ESP mint stops after the burn, its retry has to know whether the notes it finds spent were burned by that exchange or by someone else, since a PSP note is a bearer note and its holder could spend it somewhere else. A spent flag cannot say, so the PSP keeps the memo each note was burned under.

- Every note's spend signature covers the memo, so the memo cannot be changed without the holder's signature. For an exchange the memo is `SHA-256("esp-mint/exchange/v1" || service || each output's amount and blind nonce)`, which is fixed by the exchange's service and outputs.
- `/melt` writes the memo to `melt_memos.json` (a JSON object from note nonce, in hex, to memo) and syncs the file before it burns, for every input that is not yet spent. It does this with the mint lock held, so no other burn can come between the record and the burn. Every burn goes through `/melt`, since the PSP mounts no other way to spend a note. So a note that is spent was spent by the burn whose memo is on record.
- A burn that is refused after its record was written (a bad signature, say) leaves a record for a note that is not spent. `/burns` reports a memo only for notes that are spent, and the next burn of that note replaces the record.
- A note that is already spent is never given a new memo. A second burn of it is refused as a double spend and the record stays as it was.
- A note spent with no record (spent before the log existed) reports `spent: true` and `memo: null`. The ESP mint treats that as spent elsewhere.
- The file is a placeholder store, like `settlements.json`: rewritten in full on every burn (temporary file, fsync, rename, directory fsync). A missing file means no records, and an unreadable file is an error and never an empty log. It is covered by "Local store" in [ROADMAP.md](../sandbox-mint/ROADMAP.md).

## Using BTC to buy PSP

A payer buys PSP by sending a Bitcoin payment that carries a request in a data output. The PSP needs no connection from the payer. Everything it needs is on chain.

### The flow

1. The payer builds blinded outputs, each an amount and a blind nonce, for the PSP they want.
2. The payer packs them into a payload (below), encrypts it to the mint's DID public key, and creates a transaction that pays the PSP's address and has one data output holding the encrypted payload. The wallet library does the first two parts in one call, `build_psp_payload` (see "Wallet" below). The app's own Bitcoin wallet makes the transaction.
3. At 1 confirmation the PSP takes the BTC/USD price and locks it in a settlement record for the transaction. Nothing else is read yet.
4. At 6 confirmations the PSP decodes the payload, compares the payment with the request at the locked price, issues, and delivers the signatures to the DID in the reply field.

Before step 3 the scanner tries to decrypt the data output with the mint's DID secret key. Only what decrypts is read further. Anything else is not for this mint and is ignored.

### Settlement records

The PSP keeps one record per transaction, created when the payment is first seen with 1 confirmation. Records are kept in `settlements.json` in the data directory, a JSON object from txid to record. The file is a placeholder store. It is rewritten in full on every change: written to a temporary file, fsynced, renamed over the old file, and the directory fsynced. A missing file means no records yet, and an unreadable file is an error, never an empty store. This state probably needs to eventually "live on the air".

| Field | Meaning |
|---|---|
| `locked_price_cents` | USD per BTC in whole cents, taken at the first sighting and never refreshed. Null only for records imported from the old `processed_txids.json`. |
| `issued` | True once at least one note was signed. |
| `over_under_bps` | Paid versus requested, in basis points of the requested total. Positive is overpaid, negative underpaid: `1250` is 12.50% over and `-80` is 0.80% under. It is (paid value minus requested value) divided by requested value, using the exact USD value at the locked price (not rounded down to whole units), truncated toward zero. Null when there is no valid request to compare with (no data output, undecodable payload, unknown denomination or duplicate blind nonce). |
| `status` | `locked` (waiting for 6 confirmations), `issued`, `rejected`, or `paid_unsettled` (paid, short beyond the reconciliation target, nothing signed, BTC held). |
| `late` | True if the payment was first seen with more than 1 confirmation, so the lock was taken later than intended. |
| `skipped_units` | Requested units that were not signed because the payment was short (the smallest outputs are dropped first). For `paid_unsettled` it is the whole request. |
| `credit_cents` | Overpayment beyond the reconciliation target, in USD cents, owed to `payer_did`. 0 when the excess was within the target and kept. |
| `reason` | Why nothing was signed, when `status` is `rejected` or `paid_unsettled`. |
| `payer_did` | The payer's AIR DID from the payload, 66 hex characters. Absent when the payload could not be read, and on older records. With the txid it keys a credit or a held payment. |
| `requested_units` | PSP units the payload asked for. 0 when there was no readable payload. |
| `value_sats` | What the payment was worth in sats. Kept so that a new request, once AIR can carry one, can be checked against what was actually paid. 0 on older records. |

Rejected, underpaid and held payments are recorded too, not dropped. A payment that has reached `issued`, `rejected` or `paid_unsettled` is settled once and is not looked at again. Rejected payments are not refunded (see [ROADMAP.md](../sandbox-mint/ROADMAP.md), "Not decided yet").

The transaction is marked `issued` before anything is signed, so a payment is issued at most once. If the process dies after that and before the signatures are delivered, they are lost.

### Settlement rules

- **Payment value** is the sum of the outputs in that transaction that pay the PSP's address. It is read from the chain, never from the payload.
- **Rate** is 10 PSP = 1 USD (1 PSP = $0.10), paid in BTC, at the price locked at 1 confirmation (see Price below). So $1 buys exactly 10 PSP, and 10 PSP exchange at the ESP mint for 10^12 ESP. Ten to the dollar, and not a thousand, keeps the OP_RETURN small: each output the payment names costs 41 bytes (an 8-byte amount and a 33-byte point), a hundred times finer a unit would add about 7 denominations (up to 7 more outputs, 287 bytes), and the OP_RETURN limit is not raised. A purchase of one of every denomination is 25 outputs and 1,140 bytes sealed, well inside the 255 outputs the payload allows.
- **Amounts** are `u64`. Sums of amounts are checked, and a request whose total overflows is rejected. Conversions between sats, cents and units use `u128`. Tests cover the limits.
- **Requested** is the sum of the requested output amounts. It is compared with the payment value in USD at the locked price.
- Every output is validated before any is issued: the denomination exists, there are no duplicate blind nonces within the request, and no blind nonce was used by an earlier payment. A failure rejects the whole payment.
- A payment with no data output, or with more than one, is rejected. So is a payload that decrypts but cannot be decoded. See "Unreadable payments" below: the mint keeps the BTC.
- A payment whose data output does not decrypt with the mint's DID key is not for this mint. It is ignored: no record is made, no price is fetched, and it is not an error. The scanner looks at it again on every scan, which costs one failed decryption. Like the case above, the mint keeps the BTC.
- A payment with no usable price at its first sighting gets no record. It is tried again on the next run.
- A sealed payload copied into someone else's transaction is harmless. The signatures go to the DID inside the payload, which the copier cannot change, and only the payer holds the blinding factors that turn them into notes. The copier ends up paying for the payer's notes. A copied payload also cannot be issued twice, because its blind nonces are used once.

#### The price tolerance

One setting, `settle.price_tolerance` (a fraction; default `0.01`, 1%), is checked first. A payment within it of the requested amount, over or under (a miss of exactly the tolerance counts as within), **settles in full as if it were exact**: every requested note is signed, no credit is recorded, nothing is dropped and nothing is held. The reconciliation target below does not come into it. The comparison is in USD at the locked price, like everything else here. The `over/under` printed for the payment and stored in its record still shows the real miss, so the difference can be audited.

**Why it exists.** The price moves. The payer works out the amount at one moment, the mint locks the price when the payment has 1 confirmation, and settles it at 6, an hour or so on. A payer who paid exactly what their wallet showed can be a fraction of a percent off by the time the mint looks at it, in either direction, and has done nothing wrong. No payer should lose their smallest note, or be credited a few cents, for that. The reconciliation target is for payments that are genuinely wrong, and a fixed 10% would be far too wide to tell the two apart, so the ordinary movement of the market gets its own, much smaller setting.

**How to choose it.** Leave it at 1% in a calm market. In a volatile one, when BTC regularly moves more than 1% in an hour, widen it (`0.02` or `0.03`) so that honest payers are not pushed into reconciliation. A wider tolerance means the mint absorbs more of the movement: a payment 3% over settles with nothing credited, and one 3% under gets the full request. `0` turns it off, so that only an exact payment is exact. It is symmetric, and it can never be wider than `settle.payment_reconciliation_target`, because that setting handles whatever falls outside it: the mint refuses to start if the tolerance is more than the target (lower the tolerance or raise the target).

At 1%, the sums are small at any size:

| Request | Payer pays | What happens |
|---|---|---|
| $100 (1,000 PSP) | $100.90 (0.9% over) | Within the 1% ($1.00): all 1,000 PSP signed. No credit, nothing kept as an excess. |
| $100 (1,000 PSP) | $99.10 (0.9% under) | Within the tolerance: all 1,000 PSP signed. Nothing dropped. |
| $100 (1,000 PSP) | $101.10 (1.1% over) | Outside the tolerance, so the reconciliation target applies. At the default 10% it is within the $10 target: signed in full, the mint keeps the $1.10. |
| $100 (1,000 PSP: a 512, 256, 128, 64, 32 and an 8) | $98.90 (1.1% under) | Outside the tolerance. The 8 and the 32 are dropped (960 is no more than the 989 whole units paid). The unused $2.90 is 2.9% of the payment, within the target: the mint signs 960 and keeps $2.90. |
| $10,000 (100,000 PSP) | $10,090 (0.9% over) | Within the $100 tolerance: all signed. No credit. |
| $10,000 (100,000 PSP) | $9,910 (0.9% under) | Within the tolerance: all signed. |
| $10,000 (100,000 PSP: a 65,536, 32,768, 1,024, 512, 128 and a 32) | $9,890 (1.1% under) | Outside the tolerance. The 32, 128, 512 and 1,024 are dropped, leaving 98,304 of the 98,900 whole units paid. The unused $59.60 is 0.6% of the payment, within the target: the mint signs 98,304 and keeps $59.60. |

A tolerance is a percentage, so it is a bigger sum at $10,000 (up to $100) than at $100 (up to $1). The reconciliation target has the same property; see its "Future consideration" about variable or tiered thresholds.

#### The reconciliation target

One setting, `settle.payment_reconciliation_target` (a fraction; default `0.10`, 10%), decides what happens to a payment that is **outside the price tolerance**, one that is genuinely wrong rather than a little off from price movement. It is measured in USD at the locked price. It sets how far a payment can miss the requested amount before the mint stops absorbing the difference and starts reconciling it: a credit for an overpayment, a new request for an underpayment.

**Why one number.** Both mistakes have the same cause (a payer who pays a different amount than they asked for, usually because the price moved or the wallet rounded), and both ask the same question: is the gap small enough to swallow? So one tolerance covers both directions.

**How to choose it.** A tighter target (say `0.01`) is fairer to payers, because the mint keeps less of a mistaken payment, but it sends more payments to credit or reconciliation, and each of those needs the payer to answer over AIR. A looser target (say `0.25`) settles more payments automatically but keeps, or forgoes, more value. `0` accepts only an exact fit: any overpayment, however small, is credited, and any shortfall is held (the price tolerance must then be `0` too, because it cannot be more than the target). Because the target is a percentage, the same setting is a very different sum at different sizes. With the default 10%:

| Request | Payer pays | What happens |
|---|---|---|
| $10 (100 PSP) | $10.50 | Over by $0.50, within the $1 target: signed in full, the mint keeps the $0.50. |
| $10 (100 PSP) | $12.50 | Over by $2.50, beyond the target: signed in full, the whole $2.50 recorded as credit for the payer's DID. |
| $10 (100 PSP: a 64, a 32 and a 4) | $9.75 | The 4 is dropped (96 is no more than the 97 whole units paid). The remaining $0.15 is 1.5% of the payment, within the target: the 64 and 32 are signed and the mint keeps $0.15. |
| $10 (100 PSP: a 64, a 32 and a 4) | $9 | Dropping the 4 and the 32 leaves 64, so $2.60 of the $9 would go unused (29%), beyond the target. Nothing is signed; the payment is held as paid but unsettled. |
| $1,000 (10,000 PSP) | $1,090 | Within the $100 target: signed in full, the mint keeps $90. |
| $1,000 (10,000 PSP) | $1,150 | Beyond the target: signed in full, $150 recorded as credit. |
| $1,000 (10,000 PSP: an 8192, a 1024, a 512, a 256 and a 16) | $950 | The 16, the 256 and the 512 are dropped, leaving 9,216. The unused $28.40 is 3.0% of the payment, within the target: the mint signs 9,216 and keeps $28.40. |

Ten PSP are a dollar, so a request for N units is N / 10 dollars. The amounts above are in dollars, as the target is.

At $1,000 the same 10% lets the mint keep up to about $100 of a mistaken payment. An operator who expects large payments may want a smaller target; with `0.01` the $950 row above is held instead, and the $1,090 row is credited. **Future consideration:** a flat percentage is a small sum at $10 and a large one at $1,000, so variable or tiered thresholds (a larger fraction for small requests, a smaller one for large ones, or an absolute cap in USD) may be better. Not built: today there is one number.

#### Underpayment

When the payment is worth less than the request, the mint does the following.

1. It drops the smallest requested outputs, one at a time (among equal amounts the later one first), until the signed total is no more than the whole units the payment buys (the payment value in USD, rounded down).
2. If something is left and the value that would be unused is no more than the target times the amount paid (equivalently, the signed value is at least the amount paid times 1 minus the target), it signs what is left and **keeps the remainder**. The dropped outputs are not used up: their blind nonces stay free.
3. If nothing is left, or the unused value is beyond the target, it **signs nothing** and records the payment as `paid_unsettled`, keyed by the txid and the payer's DID, and holds the BTC. Dropping more outputs would only widen the gap, so the payer has to send a new request that matches what was paid.

These steps apply only to a payment that is short by more than `settle.price_tolerance` (1% by default). A payment within the tolerance is signed in full, as if exact. Beyond it, a payment short of the request buys at least one whole unit fewer than was asked, so it loses at least its smallest note. It is signed without that note if the unused value is within the target, and held if it is not (a request of a 4 and a 1, which is $0.50, paid 494 sats at $100,000 per BTC, 1.2% short, is held, because dropping the 1 would leave about 19% of the payment unused; 499 sats, 0.2% short, is within the tolerance and signed in full). Buyers should pay a little over the request, not under it.

**The second half is not built.** It runs over AIR: the mint messages the payer's DID, saying what was received and asking for a new request that matches it. The payer returns the new request over AIR (not on chain, since it is already paid). The mint checks it against the stored payment (`value_sats`, the locked price and the payer DID in the record), signs it, and marks the payment settled so that it cannot be claimed twice. AIR messaging does not exist yet, so today the payment is only recorded, printed (`paid but unsettled ...`) and held. Nothing deletes the record. A local stand-in for settling a held payment (like `esp-mint decline`) was considered and declined for now: the exchange happens over AIR, so the held payment waits for AIR.

**An unanswered reconciliation is not forgotten.** If the payer never sends a new request, the mint keeps the payment as a credit record for that DID indefinitely and never deletes it automatically. At some point it may need to be treated as a dead obligation (a time limit, or a sweep into the mint's own funds). That is a policy question for later and is not built.

#### Overpayment

- **Within the price tolerance.** Settled in full as if exact, no credit (see "The price tolerance"). Everything below is for a payment outside it.
- **Over by no more than the target.** The mint issues the requested outputs and keeps the excess.
- **Over by more than the target.** The mint issues the requested outputs and records the **whole** excess (not only the part past the target) as credit (`credit_cents`) against the transaction, with the payer's DID (`payer_did`).

The procedure for credit, a **placeholder that is not built**: the mint sends a message to the payer's DID over the AIR, signed with the mint's DID key, saying how much credit there is for the txid and asking for a new request for the remainder. The payer answers with a new request to be filled against the credit. The reply is an AIR message, paid for by the payer in AIR currency (PSP, ESP or FSP). AIR messaging does not exist yet, so today the credit is only recorded and printed. Like a held underpayment, a credit is kept indefinitely and never deleted automatically.

#### Unreadable payments

If the data output cannot be read at all (it does not decrypt with the mint's key, it decrypts but does not decode, or there is no data output), **the mint keeps the BTC, deliberately.** The payer's DID is inside the payload, so with an unreadable payload there is no way to contact the payer, and so nothing to refund to or to ask. No credit record can exist without a DID, so none is made (a payload that decrypts but does not decode, or that has no data output, is recorded as `rejected` for the operator). Whether such payments are ever refunded is in "Not decided yet".

### Signature delivery

The mint sends the signatures to the DID in the reply field over the AIR, and the mint pays for sending them. The payer needs no balance to receive them. AIR messaging does not exist yet, so this is **not built**: today the signatures are printed to the terminal once, at issue time, and are not stored. This is a **temporary development stand-in**, for testing only.

Each printed line is `signature <blind nonce hex> <signature hex>`. `ecash-wallet claim-psp` reads those lines (the whole output can be pasted in, other lines are skipped), matches each to the output it blinded by blind nonce, unblinds it and stores the note. Printing is a development stand-in for the AIR message, which will carry the same pairs.

### Fees

The PSP has no fee of its own besides the BTC purchase. Fees at the later layers (exchange to ESP, transfer, and FSP use) are paid in AIR currency and charged by AIR messaging, not by the mints. The only explicit mint fees are the BTC at PSP purchase and the ESP redeem fee. A separate fee on top of the 1 USD rate is not defined or implemented.

### Price

The price is locked when a payment is first seen with 1 confirmation or more and has no record yet. The first run that sees the transaction fetches the price, once per run, and stores it in the record. Settlement at 6 confirmations uses the stored price and never a live one. The PSP does not poll the exchanges.

| Source | Endpoint | Price | Quote time |
|---|---|---|---|
| Coinbase | `https://api.exchange.coinbase.com/products/BTC-USD/ticker` | `price` | `time`, ISO 8601 UTC |
| Bitstamp | `https://www.bitstamp.net/api/v2/ticker/btcusd/` | `last` | `timestamp`, unix seconds |
| Gemini | `https://api.gemini.com/v1/pubticker/btcusd` | `last` | `volume.timestamp`, unix milliseconds |

All three are public, need no key, and the price used is the last trade. The sources and URLs are fixed in the code (`src/price.rs`) and are not settings: changing them means changing code. Gemini's timestamp is undocumented and aligned to the minute, so it only bounds the age of its quote to about a minute.

- A source is excluded if it errors, returns no price, or its quote time is more than 120 seconds from now (`price.max_age_secs`, `PSP_PRICE_MAX_AGE_SECS`).
- 3 usable prices: take the median.
- 2 usable prices: average them (rounded down). If they differ by more than 2% of the lower one (`price.max_spread_bps`, `PSP_PRICE_MAX_SPREAD_BPS`, 200), wait and retry on the next run.
- 1 or 0 usable prices: wait and retry on the next run. The PSP never issues on a single feed.
- Prices are whole US cents per BTC. Decimal strings are parsed to cents with no floating point, and anything past the second decimal is dropped.

The price is a live ticker price taken when the run sees the payment, not a block-time price. The PSP does not use block timestamps or historical candles, because miners can skew a block's timestamp by about 2 hours. This is why the PSP must scan every few minutes (`serve` does, every 120 seconds by default): the price is taken at the first scan that sees the payment at 1 confirmation, and a run that comes late locks a later price and marks the record `late`. The rule is in `src/price.rs`, with the aggregation as a pure tested function.

### Payload format

This is the payload before encryption. It is what the PSP decodes after it has decrypted the data output. Integers are big-endian.

| Part | Size |
|---|---|
| reply length (must be 33) | 1 |
| reply: the payer's AIR DID | 33 |
| output count | 1 |
| each output: amount (8) and blind nonce (33) | 41 per output |

Total size is `35 + 41 * outputs`. For example, 3 outputs is 158 bytes. The signatures are delivered to the DID in the reply field. The mint treats the DID as opaque bytes and does not check it. A reply of any other length is rejected. The DID is inside the encrypted payload, so it is not visible on chain.

### Encryption

The payload is encrypted to the mint's DID public key, and only the mint's DID secret key can decrypt it. The cipher is AIR's (2.0.3, and unchanged in 2.1.0), its `PublicKey` and `SecretKey`, which `sp-wire` re-exports, not a new scheme: `PublicKey::encrypt` seals bytes to a public key and `SecretKey::decrypt` opens them. It does an ElligatorSwift key exchange with a fresh sender key for every message, then ChaCha20-Poly1305. No payer signature is needed.

- **Wallet.** Build the payload exactly as in "Payload format", seal it to the mint's published DID public key, and put the result in the OP_RETURN. The wallet library's `build_psp_payload` does all of it (below). `psp-mint encrypt <mint pubkey hex> <payload hex>` seals a ready payload, for testing.
- **Mint.** The scanner tries to decrypt each data output with the mint's DID secret key and reads only what decrypts. Anything that fails is not for this mint. It is ignored without a record, a price fetch or an error. This covers a payload sealed to another key, a tampered one, and a cleartext one. Nothing is accepted in cleartext any more.
- **Wire format.** `64-byte ElligatorSwift sender key || 16-byte tag || ciphertext`, and nothing else. There is no length byte and no framing, so a sealed payload is always 80 bytes longer than the payload, whatever its length, and anything under 81 bytes cannot be a sealed payload. A sealed payload is a constant-size envelope around a ciphertext as long as the payload; bytes added or removed make the tag fail, so they are rejected. The sender key is an ElligatorSwift encoding, which looks like random bytes. This is wire version 2.
- **Keys.** The mint's DID key pair is a local file, `mint_did_key.json`, in the data directory, with `0600` permissions. It is made on first use and never overwritten. `psp-mint pubkey` prints the public half, 33 bytes in hex, for wallets to seal to. Moving the key into the AIR keystore is deferred.
- **Relay.** A sealed payload is at least 156 bytes (one note), well over 83 bytes, the old standard limit for a data output. Bitcoin Core 30 and later relay larger ones by default, and 31.1 is what this targets. A node with an older policy will not relay the transaction.

Expected sizes, at 1 sat/vB and BTC at $81,672:

| Purchase | Notes | Payload | Output | Fee |
|---|---|---|---|---|
| $10 (100 PSP) | 3 | 238 B | 250 vB | about $0.20 |
| $100 (1,000 PSP) | 6 | 361 B | 376 vB | about $0.31 |
| $1,000 (10,000 PSP) | 5 | 320 B | 335 vB | about $0.27 |
| $10,000 (100,000 PSP) | 6 | 361 B | 376 vB | about $0.31 |
| $6,553.50 (65,535 PSP, a worst case under $10,000) | 16 | 771 B | 786 vB | about $0.64 |

The notes are the set bits of the purchase in PSP (it is split into powers of two), at 41 bytes each, plus 115 bytes of DID, count and encryption (35 for the reply and count, 80 for the envelope). The most any purchase can take is one of each of the 25 denominations: 1,140 bytes.

### Wallet

The `ecash-wallet` crate (a sibling folder, with a thin command line) is the client for this mint. Its hook for an AIR app is:

```
build_psp_payload(outputs, reply_did, mint_did_pubkey) -> bytes
```

`outputs` are the note sizes in PSP (powers of two). The call blinds each output, encodes the payload with `payload::encode`, seals it to `mint_did_pubkey` with `seal::encrypt`, and returns the bytes for the OP_RETURN. The wallet holds no BTC wallet, so the caller's own Bitcoin wallet sends the payment. The blinding factors and spend keys are saved in the wallet file before the bytes are returned, because the signatures cannot be turned into notes without them. When the signatures come back, `claim_psp` pairs them with the saved outputs by blind nonce, unblinds them against the keys from `GET /keys`, and stores the notes.

The wallet calls this crate's `payload::encode` and `seal::encrypt` by path, so the format is defined once, here.

## Networks

*Why mainnet by default and a flag for the rest:* the default is what a real operator runs; a test network is a choice made on purpose.

**The scan.** Each scan reads every wallet transaction since the last block it finished at (`listsinceblock`), however many there are, plus every payment that is not yet settled, so nothing is missed in a burst and a payment waiting for confirmations is looked at again. The block is kept in `scan_checkpoint.json` in the network folder, written only after a scan succeeds, so a restart carries on where it stopped. A reorg is handled: transactions removed from the chain are reported by the node and checked again, and a payment is never settled twice. A damaged checkpoint is an error, never replaced.

## Settings

It covers the BTC price moving between the payer working out the amount, the price lock and settlement, which no payer should be penalised or credited for. Operators can widen it in a volatile market.

**Over** by no more than this: the mint keeps the excess; over by more: the whole excess is recorded as credit for the payer's DID. **Short:** the smallest requested outputs are dropped until the signed total is no more than was paid; if that is within this of the amount paid the mint signs those notes and keeps the remainder, otherwise it signs nothing and holds the payment as paid but unsettled. One setting for both directions.

## Storage policy (setting only)

`storage.mint_state` (`PSP_STORAGE_MINT_STATE`) names the AIR services that will keep this mint's state: an ordered list of service IDs, index 0 the primary and the rest secondaries in promotion order. A service ID is the service's AIR DID: a 33-byte compressed key in hex (66 characters, starting `02` or `03`). The 32-byte x-only form is refused with a message that says to use the DID. In the environment the IDs are comma-separated; in `psp-mint.toml` the value is an array of strings or one string. One entry is valid (that service holds the state and the mint keeps a local backup copy); empty, the default, means local only; at most 8, no duplicates, no empty entry. The parser and checks are `sp_wire::storage` (sp-wire), shared by all three mints and the wallet, so a list means the same thing everywhere.

This is configuration and validation only. **Nothing reads or writes the AIR yet:** the state is still in local files, whatever the list says. The mint logs the list at startup like any setting. What the list will do, and what the mint's storage layer must then provide (eight numbered requirements: 1 a spend is written to the primary and at least one secondary before the mint answers, batched; 2 a local backup copy with one service; 3 only the mint promotes a secondary and updates its config; 4 writes carry a sequence number, so a returning primary is overwritten from the working copy; 5 the wallet file carries a version and is merged; 6 networks stay apart; 7 the mint's own local copy is the primary, fsynced, with external services as backup only and no cap; 8 a request's finishing entry and its ledger movements are one atomic write, which only esp-mint's per-day ledger needs), is that list: it is the whole set, and nothing else is promised. That replication belongs in mint-core's storage layer and is a candidate for AIR upstream. The mint's own local copy is the primary and every listed service is a backup (decided 2026-10-17): every spend is fsynced before the mint answers and replicated to the backups while they are reachable; while every backup is unreachable, unreplicated spends are queued and copied out in order to the first backup that returns. There is no cap: the mint carries on with no limit and never refuses or holds spends for replication, and `/health` only reports a degraded state while backups are behind. None of this is built.

## Running as a service

`psp-mint serve` runs under `launchd` (macOS) or `systemd` (Linux) as a user that is not root, with its data directory (`PSP_DATA_DIR`), settings file (`PSP_CONFIG`) and network (`PSP_NETWORK`) named in an env file. The definitions, the install and uninstall steps and the health monitor are in the README of the SPI repository ("Running as services"): `spi install` generates the systemd units or launchd property lists, the env and settings files and the health monitor from its `spi.toml`, and prints the root steps to put them in place.

**Failure and restart.** `serve` exits with status 1 if the scan thread dies or the mint lock is poisoned, and the service manager starts it again after a delay (growing under systemd, fixed under launchd) and never gives up. Downtime is not free here: a payment is priced only when a scan sees it at 1 confirmation or later, so a long outage can cost a price lock. The restart is picked up from what is on disk by the rules of a crash: the scan checkpoint is written only after a successful pass, and a pass also re-reads every payment that is not yet settled (see "The scan"), so a restart misses nothing.

**Stopping.** SIGTERM and SIGINT stop the API gracefully (no new connections, the requests being answered finish) and the process exits with status 0. The scan thread may be in the middle of a pass; that is the same as a crash in the middle of a pass, which the checkpoint and the settlement records already cover.

**Opening the data twice.** The database allows one process at a time, so a new process that starts while the old one is still stopping cannot open it; it exits with an error and the service manager starts it again. The storage that replaces sled has to give the same refusal or an equivalent single-writer guarantee, and the writes of the scan checkpoint, the settlement records and the melt memos have to be atomic (all or nothing) and durable once the call that made them returns, so that a restart sees each one whole or not at all.

**Health.** `GET /health` is `200` while the scanner is keeping up and the lock is sound and `503` otherwise. The health monitor that `spi install` sets up (`sp-health-monitor.sh`, see the SPI README, "The health monitor") polls it and reports or restarts a mint that stays unhealthy.
