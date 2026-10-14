# psp-mint

## Setup

Targets Bitcoin Core 31.1 on regtest, a private chain where you mine your own blocks and coins. These steps are for macOS on Apple Silicon.

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
$CLI createwallet psp
$CLI -rpcwallet=psp getnewaddress
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

The payer wallet sends a payment to the PSP wallet with the payload in a data output. This example payload is the known-answer payload from `src/payload.rs` (a 33-byte reply holding the DID `0279be66…1798`, two outputs). `fee_rate` is needed because a regtest node has no fee estimates.

```
PSP_ADDR=$($CLI -rpcwallet=psp getnewaddress)
HEX=210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4
$CLI -rpcwallet=payer -named send outputs="[{\"$PSP_ADDR\":0.001},{\"data\":\"$HEX\"}]" fee_rate=10
$CLI -rpcwallet=psp listtransactions
```

`send` prints a `txid` with `"complete": true`. `listtransactions` shows a `receive` of `0.00100000` with `confirmations` 0, because no block has mined it yet.

### 6. Confirm the payment and read the payload back

Mine blocks to confirm the payment. Mine to a payer address, not a PSP address. Replace `TXID` with the txid from step 5.

```
TXID=<txid from step 5>
PAYER=$($CLI -rpcwallet=payer getnewaddress)
$CLI -rpcwallet=payer generatetoaddress 1 $PAYER > /dev/null
$CLI -rpcwallet=psp gettransaction $TXID | grep confirmations
$CLI -rpcwallet=payer generatetoaddress 5 $PAYER > /dev/null
$CLI -rpcwallet=psp gettransaction $TXID | grep confirmations
$CLI -rpcwallet=psp gettransaction $TXID true true | grep -B6 nulldata
```

The confirmations are `1` and then `6`. To watch the price lock, run `cargo run` (step 7) between the two `generatetoaddress` commands, once after the first confirmation and again after the sixth. The `nulldata` output has an `asm` of `OP_RETURN` followed by the payload hex, identical to what was sent. The raw `hex` of that output is `6a4c75` followed by the payload: `OP_RETURN`, `OP_PUSHDATA1`, then the length byte.

### 7. Run the PSP

With the node running, from the `psp-mint` directory:

```
cargo test
cargo run
```

Each run scans the `psp` wallet once, moves every payment as far as it can go, and exits.

- A payment with 1 to 5 confirmations that has no record yet gets one. The run takes the BTC/USD price, stores it, and prints `price locked at ... USD/BTC, waiting: 1 of 6 confirmations`.
- A payment with 6 confirmations is settled with the stored price. For the step 5 payment (`value_sats=100000`) it prints the over/under in basis points and one `signature` line per signed output (two, for the 4 and 1 unit outputs). Its record in `settlements.json` then shows `issued: true`.
- A payment that is already settled prints `already settled`. A payment with 0 confirmations prints `waiting`.

If you run it for the first time when the payment already has 6 confirmations, it locks the price in that run and settles in the same run. The record is marked `late`.

The run needs internet access, because the price comes from three live exchanges even on regtest. A regtest coin is priced like a real one, so 100,000 sats buys about 80 PSP units at current prices, and the 5 units in the step 5 payload cost far less than that. The payment is overpaid by a wide margin, and the over/under shows it (well over +10%, so a credit is recorded). If no price can be taken it prints `waiting: no price this run` and the payment is tried again on the next run.

A payment made before the reply became a 33-byte DID (a different reply length) is rejected as `BadPayload(WrongReplyLength)`. To test again after this change, repeat steps 5 and 6 to send and confirm a new payment.

Notes on running it:

- **It runs once, and must be run every few minutes.** Each payment has to be seen at 1 confirmation so its price is locked then. There is no loop or scheduler yet (see Left to do). Run it by hand, or from `cron` or `launchd`.
- **It sees the last 1000 wallet transactions.** Older ones are not scanned.
- **Data directory.** State lives in `~/.psp-mint` (override with `PSP_DATA_DIR`): `mint_keys.json` (the mint's secret keys, created on first run), `db/` (the mint's store) and `settlements.json` (one record per payment). Do not delete it. Without the keys, notes already issued cannot be redeemed. An old `processed_txids.json` is imported into `settlements.json` on the first run, as issued records with no stored price, and left in place.
- **Denominations.** Notes are powers of two from 1 to 1,048,576 units. A request must use those, so 5 units is a 4 and a 1.
- **Data outputs.** A transaction needs exactly one data output. With none, or with more than one, it is recorded as rejected and nothing is issued.
- **Signatures.** They are printed once, at issue time, and are not stored anywhere (see Left to do).

Settings are environment variables:

| Variable | Default |
|---|---|
| `PSP_RPC_URL` | `http://127.0.0.1:18443` |
| `PSP_WALLET` | `psp` |
| `PSP_COOKIE` | `~/Library/Application Support/Bitcoin/regtest/.cookie` |
| `PSP_DATA_DIR` | `~/.psp-mint` |
| `PSP_PRICE_MAX_AGE_SECS` | `120` |
| `PSP_PRICE_MAX_SPREAD_BPS` | `200` (2%) |

## Using BTC to buy PSP

A payer buys PSP by sending a Bitcoin payment that carries a request in a data output. The PSP needs no connection from the payer. Everything it needs is on chain.

### The flow

1. The payer builds blinded outputs, each an amount and a blind nonce, for the PSP they want.
2. The payer packs them into a payload (below) and creates a transaction that pays the PSP's address and has one data output holding the payload.
3. At 1 confirmation the PSP takes the BTC/USD price and locks it in a settlement record for the transaction. Nothing else is read yet.
4. At 6 confirmations the PSP decodes the payload, compares the payment with the request at the locked price, issues, and delivers the signatures to the DID in the reply field.

### Settlement records

The PSP keeps one record per transaction, created when the payment is first seen with 1 confirmation. Records are kept in `settlements.json` in the data directory, a JSON object from txid to record. The file is a placeholder store. It is rewritten in full on every change: written to a temporary file, fsynced, renamed over the old file, and the directory fsynced. A missing file means no records yet, and an unreadable file is an error, never an empty store. This state probably needs to eventually "live on the air".

| Field | Meaning |
|---|---|
| `locked_price_cents` | USD per BTC in whole cents, taken at the first sighting and never refreshed. Null only for records imported from the old `processed_txids.json`. |
| `issued` | True once at least one note was signed. |
| `over_under_bps` | Paid versus requested, in basis points of the requested total. Positive is overpaid, negative underpaid: `1250` is 12.50% over and `-80` is 0.80% under. It is (paid value minus requested value) divided by requested value, using the exact USD value at the locked price (not rounded down to whole units), truncated toward zero. Null when there is no valid request to compare with (no data output, undecodable payload, unknown denomination or duplicate blind nonce). |
| `status` | `locked` (waiting for 6 confirmations), `issued`, or `rejected`. |
| `late` | True if the payment was first seen with more than 1 confirmation, so the lock was taken later than intended. |
| `skipped_units` | Requested units that were not signed because of a large underpayment. |
| `credit_cents` | Overpayment of 10% or more, in USD cents. |
| `reason` | Why nothing was signed, when `status` is `rejected`. |

Rejected and underpaid payments are recorded too, not dropped. A payment that has reached `issued` or `rejected` is final and is not looked at again. Rejected payments are not refunded (see Not decided yet).

The transaction is marked `issued` before anything is signed, so a payment is issued at most once. If the process dies after that and before the signatures are delivered, they are lost.

### Settlement rules

- **Payment value** is the sum of the outputs in that transaction that pay the PSP's address. It is read from the chain, never from the payload.
- **Rate** is 1 PSP = 1 USD, paid in BTC, at the price locked at 1 confirmation (see Price below).
- **Amounts** are `u64`. Sums of amounts are checked, and a request whose total overflows is rejected. Conversions between sats, cents and units use `u128`. Tests cover the limits.
- **Requested** is the sum of the requested output amounts. It is compared with the payment value in USD at the locked price.
- Every output is validated before any is issued: the denomination exists, there are no duplicate blind nonces within the request, and no blind nonce was used by an earlier payment. A failure rejects the whole payment.
- A payment with no data output or an undecodable payload is rejected.
- A payment with no usable price at its first sighting gets no record. It is tried again on the next run.
- Once payloads are encrypted (not yet implemented), a payload copied into someone else's transaction is harmless. The signatures go to the DID inside the payload, which the copier cannot change, and only the payer holds the blinding factors that turn them into notes. The copier ends up paying for the payer's notes.

#### Underpayment

- **Short by up to 1%.** Every requested output is signed. The mint absorbs the shortfall.
- **Short by more than 1%.** The mint signs the largest outputs whose total fits what was paid and skips the rest. It goes through the outputs from largest to smallest and signs each one only if the running total still fits in the whole units the payment buys (the payment value in USD, rounded down). Smaller outputs are still tried after a larger one is skipped.
- **The skipped value is kept by the mint and is not refunded or credited.** Buyers lose it. For example, 4,900 sats at $100,000 per BTC is $4.90 and buys 4 whole units. A request for a 4 and a 1 gets the 4 signed and the 1 skipped, and the mint keeps the rest. A payment that buys no whole unit gets nothing signed and is recorded as rejected.

Buyers should pay a little over the request, not under it. A payment more than 1% short loses value.

#### Overpayment

- **Below 10% over.** The mint keeps the excess.
- **10% or more over.** The mint issues the requested outputs and records the excess as credit (`credit_cents`) against the transaction.

The procedure for credit, a **placeholder that is not built**: the mint sends a message to the payer's DID over JAMJAR, signed with the mint's DID key, saying how much credit there is for the txid and asking for a new request for the remainder. The payer answers with a new request to be filled against the credit. The reply is a JAMJAR message, paid for by the payer in JAMJAR currency (PSP, ESP or FSP). JAMJAR messaging does not exist yet, so today the credit is only recorded and printed.

### Signature delivery

The mint sends the signatures to the DID in the reply field over JAMJAR, and the mint pays for sending them. The payer needs no balance to receive them. JAMJAR messaging does not exist yet, so this is **not built**: today the signatures are printed to the terminal once, at issue time, and are not stored.

### Fees

The PSP has no fee of its own besides the BTC purchase. Fees at the later layers (exchange to ESP, transfer, and FSP use) are paid in JAMJAR currency and charged by JAMJAR messaging, not by the mints. The only explicit mint fees are the BTC at PSP purchase and the ESP redeem fee. A separate fee on top of the 1 USD rate is not defined or implemented.

### Price

The price is locked when a payment is first seen with 1 confirmation or more and has no record yet. The first run that sees the transaction fetches the price, once per run, and stores it in the record. Settlement at 6 confirmations uses the stored price and never a live one. The PSP does not poll the exchanges.

| Source | Endpoint | Price | Quote time |
|---|---|---|---|
| Coinbase | `https://api.exchange.coinbase.com/products/BTC-USD/ticker` | `price` | `time`, ISO 8601 UTC |
| Bitstamp | `https://www.bitstamp.net/api/v2/ticker/btcusd/` | `last` | `timestamp`, unix seconds |
| Gemini | `https://api.gemini.com/v1/pubticker/btcusd` | `last` | `volume.timestamp`, unix milliseconds |

All three are public, need no key, and the price used is the last trade. Gemini's timestamp is undocumented and aligned to the minute, so it only bounds the age of its quote to about a minute.

- A source is excluded if it errors, returns no price, or its quote time is more than 120 seconds from now (`PSP_PRICE_MAX_AGE_SECS`).
- 3 usable prices: take the median.
- 2 usable prices: average them (rounded down). If they differ by more than 2% of the lower one (`PSP_PRICE_MAX_SPREAD_BPS`, 200), wait and retry on the next run.
- 1 or 0 usable prices: wait and retry on the next run. The PSP never issues on a single feed.
- Prices are whole US cents per BTC. Decimal strings are parsed to cents with no floating point, and anything past the second decimal is dropped.

The price is a live ticker price taken when the run sees the payment, not a block-time price. The PSP does not use block timestamps or historical candles, because miners can skew a block's timestamp by about 2 hours. This is why the PSP must run every few minutes: the price is taken at the first run that sees the payment at 1 confirmation, and a run that comes late locks a later price and marks the record `late`. The rule is in `src/price.rs`, with the aggregation as a pure tested function.

### Payload format

Integers are big-endian.

| Part | Size |
|---|---|
| reply length (must be 33) | 1 |
| reply: the payer's JAMJAR DID | 33 |
| output count | 1 |
| each output: amount (8) and blind nonce (33) | 41 per output |

Total size is `35 + 41 * outputs`. For example, 3 outputs is 158 bytes. The signatures are delivered to the DID in the reply field. The mint treats the DID as opaque bytes and does not check it. A reply of any other length is rejected.

### Encryption is not part of the spec yet

The payload is currently cleartext. It must be encrypted by the payer and decrypted by the PSP, and that is not written into the PSP spec yet. Until it is, anyone who sees the transaction can read the payload, or replace it before it confirms. Use this on regtest for proof of concept only.

When it is added, the plan is ECIES to the mint's JAMJAR public key. The payer adds a one-time public key (33 bytes) and an AEAD tag (16 bytes), 49 bytes fixed. No payer signature is needed. The payload layout above is unchanged inside the encryption.

Expected encrypted sizes, at 1 sat/vB and BTC at $81,672:

| Purchase | Notes | Payload | Output | Fee |
|---|---|---|---|---|
| $100 | 3 | 207 B | 219 vB | about $0.18 |
| $10,000 | 5 | 289 B | 304 vB | about $0.25 |
| $8,191 (worst case under $10,000) | 13 | 617 B | 632 vB | about $0.52 |

### Left to do

- **Payload encryption.** The payer must encrypt the payload and the PSP must decrypt it, with ECIES as above. This is not written into the PSP spec yet, and the payload is currently cleartext.
- **Settlement records.** They are kept in a single JSON file as a placeholder. Replace it with a proper store. This state probably needs to eventually "live on the air".
- **Running every few minutes.** The PSP is a one-shot process and nothing schedules it yet. Until a scheduler or loop exists, a payment is only priced when someone runs it.
- **JAMJAR messaging for credit.** Overpayments of 10% or more are recorded as credit only. The message to the payer's DID is not built.
- **Signature delivery.** The signatures are printed to the terminal. Sending them to the DID over JAMJAR, paid by the mint, is not built.
- **Mint key storage.** The keyfile is plain JSON with `0600` permissions in the data directory. How the real keys are protected is not decided.

### Not decided yet

Whether a rejected payment (bad payload, reused blind nonce, or too small to buy any output) is ever refunded.
