# Changelog

psp-mint 0.1.2 builds on `sp-wire` 0.2.0 (wire version 2, AIR 2.0.3) and mint-core 0.1.3. Nothing was recorded per version before this file; the section below is everything the README said about what was built and when.

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
