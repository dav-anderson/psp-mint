# psp-mint

## Setup

Targets Bitcoin Core 31.1. The mint defaults to mainnet (see "Networks"); these steps use regtest, a private chain where you mine your own blocks and coins, so every `cargo run` in them passes `--regtest`. These steps are for macOS on Apple Silicon.

psp-mint builds from tagged dependencies: mint-core at `v0.1.3` and `sp-wire` at `v0.2.0` (git tags). `sp-wire` holds the wire types this mint shares with the other mints and the wallet (see "Wire format and versions" below), and it pulls in AIR 2.0.3 (commit `8e281d5`, AIR has no tags) for the payload encryption. AIR 2.0.3 has no path dependencies, so nothing needs patching to build from git. To test against unpushed local copies of `sp-wire` or mint-core, `sandbox-mint/scripts/local-git.sh` points git at the local folders and their local tags.

### 1. Install Bitcoin Core

The `.zip` release is only the GUI app and has no `bitcoind` or `bitcoin-cli`. Use the `.tar.gz`.

```
mkdir -p ~/bitcoin-core && cd ~/bitcoin-core
curl -LO https://bitcoincore.org/bin/bitcoin-core-31.1/bitcoin-31.1-arm64-apple-darwin.tar.gz
curl -LO https://bitcoincore.org/bin/bitcoin-core-31.1/SHA256SUMS
shasum -a 256 --ignore-missing --check SHA256SUMS
tar xzf bitcoin-31.1-arm64-apple-darwin.tar.gz
./bitcoin-31.1/bin/bitcoind --version
```

The `shasum` line must print `OK`. Before using real money, also verify `SHA256SUMS.asc` with GPG. The binaries are in `~/bitcoin-core/bitcoin-31.1/bin`.

### 2. Run a regtest node

Bitcoin Core reads its config from `~/Library/Application Support/Bitcoin/bitcoin.conf`. Create it if it does not exist, with one line:

```
mkdir -p ~/Library/Application\ Support/Bitcoin
echo "regtest=1" > ~/Library/Application\ Support/Bitcoin/bitcoin.conf
```

Start the node and check it:

```
~/bitcoin-core/bitcoin-31.1/bin/bitcoind -daemon
~/bitcoin-core/bitcoin-31.1/bin/bitcoin-cli getblockchaininfo
```

`chain` must be `regtest` and a new chain starts at `blocks` 0. There is nothing to sync. To stop the node, run `bitcoin-cli stop`.

### 3. Create the PSP wallet

The PSP watches its own Bitcoin Core wallet. Create one and get a receiving address. The shell variable lasts only for that terminal window.

```
CLI=~/bitcoin-core/bitcoin-31.1/bin/bitcoin-cli
$CLI createwallet psp-regtest
$CLI -rpcwallet=psp-regtest getnewaddress
```

The address starts with `bcrt1` on regtest.

### 4. Create a funded payer wallet

A second wallet plays the buyer. On regtest you mine your own coins. A block reward can be spent only after 100 more blocks, so mine 101.

```
$CLI createwallet payer
PAYER=$($CLI -rpcwallet=payer getnewaddress)
$CLI -rpcwallet=payer generatetoaddress 101 $PAYER > /dev/null
$CLI -rpcwallet=payer getbalance
```

The balance is `50.00000000`.

### 5. Send a test payment with a payload

The payer wallet sends a payment to the PSP wallet with the payload in a data output. The payload is encrypted to the mint's DID public key first, so there are two extra steps: get the key, and seal the payload with it. The example payload is the known-answer payload from `src/payload.rs` (a 33-byte reply holding the DID `0279be66…1798`, two outputs). `fee_rate` is needed because a regtest node has no fee estimates.

```
cd ~/psp-mint
MINT_KEY=$(cargo run -q -- --regtest pubkey)
PLAIN=210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4
HEX=$(cargo run -q -- --regtest encrypt $MINT_KEY $PLAIN)
PSP_ADDR=$($CLI -rpcwallet=psp-regtest getnewaddress)
$CLI -rpcwallet=payer -named send outputs="[{\"$PSP_ADDR\":0.001},{\"data\":\"$HEX\"}]" fee_rate=10
$CLI -rpcwallet=psp-regtest listtransactions
```

`pubkey` prints the mint's DID public key (66 hex characters) and makes the key file on first use. It reads `PSP_DATA_DIR`, so use the same value as the server will. `encrypt` seals a hex payload to a public key and prints the result as hex, which is different every time. It is a test tool with a fixed payload. A real purchase is made by the wallet, which blinds its own outputs and keeps the blinding factors: `ecash-wallet buy-psp --mint-key $MINT_KEY 4 1` prints the sealed payload for the same `send` command (see the ecash-wallet README). `send` prints a `txid` with `"complete": true`. `listtransactions` shows a `receive` of `0.00100000` with `confirmations` 0, because no block has mined it yet.

### 6. Confirm the payment and read the payload back

Mine blocks to confirm the payment. Mine to a payer address, not a PSP address. Replace `TXID` with the txid from step 5.

```
TXID=<txid from step 5>
PAYER=$($CLI -rpcwallet=payer getnewaddress)
$CLI -rpcwallet=payer generatetoaddress 1 $PAYER > /dev/null
$CLI -rpcwallet=psp-regtest gettransaction $TXID | grep confirmations
$CLI -rpcwallet=payer generatetoaddress 5 $PAYER > /dev/null
$CLI -rpcwallet=psp-regtest gettransaction $TXID | grep confirmations
$CLI -rpcwallet=psp-regtest gettransaction $TXID true true | grep -B6 nulldata
```

The confirmations are `1` and then `6`. To watch the price lock, run `cargo run` (step 7) between the two `generatetoaddress` commands, once after the first confirmation and again after the sixth. The `nulldata` output has an `asm` of `OP_RETURN` followed by the sealed payload hex, identical to what was sent, and nothing in it is readable. The raw `hex` of that output is `6a4cc5` followed by the sealed payload: `OP_RETURN`, `OP_PUSHDATA1`, then the length byte (197 for the example, which is 117 plus 80).

### 7. Run the PSP once

This is the one-shot run, for testing. With the node running, from the `psp-mint` directory:

```
cargo test
cargo run -- --regtest
```

Each run first prints the settings in use and where each came from (see "Settings"). With no config file and no `PSP_` variables set:

```
psp-mint settings: no config file, so defaults and the environment:
  server.bind = 127.0.0.1:3000 host:port (default)
  server.melt_max_request_bytes = 1048576 bytes (default)
  server.burns_max_request_bytes = 1048576 bytes (default)
  server.check_state_max_request_bytes = 1048576 bytes (default)
  bitcoin.network = regtest network (command-line flag)
  bitcoin.rpc_url = http://127.0.0.1:18443 URL (default)
  bitcoin.wallet = psp-regtest name (default)
  bitcoin.cookie = /Users/you/Library/Application Support/Bitcoin/regtest/.cookie path (default)
  bitcoin.rpc_timeout_secs = 30 seconds (default)
  scan.interval_secs = 120 seconds (default)
  scan.health_intervals = 3 scan intervals (default)
  scan.health_grace_secs = 60 seconds (default)
  price.max_age_secs = 120 seconds (default)
  price.max_spread_bps = 200 basis points (default)
  price.timeout_secs = 10 seconds (default)
  settle.lock_confirmations = 1 confirmations (default)
  settle.min_confirmations = 6 confirmations (default)
  settle.price_tolerance = 0.01 fraction, 0 to 1 (0.01 is 1%) (default)
  settle.payment_reconciliation_target = 0.10 fraction, 0 to 1 (0.10 is 10%) (default)
  mint.max_denomination_units = 16777216 PSP units (default)
```

Then it scans the `psp` wallet once, moves every payment as far as it can go, and exits.

- A payment with 1 to 5 confirmations that has no record yet gets one. The run takes the BTC/USD price, stores it, and prints `price locked at ... USD/BTC, waiting: 1 of 6 confirmations`.
- A payment with 6 confirmations is settled with the stored price. For the step 5 payment (`value_sats=100000`) it prints the over/under in basis points and one `signature <blind nonce> <signature>` line per signed output (two, for the 4 and 1 unit outputs). These lines are a temporary development stand-in for delivery over the AIR. The blind nonce says which output a signature belongs to, so a wallet can claim the right notes even when the PSP signed only some of them. Its record in `settlements.json` then shows `issued: true`.
- A payment that is already settled prints `already settled`. A payment with 0 confirmations prints `waiting`.

If you run it for the first time when the payment already has 6 confirmations, it locks the price in that run and settles in the same run. The record is marked `late`.

The run needs internet access, because the price comes from three live exchanges even on regtest. A regtest coin is priced like a real one, so 100,000 sats buys about 800 PSP units ($0.10 each) at current prices, and the 5 units ($0.50) in the step 5 payload cost far less than that. The payment is overpaid by a wide margin, and the over/under shows it (far beyond the 0.10 reconciliation target, so the whole excess is recorded as credit for the payer's DID). If no price can be taken it prints `waiting: no price this run` and the payment is tried again on the next run.

A payment whose data does not open with the mint's DID key is ignored: no record, no price fetch, no rejection and no error. That covers a payload sealed to another key, a tampered one, and the cleartext payloads from before encryption. The one-shot run prints how many it ignored, and `serve` prints nothing. To test again after this change, repeat steps 5 and 6 to send and confirm a new payment.

Notes on running it:

- **The one-shot run runs once.** Each payment has to be seen at 1 confirmation so its price is locked then, so in real use run `serve` (step 8), which rescans on its own. The one-shot run cannot run while `serve` is up, because the mint's database can only be opened by one process.
- **It sees every transaction since the last scan** (`listsinceblock` from the checkpoint, see "Networks"). The first scan reads the whole wallet.
- **Data directory.** State lives in `~/.psp-mint/regtest` here (`~/.psp-mint/<network>`; override the parent with `PSP_DATA_DIR`): `mint_keys.json` (the mint's secret keys, created on first run), `mint_did_key.json` (the mint's DID key pair, which payloads are sealed to, created on first use), `db/` (the mint's store), `settlements.json` (one record per payment) and `melt_memos.json` (the memo each burned note was burned under, see "Burn memos"). Do not delete it. Without `mint_keys.json`, notes already issued cannot be redeemed, and without `mint_did_key.json`, payments already sent to the mint's DID key can no longer be read. A damaged key file is an error and is never replaced. An old `processed_txids.json` is imported into `settlements.json` on the first run, as issued records with no stored price, and left in place.
- **Denominations.** Notes are powers of two from 1 to 16,777,216 units (2^24, 25 keys), and a unit is $0.10. A request must use those, so 5 units is a 4 and a 1.
- **Data outputs.** A transaction needs exactly one data output, holding the sealed payload. With none, or with more than one, it is recorded as rejected and nothing is issued. If the one data output does not open, the payment is ignored instead (above).
- **Signatures.** They are printed once, at issue time, and are not stored anywhere. Printing is a temporary development stand-in for delivery to the payer's DID over the AIR, which is not built (see Left to do). A buyer cannot receive notes this way in real use.

### 8. Run the PSP server

`serve` runs the HTTP API and rescans the wallet on a timer, in one process. Stop the one-shot run first if it is going. For testing, use a short interval:

```
PSP_SCAN_INTERVAL_SECS=10 cargo run -- --regtest serve
```

It prints the settings first, the same list as in step 7 except that the interval comes from the environment:

```
psp-mint settings: no config file, so defaults and the environment:
  ...
  scan.interval_secs = 10 seconds (env PSP_SCAN_INTERVAL_SECS)
  ...
```

then `scanning wallet psp-regtest every 10 s` and `psp-mint api listening on http://127.0.0.1:3000`. With a 10 s interval `/health` fails after 3 × 10 + 60 = 90 s without a successful scan. In another terminal:

```
curl -s http://127.0.0.1:3000/keys
curl -s http://127.0.0.1:3000/audit
curl -s http://127.0.0.1:3000/health
```

`/keys` lists a public key for each denomination (1 to 16,777,216, 25 keys). `/audit` shows `issued`, `redeemed` and `outstanding`. To watch a payment move through, send one (step 5), mine 1 block (step 6) and wait for a scan: the server prints `price locked at ... USD/BTC, waiting: 1 of 6 confirmations`. Mine 5 more blocks and after the next scan it settles and prints the signatures. `/audit` then shows the issued total. Stop it with Ctrl-C.

Scan failures (for example the node being down) are printed and retried on the next interval. Payments that are already settled are not printed again.

The reference sections that follow (Networks, The scanner interface, Key rotation, Fees and junk requests, Settings) are not steps. Steps 1 to 8 above run straight through.

## Networks

psp-mint runs on mainnet unless told otherwise. Pick the network with a flag, before or after the command: `--regtest`, `--signet` or `--testnet` (`--mainnet` is the default and may be given). The setting `bitcoin.network` (`PSP_NETWORK`: `main`, `regtest`, `signet`, `testnet`) does the same, and a flag wins over the file and the environment. `--testnet` is testnet3 (Bitcoin Core's `chain` `test`).

Each network has its own defaults, used unless `bitcoin.rpc_url`, `bitcoin.wallet` or `bitcoin.cookie` is set:

| Network | RPC port | Wallet | Cookie (macOS) | Cookie (Linux) |
|---|---|---|---|---|
| main | 8332 | `psp-main` | `~/Library/Application Support/Bitcoin/.cookie` | `~/.bitcoin/.cookie` |
| regtest | 18443 | `psp-regtest` | `~/Library/Application Support/Bitcoin/regtest/.cookie` | `~/.bitcoin/regtest/.cookie` |
| signet | 38332 | `psp-signet` | `~/Library/Application Support/Bitcoin/signet/.cookie` | `~/.bitcoin/signet/.cookie` |
| testnet | 18332 | `psp-testnet` | `~/Library/Application Support/Bitcoin/testnet3/.cookie` | `~/.bitcoin/testnet3/.cookie` |

The cookie default is chosen by the operating system the mint runs on (macOS, else the Linux path), matching where Bitcoin Core writes it. Testnet stays testnet3. *Why mainnet by default and a flag for the rest:* the default is what a real operator runs; a test network is a choice made on purpose.

**Each network has its own data folder**, `~/.psp-mint/<network>` (`PSP_DATA_DIR` is the parent): keys, store, records and checkpoint never mix between networks. Every command prints `psp-mint on <network>: data in <folder>`. The config file `psp-mint.toml` stays in `PSP_DATA_DIR` itself.

**The node is checked.** At startup and before every scan the mint asks the node (`getblockchaininfo`) which chain it is on, and stops if that is not the configured network. `/health` and `/audit` carry `"network"`, and the ESP mint and the wallet check it before they talk to the PSP.

**Migrating old data.** Before this change the data sat directly in `~/.psp-mint` and was regtest data. The mint refuses to run (every command except `config`) while those files are there, and tells you to run `cargo run -- migrate`, which moves them into `~/.psp-mint/regtest`. It only renames, and it refuses if the target already holds a file of the same name. The regtest wallet name default changed from `psp` to `psp-regtest`: set `PSP_WALLET=psp` (or `bitcoin.wallet = "psp"`) to keep an old wallet, or create `psp-regtest`.

**The scan.** Each scan reads every wallet transaction since the last block it finished at (`listsinceblock`), however many there are, plus every payment that is not yet settled, so nothing is missed in a burst and a payment waiting for confirmations is looked at again. The block is kept in `scan_checkpoint.json` in the network folder, written only after a scan succeeds, so a restart carries on where it stopped. A reorg is handled: transactions removed from the chain are reported by the node and checked again, and a payment is never settled twice. A damaged checkpoint is an error, never replaced. `scan.max_transactions` (`PSP_SCAN_MAX_TRANSACTIONS`) is retired: the scan has no window any more, and setting it stops the mint with an error saying so.

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

psp-mint rotates its keys with the same scheme as fsp-mint (see the fsp-mint README, "Key rotation", for the full how and why): `rotation.activation_hours` (`PSP_ROTATION_ACTIVATION_HOURS`, default 8760) after the first issue, then every `rotation.period_hours` (`PSP_ROTATION_PERIOD_HOURS`, default **87600**, 10 years, because PSP is held as savings). Both are whole hours. **Either one set to 0 turns rotation off.** Restart-only settings. **Minimum period, a hard limit:** with rotation on, `psp-mint` refuses to start if the period is under 24 hours, whether it comes from `psp-mint.toml` or `PSP_ROTATION_PERIOD_HOURS`: "the PSP rotation period must be at least 24 hours, because BTC claims need time to settle before a rotation". A BTC->PSP claim takes about an hour to reach 6 confirmations, longer in slow blocks, and its outputs are blinded to the key set that was current when it was prepared. PSP only: fsp-mint and esp-mint have no policy minimum (their period is whole hours, at least 1, and in them only `activation_hours = 0` turns rotation off).

- **Two key sets, two spent lists.** Each key set (generation) is its own mint-core mint with its own sled store. Generation 0 is the existing `mint_keys.json` and `db/`, so existing data is unchanged; generation N lives in `gen-N/`. At each rotation the oldest generation is deleted with its spent list; its issued/redeemed totals move to `retired.json` so `/audit` still adds up.
- **First issue.** `first_issue.json` is written once, at the first check that sees anything issued.
- **Keyset-addressed, as fsp-mint.** Every request names keysets: a melt gives `keysets` (one per input, in order) and a rollover gives `input_keysets` and `keyset` (the set to sign the outputs with: the current set only; naming the previous set is `400 keyset_not_current`, "re-blind the outputs for the current keyset", nothing burned, and a stored answer is still replayed). The rollover request hash covers the whole request, every keyset field included. A pending rollover holds its inputs (`holds` in `rollovers.json`, saved with the pending entry before burning): another rollover naming one is refused, and a melt naming one gets `400 note_held`, nothing burned. A rollover resumed after a retirement skips a retired set's inputs only if it is proven burned under that request hash: by its `burned` flag, or by the retirement proof, taken when the set is deleted (each held note checked against the retiring spent list, the spent ones saved as `burn_proofs` in the same `retired.json` write as the retired ID, before deletion). Otherwise `400 burn_not_proven`. Each note is verified, and its spent list chosen, by the ID it names; a retired ID is refused with `keyset_retired`, any other non-live ID with `unknown_keyset`, a missing list with `missing_keyset`, all before anything is burned. One melt can mix both live sets. `/check-state` looks in both lists. `GET /keysets` lists each live keyset's keys by ID (current and previous); `/keys` is the current set only.
- **BTC claims.** A claim is always signed with the current set (the BTC is already paid and blinding does not depend on the key, so any live set can sign), and the output prints `keyset <id>` before the signatures, so a wallet unblinds with that keyset's keys (`GET /keysets`) even if it claims after a rotation. The payload may carry the keyset it was blinded for (8 bytes after the outputs; optional, old payloads still decode). **The mint ignores it.** Settlement never reads it: a claim is signed with the current set whatever the payload names, and the printed `keyset <id>` line is what tells the wallet which keys to unblind with. It is carried for the payer's own records and for a future mint that might use it.
- **Retired keyset list (fixed cap, not a setting).** When a rotation deletes a key set, its 8-byte keyset ID is appended to `retired.json`, written and synced *before* anything is deleted (a crash in between leaves the set listed and still on disk; the next start deletes it without listing it twice). A request naming a listed ID gets `keyset_retired` (those notes expired); an ID that is neither live nor listed gets `unknown_keyset` (junk, or never this mint's). That lets the mint tell genuinely expired notes from junk, so a wallet can explain a loss. The list is a cache capped at 1 MB of 8-byte IDs, **131,072 entries**, a constant in code and the same in every mint (`RETIRED_KEYSETS_CAP`); when full, the oldest ID is dropped. That covers hourly rotation for about 15 years, and a yearly mint never comes close; dropping only turns a very old `keyset_retired` answer into `unknown_keyset`. It is deliberately not configurable, an exception to "nothing hard-coded": it is too cheap to be worth tuning. In the file each ID is 16 hex characters, so a full list is about 2.5 to 3.5 MB of JSON on disk (quotes, commas and indentation included).
- **Deletion is crash-safe.** The retiring set's spent list and keys are renamed into `retiring-gen-N/` (keys last) and that folder is removed; a crash leaves the folder and the next start removes it. Stored rollover answers for a retired set are pruned (their notes are dead); burn memos are kept, because esp-mint's exchange crash recovery matches on them.

- **`GET /rotation`.** Public and read-only, the same shape as fsp-mint's: active flag, `first_issue`, `activation`, `rotation_period_hours`, current and previous keyset (ID, epoch, `starts`, `current_until`, `retires`), `next_rotation`, and the mint's clock `now`. The wallet uses it to time its staggered swap. `config` prints the same schedule for the operator.
- **`POST /rollover`.** Swaps notes for new notes of exactly the same amounts on the current set (inputs sign `transaction_sighash`; no split). Crash-safe: the request is recorded in `rollovers.json` before the burn, and a retry gets the stored answer, or is finished (already-burned inputs skipped, outputs re-signed). A request resumed after a rotation is finished too: inputs of a set that retired meanwhile were already burned, and outputs whose named set retired are signed with the current set (the reply names the keyset).
- **It is also the PSP swap for handing PSP from wallet to wallet** (a developer to its app's user; see the ecash-wallet README). It already fits: exact amounts, the same denominations, no fee, the usual 1 MiB size limit (`server.melt_max_request_bytes`), the cheap checks first (count, amounts, denominations, then keys, signatures and the spent list), and crash-safe by request hash. If any input is already spent the whole swap is refused (`400`) and nothing is burned. That check lives in this mint; mint-core's own `swap` does not make it, a known limit that only matters to code calling mint-core directly (see the top-level `~/sandbox-mint/README.md`). No new route and no fee setting were added: a per-byte fee would make it a different route, and the hand-over swap is meant to cost nothing.
- **Lost.** PSP left untouched for a full cycle (10 to 20 years) is lost.
- **Claims left a full cycle.** Signatures printed for a keyset that has since retired cannot be used: the wallet reports the outputs as expired. With a 10-year period this needs a claim left unclaimed for years.
- mint-core is unchanged: the generation is picked in psp-mint before mint-core's primitives are called.

## Fees and junk requests

Fees are charged on requests that carry value. A request that carries no value is refused before anything can be charged, so fees do not stop junk requests; the body ceiling and cheap checks first bound their cost. Per-DID and per-connection rate limits at the AIR layer are a possible further mitigation, not yet explored.

## Settings

Every setting is in `psp-mint.toml` in the data directory (or the file `PSP_CONFIG` names, which must then exist), and each has an environment variable that overrides the file. Strongest first: the environment, the file, the default. The file is TOML in sections:

```
[settle]
min_confirmations = 6
price_tolerance = 0.01
payment_reconciliation_target = 0.10

[bitcoin]
wallet = "psp"
```

The settings are read once, when the mint starts, and logged then with the value in use and where it came from (`default`, `file` or `env PSP_...`). `cargo run -- config` prints the same list and exits. An unknown key, a value of the wrong type or a value out of range stops the mint with a list of every problem. There is no reload: restart the mint to pick up a change. `PSP_DATA_DIR` (default `~/.psp-mint`) is not a setting, because the file lives in it.

| Setting | Env var | Default | What it does |
|---|---|---|---|
| `server.bind` | `PSP_BIND` | `127.0.0.1:3000` | The HTTP API's address (`serve` only). Must be an `IP:port`. |
| `server.melt_max_request_bytes` | `PSP_MELT_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a `POST /melt` body. Over it: `413` before parsing, nothing burned. 4096 to 1 GiB. |
| `server.burns_max_request_bytes` | `PSP_BURNS_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a `POST /burns` body. 4096 to 1 GiB. |
| `server.check_state_max_request_bytes` | `PSP_CHECK_STATE_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a `POST /check-state` body. 4096 to 1 GiB. |
| `bitcoin.network` | `PSP_NETWORK` | `main` | The Bitcoin network: `main`, `regtest`, `signet`, `testnet`. A `--regtest`/`--signet`/`--testnet`/`--mainnet` flag wins over it. Checked against the node. See "Networks". |
| `bitcoin.rpc_url` | `PSP_RPC_URL` | per network (`http://127.0.0.1:8332` on main) | Bitcoin Core's RPC address, without the wallet path. Must start with `http://` or `https://`. |
| `bitcoin.wallet` | `PSP_WALLET` | `psp-<network>` | The wallet that receives payments. Not empty, no `/`. |
| `bitcoin.cookie` | `PSP_COOKIE` | per network (`~/Library/Application Support/Bitcoin/.cookie` on main) | Bitcoin Core's RPC cookie file. Not empty. |
| `bitcoin.rpc_timeout_secs` | `PSP_RPC_TIMEOUT_SECS` | `30` | How long one RPC call may take. 1 to 600. |
| `scan.interval_secs` | `PSP_SCAN_INTERVAL_SECS` | `120` | Time between scans (`serve` only). 1 to 86400. |
| `scan.health_intervals` | `PSP_HEALTH_INTERVALS` | `3` | `/health` fails after this many scan intervals plus `scan.health_grace_secs` without a successful scan. 1 to 100. |
| `scan.health_grace_secs` | `PSP_HEALTH_GRACE_SECS` | `60` | Extra seconds on top of those intervals, for a slow price fetch. 0 to 86400. With the defaults `/health` fails after 3 × 120 + 60 = 420 s. |
| `price.max_age_secs` | `PSP_PRICE_MAX_AGE_SECS` | `120` | A quote further than this from now is excluded. 1 to 3600. |
| `price.max_spread_bps` | `PSP_PRICE_MAX_SPREAD_BPS` | `200` (2%) | Two usable quotes further apart than this (of the lower one) mean wait. 0 to 10000. |
| `price.timeout_secs` | `PSP_PRICE_TIMEOUT_SECS` | `10` | How long one ticker request may take. 1 to 600. |
| `settle.lock_confirmations` | `PSP_LOCK_CONFIRMATIONS` | `1` | Confirmations at which the price is locked. 1 to 1000. |
| `settle.min_confirmations` | `PSP_MIN_CONFIRMATIONS` | `6` | Confirmations at which the payment is settled. At least `lock_confirmations`, at most 1000. |
| `settle.price_tolerance` | `PSP_PRICE_TOLERANCE` | `0.01` (1%) | A fraction, 0 to 1, with at most four decimals (`0.01`, `0.005`, `0.025`). How far a payment may miss the requested amount, over or under (up to and including this), and still settle in full as if it were exact: every requested note is signed, and nothing is credited, dropped or kept. It covers the BTC price moving between the payer working out the amount, the price lock and settlement, which no payer should be penalised or credited for. Operators can widen it in a volatile market. It must not be more than `settle.payment_reconciliation_target`: the mint refuses to start otherwise. See "The price tolerance". |
| `settle.payment_reconciliation_target` | `PSP_PAYMENT_RECONCILIATION_TARGET` | `0.10` (10%) | A fraction, 0 to 1, with at most four decimals (`0.10`, `0.05`, `0.0525`). For a payment outside `settle.price_tolerance`, which is genuinely wrong: how far it may miss the requested amount before the mint stops absorbing the difference and starts reconciling it. **Over** by no more than this: the mint keeps the excess; over by more: the whole excess is recorded as credit for the payer's DID. **Short:** the smallest requested outputs are dropped until the signed total is no more than was paid; if that is within this of the amount paid the mint signs those notes and keeps the remainder, otherwise it signs nothing and holds the payment as paid but unsettled. One setting for both directions. See "The reconciliation target" for how to choose it. |
| `mint.max_denomination_units` | `PSP_MAX_DENOMINATION_UNITS` | `16777216` | Largest denomination (2^24, 25 keys). Only used when `mint_keys.json` is first made; an existing key file keeps its denominations. 1 to 2^62. |
| `rotation.activation_hours` | `PSP_ROTATION_ACTIVATION_HOURS` | `8760` (1 year) | Whole hours from the first issue to the first key rotation. **0 turns rotation off** (one key set, one spent list). 0 to 10,000,000. Restart only. See "Key rotation". |
| `rotation.period_hours` | `PSP_ROTATION_PERIOD_HOURS` | `87600` (10 years) | Whole hours between rotations. **0 also turns rotation off.** With rotation on it must be **at least 24**: BTC claims need time to settle before a rotation, and the mint refuses to start with a shorter period. 0 to 10,000,000. Restart only. |

The ranges are checked when the mint starts. They are wide on purpose: they refuse values that cannot work (0 confirmations, a ceiling too small for one note, a minimum below the lock) and values far outside any sensible use (a 1 GiB body, a 1000-confirmation wait, a day between scans), not values that are merely unusual. `settle.min_confirmations` must also be at least `settle.lock_confirmations`, so a price is always locked before settlement.

The numbers in the rest of this README (1 and 6 confirmations, the 0.01 price tolerance, the 0.10 reconciliation target, 2%, 120 s, 420 s for `/health`) are these defaults.

The price feeds are not settings. The three sources (Coinbase, Bitstamp and Gemini, see "Price") and their URLs are fixed in `src/price.rs`, and only those three are allowed. Changing a feed means changing the code.

## Wire format and versions

The request and response bodies, the payload and its sealed form, the key-set ID, the request hashes and the error codes are defined in the `sp-wire` crate, not here. `psp-mint` re-exports them (`psp_mint::payload`, `psp_mint::rotation::RolloverRequest`, `psp_mint::server::KeysetsResponse`, and so on), so the paths did not change. The ESP and FSP mints and the wallet build against the same crate, so they cannot drift from this mint by a byte.

`GET /health` returns `wire_version`, the crate's `WIRE_VERSION` (2: version 1 was the first release, with AIR 1.1.0's sealed envelope; version 2 is AIR 2.0.3's, 80 bytes of overhead instead of 82 or 83, and the only change). **Changing any encoded byte or JSON field in `sp-wire` means bumping `WIRE_VERSION`**, and a wallet refuses a mint that reports another number. Golden-bytes and size tests in `sp-wire` fail if a byte moves.

## HTTP API

The API is mint-server's handlers, the same ones `mintd` uses, mounted over the PSP's own mint in the same process. The exception is `/melt`, which is the PSP's own handler: it does what mint-server's does and also keeps the memo of each burn (see "Burn memos"), and `/burns` is the PSP's own route. The scanner and the API share the mint behind one mutex. The scanner locks it once per payment and never while it waits on the exchanges for a price.

| Route | What it does |
|---|---|
| `GET /keys` | The public key for each denomination. |
| `POST /melt` | Burns notes. Body: `{"inputs": [...], "memo": "..."}`. Every input's spend signature covers the memo, so the burn is bound to it. Returns `{"burned": ...}`. The ESP mint uses this to burn PSP in an exchange, with the memo it computes from the request. The memo is kept for every note the burn spends. |
| `POST /burns` | Body: `{"nonces": [...]}`. Returns `{"burns": [{"spent": true, "memo": "..."}, ...]}` in the same order. `memo` is the memo the note was burned under, and is `null` if the note is not spent or was spent with no memo on record. The ESP mint uses this on a retry. See "Burn memos". |
| `POST /check-state` | Body: `{"nonces": [...]}`. Returns which are spent. Looks in both live spent lists. |
| `GET /keysets` | Each live keyset's public keys by ID (current and previous). `/keys` is the current set only. |
| `GET /rotation` | Whether rotation is on, `first_issue`, `activation`, the period, the current and previous keysets (ID, epoch, start, `current_until`, `retires`), `next_rotation` and the mint's clock. |
| `POST /rollover` | Swaps notes for new notes of exactly the same amounts on the current key set. Body: `input_keysets`, inputs, `keyset`, outputs. `400` if refused, with `code` (`keyset_not_current`, `keyset_retired`, `unknown_keyset`, `missing_keyset`, `note_held`, `burn_not_proven`, or `note_spent_elsewhere` with `spent`: the positions of the inputs that are spent), and nothing is burned. Also the swap for wallet-to-wallet hand-overs. See "Key rotation". |
| `GET /audit` | `issued`, `redeemed`, `outstanding`. |
| `GET /health` | `200` with `status` `ok` (or `starting`, before the first scan finishes), `503` with `scan_stale` or `mint_lock_poisoned`. The body also has `wire_version` (an integer: the version of the wire format this mint speaks; a wallet refuses a mint whose number is not its own), `last_scan_unix`, `seconds_since_last_scan` (both null until a scan succeeds) and `stale_after_secs`. A scan counts when the wallet was read and every payment was processed as far as it could go, so a payment waiting on confirmations or a price is not a failure. `scan_stale` means no successful scan for `scan.health_intervals` intervals plus `scan.health_grace_secs` (by default three intervals plus 60 s). |

Every route that takes a body has a ceiling, 1 MiB by default (`server.*_max_request_bytes` above). A body over it is refused with `413` before it is parsed, so nothing is burned. A body that gives its `Content-Length` is refused without being read; one sent without it is read only up to the ceiling. The reply is JSON, with `error` like every other error here, plus `code` (`request_too_large`), `size_bytes` (the declared size, or `null`) and `limit_bytes`, as the ESP mint's `413` has:

```
{"error":"the request is 1048577 bytes, and this route accepts at most 1048576 bytes. Nothing was parsed or burned","code":"request_too_large","size_bytes":1048577,"limit_bytes":1048576}
```

### Burn memos

An exchange at the ESP mint burns PSP notes and then issues ESP. If the ESP mint stops after the burn, its retry has to know whether the notes it finds spent were burned by that exchange or by someone else, since a PSP note is a bearer note and its holder could spend it somewhere else. A spent flag cannot say, so the PSP keeps the memo each note was burned under.

- Every note's spend signature covers the memo, so the memo cannot be changed without the holder's signature. For an exchange the memo is `SHA-256("esp-mint/exchange/v1" || service || each output's amount and blind nonce)`, which is fixed by the exchange's service and outputs.
- `/melt` writes the memo to `melt_memos.json` (a JSON object from note nonce, in hex, to memo) and syncs the file before it burns, for every input that is not yet spent. It does this with the mint lock held, so no other burn can come between the record and the burn. Every burn goes through `/melt`, since the PSP mounts no other way to spend a note. So a note that is spent was spent by the burn whose memo is on record.
- A burn that is refused after its record was written (a bad signature, say) leaves a record for a note that is not spent. `/burns` reports a memo only for notes that are spent, and the next burn of that note replaces the record.
- A note that is already spent is never given a new memo. A second burn of it is refused as a double spend and the record stays as it was.
- A note spent with no record (spent before the log existed) reports `spent: true` and `memo: null`. The ESP mint treats that as spent elsewhere.
- The file is a placeholder store, like `settlements.json`: rewritten in full on every burn (temporary file, fsync, rename, directory fsync). A missing file means no records, and an unreadable file is an error and never an empty log. It is covered by "Local store" in "Left to do".

Not mounted, on purpose:

- **`/admin/issue`.** The PSP issues only from confirmed BTC payments. There is no unilateral issuance.
- **`/swap`.** PSP to FSP is out of scope. The plain PSP swap that settles a wallet-to-wallet hand-over is `POST /rollover` (below).

`serve` also exits (status 1, with a message on stderr) if the scan thread panics or a thread panics while holding the mint, since either leaves a process that answers but is broken. A supervisor should restart it.

There is no authentication and no TLS, and `/health` is open like the rest. A note authorizes its own spend, so a caller can only burn notes they hold. The default bind is localhost. Do not expose it to a network without TLS in front.

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

Rejected, underpaid and held payments are recorded too, not dropped. A payment that has reached `issued`, `rejected` or `paid_unsettled` is settled once and is not looked at again. Rejected payments are not refunded (see Not decided yet).

The transaction is marked `issued` before anything is signed, so a payment is issued at most once. If the process dies after that and before the signatures are delivered, they are lost.

### Settlement rules

- **Payment value** is the sum of the outputs in that transaction that pay the PSP's address. It is read from the chain, never from the payload.
- **Rate** is 10 PSP = 1 USD (1 PSP = $0.10), paid in BTC, at the price locked at 1 confirmation (see Price below). So $1 buys exactly 10 PSP, and 10 PSP exchange at the ESP mint for 10^12 ESP. Ten to the dollar, and not a thousand, keeps the OP_RETURN small: each output the payment names costs 41 bytes (an 8-byte amount and a 33-byte point), a hundred times finer a unit would add about 7 denominations (up to 7 more outputs, 287 bytes), and the OP_RETURN limit is not raised. The top denomination is 2^24 so that a purchase still covers the same dollar range (it was 2^20 at one PSP to the dollar). A purchase of one of every denomination is 25 outputs and 1,140 bytes sealed, well inside the 255 outputs the payload allows.
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

The payload is encrypted to the mint's DID public key, and only the mint's DID secret key can decrypt it. The cipher is AIR's (2.0.3), from `air::names::secp256k1`, which `sp-wire` re-exports, not a new scheme: `PublicKey::encrypt` seals bytes to a public key and `SecretKey::decrypt` opens them. It does an ElligatorSwift key exchange with a fresh sender key for every message, then ChaCha20-Poly1305. No payer signature is needed.

- **Wallet.** Build the payload exactly as in "Payload format", seal it to the mint's published DID public key, and put the result in the OP_RETURN. The wallet library's `build_psp_payload` does all of it (below). `psp-mint encrypt <mint pubkey hex> <payload hex>` seals a ready payload, for testing.
- **Mint.** The scanner tries to decrypt each data output with the mint's DID secret key and reads only what decrypts. Anything that fails is not for this mint. It is ignored without a record, a price fetch or an error. This covers a payload sealed to another key, a tampered one, and a cleartext one. Nothing is accepted in cleartext any more.
- **Wire format.** `64-byte ElligatorSwift sender key || 16-byte tag || ciphertext`, and nothing else. There is no length byte and no framing, so a sealed payload is always 80 bytes longer than the payload, whatever its length, and anything under 81 bytes cannot be a sealed payload. A sealed payload is a constant-size envelope around a ciphertext as long as the payload; bytes added or removed make the tag fail, so they are rejected. The sender key is an ElligatorSwift encoding, which looks like random bytes. This is wire version 2. Version 1 (AIR 1.1.0) used a `postcard` envelope of 82 or 83 bytes, and the mint no longer reads it (see "Wire format and versions").
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

The notes are the set bits of the purchase in PSP (it is split into powers of two), at 41 bytes each, plus 115 bytes of DID, count and encryption (35 for the reply and count, 80 for the envelope). At one PSP to the dollar a purchase under $10,000 took at most 13 notes (648 bytes); at ten to the dollar it takes at most 16 (771 bytes), the cost of the finer unit. The most any purchase can take is one of each of the 25 denominations: 1,140 bytes.

### Wallet

The `ecash-wallet` crate (a sibling folder, with a thin command line) is the client for this mint. Its hook for an AIR app is:

```
build_psp_payload(outputs, reply_did, mint_did_pubkey) -> bytes
```

`outputs` are the note sizes in PSP (powers of two). The call blinds each output, encodes the payload with `payload::encode`, seals it to `mint_did_pubkey` with `seal::encrypt`, and returns the bytes for the OP_RETURN. The wallet holds no BTC wallet, so the caller's own Bitcoin wallet sends the payment. The blinding factors and spend keys are saved in the wallet file before the bytes are returned, because the signatures cannot be turned into notes without them. When the signatures come back, `claim_psp` pairs them with the saved outputs by blind nonce, unblinds them against the keys from `GET /keys`, and stores the notes.

The wallet calls this crate's `payload::encode` and `seal::encrypt` by path, so the format is defined once, here.

### Left to do

The items every repo shares (all traffic over the AIR except local processes on one machine, the local store and the AIR storage rule, the AIR path dependency, the shared wire format, and the mint-core swap limit) are listed once in the top-level `~/sandbox-mint/README.md`. This list has only this repo's own items.

- **Settlement records (deferred to the AIR).** They are kept in a single local JSON file as a placeholder. The processed-transaction store moves into the AIR, with the mint's database.
- **Local store.** psp-mint's own state is `db/` (sled), `settlements.json`, `melt_memos.json`, the key files and `rollovers.json`. Only one process can open the mint (so `serve` and the one-shot run cannot overlap), a live `db/` must not be copied as a backup, and the `serve` design, with the API and the scanner sharing one process and one mutex, assumes this local store and will change with it. The AIR storage that replaces sled must keep the rule from mint-core v0.1.3: marking an output's blind nonce used and adding its amount to the issued total are one atomic step.
- **Running as a service.** `serve` exits with a non-zero status if the scan thread dies or the mint lock is poisoned, but nothing restarts it. A payment is only priced when a scan sees it at 1 confirmation or later, so downtime can cost a price lock. Something like `launchd` or `systemd` should restart it, and a monitor should poll `/health`.
- **API hardening.** The HTTP API has no TLS and no authentication, and binds to localhost. Rate limiting is not built.
- **AIR messaging for reconciliation (deferred to the AIR).** Two things are recorded only, because the messages to the payer's DID are not built: a credit (an overpayment beyond the reconciliation target) and a payment held as `paid_unsettled` (an underpayment no partial fill can settle). The held payment also needs the second half: the payer's new matching request arriving over AIR, being checked against the stored payment, signed, and marked settled so it cannot be claimed twice. Credit and held records are kept for the payer's DID indefinitely; whether they ever become a dead obligation is a policy question for later. A local stand-in for settling held payments is deliberately not built; it waits on AIR.
- **Signature delivery (deferred to the AIR).** The signatures are printed to the terminal, as a temporary development stand-in. They will be sent to the DID as an AIR DID message, paid for by the mint. Not built.
- **Silent payments (deferred, behind the scanner interface).** A static address, with a scanner that finds payments by a scan key, can plug in through the scanner interface (see "The scanner interface") without changing settlement. Today the PSP scans a normal wallet address.
- **Mint key storage (deferred to the AIR).** The note-signing keyfile and the DID key file are plain JSON with `0600` permissions in the data directory. The mint keystore moves into the AIR, and how the keys are protected there is not decided.
- **Publishing the DID public key.** Wallets get the key from `psp-mint pubkey`. How the key is published to them, and how it is rotated, is not decided.

### Not decided yet

Whether a rejected payment (bad payload, no data output, or a reused blind nonce) is ever refunded. The same question applies to a payment that is ignored because its data does not decrypt. Such a payment leaves no record, so the PSP could not tell the payer why nothing was issued.
