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

All data comes from RPC. There is no subgraph or explorer dependency.

## Usage

```sh
cargo build --release
./target/release/graph-monitor status
```

| Command | Does |
|---|---|
| `status` | Read every watch and show what it found. `--flat` for one line per watch, `--unreadable-only` for failures only. |
| `config show` | What config resolved to after layering, env expansion and name resolution. |
| `config addresses` | The address book for the active network. |
| `serve` | Run a cycle every `--interval` seconds (default 120) and export metrics on `--metrics-port` (default 9090). |

`status` and `serve` take repeatable filters: `--account <label|addr>`, `--group <name>`,
`--kind <kind>` and `--spender <name|addr>`.

Useful global flags: `--network`, `--rpc-url`, `--block <n|latest>`, `--json`, `--config <path>`,
`--no-defaults`, `-v`/`-vv`. Run `graph-monitor --help` for the full list.

| Exit code | Meaning |
|---|---|
| 0 | The cycle ran. Some watches may be unreadable; that is reported, not fatal. |
| 2 | Usage or config error. |
| 3 | The RPC source failed verification, so nothing could be read. |

## Docker

```sh
docker run -p 9090:9090 ghcr.io/graphprotocol/graph-monitor:main
```

The image runs `serve` by default. Pass any other command after the image name, for example
`... graph-monitor:main status --json`. Images are published for `linux/amd64` and `linux/arm64`;
see [RELEASE.md](RELEASE.md) for the tags.

## Configuration

The shipped [`config/default.toml`](config/default.toml) is compiled into the binary. It holds the
Arbitrum One network, an address book, and the default list of watched accounts, so the tool runs
with no config at all.

Layers, later wins:

1. Shipped defaults (`config/default.toml`)
2. `$XDG_CONFIG_HOME/graph-monitor/config.toml` (usually `~/.config/graph-monitor/config.toml`)
3. `./graph-monitor.toml`
4. Environment variables
5. Command-line flags

Merging is per key: a user config that sets one `rpc_url` keeps every shipped address, and user
accounts are added to the shipped list. A user account with the same label as a shipped one
replaces it, and `enabled = false` drops it. `use_defaults = false` (or `--no-defaults`) ignores
the shipped layer completely. `${VAR}` in any string value is expanded from the environment, so
API keys stay out of the file.

```toml
[networks.arbitrum-one]
rpc_url = "${ARBITRUM_ONE_RPC_URL}"

[[accounts.arbitrum-one]]
address = "0x..."
label   = "MyWallet"
group   = "infra"
watch   = ["eth", "grt"]
```

| Environment variable | Same as |
|---|---|
| `GRAPH_MONITOR_CONFIG` | `--config` |
| `GRAPH_MONITOR_NETWORK` | `--network` |
| `GRAPH_MONITOR_RPC_URL` | `--rpc-url` |
| `GRAPH_MONITOR_METRICS_PORT` | `serve --metrics-port` |

The shipped RPC endpoint is the free public one, which rate-limits quickly. Point `rpc_url` at a
real provider for anything beyond a quick look.

## Remediation

An account can carry a `remediate` block that tops up its ETH balance when it falls below a
trigger. It needs two things on purpose: the `--remediate` flag **and** a `signer_key` in config.
A key alone does nothing; the flag without a key is an error naming the TOML to add. `--dry-run`
forms the repair and logs it without broadcasting. Nothing ships enabled.

```toml
[networks.arbitrum-one]
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
