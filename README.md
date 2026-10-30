# Graph Monitor

`graph-monitor` reads the operational state of [The Graph](https://thegraph.com) protocol from
chain: the balances, allowances and on-chain activity of the wallets that billing, oracles and
infrastructure depend on. It runs once and prints a readout, or runs continuously and exports
Prometheus metrics.

Two rules shape everything it reports:

- **One block, one truth.** Every value in a readout is read at the same block height, and every
  output states that height. Pin it with `--block` to reproduce a readout.
- **Nothing is graded.** The tool holds no thresholds. A reading is a value or it is unreadable;
  whether a value is acceptable is decided in Grafana. The one exception is the optional
  remediation trigger (see [Remediation](#remediation)).

Every value the tool reads comes from RPC. Subgraphs are checked as components of the network,
but nothing is read from them yet.

## Usage

```sh
cargo build --release
./target/release/graph-monitor status
```

| Command | Does |
|---|---|
| `status` | Read every watch and show what it found. `--flat` for one line per watch, `--unreadable-only` for failures only. |
| `config show` | What config resolved to after layering, env expansion and name resolution. |
| `config addresses` | The address book of every network. |
| `serve` | Run a cycle every `--interval` seconds (default 120) and export metrics on `--metrics-port` (default 9090). |

`status` and `serve` take repeatable filters: `--account <label|addr>`, `--group <name>`,
`--kind <kind>` and `--spender <name|addr>`.

Every configured network runs: each network with accounts, and each whose subgraphs have an RPC
to be measured against. Every row says which chain it is on. `--network <name>` narrows a run to
one network. `--block` and `--rpc-url` apply to a single chain, so they need `--network` once more
than one network would run.

Useful global flags: `--network`, `--rpc-url`, `--block <n|latest>`, `--json`, `--config <path>`,
`--no-defaults`, `-v`/`-vv`. Run `graph-monitor --help` for the full list.

| Exit code | Meaning |
|---|---|
| 0 | The cycle ran. Some watches may be unreadable; that is reported, not fatal. |
| 2 | Usage or config error. |
| 3 | A network's RPC failed verification, so that network's watches were not read. A network that runs only for its subgraphs does not count. |

## Docker

```sh
docker run -p 9090:9090 ghcr.io/graphprotocol/graph-monitor:main
```

The image runs `serve` by default. Pass any other command after the image name, for example
`... graph-monitor:main status --json`. Images are published for `linux/amd64` and `linux/arm64`;
see [RELEASE.md](RELEASE.md) for the tags.

## Configuration

The shipped [`config/default.toml`](config/default.toml) is compiled into the binary. It holds the
sources, the Arbitrum One and Gnosis networks, an address book, the default list of watched
accounts, and the subgraphs to check, so the tool runs with no config at all.

The config separates two things:

- **Sources** are what the tool reads from: `[sources.rpc.<network>]` and
  `[sources.subgraph.<network>]`, both keyed by the chain they serve. Each is verified every cycle before anything is read through it; if
  one is down, the tool is blind, which is a problem with the tool rather than with the network. A
  network's RPC is how its watches read, and what its subgraphs are measured against. A subgraph
  source sets exactly one of `id`, `deployment` or `url` (and an optional `label`), and must
  answer from the expected deployment, without indexing errors, within `max_lag` (default 15
  minutes). The same subgraph can also be monitored under `[[subgraphs.*]]`.
- **Everything else** is what the tool watches, each keyed by network: address books
  (`[addresses.<network>]`), accounts, and subgraphs. Chain ids are built in for well-known
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
watch   = ["eth", "grt"]
```

| Environment variable | Same as |
|---|---|
| `GRAPH_MONITOR_CONFIG` | `--config` |
| `GRAPH_MONITOR_NETWORK` | `--network` (run only this network) |
| `GRAPH_MONITOR_RPC_URL` | `--rpc-url` (needs `--network` when several networks run) |
| `GRAPH_MONITOR_METRICS_PORT` | `serve --metrics-port` |

The shipped RPC endpoints are the free public ones, which rate-limit quickly. Point `url` at a real
provider for anything beyond a quick look.

Older configs had a `[networks.<network>]` block holding the RPC settings, chain id, address book and
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
crates/core/   config, RPC access, watches and the engine. No output formatting.
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
