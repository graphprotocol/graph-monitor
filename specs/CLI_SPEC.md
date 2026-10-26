# Graph Protocol CLI Specification

A CLI for gaining insight into Graph Protocol state.

## Overview

The tool answers one kind of question: **what is going on in this part of the protocol, right now.**
It is optimized for a human typing one command and reading one screen.

v1 is **read-only**. Write support is not ruled out, but no command in this spec mutates state,
and no command requires a private key.

All data comes from **RPC**. There is no subgraph dependency, no explorer API, and no indexed
history in v1 — which means every value the tool reports is a value that can be read from a
single block. Anything requiring history (activity timelines, "last seen", rate-of-change) is
out of scope until we add a second data source.

The binary is `graph-network`.

### Principles

1. **One block, one truth.** Every value in a readout is read at the same block height, so the
   output is internally consistent, reproducible via `--block`, and *always states the height
   it came from*. See [Block provenance](#block-provenance) — this is a hard requirement on
   every command and every output mode, not a header decoration.
2. **Config holds addresses, code does not.** No address is hardcoded in Rust. Addresses live
   in TOML: shipped defaults for what we know, user config for everything else.
3. **Human first, machine second.** Default output is an aligned table sized for a terminal.
   `--json` gives the same data unabridged for scripting.
4. **Batch aggressively.** A readout is one RPC round trip where possible (Multicall3), not
   one call per field.
5. **Flat command tree.** `<domain> <verb>`. Two levels, no deeper.

### Non-goals for v1

- Writing transactions, signing, or key management.
- Historical queries, time series, or event log scanning.
- Watching/polling. A command runs once and exits.
- A TUI. The output contract below is designed so a TUI can be added over the same core
  without changing it, but v1 ships CLI only.

---

## Stack

| Concern | Choice | Notes |
|---|---|---|
| Language | Rust 2021, stable | |
| CLI parsing | `clap` v4 (derive) | subcommands, env fallbacks, generated help |
| Ethereum | `alloy` | providers, `sol!` typed bindings, Multicall3 support |
| Async | `tokio` (multi-thread) | |
| Config | `serde` + `toml` | layered defaults → file → env → flags |
| Tables | `comfy-table` | or hand-rolled if we want tighter control of separators |
| Errors | `thiserror` in core, `anyhow` at the binary edge | |
| Logging | `tracing` + `tracing-subscriber` | `-v`/`-vv` to stderr, never stdout |

### Crate layout

```
crates/
  core/     # config, RPC client, typed bindings, domain logic. No output formatting.
  cli/      # clap command tree, table + JSON rendering. Thin.
config/
  default.toml   # shipped addresses and watchlists, embedded at build time
```

The split exists so a future TUI is a third consumer of `core`, not a rewrite. `core` returns
typed values (`U256`, `Address`, block number); it never formats a number or prints a row.
Every value `core` hands back is tagged with the block it was read at.

---

## Configuration

Resolution order, later wins:

1. **Shipped defaults** — `config/default.toml`, embedded in the binary at build time. Holds
   known networks, the address book, and the default account list.
2. `$XDG_CONFIG_HOME/graph-network/config.toml` (fallback `~/.config/graph-network/config.toml`)
3. `./graph-network.toml` in the working directory, if present
4. Environment variables
5. Command-line flags

`${VAR}` in any string value is expanded from the environment, so RPC URLs with API keys stay
out of the file.

Merging is per-key, not whole-file: a user config that sets one `rpc_url` keeps every shipped
address, and a user watchlist *adds to* the shipped one. See [Extending and replacing
defaults](#extending-and-replacing-defaults).

Setting `use_defaults = false` drops layer 1 entirely — the binary then behaves as if nothing
were shipped with it.

### Networks

`[networks.<name>]` describes how to reach a chain. `[networks.<name>.contracts]` is the
address book for that chain — plain name-to-address, ours to curate, no external source.

```toml
default_network = "arbitrum-one"

[networks.arbitrum-one]
chain_id = 42161
rpc_url  = "${ARBITRUM_ONE_RPC_URL}"

[networks.arbitrum-one.contracts]
GraphToken          = "0x9623063377AD1B27544C965cCd7342f7EA7e88C7"
HorizonStaking      = "0x..."
PaymentsEscrow      = "0x..."
GraphTallyCollector = "0x..."

[networks.arbitrum-sepolia]
chain_id = 421614
rpc_url  = "${ARBITRUM_SEPOLIA_RPC_URL}"

[networks.arbitrum-sepolia.contracts]
GraphToken = "0x..."
```

`GraphToken` is a **required** key for any network with GRT or allowance watches — it is
how the tool finds GRT. The key is always spelled `GraphToken` regardless of what the deployed
contract calls itself on that chain.

### Accounts

Keyed by network. `watch` lists the assets to read; a `remediate` block implies its asset is
watched. Groups are labels with no behavior. The full model, including what belongs here versus
in Grafana, is in `CHECKS_PLAN.md`.

```toml
[[accounts.arbitrum-one]]
address = "BillingWallet"
group   = "graph-foundation"
watch   = ["eth", "grt"]

  [[accounts.arbitrum-one.allowances]]
  spender = "NetworkMonitor"

  # The only threshold the tool holds, because repair must work when metrics are down.
  [accounts.arbitrum-one.eth.remediate]
  below     = "0.05"
  top_up_by = "0.1"
  from      = "FoundationBillingMultisig"
```

### Name resolution

A name used in `allowances` resolves in this order, within the active network:

1. A key in `[networks.<name>.contracts]`
2. An account `label` in that network's watchlist
3. Otherwise it must parse as a hex address

A name defined as *both* a contract and a label is a config error rather than a silent
precedence win — the same string meaning two addresses is exactly the kind of thing that makes
a readout lie.

### Extending and replacing defaults

User config **extends** the shipped defaults. Declaring `[[accounts.arbitrum-one]]` in
your own config adds accounts to the shipped list for that network; it does not discard it.
Same for `[networks.*.contracts]` — your names are added alongside ours.

Two escape hatches, because extension alone cannot express "watch less":

**Ignore the defaults wholesale.** A top-level `use_defaults = false` (or `--no-defaults` for a
single run) drops the shipped layer completely: no shipped networks, no shipped contract
addresses, no shipped watchlist. Your config becomes the entire truth. Note the consequence —
you now have to supply `GraphToken` and an `rpc_url` yourself, since those come from the
shipped layer too.

**Disable one account.** `enabled = false` on a watchlist entry removes it from readouts.
Restating a shipped account's `label` with `enabled = false` is how you drop exactly one
account without going nuclear.

**Collisions resolve toward the user.** A user account whose `label` matches a shipped one
replaces that entry rather than producing a duplicate row, which is how you adjust a shipped
account's thresholds without restating the list. Same for a contract name defined in both
layers.

`--json` tags every account with the layer it came from, so the effective list is inspectable
rather than inferred.

---

## Global flags

| Flag | Default | Purpose |
|---|---|---|
| `--network <name>` | `default_network` | Selects a `[networks.*]` entry. |
| `--rpc-url <url>` | from network | Overrides the endpoint without touching config. |
| `--block <n\|latest>` | `latest` | Pins the read. All values come from this block. |
| `--json` | off | Machine-readable output on stdout. |
| `--config <path>` | see above | Explicit config file. |
| `--no-defaults` | off | Ignore the shipped config layer for this run. Equivalent to `use_defaults = false`. |
| `--remediate` | off | Perform configured repairs. Needs a `signer_key` too. |
| `--dry-run` | off | Form the repair and log it without broadcasting. |
| `-v`, `-vv` | off | Log level to **stderr**. |

---

## Output contract

### Block provenance

Every command states the block its data came from, in every output mode. No exceptions:

- **Human output** opens with a header line carrying network name, chain ID, block number, and
  the block's UTC timestamp. A pasted screenshot is therefore self-describing, and two readouts
  can be compared without guessing whether they are even from the same height.
- **`--json`** carries `network`, `chain_id`, `block`, and `block_timestamp` as top-level
  fields, before any data.
- **Partial failures** still report the block. A readout with `!` cells is still pinned.
- **Errors that occur after the block is resolved** report it on stderr, so a failed run still
  says where it was looking.

A readout that cannot state its block height is a bug, not a degraded mode.

### Formatting

**Units.** GRT and ETH render as decimals with thousands separators, GRT to 2 decimal places
and ETH to 4. GRT decimals are read from the token contract in the same batch as the balances
rather than assumed to be 18. `--json` always carries the raw base-unit integer as a string
alongside a formatted field; no float ever appears in JSON.

Thousands separators are a table-alignment device and do **not** appear in `--json`: the
`formatted` field there is a plain decimal (`"1204882.11"`), so a consumer never has to strip
commas before parsing. `raw` is the exact value in both modes.

### Nothing is graded

The tool holds no thresholds and no severities, so no output says "ok" or "bad". A reading is a
value, or it is unreadable. `status` summarises a cycle as `21 read · 1 unreadable`, plus a
count of pending repairs when remediation is configured.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | The cycle ran. Some watches may be unreadable; that is reported, not fatal |
| 2 | Usage or config error (bad address, unknown network, unresolvable name) |
| 3 | The source failed verification, so nothing could be read |

There is deliberately no exit code for "a balance is low" — the tool has no opinion about that.
A dead source *is* non-zero, because a run that read nothing must not look like a clean pass to
cron.

## Commands

```
graph-network
├── status                       read every watch and show what it found
│   ├── --flat                   one line per watch instead of grouped tables
│   └── --unreadable-only        only watches that failed to read
│
├── config
│   ├── show                     what config resolved to after layering
│   └── addresses                the address book for the active network
│
└── serve                        loop, export Prometheus metrics
    ├── --interval <secs>
    └── --metrics-port <port>
```

### Component subcommands

As components beyond accounts land, each gets a top-level subcommand that narrows `status` to
that component and renders it in whatever shape suits its data:

```
graph-network billing | contracts | oracles | subgraphs | infra
```

Component-first rather than check-kind-first, because **output shape follows the component**:
billing wants an accounts table with an allowance section, contracts wants expected-vs-actual,
subgraphs wants lag in blocks. A `balances` command spanning all three would be one table with
three unrelated column sets. It also matches how an incident starts — "billing looks wrong",
not "show me allowances" — and mirrors `COMPONENTS.md`, so the doc and the CLI stay in step.

These are not implemented yet; only accounts exist, and `status` already covers them.

### Nothing here judges

`status` reads and reports. There is no pass/fail, no severity, and no threshold — the tool
exports what it saw and Grafana decides whether it is acceptable (see `CHECKS_PLAN.md`,
decision 8). This is why the unit is a **watch** producing a **reading**, rather than a check
producing a verdict.

The single exception is remediation, which needs a trigger to act on.

### Remediation is a flag, not a command

`--remediate` and `--dry-run` are global. Repair is a modifier on a cycle — the same read and
report happens either way, with repair as an extra stage — so a separate command would
duplicate every filter and still need the flag on `serve`.

Two independent gates: the flag **and** a configured `signer_key`. A key in config does
nothing on its own; the flag without a key is an error naming the exact TOML to add.

### Filters

Repeatable, shared by `status` and `serve`. A filter matching nothing is an error listing what
is configured — printing an empty table would let a typo'd label read as "this is fine".

| Flag | Selects |
|---|---|
| `--account <label\|addr>` | One account, by label or address |
| `--group <name>` | Every account in a group |
| `--kind <kind>` | One watch kind, e.g. `eth_balance` |
| `--spender <name\|addr>` | Allowances to one spender |

### JSON shape

`--json` on any command. Block provenance first, then data.

```json
{
  "network": "arbitrum-one",
  "chain_id": 42161,
  "block": 508876294,
  "block_timestamp": "2026-09-25T20:42:41Z",
  "source": { "id": "rpc:arbitrum-one", "up": true, "lag_seconds": 0 },
  "counts": { "read": 21, "unreadable": 1 },
  "watches": [
    {
      "watch": "eth_balance",
      "instance": "BillingWallet",
      "read": "ok",
      "summary": "BillingWallet holds 0.0349 ETH",
      "account": "BillingWallet",
      "group": "graph-foundation",
      "address": "0x05F9…",
      "remediation": null,
      "measurements": [
        { "name": "account_eth_balance", "value": 0.0349, "unit": "ETH" }
      ]
    }
  ]
}
```

A failed read carries `"read": "failed"` and an **empty** `measurements` array — never a zero,
which would be indistinguishable from an empty wallet.

## Data sources

### RPC

The only data source in v1.

| Value | Source |
|---|---|
| ETH balance | `eth_getBalance(address, block)` |
| GRT balance | `GraphToken.balanceOf(address)` |
| GRT allowance | `GraphToken.allowance(owner, spender)` |
| GRT decimals | `GraphToken.decimals()` |
| Block number + timestamp | `eth_getBlockByNumber(block)` |
| Chain ID check | `eth_chainId` |

All contract reads for one command are batched through **Multicall3**
(`0xcA11bde05977b3631167028862bE2a173976CA11`, same address on every supported chain) at the
pinned block, so `billing status` over N accounts is a small constant number of round trips
rather than `3N`. A chain that deployed it elsewhere can override the address with a
`Multicall3` key in `[networks.*.contracts]`. Batches are chunked so a large watchlist does not
exceed provider request-size limits. `eth_getBalance` is not multicall-able as a normal call;
it is issued concurrently against the same block number.

Sub-calls are sent with `allowFailure`, so one bad spender or a non-token address costs one
cell rather than the whole readout. This tolerance covers *per-call* failures only; systemic
ones are caught by the state probe above.

When `--block latest` is given, the tool resolves `latest` to a concrete number **once** and
uses that number for every subsequent read. Otherwise a new block landing mid-command would
produce a table that never existed at any height — and the header would name a block that
doesn't match its own rows.

---

## Errors and empty states

- **No accounts configured for this network.** Print the network name, the config path in use,
  and an example `[[billing.<network>.accounts]]` block. Exit 2. Do not print an empty table.
- **`GraphToken` missing for the active network.** Name the network and the required key. If
  `use_defaults = false` is in effect, say so — it is the usual cause, since the shipped
  addresses are gone. Exit 2.
- **Unresolvable name in `allowances`.** Name the offending string and the network, and list
  the contract names and account labels that *are* available there. Exit 2.
- **Name is both a contract and a label.** Report both addresses and refuse to guess. Exit 2.
- **Chain ID mismatch.** Report expected vs actual, naming both networks if the actual chain ID
  is one we know. Exit 3.
- **Historical block on a pruned endpoint.** Report that the block is unavailable from this
  endpoint rather than reporting zero balances. Exit 3.

  Having the block is not the same as having the *state* at it: a non-archive node serves the
  header happily and then fails every balance read. So state availability is probed once, with
  a single cheap read at the pinned block, before anything else is fetched. Without that probe
  these failures arrive as a table full of `!` cells — misleading (the block is fine, the
  endpoint is not) and, worse, *not reproducible*: which cells fail varies run to run, so two
  reads of the same pinned block disagree. One probe turns that into one accurate error naming
  the block and suggesting an archive endpoint. This was found by running the tool against a
  public RPC, not by reasoning about it.
- **Partial multicall failure.** Render the rows that succeeded, mark failed cells `!`,
  summarize what failed on stderr, and still print the block header. A single bad spender must
  not void the whole readout.
- **Zero values are real data.** A zero balance renders `0.00`, never a blank or `—`. Blank is
  reserved for "not configured", `!` for "could not read". Conflating the three is how a
  monitoring tool reports health while an endpoint is quietly failing.
