# psp-mint: regtest walkthrough

Targets Bitcoin Core 31.1. The mint defaults to mainnet (see the [README](README.md), "Networks"); these steps use regtest, a private chain where you mine your own blocks and coins, so every `cargo run` in them passes `--regtest`. These steps are for macOS on Apple Silicon.

The reference sections are in the [README](README.md) and [DESIGN.md](DESIGN.md).

## 1. Install Bitcoin Core

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

## 2. Run a regtest node

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

## 3. Create the PSP wallet

The PSP watches its own Bitcoin Core wallet. Create one and get a receiving address. The shell variable lasts only for that terminal window.

```
CLI=~/bitcoin-core/bitcoin-31.1/bin/bitcoin-cli
$CLI createwallet psp-regtest
$CLI -rpcwallet=psp-regtest getnewaddress
```

The address starts with `bcrt1` on regtest.

## 4. Create a funded payer wallet

A second wallet plays the buyer. On regtest you mine your own coins. A block reward can be spent only after 100 more blocks, so mine 101.

```
$CLI createwallet payer
PAYER=$($CLI -rpcwallet=payer getnewaddress)
$CLI -rpcwallet=payer generatetoaddress 101 $PAYER > /dev/null
$CLI -rpcwallet=payer getbalance
```

The balance is `50.00000000`.

## 5. Send a test payment with a payload

The payer wallet sends a payment to the PSP wallet with the payload in a data output. The payload is the **claim key** and nothing else: a 33-byte public key, the throwaway key the payer will later sign its claim with. It is encrypted to the mint's DID public key first, so there are two extra steps: get the key, and seal the payload with it. The example payload is the known-answer claim key from `src/payload.rs` (the DID `0279be66…1798`). `fee_rate` is needed because a regtest node has no fee estimates.

```
cd ~/psp-mint
MINT_KEY=$(cargo run -q -- --regtest pubkey)
PLAIN=0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798
HEX=$(cargo run -q -- --regtest encrypt $MINT_KEY $PLAIN)
PSP_ADDR=$($CLI -rpcwallet=psp-regtest getnewaddress)
$CLI -rpcwallet=payer -named send outputs="[{\"$PSP_ADDR\":0.001},{\"data\":\"$HEX\"}]" fee_rate=10
$CLI -rpcwallet=psp-regtest listtransactions
```

`pubkey` prints the mint's DID public key (66 hex characters) and makes the key file on first use. It reads `PSP_DATA_DIR`, so use the same value as the server will. `encrypt` seals a hex payload to a public key and prints the result as hex, which is different every time. It is a test tool with a fixed payload; a real purchase is made by the wallet, which makes its own claim key and keeps its secret (the wallet's command for it is part of pass 2 of the claim change, see the ecash-wallet README). The payment carries no notes and no amount of PSP: what it buys is worked out by the mint from the sats and the price it locks. `send` prints a `txid` with `"complete": true`. `listtransactions` shows a `receive` of `0.00100000` with `confirmations` 0, because no block has mined it yet.

## 6. Confirm the payment and read the payload back

Mine blocks to confirm the payment. Mine to a payer address, not a PSP address. Replace `TXID` with the txid from step 5.

```
TXID=<txid from step 5>
PAYER=$($CLI -rpcwallet=payer getnewaddress)
$CLI -rpcwallet=payer generatetoaddress 1 $PAYER > /dev/null
$CLI -rpcwallet=psp-regtest gettransaction $TXID | grep confirmations
$CLI -rpcwallet=payer generatetoaddress 2 $PAYER > /dev/null
$CLI -rpcwallet=psp-regtest gettransaction $TXID | grep confirmations
$CLI -rpcwallet=psp-regtest gettransaction $TXID true true | grep -B6 nulldata
```

The confirmations are `1` and then `3`. To watch the price lock, run `cargo run` (step 7) between the two `generatetoaddress` commands, once after the first confirmation and again after the third. The `nulldata` output has an `asm` of `OP_RETURN` followed by the sealed payload hex, identical to what was sent, and nothing in it is readable. The raw `hex` of that output is `6a4c71` followed by the sealed payload: `OP_RETURN`, `OP_PUSHDATA1`, then the length byte (113, which is the 33-byte claim key plus 80 bytes of encryption).

## 7. Run the PSP once

This is the one-shot run, for testing. With the node running, from the `psp-mint` directory:

```
cargo test
cargo run -- --regtest
```

Each run first prints the settings in use and where each came from (see the [README](README.md), "Settings"). With no config file and no `PSP_` variables set:

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
  settle.base_confirmations = 3 confirmations (default)
  settle.confirmation_tiers = 10000:4,50000:5,100000:6 whole USD:confirmations, comma-separated (default)
  settle.retention_days = 90 days; 0 keeps every record (default)
  settle.prune_horizon_days = 210 days; 0 keeps every marker (default)
  claim.max_attempts = 2 attempts (default)
  claim.write_fee = 0 PSP units (default)
  delivery.read_fee = 0 PSP units (default)
```

Then it scans the `psp-regtest` wallet once, moves every payment as far as it can go, and exits.

- A payment with 1 confirmation or more that has no record yet gets one. The run takes the BTC/USD price, stores it, and prints `price locked at ... USD/BTC, N unit(s) owed once it has 3 confirmations (it has 1)`. The 3 is the payment's tier: its locked value picks 3, 4, 5 or 6 confirmations ("Confirmation tiers" in [DESIGN.md](DESIGN.md)).
- A payment that has the confirmations of its tier is **owed** its units. For the step 5 payment (`value_sats=100000`) the run prints `owed N unit(s) to claim key 0279be66… (record 0); once the record is stored over AIR the wallet can claim them, once (kind 15), within the claim window`, where N is what 100,000 sats buy at the locked price, rounded down (about 800 at $80,000 per BTC). Nothing is signed in this run. The next line is `delivery <txid>: record written` (a payer's records are written one per pass, lowest index first; a second payment to the same claim key is written on the following pass) (a failed write prints `not written (...); will retry` and is tried again after 10 seconds, then 20, up to 15 minutes). The payer's wallet reads that record from the AIR server and claims it once; the claim is answered by `serve`, not by the one-shot run. The record in `settlements.json` then shows `status: owed`, the `tier_confirmations`, `due_units`, a `delivery` with `delivered: true`, and, set in the same write, `landed_at` (when the record was stored) and `open_until` (30 days after it). Before the record is stored the payment has neither.
- A payment that is already owed prints `already settled`. A payment with 0 confirmations prints `waiting`.

If you run it for the first time when the payment already has 3 confirmations, it locks the price in that run and writes the owed record in the same run. The record is marked `late`.

The run needs internet access, because the price comes from three live exchanges even on regtest. A regtest coin is priced like a real one, so the step 5 payment buys several hundred PSP units ($0.10 each). If no price can be taken it prints `waiting: no price this run` and the payment is tried again on the next run.

A payment whose data does not open with the mint's DID key is ignored: no record, no price fetch and no error. That covers a payload sealed to another key, a tampered one, and the cleartext payloads from before encryption. A payload of the old design (notes inside, wire version 3) opens but is not a claim key, so it is kept: a record with status `kept` and the reason, and nothing owed. The one-shot run prints how many it ignored, and `serve` prints nothing. To test again after this change, repeat steps 5 and 6 to send and confirm a new payment.

Notes on running it:

- **The one-shot run runs once.** Each payment has to be seen at 1 confirmation so its price is locked then, so in real use run `serve` (step 8), which rescans on its own. The one-shot run cannot run while `serve` is up, because the mint's database can only be opened by one process.
- **It sees every transaction since the last scan** (`listsinceblock` from the checkpoint, see the [README](README.md), "Networks"), asking for the deepest tier's confirmations. The first scan reads the whole wallet.
- **Data directory.** State lives in `~/.psp-mint/regtest` here (`~/.psp-mint/<network>`; override the parent with `PSP_DATA_DIR`): `mint_keys.json` (the mint's secret keys, created on first run), `mint_did_key.json` (the mint's DID key pair, which payloads are sealed to, created on first use), `db/` (the mint's store), `settlements.json` (one record per payment) and `melt_memos.json` (the memo each burned note was burned under, see "Burn memos"). Do not delete it. Without `mint_keys.json`, notes already issued cannot be redeemed, and without `mint_did_key.json`, payments already sent to the mint's DID key can no longer be read. A damaged key file is an error and is never replaced. A `settlements.json` from psp-mint 0.7.x is not read: move it aside before the first run of 0.8.0.
- **Denominations.** Notes are powers of two from 1 to 2^63 units (every keyset has all 64 keys, 2^0 to 2^63; it is not a setting), and a unit is $0.10. The claim must use those, so 5 units is a 4 and a 1, and they must add up to the units owed exactly.
- **Data outputs.** A transaction needs exactly one data output, holding the sealed claim key. With none, or with more than one, it is kept and nothing is owed. If the one data output does not open, the payment is ignored instead (above). A payment worth under one unit, or one whose prepaid costs (`delivery.read_fee`, `claim.write_fee`) are as much as it buys, is kept too.
- **Signatures.** Nothing is signed until the claim. They are stored in the payment's record (`claimed`) and sent to the payer in the answer to the claim.
- **Payments that are never claimed** stay open for 30 days from the time the owed record was stored on the AIR server (not a setting: `sp_wire::psp::CLAIM_WINDOW_SECS`); a payment whose record is not stored yet has no window and does not expire. When the window ends `serve` prints `expired <txid>`, the record in `settlements.json` gets `expired_at`, the mint keeps the bitcoin, and `/audit` (under `serve`) counts the payment in `claims.expired`. `settle.retention_days` (90 by default; 0 keeps everything) later removes a finished payment (claimed, expired, kept with nothing owed, or abandoned) from `settlements.json`, leaving a short marker (its txid and block time, so the scan never owes it again); `/audit` keeps counting it. `settle.prune_horizon_days` (210 by default; 0 keeps every marker) drops the markers of payments whose block is older than that, and the scan then ignores a payment that old with no record.

## 8. Run the PSP server

`serve` runs the AIR listener, the local HTTP API and the wallet scan on a timer, in one process. Stop the one-shot run first if it is going. `serve` needs the AIR server it listens on: `PSP_AIR_SERVER` is that server's public key (66 hex characters) and `PSP_AIR_HOST` its host (`127.0.0.1` for the one bundled with the SPI). For testing, use a short interval:

```
PSP_AIR_SERVER=<air server key> PSP_AIR_HOST=127.0.0.1 PSP_SCAN_INTERVAL_SECS=10 cargo run -- --regtest serve
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
curl -s http://127.0.0.1:3000/audit
curl -s http://127.0.0.1:3000/health
```

(The mint's keys are not on HTTP: a wallet asks for them over AIR.) `/audit` shows `issued`, `redeemed` and `outstanding`, and a `claims` section with the payments claimed, still claimable (`unclaimed`) and expired, each with payments, sats and units, then the ones kept with nothing owed and the sub-unit amount kept. To watch a payment move through, send one (step 5), mine 1 block (step 6) and wait for a scan: the server prints `price locked at ... USD/BTC, N unit(s) owed once it has 3 confirmations (it has 1)`. Mine 2 more blocks and after the next scan it prints the owed record and writes it to the AIR server. A wallet then claims it over AIR, and `/audit` shows the issued total. Stop it with Ctrl-C.

Scan failures (for example the node being down) are printed and retried on the next interval. Payments that are already settled are not printed again.

Steps 1 to 8 run straight through. The reference material is in the [README](README.md) and [DESIGN.md](DESIGN.md).
