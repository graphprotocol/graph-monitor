# Graph Monitor

`graph-monitor` reads the operational state of [The Graph](https://thegraph.com) protocol: the
balances, allowances and on-chain activity of the wallets that billing, oracles and
infrastructure depend on, the health of the protocol's subgraphs, and protocol-wide signals such
as solo deployments. It runs once and prints a readout, or runs continuously and exports
Prometheus metrics.

Two rules shape everything it reports:

- **One block, one truth.** Every value in a readout is read at the same block height, and every
  output states that height. Pin it with `--block` to reproduce a readout.
- **Nothing is graded.** The tool holds no thresholds. A reading is a value or it is unreadable;
  whether a value is acceptable is decided in Grafana. The one exception is the optional
  remediation trigger (see [Remediation](#remediation)).

## Model: sources, then checks

The tool **reads from sources** and **runs checks**. A source is an input, verified every cycle
before anything reads through it: each network's RPC (`rpc:<network>`) and, optionally, a
subgraph it reads from (`subgraph:<label>`). If a source is down, the tool is blind, and every
check that needed it reports **unreadable**. That is a problem with the tool, not with the
network.

Every check has a **subject**: the thing it is about. A check either reads its subject and
reports measurements (and, for subgraph checks, a fault when it judges the subject unhealthy), or
it could not read and says why. Could not read is never zero.

| Check | Subject | Reads from | Measurements (labels) | Repairs |
|---|---|---|---|---|
| ETH balance | account | rpc | `gm_account_eth_balance` (network, group, account, address) | yes |
| GRT balance | account | rpc | `gm_account_grt_balance` | yes |
| Activity | account | subgraph source, falling back to rpc | `gm_account_idle_seconds`, `gm_account_activity_seen` | – |
| Allowance | owner → spender pair | rpc | `gm_allowance_grt`, `_unlimited`, `_exact_max` (network, group, owner, spender) | – |
| Subgraph health | subgraph / deployment | the subgraph itself (gateway or url) | `gm_subgraph_up`, `_block_height`, `_blocks_behind`, `_lag_seconds` (network, subgraph, deployment) | – |
| Solo deployments | network, measured per indexer | subgraph source | `gm_indexer_solo_deployments`, `gm_indexer_solo_signal_grt` (network, indexer) | – |

Subjects are declared where they live in config: `[[accounts.<network>]]` expands into account
and allowance checks, `[[subgraphs.<network>]]` into subgraph checks, and
`[checks.<network>]` turns on checks about the protocol as a whole. Whether each check could
read is exported per subject kind as `gm_<subject>_readable`, and what is wrong as
`gm_<subject>_fault{kind}`. See [`grafana/`](grafana/README.md#metrics).

## Usage

```sh
cargo build --release
./target/release/graph-monitor status
```

| Command | Does |
|---|---|
| `status` | Run every check and show what it found. `--flat` for one line per check, `--flat --problems` for only the unreadable and faulty ones. |
| `config show` | What config resolved to after layering, env expansion and name resolution. |
| `config addresses` | The address book of every network. |
| `serve` | Run a cycle every `--interval` seconds (default 120) and export metrics on `--metrics-port` (default 9090). |

`status` and `serve` take repeatable filters: `--account <label|addr>`, `--group <name>`,
`--kind <kind>` and `--spender <name|addr>`. Kinds are `eth_balance`, `grt_balance`,
`activity`, `allowance`, `subgraph` and `solo_deployments`.

Every configured network runs: each network with accounts, and each whose subgraphs have an RPC
to be measured against. Every row says which chain it is on. `--network <name>` narrows a run to
one network. `--block` and `--rpc-url` apply to a single chain, so they need `--network` once more
than one network would run.

Useful global flags: `--network`, `--rpc-url`, `--block <n|latest>`, `--json`, `--config <path>`,
`--no-defaults`, `-v`/`-vv`. Run `graph-monitor --help` for the full list.

| Exit code | Meaning |
|---|---|
| 0 | The cycle ran. Some checks may be unreadable; that is reported, not fatal. |
| 2 | Usage or config error. |
| 3 | A network's RPC failed verification, so that network's account checks were not read. A network that runs only for its subgraphs does not count. |

## Docker

```sh
docker run -p 9090:9090 ghcr.io/graphprotocol/graph-monitor:main
```

The image runs `serve` by default. Pass any other command after the image name, for example
`... graph-monitor:main status --json`. Images are published for `linux/amd64` and `linux/arm64`;
see [RELEASE.md](RELEASE.md) for the tags.

## Configuration

The shipped [`config/default.toml`](config/default.toml) is compiled into the binary. It holds the
sources, the Arbitrum One and Gnosis networks, an address book, the default list of
accounts, and the subgraphs to check, so the tool runs with no config at all.

The config separates two things:

- **Sources** are what the tool reads from: `[sources.rpc.<network>]` and
  `[sources.subgraph.<network>]`, both keyed by the chain they serve. Each is verified every cycle before anything is read through it; if
  one is down, the tool is blind, which is a problem with the tool rather than with the network. A
  network's RPC is how its account checks read, and what its subgraphs are measured against. A subgraph
  source sets exactly one of `id`, `deployment` or `url` (and an optional `label`), and must
  answer from the expected deployment, without indexing errors, within `max_lag` (default 15
  minutes). The same subgraph can also be monitored under `[[subgraphs.*]]`.

  A subgraph source answers activity checks. If it carries generic `Activity` entities (id
  `<emitter>-<topic0>-<sender>`), every account with an `activity` block on that network reads its
  last-seen record in one query per cycle, instead of scanning logs. The tool knows nothing about
  which events a subgraph indexes: an event with no "any sender" record is not indexed, and that
  check searches the chain instead. So do `match = "to"` checks, and every activity check while
  the source is down. A record older than the account's `lookback` still reports its idle time,
  but does not count as seen. Network checks such as `solo_deployments` read it too, and have no
  fallback: while it is down they are unreadable.
- **Everything else** declares what the tool checks, each keyed by network: address books
  (`[addresses.<network>]`), accounts, subgraphs and network checks. Chain ids are built in for well-known
  networks (mainnet, sepolia, arbitrum-one, arbitrum-sepolia, gnosis); any other network sets
  `chain_id` in its `[sources.rpc.<network>]`, and every RPC is checked against it.

Layers, later wins:

1. Shipped defaults (`config/default.toml`)
2. `$XDG_CONFIG_HOME/graph-monitor/config.toml` (usually `~/.config/graph-monitor/config.toml`)
3. `./graph-monitor.toml`
4. Environment variables
5. Command-line flags

Merging is per key: a user config that sets one RPC `url` keeps the shipped `max_lag`, and user
accounts are added to the shipped list. A user account with the same label as a shipped one
replaces it, and `enabled = false` drops it. `use_defaults = false` (or `--no-defaults`) ignores
the shipped layer completely. `${VAR}` in any string value is expanded from the environment, so
API keys stay out of the file.

```toml
[sources.rpc.arbitrum-one]
url             = "${ARBITRUM_ONE_RPC_URL}"
max_lag         = "60s"   # how stale the head may be before the source is called down
max_concurrency = 6       # requests in flight
max_log_range   = 10000   # largest eth_getLogs span to try

[[accounts.arbitrum-one]]
address = "0x..."
label   = "MyWallet"
group   = "infra"
checks  = ["eth", "grt"]    # also "activity"; a remediate or activity block implies its check
```

| Environment variable | Same as |
|---|---|
| `GRAPH_MONITOR_CONFIG` | `--config` |
| `GRAPH_MONITOR_NETWORK` | `--network` (run only this network) |
| `GRAPH_MONITOR_RPC_URL` | `--rpc-url` (needs `--network` when several networks run) |
| `GRAPH_MONITOR_METRICS_PORT` | `serve --metrics-port` |

The shipped RPC endpoints are the free public ones, which rate-limit quickly. Point `url` at a real
provider for anything beyond a quick look.

Older configs named an account's checks `watch = [...]`; that key now fails with an error saying
it is `checks`. Older configs also had a `[networks.<network>]` block holding the RPC settings, chain id, address book and
signer key. A config still using it fails with an error mapping each field to its new place.

## Subgraphs

Subgraphs are part of the network being monitored, like account balances. The RPC is different:
it is how the tool sees, so the RPC being down means the tool is blind, while a subgraph being down
is a finding about the network.

Each configured subgraph is checked every cycle with one `_meta` query: does it answer, and how far
is its latest indexed block behind the chain head. It is reported **down** when it does not answer,
serves a different deployment than the one asked for, reports indexing errors, or trails the clock
by more than `max_lag` (default 15 minutes). It is reported **not checked** when the problem is on
our side, such as a missing or rejected API key; that says nothing about the subgraph.

List subgraphs under the network they index. Every subgraph is checked on every run, whichever chain
it indexes, since checking one needs only the gateway. Blocks behind is measured only for subgraphs
on the chain the tool's RPC serves; for others it is left blank rather than measured against the
wrong chain. Gateway queries send the API key as a bearer token.

```toml
[gateway]
api_key = "${GRAPH_API_KEY}"

[[subgraphs.arbitrum-one]]
label       = "network"
id          = "<subgraph id>"      # or url = "https://…" for a graph-node
deployments = ["Qm…", "Qm…"]       # optional, each also checked on its own
max_lag     = "15m"
```

The `id` check follows whatever version is published. Each entry in `deployments` is checked
separately, which covers an upgrade: the new version indexing while the old one still serves.

## Network checks

Some checks are about the protocol on a network rather than any one account or subgraph. They are
turned on per network and read that network's subgraph source, so one must be configured.

```toml
[checks.arbitrum-one]
solo_deployments = true
```

**Solo deployments** counts, per indexer, the deployments it alone is allocated to that have
curation signal and are not denied rewards, and the GRT signalled on them. It reads the
network-monitoring subgraph's `Indexer.soloCount` and `soloSignalledTokens`, so the source must
be a version with solo tracking; an older one makes the check unreadable with a message saying
so. No indexer is excluded: the upgrade indexer is expected to have many, and filtering it is a
choice for the dashboard.

## Remediation

An account can carry a `remediate` block that tops up its ETH balance when it falls below a
trigger. It needs two things on purpose: the `--remediate` flag **and** a `signer_key` in config.
A key alone does nothing; the flag without a key is an error naming the TOML to add. `--dry-run`
forms the repair and logs it without broadcasting. Nothing ships enabled.

```toml
[remediation.arbitrum-one]
signer_key = "${GRAPH_MONITOR_SIGNER_KEY}"

[accounts.arbitrum-one.eth.remediate]
below     = "0.05"
top_up_by = "0.1"
from      = "FoundationBillingMultisig"
```

## Monitoring

[`grafana/`](grafana) has a dashboard to import, the list of exported `gm_*` metrics, and starting
points for alert rules. Scrape `:9090/metrics` rather than pushing, so a dead process shows up as
`up == 0`.

## Repository layout

```
crates/core/   config, sources, checks and the engine. No output formatting.
crates/cli/    the graph-monitor binary: command tree, table/JSON rendering, metrics server.
config/        shipped default config, embedded at build time.
grafana/       dashboard and metrics reference.
specs/         design notes: CLI_SPEC.md, CHECKS_PLAN.md, COMPONENTS.md.
docker/        image build.
```

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

CI runs the same three checks. Commits must follow [Conventional Commits](https://www.conventionalcommits.org);
releases and images are automated, see [RELEASE.md](RELEASE.md).
