# Check Engine Plan

How `graph-monitor` grows from a snapshot CLI into the tool that replaces `network-monitor`.

## Goal

**Deprecate `network-monitor`.** One tool that answers "what is going on" interactively *and*
runs continuously, judging protocol state against expectations, optionally correcting it, and
reporting to console and metrics.

The snapshot view and the monitor are the same code. That is the whole point: the thing that
pages you at 3am and the thing you run to investigate it must not be two implementations of
the same expectation, or they drift. They already have — `network-monitor/config.yaml` and our
`config/default.toml` currently watch the same three wallets at the same three thresholds, by
hand, in two languages.

### Scope of this plan

This plan started with **account checks only**: GRT balance, ETH balance, GRT allowances,
account activity. The engine has since taken on subgraph health and the first network-wide
check (solo deployments) without changing shape; see [the model](#model-sources-then-checks).

Everything else `network-monitor` does — supply/issuance, inflation, bridge retryables, block
drift, subgraph delta, allocated stake, the issuance keeper — is out of scope here and gets
its own plan once the engine has proven itself on accounts. Accounts are the right first
domain: they are already half-built, they are pure RPC, and they are the checks most
duplicated between the two tools today.

### Non-goals

- **Any alerting sink in the tool.** The tool never pages. It exports metrics; Grafana alerts
  on them. See [Decision 8](#decisions).
- Config-defined check *logic*. Checks are Rust; config supplies parameters.
- Porting `network-monitor`'s non-account checks.

---

## Model: sources, then checks

The tool **reads from sources** and **runs checks**. Sources (`rpc:<network>`,
`subgraph:<label>`) are inputs: verified in stage 0, and if one is down the tool is blind there.
Everything else is a check, and every check has a **subject**: the thing it is about.

| Check | Subject | Reads from | Measurements (labels) | Repairs |
|---|---|---|---|---|
| ETH balance | account | rpc | `gm_account_eth_balance` (network, group, account, address) | yes |
| GRT balance | account | rpc | `gm_account_grt_balance` | yes |
| Activity | account | subgraph source, falling back to rpc | `gm_account_idle_seconds`, `gm_account_activity_seen` | – |
| Allowance | owner → spender pair | rpc | `gm_allowance_grt`, `_unlimited`, `_exact_max` (network, group, owner, spender) | – |
| Subgraph health | subgraph / deployment | the subgraph itself (gateway or url) | `gm_subgraph_up`, `_block_height`, `_blocks_behind`, `_lag_seconds` (network, subgraph, deployment) | – |
| Solo deployments | network, measured per indexer | subgraph source | `gm_indexer_solo_deployments`, `gm_indexer_solo_signal_grt` (network, indexer) | – |

What follows from the subject:

- **Labels.** A series' labels are its subject's identity, so every series about one account
  joins with every other, whichever check produced it. Labels are never shaped around one kind
  of subject: that was the old `Labels` struct, account-shaped, which subgraph checks could not
  fit and so lived outside the engine.
- **Readability.** A check either read its subject or it could not. Could not read is one
  concept with a reason (`read_failed`, `source_unavailable`, `not_configured`, `key_rejected`),
  exported as `gm_<subject>_readable` and `gm_<subject>_fault{kind}`. "Not checked" for a
  subgraph is the same thing as "unreadable" for a balance.
- **Faults.** A check that read its subject may judge it unhealthy (a subgraph down). That is a
  fault on a successful reading, in the same `gm_<subject>_fault` family.
- **Config.** Subjects are declared where they live: `[[accounts.<network>]]` expands into
  account and allowance checks, `[[subgraphs.<network>]]` into subgraph checks, and
  `[checks.<network>]` turns on checks about the protocol as a whole. "Account" is a config
  grouping, not a kind of check.
- **Repair.** Only balance checks repair; the subject being an account is what makes that
  possible, not the check kind.

A network check's subject is the network; its measurements may name finer subjects (each
indexer, for solo deployments). In code: `check::Subject`, `check::Reading`, `check::Unreadable`.

---

## Architecture: five stages

```
   VERIFY          READ            OBSERVE         REMEDIATE         REPORT
   ──────          ────            ───────         ─────────         ──────
   sources are     I/O, batched    pure function   optional, gated   console │ metrics
   live & fresh    pinned block    no network      writes on-chain   exit code

   once per cycle  ─── per check ─────────────────────────────────►  once per cycle
```

Nothing here decides whether a value is acceptable. `OBSERVE` turns a reading into
measurements and a sentence; thresholds and alerting live in Grafana (decision 8). The single
exception is `REMEDIATE`, which holds a trigger because repair must survive the metrics
pipeline being down.

### Execution model

Per cycle (or per `check run` invocation):

1. **VERIFY** runs once, for every configured data source.
2. **Pin** a block per network — resolved once, used by every read in the cycle.
3. **READ → OBSERVE** runs per check, concurrently.
4. **REMEDIATE** runs after every observation exists, **serialised per signer**.
5. **REPORT** runs once, over the whole cycle.

Three consequences worth stating, because each is a decision:

**Remediation is not folded into the per-check loop.** It runs after observation completes,
because nonce safety requires serialising every transaction a signer sends. Interleaving would
either serialise the reads too, or reintroduce the nonce races `network-monitor` avoids by
convention.

**Reported measurements are pre-remediation.** They describe the pinned block — the state that
*caused* the action. A top-up lands after that block, so the corrected balance appears next
cycle. Reporting post-remediation values would mean publishing a state no block ever had.

**One block per network per cycle, even on partial failure.** A failed read is unreadable *at
that same block*; the cycle never re-pins to paper over a failure.

### Batching: declared reads with a live fallback

*(This is where the implementation diverged from the original plan, deliberately.)*

The plan called for a transparently coalescing `ReadCtx`. What shipped is a **declared plan
plus a fallback**: checks implement `fn plan(&self, &mut ReadPlan)` listing what they need, the
engine executes the whole plan in one Multicall3 batch, and `read()` then pulls from the
result. A check that declares nothing still works — it issues its own call.

The change was forced by a real bug. With a transparent fallback on every cache miss, a flaky
endpoint produced **different answers on successive runs at the same pinned block**: a read
that failed in the batch would silently succeed on retry, or not. So a read that *was* planned
and failed now stays failed; only undeclared reads fall back. One attempt each, deterministic.

The activity check is the one that genuinely cannot declare its reads — its search is adaptive
— and it uses the fallback path by design.

```rust
pub trait Check: Send + Sync {
    type Reading: Send + Sync;

    fn id(&self) -> CheckId;
    fn labels(&self) -> Labels;
    fn plan(&self, _plan: &mut ReadPlan) {}

    /// READ — all I/O, at the pinned block.
    async fn read(&self, ctx: &ReadCtx<'_>) -> Result<Self::Reading, String>;

    /// OBSERVE — measurements and a sentence. Pure. Judges nothing.
    fn observe(&self, reading: &Self::Reading) -> Observation;

    /// REMEDIATE — does this call for repair? Pure; a shared executor performs it.
    fn remediation(&self, _reading: &Self::Reading) -> Option<RemediationIntent> { None }
}

/// Either it observed something, or it could not read. Unreadable is kept out of the value
/// space entirely, so an endpoint outage can never look like a wallet at zero.
pub struct CheckOutcome {
    pub observation: Result<Observation, String>,
    pub remediation: Option<RemediationIntent>,
    /* … */
}
```

### Stage 0: verify the sources

Before anything is read, every configured data source must prove it is **reachable, the right
chain, and current**. A check is only as trustworthy as the source under it, and a stale source
produces confidently wrong answers rather than obvious failures — a subgraph 40,000 blocks
behind will happily report a balance that was true last week.

Per source, stage 0 asserts:

| Source | Assertions |
|---|---|
| RPC | reachable; `eth_chainId` matches config; head block advancing; head timestamp within `max_lag` of wall clock |
| Subgraph | reachable; correct deployment (when pinned); no indexing errors; `_meta.block` timestamp within `max_lag` of wall clock |

Implemented in `crates/core/src/source.rs`; subgraph sources are configured under
`[sources.subgraph.<network>]` and verified with the same check as monitored subgraphs, read as a
source verdict (a subgraph that cannot be queried at all counts as down). Sources are verified
before any check reads. A verified subgraph source answers activity checks from its generic
`Activity` entities; when it is down, or does not index an event, those checks fall back to the
RPC log search, so a subgraph outage costs speed and reach, never correctness. Subgraphs that are merely
*monitored* (network, analytics, EBO, QoS) are components, not sources: they are checked in
`crates/core/src/subgraph.rs` and reported beside balances, and a failure there is a finding about
the network rather than a blind spot in the tool. Both roles share the same `_meta` query and
verdict. Lag is judged in time rather than blocks: a time limit means the same thing on every
chain, and a block limit would have to be retuned per chain's block rate. A fourth fault, `StateUnavailable`, was added
during implementation: a non-archive endpoint serves headers happily and then refuses every
balance read at a historical block. Those failures are classified by error text and promoted to
a single source fault, rather than scattering as unrelated unreadable cells.

**A failed source does not fail the cycle.** Checks that depend on it report unreadable, with
the source failure as the reason; checks on healthy sources run normally. The source itself reports
its own verdict, so "my RPC is 10 minutes stale" is a first-class alertable condition rather
than a pile of mysterious `Unknown`s.

```
gm_source_up{source,network}              gauge  1/0
gm_source_lag_seconds{source,network}     gauge
gm_source_block_height{source,network}    gauge
```

**This stage absorbs three `network-monitor` checks rather than porting them later.**
`testArbBlockProduction` (is the chain producing blocks), `testBlockDrift` (L1/L2 head drift),
and `testNetworkSubgraphDelta` (subgraph behind chain head) are all source-freshness
assertions wearing a check costume. Making freshness a stage means they arrive as configuration
of stage 0, not as three more check implementations.

**Why `observe` and `remediation` must be pure.** A `network-monitor` test does read and decide
in one function, so asserting "0.049 ETH against a 0.05 trigger fires" means standing up a fake
provider. Here both are function calls on plain data — the remediation trigger, the one rule
that can move money, is tested by passing it a literal.

**Why `read` is separate.** Declared reads can be batched. Twenty account checks issue one
Multicall3 batch at one pinned block rather than twenty round trips — the machinery already
exists in `crates/core/src/rpc.rs`.

**`Status::Unknown` is not a breach.** A failed read means *we could not see*, which is
different from *what we saw was bad*. This is the same distinction the renderer already draws
between `0.00` and `!`, promoted to the verdict. `network-monitor` collapses read failures into
a P2 `-error` alert; separating them lets you route "the chain is unhealthy" and "my RPC is
unhealthy" differently, which you will want at 3am.

**Measurements, not stringified details.** `TestResult.details` is `Record<string, string>` —
fine for a page, useless for a time series. A `Measurement` carries a name, an `f64`, a unit,
and labels. `Amount` (raw base units + decimals, never floats) stays the internal
representation; the `f64` projection happens only at the metrics edge, because Prometheus
speaks nothing else.

### Stage 3: remediation

Opt-in per check, and only with an explicitly supplied signer. The read-only path stays the
default — `check run` can never write, regardless of config; only `serve` can, and only with
`--remediate` plus a signer in the environment.

The operational rules `network-monitor` learned the hard way move into **one shared executor**
rather than being a convention each check re-implements:

- Explicit `gas_limit` with a 100% buffer, never an unpadded estimate. Arbitrum prices L1
  calldata posting as extra L2 gas off a moving base-fee estimate, so an exact limit gets
  rejected as `intrinsic gas too low` the moment that estimate ticks up.
- Serial execution per signer. No `Promise.all` equivalent after the broadcast point; local
  nonce races are avoided by construction rather than by a nonce manager.
- Success is silent. Failure is recorded as `gm_remediation_total{outcome="failed"}` and logged
  at error level — "auto-fix is broken" is the condition worth paging on, and Grafana grades it.
- A transaction that is mined but **reverts** counts as failure. It succeeded at the RPC layer
  and did nothing, which is precisely the case a naive implementation reports as success.
- `--dry-run` logs the intended action and falls back to reporting the breach.

Centralizing these is the main argument that this is a rewrite worth doing rather than a port:
the rules become enforced by construction instead of by code review.

### Stage 4: reporting

| Sink | Mode | Purpose |
|---|---|---|
| Console table | `check run` | The snapshot you already have, now with verdicts |
| JSON | `--json` | Scripting |
| Prometheus | `serve` | Grafana |
| Exit code | `check run` | Worst severity, for cron and CI |

---

## Run modes

```
graph-monitor check run [--filter <id>] [--block <n>]     # one-shot, console, exits
graph-monitor serve --interval 2m --metrics-port 9090     # loop, /metrics, optional remediation
```

One binary, two modes, one set of checks. `--block` gives the engine something
`network-monitor` structurally cannot do: **re-run a past alert at the block it fired on.**

### `serve`, following `escrow_manager`

The pattern is settled in `graph-tally/crates/bin/escrow_manager` and should be copied rather
than reinvented:

- `tokio::time::interval` with `MissedTickBehavior::Skip` — an overrunning cycle skips rather
  than queueing a backlog.
- `axum` `/metrics` on a **separate port** from anything else, spawned as its own task,
  serving `prometheus::gather()` through `TextEncoder`.
- A `lazy_static` global `METRICS` struct: `Gauge` for aggregates, `GaugeVec` labelled
  per-account, `Histogram` for cycle duration, ok/err `IntCounter` pairs per action.

One deviation: `escrow_manager`'s `ResponseMetrics::new` calls `.inc()` on both counters to
force the series into existence. That starts every counter at 1, which skews `rate()` and makes
zero errors look like one. Use `inc_by(0)` — same materialization, no skew.

---

## Metrics design

Prefix `gm_`. Every series carries `network`; the rest of its labels are its check's subject:
account (`group`, `account`, `address`), allowance (`group`, `owner`, `spender`), subgraph
(`subgraph`, `deployment`), indexer (`indexer`).

```
# ---- account checks ------------------------------------------------------------------
gm_account_eth_balance{network,group,account,address}        gauge
gm_account_grt_balance{network,group,account,address}        gauge
gm_account_idle_seconds{network,group,account,address}       gauge
gm_account_activity_seen{network,group,account,address}      gauge  1/0
gm_account_ever_transacted{network,group,account,address}    gauge  1/0

# ---- allowance checks (exact classifications f64 cannot carry) -----------------------
gm_allowance_grt{network,group,owner,spender}                gauge
gm_allowance_unlimited{network,group,owner,spender}          gauge  1/0
gm_allowance_exact_max{network,group,owner,spender}          gauge  1/0

# ---- subgraph checks -----------------------------------------------------------------
gm_subgraph_up{network,subgraph,deployment}                  gauge  1/0
gm_subgraph_block_height{network,subgraph,deployment}        gauge
gm_subgraph_blocks_behind{network,subgraph,deployment}       gauge
gm_subgraph_lag_seconds{network,subgraph,deployment}         gauge

# ---- network checks ------------------------------------------------------------------
gm_indexer_solo_deployments{network,indexer}                 gauge
gm_indexer_solo_signal_grt{network,indexer}                  gauge

# ---- readability and faults, per subject kind ----------------------------------------
gm_<subject>_readable{<subject labels>,check}                gauge  1/0
gm_<subject>_fault{<subject labels>,check,kind}              gauge  1, absent while healthy
gm_check_errors_total{network,check}                         counter
gm_check_duration_seconds{network,check}                     histogram

# ---- source health -------------------------------------------------------------------
gm_source_up{network,source}                                 gauge  1/0
gm_source_lag_seconds{network,source}                        gauge
gm_source_block_height{network,source}                       gauge
gm_source_fault{network,source,kind}                         gauge  1

# ---- remediation ---------------------------------------------------------------------
gm_account_remediation_below{network,group,account,asset}    gauge
gm_remediation_total{network,account,asset,outcome}          counter

# ---- liveness and provenance ---------------------------------------------------------
gm_block_height{network}                                     gauge
gm_cycle_last_success_timestamp                              gauge
gm_cycle_duration_seconds                                    histogram
gm_build_info{version}                                       gauge
```

`group` as a label is what makes "are any oracle wallets low" a single query rather than a
hand-maintained list of account names in a Grafana panel.

Three deliberate choices:

**No `gm_check_breach`, no severity label, no alert thresholds.** Those were artefacts of the
tool judging. Grafana holds the thresholds now; the tool publishes what it saw.

**Remediation thresholds *are* exported** — not as alert thresholds, but so a dashboard can
draw the level at which auto-top-up kicks in, and so Grafana can alert on the genuinely
interesting condition: balance below the remediation trigger *and still falling*, meaning the
auto-fix is not keeping up. That is a question neither system can answer alone.

**Block height is its own gauge, not a sample timestamp.** Our provenance rule wants the block
to travel with every value, but Prometheus timestamps samples by scrape wall-clock and rejects
out-of-order ones — backdating samples to block time would break ingestion. So the block is
published alongside, and `gm_block_height` is what you correlate against when a value looks
wrong. This is the one place the "one block, one truth" principle has to bend to the sink.

`gm_<subject>_readable` being separate from any value is what lets you alert on "this check
has been blind for 10 minutes" — a condition the current monitor reports as a low-severity
error and mostly ignores.

---

## Config redesign

Not `network-monitor`'s shape. Its config repeats a `thresholds` block and a `remediation`
block per wallet; ten oracle wallets at the same threshold means ten copies, and the wallet
address appears as a bare hex literal each time.

Three principles:

1. **Addresses are named once**, in the address book, and referenced everywhere else.
2. **An account is described in one place** — all of its checks together, because that is how
   you think about it and how you audit it.
3. **Every expectation is declared where it applies.** Nothing is inherited.

### Groups

A group is **a label and nothing more**. It marks accounts as related — the billing wallets,
the oracles — so they can be displayed together, filtered together, and queried together. It
carries no expectations, no defaults, and no behavior.

That keeps one rule true: **everything configured for an account is written on that account.**
Reading one block tells you exactly what the tool will do about that wallet, with no lookup
elsewhere and no precedence to reason about. The cost is repetition when ten wallets share a
setting; the gain is that config cannot surprise you, and nothing can be armed implicitly by
dropping a wallet into a group. If the repetition becomes painful, inheritance can be added
later — going the other way, removing it once people rely on it, is much harder.

Config declares **what to check** and **when to repair**. It declares no alert thresholds and
no severities: those live in Grafana rules.

```toml
# Checked, nothing more. Its balances are exported; Grafana decides what is too low.
[[accounts.arbitrum-one]]
address = "EpochBlockOracle"
group   = "oracles"
checks  = ["eth", "activity"]

# Checked, and repaired when ETH falls below the trigger.
[[accounts.arbitrum-one]]
address = "UpgradeIndexerCurationBot"
group   = "oracles"
checks  = ["eth"]

  [accounts.arbitrum-one.eth.remediate]
  below     = "0.001"                # the only threshold the tool holds
  top_up_by = "0.002"
  from      = "FoundationBillingMultisig"

[[accounts.arbitrum-one]]
address = "GraphTallyWallet"
group   = "billing"
checks  = ["eth", "grt", "activity"]

  [accounts.arbitrum-one.grt.remediate]
  below     = "25000"
  top_up_by = "30000"
  from      = "BillingWallet"

  # Allowances are checked by naming the spender. No `expect` — the tool exports the value
  # and the exact `unlimited` classification, and Grafana asserts whatever it wants.
  [[accounts.arbitrum-one.allowances]]
  spender = "NetworkMonitor"
```

**`checks` replaces the threshold lists.** Previously an account was checked for ETH because it
had an `eth.min`; with thresholds gone, something still has to say which assets to read. Being
explicit also keeps the read plan tight — no point fetching GRT for a wallet that only ever
holds gas.

A `remediate` block implies its balance check, so the common case does not repeat itself.
(This key was called `watch` until the model settled on checks; the old name fails with an
error pointing here.)

Where the group does show up:

- **Metrics label** — `gm_account_eth_balance{group="oracles"}`, so "is any oracle wallet low"
  is one query rather than a hand-maintained list of account names in a panel.
- **Console grouping** — rows sectioned by group.
- **CLI filter** — `--group oracles`.

**One group per account**, because the metrics label is a single string and a multi-valued
label has no clean Prometheus representation. **Ungrouped is legal** — nothing forces you to
invent a taxonomy before adding a wallet.

Notes on the rest of the design:

- **Allowances gain an `expect`.** Today we report a number; a check has to judge it. `expect`
  turns "unlimited" from a rendering into an assertion, which is what catches an approval that
  was revoked or never set. `unlimited` accepts `~unlimited` (a drawn-down max approval) —
  effectively-infinite is the property being asserted, not exactness.
- **Direction is not expressible wrongly.** `allowances` on an account always means
  `allowance(this_account, spender)`. The flow-shaped config that would let you say it the
  other way around is deliberately not here; see the open question below.
- **Thresholds and severities are gone from config entirely** — superseded by decision 8. The
  only number left is the remediation trigger.
- **`remediate` sits inside the check it repairs**, not in a parallel block, so you cannot
  configure a top-up for a threshold that does not exist.
- **Severities live on Grafana rules**, not here. `P0`/`P1`/`P2` are kept as names there,
  because on-call muscle memory outweighs nicer ones.

The existing `[[billing.<network>.accounts]]` watchlist is superseded by `[[accounts.<network>]]`.

---

## Phases — all implemented

**Phase 0 — source verification.** ✅ `crates/core/src/source.rs`. Reachability, chain id, head
freshness against `max_lag`. A faulted source makes every check unreadable with the fault as
the reason, rather than failing the cycle silently.

**Phase 1 — engine and balance checks.** ✅ `check.rs`, `checks.rs`, `engine.rs`, `read.rs`.
ETH and GRT balances, batched through one Multicall3 at one pinned block.

**Phase 2 — allowances.** ✅ Value plus the exact `unlimited` / `exact_max` classifications
that `f64` cannot carry.

**Phase 3 — activity.** ✅ `crates/core/src/activity.rs`. Doubling-stride search then bisection
over historical nonces. **Archive-only**, and it degrades to unreadable rather than to a false
"inactive" — which matters most precisely for the least active wallets, since those are the
ones whose search reaches past a pruned node's retained window.

**Phase 4 — serve and metrics.** ✅ `crates/cli/src/metrics.rs`. `tokio::time::interval` with
`MissedTickBehavior::Skip`, axum `/metrics` on its own port, labelled families reset each cycle
so a removed account stops exporting.

**Phase 5 — remediation.** ✅ `crates/core/src/remediate.rs`. Two independent gates
(`--remediate` plus a configured `signer_key`), serial execution per signer, doubled gas limit,
`--dry-run`, and a mined-but-reverted transaction reported as failure rather than success.

**Phase 6 — parity.** ✅ `scripts/parity.py`. Compares coverage against `network-monitor`'s
`config.yaml` and distinguishes *not watched* (a coverage failure) from *watched but blind* (an
endpoint problem). Currently PASSES: every wallet `network-monitor` watches is covered here.

### Retiring network-monitor's account checks

`scripts/parity.py` passing is necessary, not sufficient. Before deleting
`testETHBalanceThreshold`, `testTokenBalanceThreshold` and `testAddressActivity` from
`network-monitor/config.yaml`:

1. Grafana rules exist for the thresholds those tests encode, paging a **test** channel.
2. Both tools have run side by side long enough to compare what fired.
3. An archive endpoint is configured, or the activity checks are accepted as blind.

## Decisions

**1. Groups are labels only.** They mark related accounts for display, filtering, and querying.
They carry no expectations and no defaults, and nothing is inherited — every threshold is
written on the account it applies to. See [Groups](#groups).

**2. No flow-shaped config.** `allowances` stays on the account and always means
`allowance(this_account, spender)` — there is no second way to express an approval, so there is
no direction to get backwards. The misconfiguration this was meant to prevent is instead caught
by `expect`: an approval that is missing now fails a check rather than rendering a quiet `none`.
A money-path model earns its place only if multi-hop *visualization* becomes a goal; it should
not become a second dialect for declaring the same assertion.

**3. The signer key lives in the config file**, consistent with `escrow_manager`'s
`secret_key`. Four constraints follow, and they are not optional:

- **Only from a user layer.** A key in the embedded `config/default.toml` is refused outright —
  that file ships inside the binary.
- **`${ENV}` expansion still applies**, so `secret_key = "${SIGNER_KEY}"` keeps the key out of
  the file for anyone who wants that, without a separate code path.
- **Permissions are checked.** A config file containing a key that is group- or world-readable
  is a startup error, the way `ssh` treats a private key.
- **Never rendered.** The key is redacted in logs, in `--json`, and in any config-dump
  diagnostic, and its type does not implement `Debug` or `Display` in a way that prints it.

**4. The configured RPC is sufficient, and the public endpoint stays the default.** The
consequence lands entirely on the activity check (Phase 3): it needs archive reads the public
endpoint refuses, so on a non-archive endpoint it reports `Unknown` with "this endpoint cannot
serve historical state" — never a false "inactive". Stage 0 can detect this once per cycle
rather than letting every account discover it separately.

**5. A partial cycle publishes what succeeded.** Failed reads report `Unknown`; everything else
reports normally. Because the block is pinned once per network per cycle, this does *not*
produce a scrape with mixed block heights — the failures are `Unknown` at the same block the
successes were read at.

**6. `billing` is renamed to `account`.** The commands read accounts — balances, allowances,
activity — and `billing` is now a *group* name, which is where that word belongs. Command tree
becomes `account status` / `account balances` / `account allowances`.

**7. Source freshness is configurable, with per-network defaults.** Expressed as a duration
against the head block's timestamp:

```toml
[sources.rpc.arbitrum-one]
url     = "https://arb1.arbitrum.io/rpc"
max_lag = "60s"            # head timestamp must be within this of wall clock
```

Defaults, chosen as roughly an order of magnitude above normal block time so ordinary jitter
never trips them:

| Network | Block time | Default `max_lag` |
|---|---|---|
| Arbitrum One / Sepolia | ~0.25s | `60s` |
| Ethereum mainnet | ~12s | `120s` |
| Anything else | — | `120s` |

Subgraphs use a time-based `max_lag` too (default 15 minutes), for the same reason: it means the
same thing on every chain. Blocks behind the RPC head is exported, not judged.

**8. Grafana owns thresholds and alerting. The tool reports facts and repairs.**

The tool reads state, exports it, and — where configured — fixes it. It holds no opinion about
what counts as "too low", and it never pages. Every alert threshold, notification policy,
silence and escalation lives in Grafana.

This is not a change of destination. **Rootly stays where pages land**; Grafana notifies it
instead of the tool doing so. On-call workflow is unchanged.

**The exception: remediation triggers stay in the tool.** A top-up needs a number to act on,
and that number must live where the action lives. The reason is not convenience — **repair
must not depend on the alerting pipeline.** If Prometheus is down, wallets should still get
topped up. Routing remediation through a Grafana webhook would put the metrics stack in the
critical path of keeping the protocol funded.

That the two numbers differ is a feature, not duplication. You top up *earlier* than you page:
auto-fix at 0.05, alert at 0.02 only if auto-fix is not keeping up. They are answers to
different questions, and they belong in different systems.

What follows:

- **The tool stays stateless, and can.** Alert dedup and cooldown buckets exist in
  `network-monitor` only because a stateless process that pages must remember what it already
  paged about. Moving alerting out is what permits statelessness rather than merely allowing
  it. `alertDedup.ts` and `computeDedupBucket` have no successor here.
- **Transience is the alert layer's problem.** `for:` durations absorb blips, so the tool does
  not need retry-and-classify machinery to avoid waking someone over one failed query.
- **Severity leaves the tool.** `P0`/`P1`/`P2` become labels on Grafana rules. Config carries
  no severities; `severity.rs` is deleted.
- **`judge` narrows rather than disappears.** It stops producing alert verdicts and starts
  answering one question: does this reading call for remediation? Still pure, still unit
  testable without a network. `Status` reduces to *read* or *unreadable*.
- **The CLI's Status column goes.** With no thresholds there is no basis for `ok`. `account
  status` becomes what it was first built to be: a snapshot of what *is*. It keeps `!` for
  unreadable, and gains a column showing remediation intent where configured.
- **`--fail-on-warn` goes** with it. Exit codes reduce to 0 success, 2 config/usage, 3 source.

### The one carve-out: facts `f64` cannot carry

Some properties are not thresholds but classifications that a Prometheus float cannot express.
An unlimited allowance is ~`1.157e59`; asking PromQL whether it is "at least 2^255 scaled by
1e18" is a float comparison at a magnitude where `f64` has discarded forty digits.

So the tool exports the *classification* alongside the value, computed on exact `U256`:

```
gm_account_allowance_grt{…}        gauge   the value, lossy above 2^53
gm_account_allowance_unlimited{…}  gauge   1/0, exact
gm_account_allowance_exact_max{…}  gauge   1/0, exact — never drawn against
```

This is not the tool judging. It is the tool reporting something the wire format would
otherwise destroy. Grafana still decides whether `unlimited == 0` is worth paging about.

- **`Unknown` maps onto Grafana's No Data / Error states**, which is where the decision to keep
  "could not see" distinct from "saw something bad" pays off.
- **Alert rules live in this repo** as provisioned rule files, so they review alongside the
  remediation thresholds they interact with. UI-defined alerts drift out of git.

### Watching the watchmen

Alerting through a pipeline adds hops that can fail quietly, so the deadman is part of the
design rather than an afterthought:

| Failure | Detection |
|---|---|
| Process dead | `up{job="graph-monitor"} == 0` — free with a scrape target |
| Process alive, cycles wedged | `time() - gm_cycle_last_success_timestamp > 10m` |
| Whole metrics stack down | External heartbeat (healthchecks.io or similar) |

This is the concrete payoff of `serve`-and-scrape over push: with a Pushgateway a dead job's
metrics persist at their last value, so the pipeline looks healthy while nothing runs.

## Open questions

1. **Where does the human-readable summary go?** The verdict carries a crafted sentence
   (`"BillingWallet holds 0.03 GRT, below the 25,000 GRT minimum"`) that cannot live in a
   Prometheus label. Options: structured logs to an aggregator with the alert linking to them,
   or accept a terser page plus a dashboard link.
2. **Is `up == 0` sufficient, or is an external heartbeat wanted too?**

---

## Consequences for `specs/CLI_SPEC.md`

`CLI_SPEC.md` describes the snapshot CLI as it stands and stays the reference for output
formatting, block provenance, and exit codes. This plan supersedes its **Billing watchlist**
configuration section once Phase 1 lands, and extends its exit-code table with severity
mapping. The block-provenance rule survives intact for console output and bends only at the
Prometheus boundary, as described above.
