# psp-mint

## Setup

Targets Bitcoin Core 31.1 on regtest, a private chain where you mine your own blocks and coins. These steps are for macOS on Apple Silicon.

psp-mint depends on the `air` crate by path (`../air`, which itself needs `../substance`) for the payload encryption, so those two folders must sit next to `psp-mint`.

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

The payer wallet sends a payment to the PSP wallet with the payload in a data output. The payload is encrypted to the mint's DID public key first, so there are two extra steps: get the key, and seal the payload with it. The example payload is the known-answer payload from `src/payload.rs` (a 33-byte reply holding the DID `0279be66…1798`, two outputs). `fee_rate` is needed because a regtest node has no fee estimates.

```
cd ~/psp-mint
MINT_KEY=$(cargo run -q -- pubkey)
PLAIN=210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4
HEX=$(cargo run -q -- encrypt $MINT_KEY $PLAIN)
PSP_ADDR=$($CLI -rpcwallet=psp getnewaddress)
$CLI -rpcwallet=payer -named send outputs="[{\"$PSP_ADDR\":0.001},{\"data\":\"$HEX\"}]" fee_rate=10
$CLI -rpcwallet=psp listtransactions
```

`pubkey` prints the mint's DID public key (66 hex characters) and makes the key file on first use. It reads `PSP_DATA_DIR`, so use the same value as the server will. `encrypt` seals a hex payload to a public key and prints the result as hex, which is different every time. It is a test tool with a fixed payload. A real purchase is made by the wallet, which blinds its own outputs and keeps the blinding factors: `ecash-wallet buy-psp --mint-key $MINT_KEY 4 1` prints the sealed payload for the same `send` command (see the ecash-wallet README). `send` prints a `txid` with `"complete": true`. `listtransactions` shows a `receive` of `0.00100000` with `confirmations` 0, because no block has mined it yet.

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

The confirmations are `1` and then `6`. To watch the price lock, run `cargo run` (step 7) between the two `generatetoaddress` commands, once after the first confirmation and again after the sixth. The `nulldata` output has an `asm` of `OP_RETURN` followed by the sealed payload hex, identical to what was sent, and nothing in it is readable. The raw `hex` of that output is `6a4cc7` followed by the sealed payload: `OP_RETURN`, `OP_PUSHDATA1`, then the length byte (199 for the example, which is 117 plus 82).

### 7. Run the PSP once

This is the one-shot run, for testing. With the node running, from the `psp-mint` directory:

```
cargo test
cargo run
```

Each run scans the `psp` wallet once, moves every payment as far as it can go, and exits.

- A payment with 1 to 5 confirmations that has no record yet gets one. The run takes the BTC/USD price, stores it, and prints `price locked at ... USD/BTC, waiting: 1 of 6 confirmations`.
- A payment with 6 confirmations is settled with the stored price. For the step 5 payment (`value_sats=100000`) it prints the over/under in basis points and one `signature <blind nonce> <signature>` line per signed output (two, for the 4 and 1 unit outputs). These lines are a temporary development stand-in for delivery over the AIR. The blind nonce says which output a signature belongs to, so a wallet can claim the right notes even when the PSP signed only some of them. Its record in `settlements.json` then shows `issued: true`.
- A payment that is already settled prints `already settled`. A payment with 0 confirmations prints `waiting`.

If you run it for the first time when the payment already has 6 confirmations, it locks the price in that run and settles in the same run. The record is marked `late`.

The run needs internet access, because the price comes from three live exchanges even on regtest. A regtest coin is priced like a real one, so 100,000 sats buys about 80 PSP units at current prices, and the 5 units in the step 5 payload cost far less than that. The payment is overpaid by a wide margin, and the over/under shows it (well over +10%, so a credit is recorded). If no price can be taken it prints `waiting: no price this run` and the payment is tried again on the next run.

A payment whose data does not open with the mint's DID key is ignored: no record, no price fetch, no rejection and no error. That covers a payload sealed to another key, a tampered one, and the cleartext payloads from before encryption. The one-shot run prints how many it ignored, and `serve` prints nothing. To test again after this change, repeat steps 5 and 6 to send and confirm a new payment.

Notes on running it:

- **The one-shot run runs once.** Each payment has to be seen at 1 confirmation so its price is locked then, so in real use run `serve` (step 8), which rescans on its own. The one-shot run cannot run while `serve` is up, because the mint's database can only be opened by one process.
- **It sees the last 1000 wallet transactions.** Older ones are not scanned.
- **Data directory.** State lives in `~/.psp-mint` (override with `PSP_DATA_DIR`): `mint_keys.json` (the mint's secret keys, created on first run), `mint_did_key.json` (the mint's DID key pair, which payloads are sealed to, created on first use), `db/` (the mint's store) and `settlements.json` (one record per payment). Do not delete it. Without `mint_keys.json`, notes already issued cannot be redeemed, and without `mint_did_key.json`, payments already sent to the mint's DID key can no longer be read. A damaged key file is an error and is never replaced. An old `processed_txids.json` is imported into `settlements.json` on the first run, as issued records with no stored price, and left in place.
- **Denominations.** Notes are powers of two from 1 to 1,048,576 units. A request must use those, so 5 units is a 4 and a 1.
- **Data outputs.** A transaction needs exactly one data output, holding the sealed payload. With none, or with more than one, it is recorded as rejected and nothing is issued. If the one data output does not open, the payment is ignored instead (above).
- **Signatures.** They are printed once, at issue time, and are not stored anywhere. Printing is a temporary development stand-in for delivery to the payer's DID over the AIR, which is not built (see Left to do). A buyer cannot receive notes this way in real use.

Settings are environment variables:

| Variable | Default |
|---|---|
| `PSP_RPC_URL` | `http://127.0.0.1:18443` |
| `PSP_WALLET` | `psp` |
| `PSP_COOKIE` | `~/Library/Application Support/Bitcoin/regtest/.cookie` |
| `PSP_DATA_DIR` | `~/.psp-mint` |
| `PSP_BIND` | `127.0.0.1:3000` (serve only) |
| `PSP_SCAN_INTERVAL_SECS` | `120` (serve only) |
| `PSP_PRICE_MAX_AGE_SECS` | `120` |
| `PSP_PRICE_MAX_SPREAD_BPS` | `200` (2%) |

### 8. Run the PSP server

`serve` runs the HTTP API and rescans the wallet on a timer, in one process. Stop the one-shot run first if it is going. For testing, use a short interval:

```
PSP_SCAN_INTERVAL_SECS=10 cargo run -- serve
```

It prints `scanning wallet psp every 10 s` and `psp-mint api listening on http://127.0.0.1:3000`. In another terminal:

```
curl -s http://127.0.0.1:3000/keys
curl -s http://127.0.0.1:3000/audit
curl -s http://127.0.0.1:3000/health
```

`/keys` lists a public key for each denomination (1 to 1,048,576). `/audit` shows `issued`, `redeemed` and `outstanding`. To watch a payment move through, send one (step 5), mine 1 block (step 6) and wait for a scan: the server prints `price locked at ... USD/BTC, waiting: 1 of 6 confirmations`. Mine 5 more blocks and after the next scan it settles and prints the signatures. `/audit` then shows the issued total. Stop it with Ctrl-C.

Scan failures (for example the node being down) are printed and retried on the next interval. Payments that are already settled are not printed again.

## HTTP API

The API is mint-server's handlers, the same ones `mintd` uses, mounted over the PSP's own mint in the same process. The scanner and the API share the mint behind one mutex. The scanner locks it once per payment and never while it waits on the exchanges for a price.

| Route | What it does |
|---|---|
| `GET /keys` | The public key for each denomination. |
| `POST /melt` | Burns notes. Body: `{"inputs": [...], "memo": "..."}`. Every input's spend signature covers the memo, so the burn is bound to it. Returns `{"burned": ...}`. The ESP mint uses this to burn PSP in an exchange, with the memo it computes from the request. |
| `POST /check-state` | Body: `{"nonces": [...]}`. Returns which are spent. |
| `GET /audit` | `issued`, `redeemed`, `outstanding`. |
| `GET /health` | `200` with `status` `ok` (or `starting`, before the first scan finishes), `503` with `scan_stale` or `mint_lock_poisoned`. The body also has `last_scan_unix`, `seconds_since_last_scan` (both null until a scan succeeds) and `stale_after_secs`. A scan counts when the wallet was read and every payment was processed as far as it could go, so a payment waiting on confirmations or a price is not a failure. `scan_stale` means no successful scan for three intervals plus 60 s. |

Not mounted, on purpose:

- **`/admin/issue`.** The PSP issues only from confirmed BTC payments. There is no unilateral issuance.
- **`/swap`.** PSP to FSP is out of scope, and Service Points are not exchanged peer to peer.

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
- A payment with no data output, or with more than one, is rejected. So is a payload that decrypts but cannot be decoded.
- A payment whose data output does not decrypt with the mint's DID key is not for this mint. It is ignored: no record is made, no price is fetched, and it is not an error. The scanner looks at it again on every scan, which costs one failed decryption.
- A payment with no usable price at its first sighting gets no record. It is tried again on the next run.
- A sealed payload copied into someone else's transaction is harmless. The signatures go to the DID inside the payload, which the copier cannot change, and only the payer holds the blinding factors that turn them into notes. The copier ends up paying for the payer's notes. A copied payload also cannot be issued twice, because its blind nonces are used once.

#### Underpayment

- **Short by up to 1%.** Every requested output is signed. The mint absorbs the shortfall.
- **Short by more than 1%.** The mint signs the largest outputs whose total fits what was paid and skips the rest. It goes through the outputs from largest to smallest and signs each one only if the running total still fits in the whole units the payment buys (the payment value in USD, rounded down). Smaller outputs are still tried after a larger one is skipped.
- **The skipped value is kept by the mint and is not refunded or credited.** Buyers lose it. For example, 4,900 sats at $100,000 per BTC is $4.90 and buys 4 whole units. A request for a 4 and a 1 gets the 4 signed and the 1 skipped, and the mint keeps the rest. A payment that buys no whole unit gets nothing signed and is recorded as rejected.

Buyers should pay a little over the request, not under it. A payment more than 1% short loses value.

#### Overpayment

- **Below 10% over.** The mint keeps the excess.
- **10% or more over.** The mint issues the requested outputs and records the excess as credit (`credit_cents`) against the transaction.

The procedure for credit, a **placeholder that is not built**: the mint sends a message to the payer's DID over the AIR, signed with the mint's DID key, saying how much credit there is for the txid and asking for a new request for the remainder. The payer answers with a new request to be filled against the credit. The reply is an AIR message, paid for by the payer in AIR currency (PSP, ESP or FSP). AIR messaging does not exist yet, so today the credit is only recorded and printed.

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

All three are public, need no key, and the price used is the last trade. Gemini's timestamp is undocumented and aligned to the minute, so it only bounds the age of its quote to about a minute.

- A source is excluded if it errors, returns no price, or its quote time is more than 120 seconds from now (`PSP_PRICE_MAX_AGE_SECS`).
- 3 usable prices: take the median.
- 2 usable prices: average them (rounded down). If they differ by more than 2% of the lower one (`PSP_PRICE_MAX_SPREAD_BPS`, 200), wait and retry on the next run.
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

The payload is encrypted to the mint's DID public key, and only the mint's DID secret key can decrypt it. The cipher is AIR's, from `air::names::secp256k1`, not a new scheme: `PublicKey::encrypt` seals bytes to a public key and `SecretKey::decrypt` opens them. It does an ElligatorSwift key exchange with a fresh sender key for every message, then ChaCha20-Poly1305. No payer signature is needed.

- **Wallet.** Build the payload exactly as in "Payload format", seal it to the mint's published DID public key, and put the result in the OP_RETURN. The wallet library's `build_psp_payload` does all of it (below). `psp-mint encrypt <mint pubkey hex> <payload hex>` seals a ready payload, for testing.
- **Mint.** The scanner tries to decrypt each data output with the mint's DID secret key and reads only what decrypts. Anything that fails is not for this mint. It is ignored without a record, a price fetch or an error. This covers a payload sealed to another key, a tampered one, and a cleartext one. Nothing is accepted in cleartext any more.
- **Wire format.** AIR's `Encrypted` has private fields, so the bytes on chain are its `postcard` encoding (serde is the only way in or out): `0x40`, the 64-byte sender key, the 16-byte tag, the length of the ciphertext (1 byte below 128, 2 bytes below 16,384), then the ciphertext, which is as long as the payload. Bytes left over after that are rejected. A payload is therefore 82 bytes longer when encrypted (83 from 128 bytes up). The sender key is sent as an ElligatorSwift encoding, which looks like random bytes.
- **Keys.** The mint's DID key pair is a local file, `mint_did_key.json`, in the data directory, with `0600` permissions. It is made on first use and never overwritten. `psp-mint pubkey` prints the public half, 33 bytes in hex, for wallets to seal to. Moving the key into the AIR keystore is deferred.
- **Relay.** A sealed payload is over 83 bytes, the old standard limit for a data output. Bitcoin Core 30 and later relay larger ones by default, and 31.1 is what this targets. A node with an older policy will not relay the transaction.

Expected sizes, at 1 sat/vB and BTC at $81,672:

| Purchase | Notes | Payload | Output | Fee |
|---|---|---|---|---|
| $100 | 3 | 241 B | 253 vB | about $0.21 |
| $10,000 | 5 | 323 B | 338 vB | about $0.28 |
| $8,191 (worst case under $10,000) | 13 | 651 B | 666 vB | about $0.54 |

### Wallet

The `ecash-wallet` crate (a sibling folder, with a thin command line) is the client for this mint. Its hook for an AIR app is:

```
build_psp_payload(outputs, reply_did, mint_did_pubkey) -> bytes
```

`outputs` are the note sizes in PSP (powers of two). The call blinds each output, encodes the payload with `payload::encode`, seals it to `mint_did_pubkey` with `seal::encrypt`, and returns the bytes for the OP_RETURN. The wallet holds no BTC wallet, so the caller's own Bitcoin wallet sends the payment. The blinding factors and spend keys are saved in the wallet file before the bytes are returned, because the signatures cannot be turned into notes without them. When the signatures come back, `claim_psp` pairs them with the saved outputs by blind nonce, unblinds them against the keys from `GET /keys`, and stores the notes.

The wallet calls this crate's `payload::encode` and `seal::encrypt` by path, so the format is defined once, here.

### Left to do

- **Settlement records (deferred to the AIR).** They are kept in a single local JSON file as a placeholder. The processed-transaction store moves into the AIR, with the mint's database.
- **Local store is for development.** The mint's database (`db/`, sled) and the settlement records are on local disk only while the other pieces are built. They will eventually be stored in the AIR. Until then: only one process can open the mint (so `serve` and the one-shot run cannot overlap, and there is one instance, with an outage while it restarts), a live `db/` must not be copied as a backup (stop the process first, or the copy may be inconsistent), and everything about the data directory above applies. The `serve` design, with the API and the scanner sharing one process and one mutex, assumes this local store and will change with it.
- **Running as a service.** `serve` exits with a non-zero status if the scan thread dies or the mint lock is poisoned, but nothing restarts it. A payment is only priced when a scan sees it at 1 confirmation or later, so downtime can cost a price lock. Something like `launchd` or `systemd` should restart it, and a monitor should poll `/health`.
- **API hardening.** The HTTP API has no TLS and no authentication, and binds to localhost. Rate limiting is not built.
- **AIR messaging for credit.** Overpayments of 10% or more are recorded as credit only. The message to the payer's DID is not built.
- **Signature delivery (deferred to the AIR).** The signatures are printed to the terminal, as a temporary development stand-in. They will be sent to the DID as an AIR DID message, paid for by the mint. Not built.
- **API over the AIR (deferred).** All mint API calls will travel over the AIR. The HTTP API is the development transport.
- **Silent payments (deferred).** The PSP will switch to silent payments: a static address, with the scanner using a scan key. Today it scans a normal wallet address.
- **Mint key storage (deferred to the AIR).** The note-signing keyfile and the DID key file are plain JSON with `0600` permissions in the data directory. The mint keystore moves into the AIR, and how the keys are protected there is not decided.
- **AIR as a path dependency.** `air` is pulled in by path (`../air`, which needs `../substance`), so psp-mint builds only with those folders beside it. It also brings in AIR's whole dependency tree, for one encrypt and decrypt call. Using a published crate, or only the `names::secp256k1` module, is not decided.
- **Shared wire crate.** `ecash-wallet` depends on `psp-mint` by path for the payload format and the sealing, which also pulls in its HTTP server and the AIR tree. A small crate for just the wire format would be lighter for apps that embed the wallet. Path dependencies are fine while this is in development. Before anyone else needs to clone and build it, move the wire format into a small crate, or switch to git dependencies with pinned tags like mint-core.
- **Publishing the DID public key.** Wallets get the key from `psp-mint pubkey`. How the key is published to them, and how it is rotated, is not decided.

### Not decided yet

Whether a rejected payment (bad payload, reused blind nonce, or too small to buy any output) is ever refunded. The same question applies to a payment that is ignored because its data does not decrypt. Such a payment leaves no record, so the PSP could not tell the payer why nothing was issued.
