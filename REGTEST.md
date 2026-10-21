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

## 6. Confirm the payment and read the payload back

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
  settle.min_confirmations = 6 confirmations (default)
  settle.price_tolerance = 0.01 fraction, 0 to 1 (0.01 is 1%) (default)
  settle.payment_reconciliation_target = 0.10 fraction, 0 to 1 (0.10 is 10%) (default)
  mint.max_denomination_units = 16777216 PSP units (default)
```

Then it scans the `psp-regtest` wallet once, moves every payment as far as it can go, and exits.

- A payment with 1 to 5 confirmations that has no record yet gets one. The run takes the BTC/USD price, stores it, and prints `price locked at ... USD/BTC, waiting: 1 of 6 confirmations`.
- A payment with 6 confirmations is settled with the stored price. For the step 5 payment (`value_sats=100000`) it prints the over/under in basis points and one `signature <blind nonce> <signature>` line per signed output (two, for the 4 and 1 unit outputs). These lines are a log: the payer gets the same pairs, and the keysets, as one record on the AIR server, and the next line is `delivery <txid>: record written` (a failed write prints `not written (...); will retry` and is tried again after 10 seconds, then 20, up to 15 minutes). The wallet claims them with `claim-psp`, with nothing pasted. Its record in `settlements.json` then shows `issued: true` and a `delivery` with `delivered: true`.
- A payment that is already settled prints `already settled`. A payment with 0 confirmations prints `waiting`.

If you run it for the first time when the payment already has 6 confirmations, it locks the price in that run and settles in the same run. The record is marked `late`.

The run needs internet access, because the price comes from three live exchanges even on regtest. A regtest coin is priced like a real one, so 100,000 sats buys about 800 PSP units ($0.10 each) at current prices, and the 5 units ($0.50) in the step 5 payload cost far less than that. The payment is overpaid by a wide margin, and the over/under shows it (far beyond the 0.10 reconciliation target, so the whole excess is recorded as credit for the payer's DID, open for the reconciliation window and then forfeited if nothing claims it). If no price can be taken it prints `waiting: no price this run` and the payment is tried again on the next run.

A payment whose data does not open with the mint's DID key is ignored: no record, no price fetch, no rejection and no error. That covers a payload sealed to another key, a tampered one, and the cleartext payloads from before encryption. The one-shot run prints how many it ignored, and `serve` prints nothing. To test again after this change, repeat steps 5 and 6 to send and confirm a new payment.

Notes on running it:

- **The one-shot run runs once.** Each payment has to be seen at 1 confirmation so its price is locked then, so in real use run `serve` (step 8), which rescans on its own. The one-shot run cannot run while `serve` is up, because the mint's database can only be opened by one process.
- **It sees every transaction since the last scan** (`listsinceblock` from the checkpoint, see the [README](README.md), "Networks"). The first scan reads the whole wallet.
- **Data directory.** State lives in `~/.psp-mint/regtest` here (`~/.psp-mint/<network>`; override the parent with `PSP_DATA_DIR`): `mint_keys.json` (the mint's secret keys, created on first run), `mint_did_key.json` (the mint's DID key pair, which payloads are sealed to, created on first use), `db/` (the mint's store), `settlements.json` (one record per payment) and `melt_memos.json` (the memo each burned note was burned under, see "Burn memos"). Do not delete it. Without `mint_keys.json`, notes already issued cannot be redeemed, and without `mint_did_key.json`, payments already sent to the mint's DID key can no longer be read. A damaged key file is an error and is never replaced. An old `processed_txids.json` is imported into `settlements.json` on the first run, as issued records with no stored price, and left in place.
- **Denominations.** Notes are powers of two from 1 to 16,777,216 units (2^24, 25 keys), and a unit is $0.10. A request must use those, so 5 units is a 4 and a 1.
- **Data outputs.** A transaction needs exactly one data output, holding the sealed payload. With none, or with more than one, it is recorded as rejected and nothing is issued. If the one data output does not open, the payment is ignored instead (above).
- **Signatures.** The `signature` lines are a log. The signatures are stored in the payment's record (`delivery`) and written to the AIR server as one record the payer's wallet reads (`claim-psp`).
- **Held and rejected payments** get a record too (that the payment is held, or the reason it was rejected, with what was received) and stay open for `settle.reconciliation_window_days` (7 by default; set `PSP_RECONCILIATION_WINDOW_DAYS=1` to watch it close). When the window ends the run prints `forfeited <txid>`, the record in `settlements.json` gets `forfeited_at`, the mint keeps the bitcoin, and `/audit` (under `serve`) lists the payment in `reconciliation.forfeited`.

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

(The mint's keys are not on HTTP: a wallet asks for them over AIR.) `/audit` shows `issued`, `redeemed` and `outstanding`, and a `reconciliation` section with the payments still open and the ones forfeited, apart from those. To watch a payment move through, send one (step 5), mine 1 block (step 6) and wait for a scan: the server prints `price locked at ... USD/BTC, waiting: 1 of 6 confirmations`. Mine 5 more blocks and after the next scan it settles, prints the signatures and writes the delivery record. `/audit` then shows the issued total. Stop it with Ctrl-C.

Scan failures (for example the node being down) are printed and retried on the next interval. Payments that are already settled are not printed again.

Steps 1 to 8 run straight through. The reference material is in the [README](README.md) and [DESIGN.md](DESIGN.md).
