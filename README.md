# psp-mint

psp-mint is the Primary Service Point (PSP) mint. A payer buys PSP with a Bitcoin payment that carries a sealed request in a data output; the mint watches its own Bitcoin Core wallet, locks the BTC/USD price at 1 confirmation and signs the requested blinded notes at 6. 10 PSP = $1. The ESP mint burns PSP here when a holder exchanges it for ESP. The mint rotates its keys and keeps no more than two key sets.

## Building

psp-mint builds from tagged dependencies: mint-core at `v0.1.3` and `sp-wire` at `v0.2.0` (git tags). `sp-wire` holds the wire types this mint shares with the other mints and the wallet (see [DESIGN.md](DESIGN.md), "Wire format and versions"), and it pulls in AIR 2.0.3 (commit `8e281d5`, AIR has no tags) for the payload encryption. AIR 2.0.3 has no path dependencies, so nothing needs patching to build from git. To test against unpushed local copies of `sp-wire` or mint-core, `sandbox-mint/scripts/local-git.sh` points git at the local folders and their local tags.

## Run

```
cargo test
cargo run -- --regtest serve
```

Pick the network with `--mainnet` (the default), `--regtest`, `--signet` or `--testnet`. `serve` runs the HTTP API and rescans the wallet on a timer. The full setup of a regtest node and wallets, and a purchase from payment to signatures, are in [REGTEST.md](REGTEST.md).

## Commands

| Command | What it does |
|---|---|
| `serve` | The HTTP API and the wallet scan, in one process. |
| `run` (the default) | Scans the wallet once, settles what it can, and exits. For testing. |
| `config` | Prints the settings in use and where each came from. |
| `pubkey` | Prints the mint's DID public key (66 hex characters), making the key file on first use. Wallets seal payloads to it. |
| `encrypt <mint pubkey hex> <payload hex>` | Seals a payload to a public key and prints it as hex, for testing. |
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
| `server.bind` | `PSP_BIND` | `127.0.0.1:3000` | The HTTP API's address (`serve` only). Must be an `IP:port`. |
| `server.melt_max_request_bytes` | `PSP_MELT_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a `POST /melt` body. Over it: `413` before parsing, nothing burned. 4096 to 1 GiB. |
| `server.burns_max_request_bytes` | `PSP_BURNS_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a `POST /burns` body. 4096 to 1 GiB. |
| `server.rollover_max_request_bytes` | `PSP_ROLLOVER_MAX_BYTES` | `1048576` (1 MiB) | Ceiling on a `POST /rollover` body. Over it: `413` before parsing, nothing burned. 4096 to 1 GiB. |
| `rollover.max_items` | `PSP_ROLLOVER_MAX_ITEMS` | `1000` | The most notes in one `POST /rollover`. More is refused `400` before anything is burned. 1 to 100,000. |
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
| `settle.price_tolerance` | `PSP_PRICE_TOLERANCE` | `0.01` (1%) | A fraction, 0 to 1, with at most four decimals (`0.01`, `0.005`, `0.025`). How far a payment may miss the requested amount, over or under (up to and including this), and still settle in full as if it were exact: every requested note is signed, and nothing is credited, dropped or kept. It must not be more than `settle.payment_reconciliation_target`: the mint refuses to start otherwise. See [DESIGN.md](DESIGN.md), "The price tolerance". |
| `settle.payment_reconciliation_target` | `PSP_PAYMENT_RECONCILIATION_TARGET` | `0.10` (10%) | A fraction, 0 to 1, with at most four decimals (`0.10`, `0.05`, `0.0525`). For a payment outside `settle.price_tolerance`, which is genuinely wrong: how far it may miss the requested amount before the mint stops absorbing the difference and starts reconciling it. See [DESIGN.md](DESIGN.md), "The reconciliation target" for how to choose it. |
| `mint.max_denomination_units` | `PSP_MAX_DENOMINATION_UNITS` | `16777216` | Largest denomination (2^24, 25 keys). Only used when `mint_keys.json` is first made; an existing key file keeps its denominations. 1 to 2^62. |
| `rotation.activation_hours` | `PSP_ROTATION_ACTIVATION_HOURS` | `8760` (1 year) | Whole hours from the first issue to the first key rotation. **0 turns rotation off** (one key set, one spent list). 0 to 10,000,000. Restart only. See [DESIGN.md](DESIGN.md), "Key rotation". |
| `rotation.period_hours` | `PSP_ROTATION_PERIOD_HOURS` | `87600` (10 years) | Whole hours between rotations. **0 also turns rotation off.** With rotation on it must be **at least 24**: BTC claims need time to settle before a rotation, and the mint refuses to start with a shorter period. 0 to 10,000,000. Restart only. |

The ranges are checked when the mint starts. They are wide on purpose: they refuse values that cannot work (0 confirmations, a ceiling too small for one note, a minimum below the lock) and values far outside any sensible use (a 1 GiB body, a 1000-confirmation wait, a day between scans), not values that are merely unusual. `settle.min_confirmations` must also be at least `settle.lock_confirmations`, so a price is always locked before settlement.

The numbers in the other documents (1 and 6 confirmations, the 0.01 price tolerance, the 0.10 reconciliation target, 2%, 120 s, 420 s for `/health`) are these defaults.

The price feeds are not settings. The three sources (Coinbase, Bitstamp and Gemini, see [DESIGN.md](DESIGN.md), "Price") and their URLs are fixed in `src/price.rs`, and only those three are allowed. Changing a feed means changing the code.

## HTTP API

The API is mint-server's handlers, the same ones `mintd` uses, mounted over the PSP's own mint in the same process. The exception is `/melt`, which is the PSP's own handler: it does what mint-server's does and also keeps the memo of each burn (see [DESIGN.md](DESIGN.md), "Burn memos"), and `/burns` is the PSP's own route. The scanner and the API share the mint behind one mutex. The scanner locks it once per payment and never while it waits on the exchanges for a price.

| Route | What it does |
|---|---|
| `GET /keys` | The public key for each denomination. |
| `POST /melt` | Burns notes. Body: `{"inputs": [...], "memo": "..."}`. Every input's spend signature covers the memo, so the burn is bound to it. Returns `{"burned": ...}`. The ESP mint uses this to burn PSP in an exchange, with the memo it computes from the request. The memo is kept for every note the burn spends. |
| `POST /burns` | Body: `{"nonces": [...]}`. Returns `{"burns": [{"spent": true, "memo": "..."}, ...]}` in the same order. `memo` is the memo the note was burned under, and is `null` if the note is not spent or was spent with no memo on record. The ESP mint uses this on a retry. See [DESIGN.md](DESIGN.md), "Burn memos". |
| `POST /check-state` | Body: `{"nonces": [...]}`. Returns which are spent. Looks in both live spent lists. |
| `GET /keysets` | Each live keyset's public keys by ID (current and previous). `/keys` is the current set only. |
| `GET /rotation` | Whether rotation is on, `first_issue`, `activation`, the period, the current and previous keysets (ID, epoch, start, `current_until`, `retires`), `next_rotation` and the mint's clock. |
| `POST /rollover` | Swaps notes for new notes of exactly the same amounts on the key set the request names, current or previous. Body: `input_keysets`, inputs, `keyset`, outputs. `400` if refused, with `code` (`keyset_retired`, `unknown_keyset`, `missing_keyset`, `note_held`, `burn_not_proven`, or `note_spent_elsewhere` with `spent`: the positions of the inputs that are spent), and nothing is burned. Also the swap for wallet-to-wallet hand-overs. See [DESIGN.md](DESIGN.md), "Key rotation". |
| `GET /audit` | `issued`, `redeemed`, `outstanding`. |
| `GET /health` | `200` with `status` `ok` (or `starting`, before the first scan finishes), `503` with `scan_stale` or `mint_lock_poisoned`. The body also has `wire_version` (an integer: the version of the wire format this mint speaks; a wallet refuses a mint whose number is not its own), `last_scan_unix`, `seconds_since_last_scan` (both null until a scan succeeds) and `stale_after_secs`. A scan counts when the wallet was read and every payment was processed as far as it could go, so a payment waiting on confirmations or a price is not a failure. `scan_stale` means no successful scan for `scan.health_intervals` intervals plus `scan.health_grace_secs` (by default three intervals plus 60 s). |

Every route that takes a body has a ceiling, 1 MiB by default (`server.*_max_request_bytes` above). A body over it is refused with `413` before it is parsed, so nothing is burned. A body that gives its `Content-Length` is refused without being read; one sent without it is read only up to the ceiling. The reply is JSON, with `error` like every other error here, plus `code` (`request_too_large`), `size_bytes` (the declared size, or `null`) and `limit_bytes`, as the ESP mint's `413` has:

```
{"error":"the request is 1048577 bytes, and this route accepts at most 1048576 bytes. Nothing was parsed or burned","code":"request_too_large","size_bytes":1048577,"limit_bytes":1048576}
```

Not mounted, on purpose:

- **`/admin/issue`.** The PSP issues only from confirmed BTC payments. There is no unilateral issuance.
- **`/swap`.** PSP to FSP is out of scope. The plain PSP swap that settles a wallet-to-wallet hand-over is `POST /rollover` (see the table above).

`serve` also exits (status 1, with a message on stderr) if the scan thread panics or a thread panics while holding the mint, since either leaves a process that answers but is broken. A supervisor should restart it.

There is no authentication and no TLS, and `/health` is open like the rest. A note authorizes its own spend, so a caller can only burn notes they hold. The default bind is localhost. Do not expose it to a network without TLS in front.

## Wire version

The request and response types come from [`sp-wire`](../sp-wire/README.md), and `GET /health` returns `wire_version`. A wallet refuses a mint whose number is not its own. Details in [DESIGN.md](DESIGN.md), "Wire format and versions".

## Tests

```
cargo test
cargo fmt --check
```

The integration tests in `tests/` run the HTTP routes (`api.rs`, `limits_http.rs`, `network_http.rs`, `rotation_http.rs`) against a real mint in the process. The end-to-end run with a Bitcoin Core regtest node is [REGTEST.md](REGTEST.md).

## Documentation

- [REGTEST.md](REGTEST.md): setting up a regtest node and wallets, and running a purchase through the mint (steps 1 to 8).
- [DESIGN.md](DESIGN.md): how psp-mint works and why: the scanner, key rotation, settlement and the price tolerance, the price, the payload and its encryption, burn memos.
- [CHANGELOG.md](CHANGELOG.md): what was built and what changed.
- [ROADMAP.md](../sandbox-mint/ROADMAP.md) (in `sandbox-mint`): left to do and not decided yet, for every repo.
- [esp-mint](../esp-mint/README.md) and [ecash-wallet](../ecash-wallet/README.md): the other parts.
- [sandbox-mint README](../sandbox-mint/README.md): the family of repos.
