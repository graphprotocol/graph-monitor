# Grafana

`graph-network-dashboard.json` — import via **Dashboards → New → Import → Upload JSON**, then
pick your Prometheus data source.

Feed it with:

```
graph-network serve --interval 120 --metrics-port 9090
```

and scrape `:9090/metrics`. Use a **scrape** target rather than a push gateway: `up == 0` then
tells you the process died. A pushed metric persists at its last value after the job stops, so
a dead exporter looks perfectly healthy.

## Reading it

The tool holds no thresholds and grades nothing — it reports what it read. Whether a value is
acceptable is decided here. Three distinctions the panels preserve, because collapsing them is
how a monitor lies:

| | Means | Panel |
|---|---|---|
| a value | read successfully | balance / allowance panels |
| **unreadable** | the tool could not see | *Unreadable* stat, *Read failures* |
| **not seen** | read fine, no activity in the window | *Not seen in the lookback* |

A blank cell in *Accounts now* means that asset is not watched for that account — not that it
is zero. A failed read publishes **no** value at all, so an outage can never be mistaken for an
empty wallet.

**Use the `Unlimited` column, not `Raw GRT`.** An unlimited approval is ~`1.157e59`; a float has
discarded forty digits by then, so comparing the raw gauge against `2^255` gives wrong answers.
`gn_account_allowance_unlimited` is computed on exact integers.

## Alert rules

None ship yet. These are the queries worth starting from — keep them in git next to this file
rather than defining them in the UI, so a rule and the config it depends on review together.

**Deadman — the exporter stopped.** `up` catches a dead process; this catches one that is alive
but wedged, which is the failure that otherwise goes unnoticed.

```promql
up{job="graph-network"} == 0
time() - gn_cycle_last_success_timestamp > 600
```

**Blind, rather than unhealthy.** Alert separately from value alerts: the fix is an endpoint,
not a wallet.

```promql
gn_read_unknown == 1          # for: 15m
gn_source_up == 0             # for: 5m
```

**Low balances.** Thresholds live here, not in the tool.

```promql
gn_account_eth_balance{account="NetworkMonitor"} < 0.3
gn_account_grt_balance{account="GraphTallyWallet"} < 25000
```

**Better than a threshold** — a static floor is a proxy for the question you actually care
about:

```promql
predict_linear(gn_account_eth_balance[6h], 4*3600) < 0
```

**An oracle has gone quiet.** `activity_seen == 0` means nothing in the lookback at all;
`idle_seconds` lets you alert earlier at a level you choose.

```promql
gn_account_activity_seen == 0
gn_account_idle_seconds{account=~"SamOracle.*"} > 86400
```

**An approval was revoked.** Breaks a money path silently — nothing else changes.

```promql
gn_account_allowance_unlimited{owner="BillingWallet", spender="NetworkMonitor"} == 0
```

**Auto-fix is broken.** More urgent than the balance that triggered it.

```promql
increase(gn_remediation_total{outcome="failed"}[1h]) > 0
```

**The tool is falling behind.** Cycles start skipping ticks once p95 approaches the interval.

```promql
histogram_quantile(0.95, sum by (le) (rate(gn_cycle_duration_seconds_bucket[15m]))) > 100
```

## Metrics

| Metric | Type | Notes |
|---|---|---|
| `gn_account_eth_balance` | gauge | ETH, whole units |
| `gn_account_grt_balance` | gauge | GRT, whole units |
| `gn_account_allowance_grt` | gauge | Lossy above 2^53 — see above |
| `gn_account_allowance_unlimited` | gauge | 1/0, exact |
| `gn_account_allowance_exact_max` | gauge | 1/0, exactly `uint256::max` — never drawn against |
| `gn_account_idle_seconds` | gauge | Since last activity, at the pinned block |
| `gn_account_activity_seen` | gauge | 1/0 within the lookback |
| `gn_account_ever_transacted` | gauge | Nonce watches only, where "never" is knowable |
| `gn_account_remediation_below` | gauge | Configured top-up trigger, exported whether or not it fires |
| `gn_read_ok` / `gn_read_unknown` | gauge | 1/0 per watch |
| `gn_read_errors_total` | counter | Failed reads by watch kind |
| `gn_read_duration_seconds` | histogram | Per watch kind |
| `gn_source_up` / `_lag_seconds` / `_block_height` | gauge | Stage 0 |
| `gn_block_height` | gauge | Height every value in the cycle was read at |
| `gn_cycle_last_success_timestamp` | gauge | Deadman |
| `gn_cycle_duration_seconds` | histogram | |
| `gn_remediation_total` | counter | By `outcome`: sent, would_send, skipped, failed |
| `gn_build_info` | gauge | Always 1, carries `version` |

Block height rides as its own gauge rather than as a sample timestamp: Prometheus stamps
samples by scrape time and rejects backdated ones, so correlate against `gn_block_height` when
a value looks wrong.
