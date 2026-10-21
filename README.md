# psp-mint

psp-mint is the Primary Service Point (PSP) mint. A payer buys PSP with a Bitcoin payment that carries a sealed request in a data output; the mint watches its own Bitcoin Core wallet, locks the BTC/USD price at 1 confirmation and signs the requested blinded notes at 6. 10 PSP = $1. The ESP mint burns PSP here when a holder exchanges it for ESP. The mint rotates its keys and keeps no more than two key sets.

## Building

psp-mint builds from tagged dependencies: mint-core at `v0.1.3` and `sp-wire` at `v0.4.0` (git tags). `sp-wire` holds the wire types this mint shares with the other mints and the wallet (see [DESIGN.md](DESIGN.md), "Wire format and versions"), and it pulls in AIR (2.0.3, commit `8e281d5`, AIR has no tags) for the payload encryption. The next sp-wire (0.4.1) moves AIR to 2.1.0 (commit `741b1cf`): the sealed bytes and the `mint_did_key.json` format are the same, `WIRE_VERSION` stays 2, and psp-mint's code does not change. AIR 2.1.0 does not export `SecretKey` yet, so until sp-wire 0.4.1 is tagged and its AIR pin is a commit that does, psp-mint is built against the local `sp-wire` and AIR folders with a git-ignored `.cargo/config.toml` (sp-wire's README shows the AIR block). The repo's committed `Cargo.lock` keeps sp-wire at `v0.4.0` and cargo then reports the patch as "was not used", so run `cargo update -p sp-wire` once after adding the patches, check that `cargo tree --all-features -i air` shows the local AIR folder, and run `git restore Cargo.lock` before committing (a lock file that names a local path would not build from GitHub). **The AIR listener needs `air_server` too.** psp-mint asks sp-wire for its `air-client` feature (the transport of the mint-info request, see "The AIR listener"), which builds `air_server`; `air_server` names AIR as `path = "../air"` (upstream issue 3), so the git-ignored `.cargo/config.toml` also needs an `air_server` patch, and the AIR patch must be the folder `<air_server>/../air`, or two `air` crates are built and their types do not match. For now both are the sandbox checkouts: `air = { path = ".../sandbox-mint/air-sandbox/air" }` under `[patch."https://github.com/ramp-stack/air"]` and `air_server = { path = ".../sandbox-mint/air-sandbox/air_server" }` under `[patch."https://github.com/ramp-stack/air_server"]`. The listener is part of `serve` and always runs: `serve` will not start without `air.server` and `air.host`. To test against unpushed local copies of `sp-wire` or mint-core, `sandbox-mint/scripts/local-git.sh` points git at the local folders and their local tags.

## Run

```
cargo test
cargo run -- --regtest serve
```

Pick the network with `--mainnet` (the default), `--regtest`, `--signet` or `--testnet`. `serve` runs the AIR listener (the wallet's calls), the SPI's local HTTP API and the wallet scan on a timer. It needs `air.server` and `air.host` (`PSP_AIR_SERVER`, `PSP_AIR_HOST`). The full setup of a regtest node and wallets, and a purchase from payment to signatures, are in [REGTEST.md](REGTEST.md).

## Commands

| Command | What it does |
|---|---|
| `serve` | The AIR listener, the SPI's local HTTP API, the wallet scan and the delivery of each settled purchase's record, in one process. Needs `air.server` and `air.host`. |
| `run` (the default) | Scans the wallet once, settles what it can, delivers what it settled (when `air.server` and `air.host` are set; else it says how many are waiting) and exits. For testing. |
| `config` | Prints the settings in use and where each came from. |
| `pubkey` | Prints the mint's DID public key (66 hex characters), making the key file on first use. Wallets seal payloads to it. |
| `encrypt <mint pubkey hex> <payload hex>` | Seals a payload to a public key and prints it as hex, for testing. |
| `--help` | Prints the commands and exits. |
| `migrate` | Moves data that sits directly in `~/.psp-mint` (from before the per-network folders) into `~/.psp-mint/regtest`. The mint refuses to run while those files are there. |

## Networks

psp-mint runs on mainnet unless told otherwise. Pick the network with a flag, before or after the command: `--regtest`, `--signet` or `--testnet` (`--mainnet` is the default and may be given). The setting `bitcoin.network` (`PSP_NETWORK`: `main`, `regtest`, `signet`, `testnet`) does the same, and a flag wins over the file and the environment. `--testnet` is testnet3 (Bitcoin Core's `chain` `test`).

Each network has its own defaults, used unless `bitcoin.rpc_url`, `bitcoin.wallet` or `bitcoin.cookie` is set:

| Network | RPC port | Wallet | Cookie (macOS) | Cookie (Linux) |
|---|---|---|---|---|
| main | 8332 | `psp-main` | `~/Library/Application Support/Bitcoin/.cookie` | `~/.bitcoin/.cookie` |
| regtest | 18443 | `psp-regtest` | `~/Library/Application Support/Bitcoin/regtest/.cookie` | `~/.bitcoin/regtest/.cookie` |
| signet | 38332 | `psp-signet` | `~/Library/Application Support/Bitcoin/signet/.cookie` | `~/.bitcoin/signet/.cookie` |
| testnet | 18332 | `psp-testnet` | `~/Library/Application Support/Bitcoin/testnet3/.cookie` | `~/.bitcoin/testnet3/.cookie` |

The cookie default is chosen by the operating system the mint runs on (macOS, else the Linux path), matching where Bitcoin Core writes it. Testnet stays testnet3.

**Each network has its own data folder**, `~/.psp-mint/<network>` (`PSP_DATA_DIR` is the parent): keys, store, records and checkpoint never mix between networks. Every command prints `psp-mint on <network>: data in <folder>`. The config file `psp-mint.toml` stays in `PSP_DATA_DIR` itself.

**The node is checked.** At startup and before every scan the mint asks the node (`getblockchaininfo`) which chain it is on, and stops if that is not the configured network. `/health` and `/audit` carry `"network"`, and the ESP mint and the wallet check it before they talk to the PSP.

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
| `server.bind` | `PSP_BIND` | `127.0.0.1:3000` | The local HTTP API's address (`serve` only): the SPI's `/melt`, `/burns`, `/audit` and `/health`. Must be a loopback `IP:port` (`127.0.0.1`, `::1`); any other address stops the mint when the settings load. Wallets do not use it, they reach the mint over AIR. |
| `server.melt_max_request_bytes` | `PSP_MELT_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a `POST /melt` body. Over it: `413` before parsing, nothing burned. 4096 to 1 GiB. |
| `server.burns_max_request_bytes` | `PSP_BURNS_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a `POST /burns` body. 4096 to 1 GiB. |
| `server.rollover_max_request_bytes` | `PSP_ROLLOVER_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a rollover or spend request body (over AIR). Over it: a signed refusal with the `413` JSON, nothing parsed or burned. 4096 to 1 GiB. |
| `rollover.max_items` | `PSP_ROLLOVER_MAX_ITEMS` | `1000` | The most notes in one rollover, and the most notes in and change outputs out of one spend. More is refused before anything is burned. 1 to 100,000. |
| `server.check_state_max_request_bytes` | `PSP_CHECK_STATE_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a check-state request body (over AIR). 4096 to 1 GiB. |
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
| `settle.price_tolerance` | `PSP_PRICE_TOLERANCE` | `0.01` (1%) | A fraction, 0 to 1, with at most four decimals (`0.01`, `0.005`, `0.025`). How far a payment may miss the requested amount, over or under (up to and including this), and still settle in full as if it were exact: every requested note is signed, and nothing is credited, dropped or kept. It must not be more than `settle.payment_reconciliation_target`: the mint refuses to start otherwise. See [DESIGN.md](DESIGN.md), "The price tolerance". |
| `settle.reconciliation_window_days` | `PSP_RECONCILIATION_WINDOW_DAYS` | `14` | Whole days, 1 to 365. How long a credit, a held payment or a rejected purchase stays open for the payer. The end is written into the record the payer gets (`open_until`). If the payment is still unsettled then, the mint marks it forfeited, keeps the bitcoin, and `/audit` shows it apart. See [DESIGN.md](DESIGN.md), "The reconciliation window". |
| `settle.payment_reconciliation_target` | `PSP_PAYMENT_RECONCILIATION_TARGET` | `0.10` (10%) | A fraction, 0 to 1, with at most four decimals (`0.10`, `0.05`, `0.0525`). For a payment outside `settle.price_tolerance`, which is genuinely wrong: how far it may miss the requested amount before the mint stops absorbing the difference and starts reconciling it. See [DESIGN.md](DESIGN.md), "The reconciliation target" for how to choose it. |
| `mint.max_denomination_units` | `PSP_MAX_DENOMINATION_UNITS` | `16777216` | Largest denomination (2^24, 25 keys). Only used when `mint_keys.json` is first made; an existing key file keeps its denominations. 1 to 2^62. |
| `rotation.activation_hours` | `PSP_ROTATION_ACTIVATION_HOURS` | `8760` (1 year) | Whole hours from the first issue to the first key rotation. **0 turns rotation off** (one key set, one spent list). 0 to 10,000,000. Restart only. See [DESIGN.md](DESIGN.md), "Key rotation". |
| `rotation.period_hours` | `PSP_ROTATION_PERIOD_HOURS` | `87600` (10 years) | Whole hours between rotations. **0 also turns rotation off.** With rotation on it must be **at least 24**: BTC claims need time to settle before a rotation, and the mint refuses to start with a shorter period. 0 to 10,000,000. Restart only. |
| `storage.mint_state` | `PSP_STORAGE_MINT_STATE` | none (local only) | Service DIDs (66 hex, comma-separated) that will keep the mint's state; index 0 is the primary. One is valid; at most 8. **Setting only: nothing reads or writes the AIR yet.** See [DESIGN.md](DESIGN.md), "Storage policy". |
| `air.server` | `PSP_AIR_SERVER` | empty | The AIR server's name: its public key, the compressed hex AIR prints (66 characters). **Required by `serve`.** The client checks the server's signed challenge against it, so only the server with this key is accepted. |
| `air.host` | `PSP_AIR_HOST` | empty | The AIR server's host name or IP, no scheme, no path, no port (the AIR client always dials port 5702). `127.0.0.1` for the server bundled with the SPI on this machine. **Required by `serve`.** |
| `air.held_reads` | `PSP_AIR_HELD_READS` | `16` | How many mailbox slots ahead of the counter the listener holds a read on. 1 to 200 (the AIR server refuses a session's 257th held read and closes it). |
| `air.timeout_secs` | `PSP_AIR_TIMEOUT_SECS` | `10` | How long the AIR server may take to answer one `Create` or `Read` before that step fails, and how long a connection to it may take when the mint writes a delivery record. 1 to 600. |

**`air.enabled` (`PSP_AIR_ENABLED`) is gone.** The AIR listener always runs, because it is the only way a wallet reaches this mint. A config file or an environment that still sets it stops the mint with a message that says so; delete the setting.

**psp-mint has no fee setting.** None of the settings above is a fee, so there is no fee default to set: a melt, a rollover and a swap are exact (the amounts in equal the amounts out).

The ranges are checked when the mint starts. They are wide on purpose: they refuse values that cannot work (0 confirmations, a ceiling too small for one note, a minimum below the lock) and values far outside any sensible use (a 1 GiB body, a 1000-confirmation wait, a day between scans), not values that are merely unusual. `settle.min_confirmations` must also be at least `settle.lock_confirmations`, so a price is always locked before settlement.

The numbers in the other documents (1 and 6 confirmations, the 0.01 price tolerance, the 0.10 reconciliation target, 2%, 120 s, 420 s for `/health`) are these defaults.

The price feeds are not settings. The three sources (Coinbase, Bitstamp and Gemini, see [DESIGN.md](DESIGN.md), "Price") and their URLs are fixed in `src/price.rs`, and only those three are allowed. Changing a feed means changing the code.

## The AIR listener

Wallets reach this mint over AIR and only over AIR. The calls are **mint info** (what `/health` reports: `status`, `wire_version`, `network`), the three reads of the mint's keys, **keys**, **keysets** and **rotation**, and the three calls with a body, **rollover**, **check-state** and **spend**, each answered with a reply signed by the mint. `serve` always starts the listener (a thread beside the HTTP server), and refuses to start without `air.server` and `air.host`. What is left on HTTP is local to the SPI: `/melt`, `/burns`, `/audit` and `/health` (see "Local HTTP API"), on a loopback address.

- **How it works.** The listener holds reads on the next `air.held_reads` (16) mailbox slots of the mint's DID on the configured AIR server. When a wallet writes a sealed request into one, the listener opens it with the mint's DID key (`mint_did_key.json`, the one that already opens PSP payloads), answers with the JSON body of the request's kind as it is now (for mint info the status and the wire version and network `/health` reports; for keys, keysets and rotation the key list, the keysets and the rotation window), signs the reply with the same key, seals it to the wallet's one-off key and writes it into the reply cell only that wallet and the mint can find. The reply cell is write-once, so the first write wins: a request that reached the mint twice, or two mint processes, give one reply. Any other kind of request is logged and skipped.
- **Rollover and check-state.** The body is the request JSON the route takes (sealed to the mint's DID with the rest of the request, so the notes in it are readable by the mint only), and the reply body is the JSON the route returns. The ceilings are `server.rollover_max_request_bytes`, `server.check_state_max_request_bytes` and `rollover.max_items`. A refusal is a signed reply with status `refused` and a JSON body with `error` and `code` (a keyset code, `note_spent_elsewhere` with `spent`, and so on); over a ceiling the body is the `request_too_large` JSON (`size_bytes`, `limit_bytes`), a body that is not a request is `malformed_request`, and a failure of the mint's own is the signed `unavailable`. **A resent request is not applied twice:** the rollover is stored by request hash before any note is spent, so a retry, a copy in another slot, a crash between answering and writing the counter and a wallet that starts over all get the stored answer. A rollover holds the mint lock while it runs; the listener reads no further slots meanwhile.
- **Spend (kind 14).** A payment in PSP: notes in, change out, an amount `pay` that the mint burns. The body is `sp_wire::psp::PspSpendRequest` (`input_keysets`, `inputs`, `keyset` for the change, `outputs` for the change, `pay` in PSP units, `memo` of at most 256 bytes); every input's `spend_signature` covers its digest, which names `pay`, the memo, every input's keyset and every output. The inputs must be worth the change plus `pay`, exactly. The reply is the change's blind signatures (`outcomes`, `keys`, the `keyset` and `epoch` that signed them) and `paid`. `pay` is **never reissued**: it is the SPI's revenue, burned, and `/audit` counts it in `paid`. The checks run cheapest first, and any failure burns nothing: shape and sizes (`no_inputs`, `too_many_items`, `memo_too_long`, `missing_keyset`, `nothing_to_pay` for a `pay` of 0, which would be a plain rollover; `not_exact` when the notes are not the change plus `pay`; `duplicate_input`, `duplicate_blind_nonce`), denominations (`bad_denomination`), the keysets (the current or the previous one, as a rollover: `keyset_retired`, `unknown_keyset`), each note's proof and its signature over the digest (`bad_note`, `bad_spend_signature`), and then the spent lists under the mint lock (`note_spent_elsewhere` with `spent`, the index of each note already spent; `note_held` for a note an unfinished request holds). A spend with no change names no change keyset. A resent spend gets the stored answer and burns nothing more, and a spend cut short by a crash is finished by the same request sent again (see DESIGN.md, "The spend"). The ceilings are the rollover's: `server.rollover_max_request_bytes` and `rollover.max_items`. A spend adds no fee and no price; fees stay 0. Nothing here attaches a spend to another request.
- **Junk is skipped.** A slot that does not open, is malformed, is of an unknown kind or names a key that is not one is logged and skipped. Junk changes no state; a valid request costs the mint one short lock.
- **A call the mint cannot answer now** (the mint lock is poisoned, or a rotation step failed) gets a signed reply with status `unavailable` and no body, so the wallet fails at once with the reason instead of waiting out its tries. Mint info is always answered (a poisoned lock is `mint_lock_poisoned`, as in `/health`).
- **What the signed reply gives.** The keys come in a reply signed by the mint's DID key, so nothing between the wallet and the mint, not the AIR server and not a man in the middle, can substitute keys.
- **The counter** (`next`, the lowest slot not finished) is the text file `air_mailbox_next` in the network's data folder (`~/.psp-mint/<network>`), with the rest of the mint's local state, and **not** under `storage.mint_state`: it is only an optimisation. Without it the listener starts at slot 0 and an answered slot finds its reply cell taken and is passed over. It is written by temp file, fsync and rename after each slot is finished, so a crash between answering and writing it repeats a slot, which is harmless. A damaged counter file stops the listener with a message; it is never replaced.
- **If the AIR client gives up** (more than five held reads end within ten seconds, which is what a session the server refused or closed looks like, or a reply could not be written after three tries) the listener logs `AIR listener failed: ...` and the process exits with status 1, because a mint that no wallet can reach is not running. A supervisor restarts it, and the listener resumes from the counter. A restart of the AIR server needs nothing: the client reconnects and its held reads resume.
- **Held reads left behind by a stopped listener are never cleaned up by the AIR server** (upstream item 17); with 16, a handful of restarts costs little.
- **The AIR server** is the one bundled with the SPI, reached over loopback; its name is `air.server` and its host `air.host`. Resolving the server from the mint's DID is upstream item 4; until then both are settings.

## Delivering a purchase

When the mint has handled a payment, it writes **one record** to the AIR server (`air.server`, `air.host`): the purchase's signatures and the current keysets, or that the payment is **held** (paid, but short beyond the reconciliation target), or the reason the purchase was **rejected**. A held or rejected record also says what was received (satoshis, and US cents at the mint's price). It is signed by the mint's DID key and sealed to the reply DID in the payload (a one-off key of the payer's for this purchase), at an address only the payer and the mint can compute (`sp_wire::air_request::delivery`). The payer's wallet reads it and claims the notes (`ecash-wallet claim-psp`); nothing is pasted. The `signature` lines `run` and `serve` print are a log only.

The record is stored in the purchase's settlement record first and written after every scan until the AIR server has it, with a wait that doubles from 10 seconds to 15 minutes between tries of one record, across restarts. A delivery never blocks or undoes issuance. A payment whose payload cannot be read gets no record (the mint has no key to seal one to): the payer takes the loss. A rejection delivers the reason; it does not refund the bitcoin (see ROADMAP.md, "Not decided yet").

**The reconciliation window.** A credit, a held payment and a rejected purchase with a readable reply key stay open for `settle.reconciliation_window_days` (14 by default). The record carries the end of the window (`open_until`); the wallet keeps the purchase and its key until then. What settles an open payment (a corrected request, or the credit) is not built yet. When the window closes with the payment unsettled, the mint marks it **forfeited**, keeps the bitcoin, and no longer lists it as owed. `/audit` shows open and forfeited payments apart from the issued, redeemed and outstanding totals. See [DESIGN.md](DESIGN.md), "Signature delivery" and "The reconciliation window".

## Local HTTP API

The HTTP API is local to the SPI and the operator; wallets do not use it (they use AIR, above). It is served on `server.bind`, which must be a loopback address. The handlers are mint-server's, mounted over the PSP's own mint in the same process, except `/melt`, which is the PSP's own handler: it does what mint-server's does and also keeps the memo of each burn (see [DESIGN.md](DESIGN.md), "Burn memos"), and `/burns`, which is the PSP's own route. The scanner and the API share the mint behind one mutex. The scanner locks it once per payment and never while it waits on the exchanges for a price.

| Route | What it does |
|---|---|
| `POST /melt` | Burns notes. Body: `{"inputs": [...], "memo": "..."}`. Every input's spend signature covers the memo, so the burn is bound to it. Returns `{"burned": ...}`. The ESP mint uses this to burn PSP in an exchange, with the memo it computes from the request. The memo is kept for every note the burn spends. |
| `POST /burns` | Body: `{"nonces": [...]}`. Returns `{"burns": [{"spent": true, "memo": "..."}, ...]}` in the same order. `memo` is the memo the note was burned under, and is `null` if the note is not spent or was spent with no memo on record. The ESP mint uses this on a retry. See [DESIGN.md](DESIGN.md), "Burn memos". |
| `GET /audit` | `issued`, `redeemed`, `outstanding`, `paid` (the PSP units burned as payments by spends, kept apart from every other burn such as an exchange's `/melt`; it is part of `redeemed`), and (under `serve`) `reconciliation`: `open` and `forfeited`, each with `payments`, `sats` (the bitcoin held) and `credit_cents` (credits owed, for an open one). A forfeited payment is not part of the issued, redeemed and outstanding totals. |
| `GET /health` | `200` with `status` `ok` (or `starting`, before the first scan finishes), `503` with `scan_stale` or `mint_lock_poisoned`. The body also has `wire_version` (an integer: the version of the wire format this mint speaks; a wallet refuses a mint whose number is not its own), `last_scan_unix`, `seconds_since_last_scan` (both null until a scan succeeds) and `stale_after_secs`. A scan counts when the wallet was read and every payment was processed as far as it could go, so a payment waiting on confirmations or a price is not a failure. `scan_stale` means no successful scan for `scan.health_intervals` intervals plus `scan.health_grace_secs` (by default three intervals plus 60 s). |

Gone from HTTP (they are AIR calls now): `GET /keys`, `GET /keysets`, `GET /rotation`, `POST /rollover` and `POST /check-state`. A request for one is a `404`. They are answered by the AIR listener with the same bodies; see "The AIR listener".

`/melt` and `/burns` have a ceiling, 1 MiB by default (`server.*_max_request_bytes` above); so do the two AIR calls with a body, with the same JSON for a refusal. A body over it is refused (`413` over HTTP) before it is parsed, so nothing is burned. A body that gives its `Content-Length` is refused without being read; one sent without it is read only up to the ceiling. The reply is JSON, with `error` like every other error here, plus `code` (`request_too_large`), `size_bytes` (the declared size, or `null`) and `limit_bytes`, as the ESP mint's `413` has:

```
{"error":"the request is 1048577 bytes, and this route accepts at most 1048576 bytes. Nothing was parsed or burned","code":"request_too_large","size_bytes":1048577,"limit_bytes":1048576}
```

Not mounted, on purpose:

- **`/admin/issue`.** The PSP issues only from confirmed BTC payments. There is no unilateral issuance.
- **`/swap`.** PSP to FSP is out of scope. The plain PSP swap that settles a wallet-to-wallet hand-over is the rollover, over AIR.

`serve` also exits (status 1, with a message on stderr) if the scan thread panics or a thread panics while holding the mint, since either leaves a process that answers but is broken. A supervisor should restart it.

There is no authentication and no TLS, and `/health` is open like the rest. A note authorizes its own spend, so a caller can only burn notes they hold. The bind must be a loopback address (the settings refuse any other), so the API can be reached only from this machine; do not put a proxy in front of it.

## Wire version

The request and response types come from [`sp-wire`](../sp-wire/README.md), and `GET /health` returns `wire_version`. A wallet refuses a mint whose number is not its own. Details in [DESIGN.md](DESIGN.md), "Wire format and versions".

## Tests

```
cargo test
cargo fmt --check
```

`tests/delivery.rs` settles payments with the real settlement code and delivers them to a real `air_server` in the test process, reading them back the way the wallet does (a settled purchase, a rejection that carries what was paid and a window, a held payment that does too and is forfeited when its window closes, an unreadable payment that has no record, the AIR server down and a restart that delivers exactly one record with the same bytes, a write found already stored, a different record never overwritten, a reply DID that is not a key, and a settlements file from before delivery). `tests/reconciliation.rs` runs the router against a real server on a local port and checks that `/audit` shows open and forfeited payments apart, with and without rotation, and has no `reconciliation` section when the router is given no settlement records. `tests/air_info.rs` runs the AIR listener against a real `air_server` started in the test process (a happy path, junk in the slots, a restart that resumes from the counter, a mint that dies before replying and the wallet's retry, tampered and forged replies, a rebuilt counter, and keys, keysets, rotation, rollover and check-state against a real rotating mint, including a 1000-note rollover: a mint whose lock is poisoned answers `unavailable`, and tampered or forged replies are rejected). The AIR client always dials port 5702 (upstream item 12), so that file needs the port free; it needs the two local patches described under "Building".

The integration tests in `tests/` (`api.rs`, `limits_http.rs`, `network_http.rs`, `rotation_http.rs`, `spend.rs`) run the SPI's HTTP routes against a real mint in the process, and the wallet's calls (keys, keysets, rotation, rollover, check-state) over AIR, through the mint's real listener on a real `air_server` (`tests/common`; they take turns on the one server, so they need port 5702 free too). The file names keep `_http` from when all of it was HTTP. The end-to-end run with a Bitcoin Core regtest node is [REGTEST.md](REGTEST.md).

## Documentation

- [REGTEST.md](REGTEST.md): setting up a regtest node and wallets, and running a purchase through the mint (steps 1 to 8).
- [DESIGN.md](DESIGN.md): how psp-mint works and why: the scanner, key rotation, settlement and the price tolerance, the price, the payload and its encryption, burn memos.
- [CHANGELOG.md](CHANGELOG.md): what was built and what changed.
- [ROADMAP.md](../sandbox-mint/ROADMAP.md) (in `sandbox-mint`): left to do and not decided yet, for every repo.
- [OPERATIONS.md](../sandbox-mint/OPERATIONS.md) (in `sandbox-mint`): running the mints as services: `launchd` and `systemd` definitions, install and uninstall, the health monitor.
- [esp-mint](../esp-mint/README.md) and [ecash-wallet](../ecash-wallet/README.md): the other parts.
- [sandbox-mint README](../sandbox-mint/README.md): the family of repos.
