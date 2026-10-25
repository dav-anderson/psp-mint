# psp-mint

psp-mint is the Primary Service Point (PSP) mint. A payer buys PSP with a Bitcoin payment that carries a sealed request in a data output; the mint watches its own Bitcoin Core wallet, locks the BTC/USD price at 1 confirmation and signs the requested blinded notes at 6. 10 PSP = $1. The ESP mint burns PSP here when a holder exchanges it for ESP. The mint rotates its keys and keeps no more than two key sets.

## Building

psp-mint builds from GitHub as it is: `Cargo.toml` names mint-core at `v0.1.3` and `sp-wire` at `v0.5.1` (git tags), wire version 3, and the committed `Cargo.lock` matches. `sp-wire` holds the wire types this mint shares with the other mints and the wallet (see [DESIGN.md](DESIGN.md), "Wire format and versions"), and it pulls in AIR for the payload encryption and, through its `air-client` feature (the transport of the mint-info request, see "The AIR listener"), `air_server`. sp-wire pins AIR at `eae5133` (AIR 3.0.0 plus the `SecretKey` export, on AIR's branch `air3-export-secretkey`) and air_server at `v1.0.1`, which takes `air` from crates.io; the `[patch.crates-io]` block in `Cargo.toml` points that at the same AIR commit, or two `air` crates are built and their types do not match (a patch applies only in the crate being built, so sp-wire's own block does not reach this one). The listener is part of `serve` and always runs: `serve` will not start without `air.server` and `air.host`. To test against unpushed local copies of `sp-wire` or mint-core, `sandbox-mint/scripts/local-git.sh` points git at the local folders and their local tags.

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
| `encrypt <mint pubkey hex> <payload hex>` | Seals a payload to a public key and prints it as hex, for testing. Takes exactly those two arguments; with any other number it says what it takes and exits 1. |
| `--help` | Prints the commands and exits. |
| `--version` | Prints the version of psp-mint, then the versions of sp-wire, mint-core and air that `Cargo.lock` locked for this build, one `name version` per line, and exits. It reads no settings and touches nothing, so it is safe while the mint serves. Build with `--locked` for the lines to be the ones used. |

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

**Each network has its own data folder**, `~/.psp-mint/<network>` (`PSP_DATA_DIR` is the parent): keys, store, records and checkpoint never mix between networks. `serve`, `run` and `pubkey` print `psp-mint on <network>: data in <folder>` on stderr first, so `$(psp-mint pubkey)` is only the key. The config file `psp-mint.toml` stays in `PSP_DATA_DIR` itself.

**The node is checked.** At startup and before every scan the mint asks the node (`getblockchaininfo`) which chain it is on, and stops if that is not the configured network. `/health` and `/audit` carry `"network"`, and the ESP mint and the wallet check it before they talk to the PSP.

## Settings

Every setting is in `psp-mint.toml` in the data directory (or the file `PSP_CONFIG` names, which must then exist), and each has an environment variable that overrides the file. Strongest first: the environment, the file, the default. The file is TOML in sections:

```
[settle]
min_confirmations = 6
price_tolerance = 0.01
payment_reconciliation_target = 0.10

[reconciliation]
fee = 0
max_attempts = 2
write_fee = 0
round_up_from = 9

[delivery]
read_fee = 0

[bitcoin]
wallet = "psp"
```

The settings are read when the mint starts, and logged then with the value in use and where it came from (`default`, `file` or `env PSP_...`). `cargo run -- config` prints the same list and exits. An unknown key, a value of the wrong type or a value out of range stops the mint with a list of every problem. `SIGHUP` reads the file again while the mint serves (see "Reloading, restarting and snapshots"). `PSP_DATA_DIR` (default `~/.psp-mint`) is not a setting, because the file lives in it.

| Setting | Env var | Default | Units and range | A change applies | What it does |
|---|---|---|---|---|---|
| `server.bind` | `PSP_BIND` | `127.0.0.1:3000` | host:port; a loopback address | **needs a restart** | The local HTTP API's address (`serve` only): the SPI's `/melt`, `/burns`, `/audit` and `/health`. Must be a loopback `IP:port` (`127.0.0.1`, `::1`); any other address stops the mint when the settings load. Wallets do not use it, they reach the mint over AIR. |
| `server.melt_max_request_bytes` | `PSP_MELT_MAX_BYTES` | `1048576` (1 MiB) | bytes; 16,384 to 15,728,640 (15 MiB) | reloads live | Ceiling on a `POST /melt` body. Over it: `413` before parsing, nothing burned. |
| `server.rollover_max_request_bytes` | `PSP_ROLLOVER_MAX_BYTES` | `1048576` (1 MiB) | bytes; 16,384 to 15,728,640 (15 MiB) | reloads live | Ceiling on a rollover or spend request body (over AIR). Over it: a signed refusal with the `413` JSON, nothing parsed or burned. |
| `rollover.max_items` | `PSP_ROLLOVER_MAX_ITEMS` | `1000` | notes; 64 to 25,000 | reloads live | The most notes in one rollover, and the most notes in and change outputs out of one spend. More is refused before anything is burned. At least the 64 outputs one amount can need; at most what fits one AIR message. |
| `server.burns_max_request_bytes` | `PSP_BURNS_MAX_BYTES` | `1048576` (1 MiB) | bytes; 16,384 to 15,728,640 (15 MiB) | reloads live | Ceiling on a `POST /burns` body. |
| `server.check_state_max_request_bytes` | `PSP_CHECK_STATE_MAX_BYTES` | `1048576` (1 MiB) | bytes; 16,384 to 15,728,640 (15 MiB) | reloads live | Ceiling on a check-state request body (over AIR). |
| `bitcoin.network` | `PSP_NETWORK` | `main` | network; `main`, `regtest`, `signet`, `testnet` | **needs a restart** | The Bitcoin network: `main`, `regtest`, `signet`, `testnet`. A `--regtest`/`--signet`/`--testnet`/`--mainnet` flag wins over it. Checked against the node. See "Networks". |
| `bitcoin.rpc_url` | `PSP_RPC_URL` | per network (`http://127.0.0.1:8332` on main) | URL; starts with `http://` or `https://` | reloads live | Bitcoin Core's RPC address, without the wallet path. Must start with `http://` or `https://`. |
| `bitcoin.wallet` | `PSP_WALLET` | per network (`psp-<network>`) | name; not empty, no `/` | **needs a restart** | The wallet that receives payments. Not empty, no `/`. |
| `bitcoin.cookie` | `PSP_COOKIE` | per network (`~/Library/Application Support/Bitcoin/.cookie` on main) | path; not empty | reloads live | Bitcoin Core's RPC cookie file. Not empty. |
| `bitcoin.rpc_timeout_secs` | `PSP_RPC_TIMEOUT_SECS` | `30` | seconds; 1 to 600 | reloads live | How long one RPC call may take. |
| `scan.interval_secs` | `PSP_SCAN_INTERVAL_SECS` | `120` | seconds; 1 to 86,400 | reloads live | Time between scans (`serve` only). |
| `scan.health_intervals` | `PSP_HEALTH_INTERVALS` | `3` | scan intervals; 1 to 100 | reloads live | `/health` fails after this many scan intervals plus `scan.health_grace_secs` without a successful scan. |
| `scan.health_grace_secs` | `PSP_HEALTH_GRACE_SECS` | `60` | seconds; 0 to 86,400 | reloads live | Extra seconds on top of those intervals, for a slow price fetch. 0 to 86400. With the defaults `/health` fails after 3 × 120 + 60 = 420 s. |
| `price.max_age_secs` | `PSP_PRICE_MAX_AGE_SECS` | `120` | seconds; 1 to 3,600 | reloads live | A quote further than this from now is excluded. |
| `price.max_spread_bps` | `PSP_PRICE_MAX_SPREAD_BPS` | `200` (2%) | basis points; 0 to 10,000 | reloads live | Two usable quotes further apart than this (of the lower one) mean wait. |
| `price.timeout_secs` | `PSP_PRICE_TIMEOUT_SECS` | `10` | seconds; 1 to 600 | reloads live | How long one ticker request may take. |
| `settle.lock_confirmations` | `PSP_LOCK_CONFIRMATIONS` | `1` | confirmations; 1 to 1,000 | reloads live | Confirmations at which the price is locked. |
| `settle.min_confirmations` | `PSP_MIN_CONFIRMATIONS` | `6` | confirmations; `settle.lock_confirmations` to 1,000 | reloads live | Confirmations at which the payment is settled. |
| `settle.price_tolerance` | `PSP_PRICE_TOLERANCE` | `0.01` (1%) | fraction; 0 to 1, at most four decimals | reloads live | A fraction, 0 to 1, with at most four decimals (`0.01`, `0.005`, `0.025`). How far a payment may miss the requested amount, over or under (up to and including this), and still settle in full as if it were exact: every requested note is signed, and nothing is credited, dropped or kept. It must not be more than `settle.payment_reconciliation_target`: the mint refuses to start otherwise. See [DESIGN.md](DESIGN.md), "The price tolerance". |
| `settle.payment_reconciliation_target` | `PSP_PAYMENT_RECONCILIATION_TARGET` | `0.10` (10%) | fraction; 0 to 1, at most four decimals | reloads live | A fraction, 0 to 1, with at most four decimals (`0.10`, `0.05`, `0.0525`). For a payment outside `settle.price_tolerance`, which is genuinely wrong: how far it may miss the requested amount before the mint stops absorbing the difference and starts reconciling it. See [DESIGN.md](DESIGN.md), "The reconciliation target" for how to choose it. |
| `settle.reconciliation_window_days` | `PSP_RECONCILIATION_WINDOW_DAYS` | `14` | days; 1 to 365 | reloads live | Whole days, 1 to 365. How long a credit, a held payment or a rejected purchase stays open for the payer. The end is written into the record the payer gets (`open_until`). If the payment is still unsettled then, the mint marks it forfeited, keeps the bitcoin, and `/audit` shows it apart. See [DESIGN.md](DESIGN.md), "The reconciliation window". |
| `reconciliation.fee` | `PSP_RECONCILIATION_FEE` | `0` | PSP units | reloads live | Whole PSP units, 0 or more. Has no cap (see "Caps on sizes and fees" in [DESIGN.md](DESIGN.md)): a fee that is more than a credit can carry takes the whole credit, which then comes to 0 units due. A reconciliation is then refused with `nothing_to_issue` and the credit is forfeited when its window closes; nothing overflows and nothing unpaid is signed. Taken from what a reconciliation (kind 15) may carry. It is PSP revenue, never issued as notes, and `/audit` shows it as `fees_units`. Fixed for a payment when its window opens (it is in the rules the payer is sent). Fees are 0 by default. See [DESIGN.md](DESIGN.md), "Reconciliation over AIR". |
| `reconciliation.max_attempts` | `PSP_RECONCILIATION_MAX_ATTEMPTS` | `2` | attempts; 1 to 1,000 | reloads live | Whole number, 1 to 1000. How many requests refused for their content a payment counts before it accepts no more. The second exists for the keyset-rotation race: a request that names a keyset which rotated out meanwhile is one attempt, and the wallet sends it again with the new one. Fixed for a payment when its window opens. |
| `reconciliation.write_fee` | `PSP_RECONCILIATION_WRITE_FEE` | `0` | PSP units | reloads live | Whole PSP units, 0 or more, with no cap. The cost of the one kind 15 write, which the SPI prepays on the wallet's behalf. Taken from what the request may carry, once per payment (the SPI absorbs a second attempt). `/audit` shows it as `write_prepaid_units`. Only the amount is recorded today: the prepaid AIR write itself is not built (it waits on AIR). |
| `reconciliation.round_up_from` | `PSP_RECONCILIATION_ROUND_UP_FROM` | `9` | cents; 1 to 10 | reloads live | Cents, 1 to 10. The value is the cents remainder from which a credit rounds up to the next whole PSP unit: 1 means round up from 0.1, 9 (the default) from 0.9, and 10 never rounds up. A credit of 259 cents is 25 units and 9 cents over: at 9 it rounds up to 26 units and the SPI gives away 1 cent; at 10 it rounds down to 25 and the SPI keeps the 9 cents. `/audit` shows `rounding_kept_cents` and `rounding_given_cents`. |
| `delivery.read_fee` | `PSP_DELIVERY_READ_FEE` | `0` | PSP units | reloads live | Whole PSP units, 0 or more, with no cap. It is taken twice (the first record's read and the follow-up's). Raising it can eat up a small PSP purchase before it is delivered: the first read's cost comes off the payment before it is assessed, so a purchase it takes down to nothing is short of its request, and `settle.rs` holds it as `paid_unsettled` (a `Held` delivery, 0 units due). Any reconciliation is then refused with `nothing_to_issue` and the payment is forfeited when its window closes; nothing overflows and nothing unpaid is signed. The read cost the PSP prepays for each record a payer must read: the first delivery record and the follow-up (the answer to an accepted reconciliation). The first one's cost comes off the bitcoin payment before it is assessed (the purchase buys that many units less); the follow-up's comes off what a reconciliation may carry. Meant to be very small. `/audit` shows it as `read_prepaid_units`. Only the amounts are recorded today: the prepaid AIR read is not built. |
| `burn.enabled` | `PSP_BURN_ENABLED` | `false` | true or false | reloads live | Whether the PSP paid in a spend (kind 14) may be burned. `false` (the default): none is, and the mint reissues every paid unit to the SPI's ecash-wallet (`revenue/`, taken in with `ecash-wallet receive-psp`). `true`: the mint reissues paid PSP only while the SPI's wallet holds less than `burn.threshold_psp` (see that row for the figure compared) and burns the rest. With no usable balance figure, nothing is burned. Only paid PSP is ever burned. |
| `burn.threshold_psp` | `PSP_BURN_THRESHOLD_PSP` | `0` | PSP units; 0 to 2^62 | reloads live | With `burn.enabled` true: the PSP the SPI's wallet is kept at. **The figure compared is the SPI wallet's current PSP balance**, not a total over the mint's life: `held` = the balance `spi` last published in `burn.balance_file` plus the PSP the mint has reissued since that figure (`reissued` in `/audit` now, less `reissued` in the file; those notes may not be in the wallet yet). A spend keeps `min(pay, threshold - held)` for the wallet (0 when `held` is at or over the threshold) and burns the rest, so reissuing starts again once the wallet spends. 0 burns every paid unit whenever a figure is usable. Decided once, when the request is first saved: a retry keeps the saved plan. Ignored while `burn.enabled` is false. |
| `burn.balance_file` | `PSP_BURN_BALANCE_FILE` | empty: `wallet_balance.json` in the network's data folder | path | reloads live | Where `spi` publishes the wallet's balance for `burn.threshold_psp`: a JSON file `{"psp": N, "reissued": N, "at": N}` (the wallet's PSP balance, this mint's `reissued` total read right after it, and the time in seconds). `spi` writes it on `spi collect` and `spi audit` (and by hand with `spi publish-balance`), while `[psp] burn` is true, by temp file and rename; there is no timer. **A spend made with `ecash-wallet` directly is not seen until the next `collect` or `audit`**: until then the mint counts the wallet as holding more than it does and keeps less of a payment for it, so it burns more than it should have. Read at each spend, only while `burn.enabled` is true. **A file that is missing, unreadable, not that shape, older than `burn.balance_max_age_secs` or dated more than a minute ahead gives no figure, and then every paid unit is reissued**: the mint never burns on a guess, and the first spend after a change logs why. |
| `burn.balance_max_age_secs` | `PSP_BURN_BALANCE_MAX_AGE_SECS` | `86400` (24 hours) | seconds, 1 to 604,800 | reloads live | How old the figure in `burn.balance_file` may be before the mint ignores it (and reissues every paid unit). 1 to 604,800 (a week). Older than this, the mint reissues every paid unit and burns nothing (safe). Ignored while `burn.enabled` is false. |
| `mint.max_denomination_units` | `PSP_MAX_DENOMINATION_UNITS` | `16777216` | PSP units; 1 to 2^62 | **needs a restart** | Largest denomination (2^24, 25 keys). Only used when `mint_keys.json` is first made; an existing key file keeps its denominations. |
| `rotation.activation_hours` | `PSP_ROTATION_ACTIVATION_HOURS` | `8760` (1 year) | hours; 0 to 10,000,000 (0 is off) | **needs a restart** | Whole hours from the first issue to the first key rotation. **0 turns rotation off** (one key set, one spent list). 0 to 10,000,000. See [DESIGN.md](DESIGN.md), "Key rotation". |
| `rotation.period_hours` | `PSP_ROTATION_PERIOD_HOURS` | `87600` (10 years) | hours; 0 (off), or 24 to 10,000,000 | **needs a restart** | Whole hours between rotations. **0 also turns rotation off.** With rotation on it must be **at least 24**: BTC claims need time to settle before a rotation, and the mint refuses to start with a shorter period. 0 to 10,000,000. |
| `storage.mint_state` | `PSP_STORAGE_MINT_STATE` | none (local only) | service DIDs (66 hex characters each), comma-separated; at most 8, no duplicates | **needs a restart** | Service DIDs (66 hex, comma-separated) that will keep the mint's state; index 0 is the primary. One is valid; at most 8. **Setting only: nothing reads or writes the AIR yet.** See [DESIGN.md](DESIGN.md), "Storage policy". |
| `air.server` | `PSP_AIR_SERVER` | empty | AIR server name (66 hex characters) | **needs a restart** | The AIR server's name: its public key, the compressed hex AIR prints (66 characters). **Required by `serve`.** The client checks the server's signed challenge against it, so only the server with this key is accepted. |
| `air.host` | `PSP_AIR_HOST` | empty | host or IP; no scheme, path or port | **needs a restart** | The AIR server's host name or IP, no scheme, no path, no port (the AIR client always dials port 5702). `127.0.0.1` for the server bundled with the SPI on this machine. **Required by `serve`.** |
| `air.held_reads` | `PSP_AIR_HELD_READS` | `16` | reads; 1 to 200 | **needs a restart** | How many mailbox slots ahead of the counter the listener holds a read on. 1 to 200 (the AIR server refuses a session's 257th held read and closes it). |
| `air.timeout_secs` | `PSP_AIR_TIMEOUT_SECS` | `10` | seconds; 1 to 600 | **needs a restart** | How long the AIR server may take to answer one `Create` or `Read` before that step fails, and how long a connection to it may take when the mint writes a delivery record. |

**`air.enabled` (`PSP_AIR_ENABLED`) is gone.** The AIR listener always runs, because it is the only way a wallet reaches this mint. A config file or an environment that still sets it stops the mint with a message that says so; delete the setting.

**Fees are 0 by default.** `reconciliation.fee`, `reconciliation.write_fee` and `delivery.read_fee` are fees in PSP units, and nothing is taken unless they are set. A melt, a rollover, a swap and a spend are exact (the amounts in equal the amounts out).

The ranges are checked when the mint starts. They are wide on purpose: they refuse values that cannot work (0 confirmations, a ceiling too small for one note, a minimum below the lock) and values far outside any sensible use (a 1 GiB body, a 1000-confirmation wait, a day between scans), not values that are merely unusual. `settle.min_confirmations` must also be at least `settle.lock_confirmations`, so a price is always locked before settlement.

The numbers in the other documents (1 and 6 confirmations, the 0.01 price tolerance, the 0.10 reconciliation target, 2%, 120 s, 420 s for `/health`) are these defaults.

The price feeds are not settings. The three sources (Coinbase, Bitstamp and Gemini, see [DESIGN.md](DESIGN.md), "Price") and their URLs are fixed in `src/price.rs`, and only those three are allowed. Changing a feed means changing the code.

## Reloading, restarting and snapshots

A running `serve` is changed with signals. Reloading and snapshots do not stop the mint's service for wallets; a restart does, briefly.

| Signal | What it does |
|---|---|
| `SIGHUP` | Reads `psp-mint.toml` and the environment again. Every setting marked **reloads live** applies at once. A setting marked **needs a restart** that the file changes is named in the log, with the old and the new value, and keeps its running value until the mint is restarted; nothing is restarted. A file with any problem changes nothing; the log says what is wrong. |
| `SIGUSR1` | A snapshot of the network's data folder (see "Snapshots"). |
| `SIGTERM`, `SIGINT` | Stop: the requests being answered finish, every store is written and closed, and the process exits with status 0. |

With `systemd`: `systemctl reload psp-mint` (`SIGHUP`) and `systemctl kill -s USR1 --kill-whom=main psp-mint`. With `launchd`: `launchctl kill HUP|USR1 system/<label>`. `spi reload` sends `SIGHUP` and `spi snapshot` sends `SIGUSR1`. The environment is the one the process started with: a variable changed later is not seen, so change the file.

Every setting is one of two kinds, and the Settings table marks each one (`config::SPECS`, field `reload`):

- **Reloads live** (applied on `SIGHUP` with nothing reopened): `server.melt_max_request_bytes`, `server.rollover_max_request_bytes`, `rollover.max_items`, `server.burns_max_request_bytes`, `server.check_state_max_request_bytes`, `bitcoin.rpc_url`, `bitcoin.cookie`, `bitcoin.rpc_timeout_secs`, `scan.interval_secs` (from the next wait), `scan.health_intervals` and `scan.health_grace_secs` (the next `/health` uses them), every `price.*`, every `settle.*`, every `reconciliation.*`, `delivery.read_fee`, `burn.enabled`, `burn.threshold_psp`, `burn.balance_file` and `burn.balance_max_age_secs`. Fees and limits are all here. A request already being answered finishes with the values it started with.
- **Needs a restart**: `server.bind`, `bitcoin.network`, `bitcoin.wallet`, `mint.max_denomination_units`, `rotation.activation_hours`, `rotation.period_hours`, `storage.mint_state`, `air.server`, `air.host`, `air.held_reads` and `air.timeout_secs`. Each is read once, when the mint opens its data, because it shapes something that stays open for the life of that opening (the listening socket, the key sets, the AIR connection); [DESIGN.md](DESIGN.md), "Restarting", gives the reason for each. Until the mint is restarted it keeps the value it has and the log says so.

### Restarting (a brief downtime)

A setting that needs a restart, and a new version of psp-mint, take effect when the process is stopped and started again. That is a brief downtime, and it is the only thing besides a decommission that stops the mint. One process holds the mint's data (sled's exclusive lock), so the new process starts only after the old one has stopped. While it is down, a wallet's AIR request waits in its mailbox slot on the AIR server and is answered when the mint is back (the listener resumes from the stored counter `air_mailbox_next`, and a reply is written once); the local API refuses connections, and its callers (the SPI, esp-mint) retry.

With the SPI, `spi restart psp-mint` does it: it says that the AIR service and every mint it runs will be shut down, asks for confirmation, and changes nothing without it. `spi upgrade` does the same for a new version and checks that `/health` reports the version it installed.

The stop takes as long as the slowest thing it waits for: the scan finishes the RPC it is in (`bitcoin.rpc_timeout_secs`, 30 s by default), the AIR listener finishes the slot it is answering, the local API gets 30 s to finish its requests, and the runtime then gets 5 s to drop what is left. The service files give the stop 90 s (see `ops/` in the SPI).

### Snapshots

`SIGUSR1` copies the network's data folder (keys, notes, records, the scan checkpoint, the AIR counter, `revenue/`) to `<data folder>/snapshots/<UTC time>/`. The scan stops after its pass, the AIR listener finishes the request it is answering and stops (a request wallets send meanwhile waits in its mailbox slot), the local API stops accepting and finishes the requests it is answering (30 s at most) while its listening socket stays open, so a connection made now waits in the kernel's queue. Every thread is joined and the mint, its records and the rotation are dropped: sled writes everything and releases its lock when the last handle goes. Then the copy is made, of files nothing has open, and the mint opens its data again. If a task still holds the data after the local API was stopped, the snapshot is refused with a log line and the process exits with status 1 so that the service manager starts it again, because the mint cannot open files that a task of its own still holds.

`snapshots/LATEST` names the newest complete folder and is written last, so whoever reads it never sees one that is half copied; a copy that fails is removed and the mint goes on. **The folder holds the mint's keys** (mode 0700, readable by the mint's account only). The copy is the whole folder, so the pause grows with its size, roughly linearly: the copy and its `fsync`, plus the time sled takes to open the data again. A connection the local API had accepted but whose request it had not read yet is closed when it stops, which no server can avoid; the clients of the local API (the SPI, esp-mint) retry a read on a closed connection, so they get their answer. To start a mint from a snapshot, put the folder's contents at `<base>/<network>/` with the mint stopped.

## The AIR listener

Wallets reach this mint over AIR and only over AIR. The calls are **mint info** (what `/health` reports: `status`, `wire_version`, `network`), the three reads of the mint's keys, **keys**, **keysets** and **rotation**, and the four calls with a body, **rollover**, **check-state**, **spend** and **reconcile** (kind 15, see "Reconciling a payment"), each answered with a reply signed by the mint, except that a reconciliation that is not accepted is not answered at all. `serve` always starts the listener (a thread beside the HTTP server), and refuses to start without `air.server` and `air.host`. What is left on HTTP is local to the SPI: `/melt`, `/burns`, `/audit` and `/health` (see "Local HTTP API"), on a loopback address.

- **How it works.** The listener holds reads on the next `air.held_reads` (16) mailbox slots of the mint's DID on the configured AIR server. When a wallet writes a sealed request into one, the listener opens it with the mint's DID key (`mint_did_key.json`, the one that already opens PSP payloads), answers with the JSON body of the request's kind as it is now (for mint info the status and the wire version and network `/health` reports; for keys, keysets and rotation the key list, the keysets and the rotation window), signs the reply with the same key, seals it to the wallet's one-off key and writes it into the reply cell only that wallet and the mint can find. The reply cell is write-once, so the first write wins: a request that reached the mint twice, or two mint processes, give one reply. Any other kind of request is logged and skipped.
- **Rollover and check-state.** The body is the request JSON the route takes (sealed to the mint's DID with the rest of the request, so the notes in it are readable by the mint only), and the reply body is the JSON the route returns. The ceilings are `server.rollover_max_request_bytes`, `server.check_state_max_request_bytes` and `rollover.max_items`. A refusal is a signed reply with status `refused` and a JSON body with `error` and `code` (a keyset code, `note_spent_elsewhere` with `spent`, and so on); over a ceiling the body is the `request_too_large` JSON (`size_bytes`, `limit_bytes`), a body that is not a request is `malformed_request`, and a failure of the mint's own is the signed `unavailable`. **A resent request is not applied twice:** the rollover is stored by request hash before any note is spent, so a retry, a copy in another slot, a crash between answering and writing the counter and a wallet that starts over all get the stored answer. A rollover holds the mint lock while it runs; the listener reads no further slots meanwhile.
- **Spend (kind 14).** A payment in PSP: notes in, change out, an amount `pay` that is the SPI's revenue. The body is `sp_wire::psp::PspSpendRequest` (`input_keysets`, `inputs`, `keyset` for the change, `outputs` for the change, `pay` in PSP units, `memo` of at most 256 bytes); every input's `spend_signature` covers its digest, which names `pay`, the memo, every input's keyset and every output. The inputs must be worth the change plus `pay`, exactly. The reply is the change's blind signatures (`outcomes`, `keys`, the `keyset` and `epoch` that signed them) and `paid`. `pay` is **reissued to the SPI's own ecash-wallet** unless burning is on (`burn.enabled`, off by default): the mint makes the notes itself, saves their plan with the request before anything is burned, and writes them to `revenue/<request hash>.json` in the data folder (a `sp_wire::psp::PspHandover`, mode 0640, in a folder of mode 0750, so the operator in the service's group can read it), where `ecash-wallet receive-psp` takes them in. With `burn.enabled` true the mint reissues only up to `burn.threshold_psp` PSP in all and burns the rest of each `pay`. Only `pay` is ever burned: the PSP the ESP mint burns for an exchange, and the PSP a purchase buys, are not touched. `/audit` counts `pay` in `paid`, split into `burned` and `reissued`. The checks run cheapest first, and any failure burns nothing: shape and sizes (`no_inputs`, `too_many_items`, `memo_too_long`, `missing_keyset`, `nothing_to_pay` for a `pay` of 0, which would be a plain rollover; `not_exact` when the notes are not the change plus `pay`; `duplicate_input`, `duplicate_blind_nonce`), denominations (`bad_denomination`), the keysets (the current or the previous one, as a rollover: `keyset_retired`, `unknown_keyset`), each note's proof and its signature over the digest (`bad_note`, `bad_spend_signature`), and then the spent lists under the mint lock (`note_spent_elsewhere` with `spent`, the index of each note already spent; `note_held` for a note an unfinished request holds). A spend with no change names no change keyset. A resent spend gets the stored answer and burns nothing more, and a spend cut short by a crash is finished by the same request sent again (see DESIGN.md, "The spend"). The ceilings are the rollover's: `server.rollover_max_request_bytes` and `rollover.max_items`. A spend adds no fee and no price; fees stay 0. Nothing here attaches a spend to another request.
- **Junk is skipped.** A slot that does not open, is malformed, is of an unknown kind or names a key that is not one is logged and skipped. Junk changes no state; a valid request costs the mint one short lock.
- **A call the mint cannot answer now** (the mint lock is poisoned, or a rotation step failed) gets a signed reply with status `unavailable` and no body, so the wallet fails at once with the reason instead of waiting out its tries. Mint info is always answered (a poisoned lock is `mint_lock_poisoned`, as in `/health`).
- **What the signed reply gives.** The keys come in a reply signed by the mint's DID key, so nothing between the wallet and the mint, not the AIR server and not a man in the middle, can substitute keys.
- **The counter** (`next`, the lowest slot not finished) is the text file `air_mailbox_next` in the network's data folder (`~/.psp-mint/<network>`), with the rest of the mint's local state, and **not** under `storage.mint_state`: it is only an optimisation. Without it the listener starts at slot 0 and an answered slot finds its reply cell taken and is passed over. It is written by temp file, fsync and rename after each slot is finished, so a crash between answering and writing it repeats a slot, which is harmless. A damaged counter file stops the listener with a message; it is never replaced.
- **If the AIR client gives up** (more than five held reads end within ten seconds, which is what a session the server refused or closed looks like, or a reply could not be written after three tries) the listener logs `AIR listener failed: ...` and the process exits with status 1, because a mint that no wallet can reach is not running. A supervisor restarts it, and the listener resumes from the counter. A restart of the AIR server needs nothing: the client reconnects and its held reads resume.
- **Held reads left behind by a stopped listener are never cleaned up by the AIR server** (upstream item 17); with 16, a handful of restarts costs little.
- **The AIR server** is the one bundled with the SPI, reached over loopback; its name is `air.server` and its host `air.host`. Resolving the server from the mint's DID is upstream item 4; until then both are settings.

## Delivering a purchase

When the mint has handled a payment, it writes **one record** to the AIR server (`air.server`, `air.host`): the purchase's signatures and the current keysets, or that the payment is **held** (paid, but short beyond the reconciliation target), or the reason the purchase was **rejected**. A held or rejected record also says what was received (satoshis, and US cents at the mint's price). It is signed by the mint's DID key and sealed to the reply DID in the payload (a one-off key of the payer's for this purchase), at an address only the payer and the mint can compute (`sp_wire::air_request::delivery`). The payer's wallet reads it and claims the notes (`ecash-wallet claim-psp`); nothing is pasted. The `signature` lines `run` and `serve` print are a log only.

The record is stored in the purchase's settlement record first and written after every scan until the AIR server has it, with a wait that doubles from 10 seconds to 15 minutes between tries of one record, across restarts. A delivery never blocks or undoes issuance. A payment whose payload cannot be read gets no record (the mint has no key to seal one to): the payer takes the loss. A rejection delivers the reason; it does not refund the bitcoin. Never refunded (decided 2026-10-22, ROADMAP.md "Shared items").

**The reconciliation window.** A credit, a held payment and a rejected purchase with a readable reply key stay open for `settle.reconciliation_window_days` (14 by default). The record carries the end of the window (`open_until`); the wallet keeps the purchase and its key until then. A corrected request or a credit claim over AIR settles an open payment (see "Reconciling a payment (kind 15)"). When the window closes with the payment unsettled, the mint marks it **forfeited**, keeps the bitcoin, and no longer lists it as owed. `/audit` shows open and forfeited payments apart from the issued, redeemed and outstanding totals. See [DESIGN.md](DESIGN.md), "Signature delivery" and "The reconciliation window".

## Reconciling a payment (kind 15)

A payment that is **held**, a purchase that was **rejected** with a readable reply key, and a purchase that was **signed with a credit owed** are open for the reconciliation window. One AIR request kind settles all three, `KIND_PSP_RECONCILE` (15). The body is `sp_wire::psp::ReconcileRequest`: the `reply_did` (the purchase's one-off reply key), the `purchase` id, the `keyset` to sign with (the current or the previous one), the new blinded `outputs`, and a BIP340 `signature` by the reply key over the request's digest. For a held or rejected payment it is the **corrected request**; for a credit it is the **credit claim**. The mint tells them apart by the rules it stored for the payment (`send`).

**The rules are up front.** The first delivery record (kind 13) says what a reconciliation must carry, in `rules`: `send` (`corrected_request` or `credit_claim`), `kind` (15), `due_units` (the exact total of the outputs, after every fee), `reconciliation_fee_units`, `read_prepaid_units`, `write_prepaid_units`, `open_until`, `max_attempts` and `on_failure`, which says that a failed or late reconciliation forfeits the payment and the funds are lost, and that a refused request gets no reply. The mint stores the same rules in the payment's record when the window opens and checks every request against them, never against the current settings, so changing a setting changes no payment already open. A reconciliation is a **fallback for a fallback**: the first delivery is the normal path, and this is for a payment that did not settle on it.

**No refusal replies.** An accepted request is answered with `ReconcileResponse` (the signatures, the keysets, and what was taken); anything else gets no reply at all and the reason is only in the mint's log, with a code: `bad_signature`, `unknown_payment`, `ambiguous_payment`, `payment_closed`, `too_many_attempts`, `in_progress`, `wrong_total`, `nothing_to_issue`, `unknown_denomination`, `duplicate_blind_nonce`, `blind_nonce_used`, `keyset_retired`, `unknown_keyset`, `malformed_request`, `request_too_large` and `unavailable`. A request signed by the right key, for an open payment, refused for its content and not a resend of an earlier refused request is one **counted attempt**; the payment accepts `reconciliation.max_attempts` of them. A bad signature, an unknown or closed payment, a resend of a refused request and a failure of the mint are not counted. A wallet that gets no reply after a while sends the identical request again.

**What is due.** For a held or rejected payment: the units the payment buys at the locked price, less the first delivery's read cost, the fee, the one prepaid write and the follow-up's read cost, never below 0. For a credit: the credit in whole PSP units (10 cents each, rounded by `reconciliation.round_up_from`), less the fee, the write and the follow-up's read. The outputs must add up to exactly that: the mint signs a request whole or not at all.

**One payload, one payment.** A purchase's payload is **one-time use**. A second bitcoin payment that carries the same payload is kept and shown in `/audit` as a `duplicate`; it buys nothing, gets no delivery and no window, and can never be reconciled.

## Local HTTP API

The HTTP API is local to the SPI and the operator; wallets do not use it (they use AIR, above). It is served on `server.bind`, which must be a loopback address. The handlers are this mint's own, over the PSP's own mint in the same process (mint-core's `AppState`: the mint behind one mutex); `/melt` also keeps the memo of each burn (see [DESIGN.md](DESIGN.md), "Burn memos"), and `/burns` is a route of this mint only. The bodies of `/keys`, `/check-state` and `/melt` are sp-wire's (`keys`, `melt`). The scanner and the API share the mint behind one mutex. The scanner locks it once per payment and never while it waits on the exchanges for a price.

| Route | What it does |
|---|---|
| `POST /melt` | Burns notes. Body: `{"inputs": [...], "memo": "..."}`. Every input's spend signature covers the memo, so the burn is bound to it. Returns `{"burned": ...}`. The ESP mint uses this to burn PSP in an exchange, with the memo it computes from the request. The memo is kept for every note the burn spends, and only after every input has been checked: a `400` means nothing was burned and nothing was recorded, and a fault of the store is a `500`. A rollover or a spend that burns a note clears any memo an earlier melt left on it. |
| `POST /burns` | Body: `{"nonces": [...]}`. Returns `{"burns": [{"spent": true, "memo": "..."}, ...]}` in the same order. `memo` is the memo the note was burned under, and is `null` if the note is not spent or was spent with no memo on record. The ESP mint uses this on a retry. See [DESIGN.md](DESIGN.md), "Burn memos". |
| `GET /audit` | `issued`, `redeemed` and `outstanding`, which are **decimal strings** (totals are `u128`; a JSON number cannot hold one past `u64`), `paid` (the PSP units paid to the SPI by spends, kept apart from every other burn such as an exchange's `/melt`; the inputs of a spend are part of `redeemed`), of which `burned` (destroyed) and `reissued` (made again as notes for the SPI's wallet, in `revenue/`; part of `issued`), and `reconciliation`: `open`, `forfeited` and `settled` (settled by a kind 15 request), and `duplicates` (a payload paid for twice, apart from `forfeited`), each with `payments`, `sats` (the bitcoin held) and `credit_cents` (credits owed, or claimed for `settled`); and the PSP's reconciliation revenue, `fees_units`, `read_prepaid_units`, `write_prepaid_units`, the credit rounding `rounding_kept_cents` and `rounding_given_cents`, and `refused_attempts` (requests refused for their content, so an operator can answer a support request). `paid`, `burned`, `reissued` and the reconciliation figures are JSON numbers. A forfeited payment is not part of the issued, redeemed and outstanding totals. |
| `GET /health` | `200` with `status` `ok` (or `starting`, before the first scan finishes), `503` with `scan_stale` or `mint_lock_poisoned`. The body also has `version` (this build's version, which `spi upgrade` reads after a restart to see that the new build is the one answering) and `wire_version` (an integer: the version of the wire format this mint speaks; a wallet refuses a mint whose number is not its own), `last_scan_unix`, `seconds_since_last_scan` (both null until a scan succeeds) and `stale_after_secs`. A scan counts when the wallet was read and every payment was processed as far as it could go, so a payment waiting on confirmations or a price is not a failure. `scan_stale` means no successful scan for `scan.health_intervals` intervals plus `scan.health_grace_secs` (by default three intervals plus 60 s). |

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
cargo test --all-features
cargo fmt --check
```

The tests run the code the mint runs. A mint in a test is built the way `serve` builds it (`tests/common`, `PspMint`: a rotor and its rollovers, the settlement records shared with the AIR listener, the burn memos, a health with a network), and a payment is settled, delivered, forfeited and reconciled by the functions `serve` calls (`service::cycle`, `service::run_once_shared`, `Deliverer::pass_shared`, `server::air_served_with`, `server::router_live`). There is no function in the library that only a test calls. The unit tests are in `src/`, beside the code they test; the integration tests are in `tests/`:

| File | What it covers |
|---|---|
| `air_info.rs` | The AIR listener against a real `air_server` started in the test process: a happy path, junk in the slots, a restart that resumes from the counter, a mint that dies before replying and the wallet's retry, keys, keysets, rotation, rollover and check-state answered, a mint that cannot answer (a signed refusal for each kind), a rollover resent and not applied twice, and a full rollover of a thousand notes. |
| `api.rs` | The local HTTP API (`/melt`, `/burns`, `/audit`, `/health`) and that the calls that moved to AIR are `404` on HTTP. |
| `cli.rs` | The binary as a process, with an empty environment and a data folder of its own: `--help`, unknown commands and flags, `encrypt`, `pubkey`, `config` and every way it refuses, `serve` without its AIR settings, and `run` with bitcoind unreachable. |
| `cycle.rs` | One scan pass (`service::cycle`, then `deliver` and `forfeit`) with a stand-in wallet, a price closure and the real AIR server: payments settled with fees on, delivered and read back as the payer reads them, a held payment forfeited when its window closes, and a pass that fails leaving the checkpoint and the records as they were. |
| `delivery.rs` | Settlement and delivery to a real `air_server`, read back the way the wallet does: a settled purchase, a rejection and a held payment (each with what was paid and a window), fees on, an unreadable payment with no record, the AIR server down and a restart that delivers one record, a write found already stored, a different record never overwritten, and a reply DID that is not a key. |
| `docs.rs` | This README against the program: its commands are the ones `--help` prints and its settings table is the table the program reads. |
| `limits.rs` | Body ceilings on `/melt`, `/burns` and over AIR: refused before parsing, nothing burned. |
| `network_http.rs` | `/health` and `/audit` carry the network. |
| `reconcile.rs` | Kind 15: a held payment and a rejected purchase settled at exactly what is due, a credit claim and its rounding, the fees (once, and only on the attempt that signs, after the keyset-rotation race), every refusal and whether it counts as an attempt, requests ignored with nothing recorded, a crash between pending and close finished by the resend, a poisoned records lock, a body over the ceiling refused before any lock, `/audit` with every figure, and over AIR an accepted request answered and a refused one left without a reply. |
| `reconciliation.rs` | `/audit` shows open and forfeited payments apart, and is current as of the last write. |
| `rotation.rs` | Key rotation and rollover over AIR: previous notes kept for one period, a rollover with a spent input burning none, one interrupted after the burn and finished by the retry, a melt mixing both sets, resumed rollovers after a retirement. |
| `spend.rs` | A spend with change over AIR (kind 14). |

What the tests do not do: nothing here starts bitcoind. `cli.rs` cannot run a scan that finds payments, or `serve` past its start-up checks; `cycle.rs` covers the pass through the library with a stand-in node. The end-to-end run with a Bitcoin Core regtest node is [REGTEST.md](REGTEST.md). The tests that use AIR take turns on one local `air_server` (port 5702 must be free).

## Documentation

- [REGTEST.md](REGTEST.md): setting up a regtest node and wallets, and running a purchase through the mint (steps 1 to 8).
- [DESIGN.md](DESIGN.md): how psp-mint works and why: the scanner, key rotation, settlement and the price tolerance, the price, the payload and its encryption, burn memos.
- [CHANGELOG.md](CHANGELOG.md): what was built and what changed.
- [ROADMAP.md](../sandbox-mint/ROADMAP.md) (in `sandbox-mint`): left to do and not decided yet, for every repo.
- [OPERATIONS.md](../sandbox-mint/OPERATIONS.md) (in `sandbox-mint`): running the mints as services: `launchd` and `systemd` definitions, install and uninstall, the health monitor.
- [esp-mint](../esp-mint/README.md) and [ecash-wallet](../ecash-wallet/README.md): the other parts.
- [sandbox-mint README](../sandbox-mint/README.md): the family of repos.
