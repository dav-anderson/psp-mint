# Changelog

psp-mint 0.1.2 builds on `sp-wire` 0.2.0 (wire version 2, AIR 2.0.3) and mint-core 0.1.3. Nothing was recorded per version before this file; the section below is everything the README said about what was built and when.

## 0.4.0 (unreleased)

**Wire version 3: the service ID is the 33-byte AIR DID.** Builds on `sp-wire` 0.5.0 (`WIRE_VERSION` 3). `Cargo.toml` still names `sp-wire` by `tag = "v0.4.0"`, which moves when the new release is tagged (until then a local `[patch]` is used, see the README of `sp-wire`). `GET /health` reports `wire_version` 3 because it reads the constant. No other code changed: the PSP has no service in any request or file, only the storage policy.

- `storage.mint_state` (`PSP_STORAGE_MINT_STATE`) takes DIDs, 66 hex characters starting `02` or `03`. The 32-byte x-only form is refused when the config loads, with a message that says to use the DID, and a 33-byte DID is no longer reduced to its x coordinate.
- Tests: the storage config test uses DIDs and checks that the x-only form is refused. Documentation: README and DESIGN.md say 66 hex characters.

## 0.4.0, AIR listener (unreleased)

- **An AIR listener answers a wallet's mint-info request, beside the HTTP server. Off unless `air.enabled` is true.** The first call between a wallet and this mint that does not go over HTTP: what `GET /health` reports (`status`, `wire_version`, `network`), signed with the mint's DID key (`mint_did_key.json`). The listener holds 16 reads on mailbox slots of the mint's DID, opens each request, answers it, and creates the reply in the write-once reply cell (first write wins, so a retry gets one reply). Junk in a slot is skipped. The counter `next` is the file `air_mailbox_next` in the network's data folder, beside the rest of the local state and not under the storage policy; it is rebuilt by probing from slot 0. If the AIR client gives up, the listener logs it and ends; HTTP keeps running. New module `air_listener`; `Health::mint_info`; `serve` starts the thread. The wire is `sp_wire::air_request` (sp-wire's new `air-request` and `air-client` features); no `WIRE_VERSION` bump.
- **New settings:** `air.enabled` (`PSP_AIR_ENABLED`, `false`), `air.server` (`PSP_AIR_SERVER`, empty), `air.host` (`PSP_AIR_HOST`, empty), `air.held_reads` (`PSP_AIR_HELD_READS`, 16, 1 to 200), `air.timeout_secs` (`PSP_AIR_TIMEOUT_SECS`, 10, 1 to 600). With `air.enabled` true, `air.server` must be a public key and `air.host` a host or IP with no port.
- **Building now needs `air_server`** (a git dependency of sp-wire's `air-client`, pinned by commit `bab9ae0`) and, for now, a local patch for it beside the AIR patch (README, "Building"). Tests: `tests/air_info.rs` against a real `air_server` in the process, which needs port 5702 free; unit tests for the counter, the settings and `Health::mint_info`.

## Unreleased

- AIR 2.1.0 (commit `741b1cf`) comes with the next sp-wire (0.4.1): `PublicKey` and `SecretKey` are the same types under the same names, the sealed bytes and the `mint_did_key.json` format are unchanged, and `WIRE_VERSION` stays 2. psp-mint's code is not changed for it. Comments in `src/seal.rs`, README and DESIGN.md name the AIR types as they are exported now (not `air::names::secp256k1`, which is private in 2.1.0).
- **`psp-mint --help` prints the commands** (it had none; the option was read as an unknown command after the settings were loaded). Errors are printed as `error: ...` in plain text, not as the debug form of the value (`Error: "..."`).
- Lint cleanups, no change in behaviour: unused imports in `rotation.rs` and two test files, `assert!` instead of a comparison with `true`, a `vec!` that was an array, `describe_offline` moved above the test module, a test that held a mutex guard across an await now holds the lock on another thread, and `&Path` instead of `&PathBuf` in a test helper.
- Documentation stands alone: the service files and the health monitor are described by the SPI repository, and the storage requirements are the numbered set (1 to 8) in DESIGN.md. README: psp-mint has no fee setting.

## 0.3.0 (2026-10-16)

- **Setting `storage.mint_state` (`PSP_STORAGE_MINT_STATE`): which AIR services will keep this mint's state.** An ordered list of service IDs (index 0 the primary, the rest secondaries); one entry is valid, empty (the default) means local only, at most 8, no duplicates. Config and validation only: nothing reads or writes the AIR, the state is still local files. The parser and checks are `sp_wire::storage` (sp-wire 0.4.0), shared with the other mints and the wallet. In `psp-mint.toml` the value is an array of strings or one string. DESIGN.md has "Storage policy (setting only)"; the planned behaviour is in sandbox-mint's DESIGN.md, "AIR storage requirements".
- `sp-wire` is now the `v0.4.0` tag. Tests: the setting from the file and the environment, both forms, and every kind of refusal.

## 0.2.1 (2026-10-16)

- **Running as a service.** SIGTERM now stops `serve` the way Ctrl-C does (the API drains and the process exits 0); before, only SIGINT did, and a service manager's stop killed it. `launchd` and `systemd` definitions, env and monitor settings examples and `sp-health-monitor.sh` are in `sandbox-mint/ops`, with the steps in `sandbox-mint/OPERATIONS.md`. DESIGN.md: "Running as a service", with what the AIR storage has to keep (single writer, atomic durable writes).

## 0.2.0

- **`POST /rollover` signs the outputs with the live key set the request names, current or previous.** Before, a request naming the previous set was refused `400 keyset_not_current`. Now only a set that is not live is refused: `keyset_retired` for a deleted one, `unknown_keyset` for anything else, before anything is burned. A request resumed after a crash is signed by the named set while that is live, and by the current set once it has been deleted. `keyset_not_current` is no longer sent; `sp-wire` keeps the name reserved.
- Tests: the rotation test now rolls over with outputs naming the previous set (signed by it, the answer names it), a retired set (`keyset_retired`) and an unknown one (`unknown_keyset`).
- **Settings for the `/rollover` limits.** `server.rollover_max_request_bytes` (`PSP_ROLLOVER_MAX_BYTES`, default 1 MiB) is the ceiling on a `POST /rollover` body; it used to borrow `server.melt_max_request_bytes`. `rollover.max_items` (`PSP_ROLLOVER_MAX_ITEMS`, default 1000, the old constant `MAX_ROLLOVER_NOTES`) is the most notes in one rollover. Tests: the settings' defaults, ranges and environment overrides; a rollover body over the ceiling gets `413`; a rollover over the note cap gets `400` and burns nothing.
- **A BTC claim is signed with the live key set its payload names.** The wallet now names the set it last saw in a purchase (before, it sent none). The mint signs with that set while it is live (current or previous), else with the current set. The printed `keyset <id>` line still says which signed. No byte of the wire format changes. A note signed by the previous set expires sooner, at the next rotation. Tests: a payload naming the previous set is signed by it; one naming no set, or a deleted one, by the current set.

## Moved from the README

The sections and sentences below were in the README and describe what was built, when, or what changed. They are kept here word for word. A short fragment is a part of a sentence that was cut out; the rest of the sentence stays in the README or DESIGN.md under the same heading.

### Migrating old data

**Migrating old data.** Before this change the data sat directly in `~/.psp-mint` and was regtest data. The mint refuses to run (every command except `config`) while those files are there, and tells you to run `cargo run -- migrate`, which moves them into `~/.psp-mint/regtest`. It only renames, and it refuses if the target already holds a file of the same name. The regtest wallet name default changed from `psp` to `psp-regtest`: set `PSP_WALLET=psp` (or `bitcoin.wallet = "psp"`) to keep an old wallet, or create `psp-regtest`.

### The scan

`scan.max_transactions` (`PSP_SCAN_MAX_TRANSACTIONS`) is retired: the scan has no window any more, and setting it stops the mint with an error saying so.

### Wire version

, so the paths did not change

(2: version 1 was the first release, with AIR 1.1.0's sealed envelope; version 2 is AIR 2.0.3's, 80 bytes of overhead instead of 82 or 83, and the only change)

### Settlement rules

The top denomination is 2^24 so that a purchase still covers the same dollar range (it was 2^20 at one PSP to the dollar).

### Encryption

Version 1 (AIR 1.1.0) used a `postcard` envelope of 82 or 83 bytes, and the mint no longer reads it (see "Wire format and versions").

At one PSP to the dollar a purchase under $10,000 took at most 13 notes (648 bytes); at ten to the dollar it takes at most 16 (771 bytes), the cost of the finer unit.
