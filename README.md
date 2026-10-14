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

The confirmations are `1` and then `6`. The `nulldata` output has an `asm` of `OP_RETURN` followed by the payload hex, identical to what was sent. The raw `hex` of that output is `6a4c75` followed by the payload: `OP_RETURN`, `OP_PUSHDATA1`, then the length byte.

### 7. Run the PSP

With the node running, from the `psp-mint` directory:

```
cargo test
cargo run
```

Each run scans the `psp` wallet once, settles every payment that is ready, and exits. For the step 5 payment it prints the txid with `confirmations=6` and `value_sats=100000`, then the BTC/USD price it used, then one `signature` line per requested output (two, for the 4 and 1 unit outputs). Run it again and it prints `already processed`. A payment with fewer than 6 confirmations prints `waiting`.

The run needs internet access, because the price comes from three live exchanges even on regtest. A regtest coin is priced like a real one, so 100,000 sats buys about 80 PSP units at current prices. If no price can be taken it prints `waiting: no price this run` and the payment is tried again on the next run.

A payment made before the reply became a 33-byte DID (a different reply length) is rejected as `BadPayload(WrongReplyLength)`. To test again after this change, repeat steps 5 and 6 to send and confirm a new payment.

Notes on running it:

- **It runs once.** Run it again as payments reach 6 confirmations. There is no loop yet.
- **It sees the last 1000 wallet transactions.** Older ones are not scanned.
- **Data directory.** State lives in `~/.psp-mint` (override with `PSP_DATA_DIR`): `mint_keys.json` (the mint's secret keys, created on first run), `db/` (the mint's store) and `processed_txids.json`. Do not delete it. Without the keys, notes already issued cannot be redeemed.
- **Denominations.** Notes are powers of two from 1 to 1,048,576 units. A request must use those, so 5 units is a 4 and a 1.
- **Data outputs.** A transaction needs exactly one data output. With none, or with more than one, it is skipped and nothing is issued.
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
3. The PSP does nothing with the transaction until it has 6 confirmations. Nothing is read before that.
4. At 6 confirmations the PSP decodes the payload, checks it against the payment, issues, and delivers the signatures to the DID in the reply field.

### Settlement rules

- **Payment value** is the sum of the outputs in that transaction that pay the PSP's address. It is read from the chain, never from the payload.
- **Rate** is 1 PSP = 1 USD, paid in BTC. The BTC/USD price is taken as described under Price below.
- **Allowed amount** is the payment value in USD at that price, in whole PSP units, rounded down: `floor(sats * usd_cents_per_btc / 10^10)`. For example, 100,000 sats at $100,000 per BTC is 100 units.
- The sum of the requested output amounts must be less than or equal to the allowed amount, or nothing is issued.
- Every output is validated before any is issued: the denomination exists, there are no duplicate blind nonces within the request, and no blind nonce was used by an earlier payment.
- Each transaction is processed once, keyed by txid. The processed txids are kept in a single JSON file, a sorted array of txid strings, rewritten in full after each addition (written to a temporary file, then renamed). A missing file means none processed yet, and an unreadable file is an error, never an empty set. This is a placeholder, and the state probably needs to eventually "live on the air".
- The txid is recorded before anything is issued, so a payment is issued at most once. If the process dies after that and before the signatures are delivered, they are lost.
- A payment that is rejected (bad payload, unknown denomination, duplicate or already used blind nonce, or a request above the allowed amount) is not recorded and nothing is issued. It is reported again on every run.
- A payment with no usable price this run is also not recorded. It is tried again on the next run.
- Overpayment is kept by the PSP for now (see Not decided yet), so request outputs that use the whole payment.
- Once payloads are encrypted (not yet implemented), a payload copied into someone else's transaction is harmless. The signatures go to the DID inside the payload, which the copier cannot change, and only the payer holds the blinding factors that turn them into notes. The copier ends up paying for the payer's notes.

### Price

The price is fetched only when a payment is ready to settle: 6 or more confirmations, not processed, and the payload decodes and validates. It is fetched at most once per run, never polled.

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

The price is taken at run time, not exactly at the 6th confirmation. A confirmed payment waits for the next run, so the price can differ from the price at the 6th confirmation. Waiting for a price adds the same kind of delay. The rule is in `src/price.rs`, with the aggregation as a pure tested function.

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
- **Processed txids.** They are kept in a single JSON file as a placeholder. Replace it with a proper store. This state probably needs to eventually "live on the air".
- **Signature delivery.** The signatures are printed to the terminal. How they reach the DID in the reply field is not designed (see Not decided yet).
- **Mint key storage.** The keyfile is plain JSON with `0600` permissions in the data directory. How the real keys are protected is not decided.

### Not decided yet

Three decisions are open. Options are listed, none is chosen.

**1. Overpayment leftover.** A payment buys whole units at the run-time price, and the rest is kept by the PSP today. Options:

- Keep it. Nothing to build. The payer loses the leftover.
- Refund it in BTC. The payload would need a refund address, and the on-chain fee to send it can be larger than a small leftover.
- Credit it to the DID, to be used on a later purchase. The mint would need state per DID.

**2. BTC/USD price timing and source.** The source is set (three exchanges, median). The timing is open. Today it is the price at run time. Options:

- Price at run time (current). Simple, but it depends on when the run happens.
- Price when the payment is first seen. Needs the run to see it before it confirms, and a stored price.
- Price at the 6th confirmation block's time. Closest to the intended moment, but the ticker endpoints only give the current price, so it needs a historical price source or prices recorded at the time.

**3. Signature delivery when the payer has no AIR balance.** Options:

- The mint pays the delivery cost.
- A fixed delivery fee is taken out of each purchase.
- The payer fetches the signatures from the mint by txid or DID, which means the mint stores them.
- A public feed keyed by txid. The signatures are of no use without the payer's blinding factors, but the feed grows without bound.

Also not decided: what happens to a payment the PSP rejects, and the fee.
