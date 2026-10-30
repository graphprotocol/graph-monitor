//! Cycle orchestration: verify, pin, read, judge, report.
//!
//! One cycle covers every configured network. Each network is a lane: its RPC is verified, its
//! head pinned and its watches read, independently of the others, so one chain's outage blinds
//! only that chain's watches. Subgraphs are checked alongside, each against its own chain.
//!
//! One cycle produces one `CycleReport`, which every reporter — console, JSON, metrics —
//! renders from.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Resolved;
use crate::error::Result;
use crate::read::{ReadCtx, ReadPlan};
use crate::rpc::{BlockSpec, Client, PinnedBlock};
use crate::source::{RpcSource, SourceFault, SourceHealth, SourceId};
use crate::subgraph::{SubgraphCheck, SubgraphHealth};
use crate::watch::{DynWatch, Reading};
use crate::watches;

/// What happened on one network this cycle.
pub struct NetworkReport {
    pub network: String,
    pub chain_id: u64,
    pub source: SourceHealth,
    /// Absent when the source failed verification and nothing could be read.
    pub block: Option<PinnedBlock>,
    /// How many watches this network has. Zero for a network that runs only so its subgraphs
    /// can be measured; its RPC being down blinds nothing else.
    pub watches: usize,
}

impl NetworkReport {
    /// The RPC failed while watches depended on it: those watches were not evaluated.
    pub fn is_blind(&self) -> bool {
        !self.source.is_up() && self.watches > 0
    }
}

/// One pass over every configured check, on every network.
pub struct CycleReport {
    pub networks: Vec<NetworkReport>,
    /// Subgraphs the tool reads from, verified like the RPCs. Nothing reads through one yet,
    /// so one being down blinds no watch.
    pub subgraph_sources: Vec<SourceHealth>,
    /// Every network's readings. Each carries its network in `labels.network`.
    pub readings: Vec<Reading>,
    /// One per subgraph check. Subgraphs are monitored components, not inputs: no watch reads
    /// one, so a subgraph being down makes nothing else unreadable.
    pub subgraphs: Vec<SubgraphHealth>,
    pub duration: Duration,
}

impl CycleReport {
    pub fn network(&self, name: &str) -> Option<&NetworkReport> {
        self.networks.iter().find(|n| n.network == name)
    }

    /// Networks whose watches went unread because their RPC failed.
    pub fn blind_networks(&self) -> impl Iterator<Item = &NetworkReport> {
        self.networks.iter().filter(|n| n.is_blind())
    }

    /// True when at least one network was read. What the deadman counts as a cycle.
    pub fn read_anything(&self) -> bool {
        self.networks.iter().any(|n| n.source.is_up())
    }

    /// Subgraphs checked and found unhealthy. A finding about the network.
    pub fn subgraphs_down(&self) -> impl Iterator<Item = &SubgraphHealth> {
        self.subgraphs.iter().filter(|s| s.is_down())
    }

    /// Subgraphs we could not check, e.g. no API key. A problem on our side.
    pub fn subgraphs_not_checked(&self) -> impl Iterator<Item = &SubgraphHealth> {
        self.subgraphs.iter().filter(|s| s.is_not_checked())
    }

    /// Checks that could not be read. Distinct from any value, including zero: an endpoint
    /// outage must never look like an empty wallet.
    pub fn unreadable(&self) -> impl Iterator<Item = &Reading> {
        self.readings.iter().filter(|o| o.is_unreadable())
    }

    /// Repairs this cycle would perform, if remediation is enabled.
    pub fn remediations(&self) -> impl Iterator<Item = &Reading> {
        self.readings.iter().filter(|o| o.remediation.is_some())
    }

    /// (read, unreadable)
    pub fn counts(&self) -> (usize, usize) {
        let unreadable = self.readings.iter().filter(|o| o.is_unreadable()).count();
        (self.readings.len() - unreadable, unreadable)
    }
}

/// One network: its RPC and the watches read through it.
struct Lane {
    network: String,
    chain_id: u64,
    client: Client,
    source: RpcSource,
    checks: Vec<Box<dyn DynWatch>>,
}

/// Everything needed to run cycles against every configured network.
pub struct Engine {
    lanes: Vec<Lane>,
    subgraph_sources: Vec<(SourceId, SubgraphCheck)>,
    subgraphs: Vec<SubgraphCheck>,
    http: alloy::transports::http::reqwest::Client,
}

impl Engine {
    pub fn new(config: Resolved) -> Result<Self> {
        let mut lanes = Vec::new();
        for net in &config.networks {
            // Only needed if some account actually checks a token. Resolving it lazily keeps a
            // pure-ETH network working without a GraphToken entry.
            let token = if net.accounts.iter().any(|a| a.needs_token()) {
                net.graph_token()?
            } else {
                alloy::primitives::Address::ZERO
            };

            let mut checks = Vec::new();
            for account in &net.accounts {
                checks.extend(watches::for_account(&net.name, account, token));
            }

            lanes.push(Lane {
                network: net.name.clone(),
                chain_id: net.chain_id,
                client: Client::new(
                    &net.rpc.url,
                    net.multicall3(),
                    net.rpc.max_concurrency,
                    net.rpc.max_log_range,
                )?,
                source: RpcSource::new(&net.name, net.chain_id, net.rpc.max_lag),
                checks,
            });
        }

        let subgraphs = config
            .subgraphs
            .iter()
            .map(|sg| SubgraphCheck {
                network: sg.network.clone(),
                subgraph: sg.subgraph.clone(),
                deployment: sg.deployment.clone(),
                endpoint: sg.endpoint.clone(),
                max_lag: sg.max_lag,
            })
            .collect();

        let subgraph_sources = config
            .subgraph_sources
            .iter()
            .map(|src| {
                (
                    SourceId::subgraph(&src.name),
                    SubgraphCheck {
                        network: src.network.clone(),
                        subgraph: src.name.clone(),
                        deployment: src.deployment.clone(),
                        endpoint: src.endpoint.clone(),
                        max_lag: src.max_lag,
                    },
                )
            })
            .collect();

        Ok(Self {
            lanes,
            subgraph_sources,
            subgraphs,
            http: alloy::transports::http::reqwest::Client::new(),
        })
    }

    pub fn watch_count(&self) -> usize {
        self.lanes.iter().map(|l| l.checks.len()).sum()
    }

    pub fn network_count(&self) -> usize {
        self.lanes.len()
    }

    pub fn subgraph_count(&self) -> usize {
        self.subgraphs.len()
    }

    /// Run every stage once, on every network.
    ///
    /// `block` selects the height. `BlockSpec::Latest` is resolved to a concrete number once
    /// per network, and every read on that network uses it — including reads that fail, which
    /// is what keeps a partial cycle internally consistent. A specific number only makes sense
    /// for one chain; the caller narrows the run to one network before passing one.
    pub async fn run_cycle(&self, block: BlockSpec) -> CycleReport {
        let started = Instant::now();

        // ---- stage 0: verify every RPC -------------------------------------------------
        let now = now_unix();
        let healths = futures::future::join_all(
            self.lanes
                .iter()
                .map(|lane| lane.source.verify(&lane.client, now)),
        )
        .await;

        // A stale head would make every subgraph look closer to current than it is, so a
        // block gap is only measured against a verified one.
        let heads: Vec<(&str, &Client, Option<PinnedBlock>)> = self
            .lanes
            .iter()
            .zip(&healths)
            .map(|(lane, h)| {
                (
                    lane.network.as_str(),
                    &lane.client,
                    h.head.filter(|_| h.is_up()),
                )
            })
            .collect();

        // Subgraphs need nothing from the watches, so they are checked while the watches read.
        // Subgraph sources too, for now: no watch reads through one yet.
        let (lanes, subgraphs, sources) = tokio::join!(
            futures::future::join_all(
                self.lanes
                    .iter()
                    .zip(healths)
                    .map(|(lane, health)| lane.read(health, block)),
            ),
            self.check_subgraphs(self.subgraphs.iter(), &heads, now),
            self.check_subgraphs(self.subgraph_sources.iter().map(|(_, c)| c), &heads, now)
        );
        let subgraph_sources = self
            .subgraph_sources
            .iter()
            .zip(sources)
            .map(|((id, _), health)| health.into_source_health(id.clone()))
            .collect();

        let mut networks = Vec::new();
        let mut readings = Vec::new();
        for (report, lane_readings) in lanes {
            networks.push(report);
            readings.extend(lane_readings);
        }

        CycleReport {
            networks,
            subgraph_sources,
            readings,
            subgraphs,
            duration: started.elapsed(),
        }
    }

    /// Subgraph checks, concurrently, each measured against its own chain's head.
    async fn check_subgraphs<'a>(
        &self,
        checks: impl Iterator<Item = &'a SubgraphCheck>,
        heads: &[(&str, &Client, Option<PinnedBlock>)],
        now: u64,
    ) -> Vec<SubgraphHealth> {
        futures::future::join_all(checks.map(|sg| {
            // A subgraph is measured only against its own chain. Measuring a Gnosis subgraph
            // against an Arbitrum block number would be a confident wrong answer; no number
            // is better.
            let (client, head) = match heads.iter().find(|(n, _, _)| *n == sg.network) {
                Some((_, client, head)) => (Some(*client), *head),
                None => (None, None),
            };
            sg.run(&self.http, client, head, now)
        }))
        .await
    }
}

impl Lane {
    /// Every check reports unreadable, attributed to the source fault that prevented reading.
    fn all_unreadable(
        &self,
        health: SourceHealth,
        block: Option<PinnedBlock>,
    ) -> (NetworkReport, Vec<Reading>) {
        let reason = format!("source {} is unavailable: {}", health.id, health.summary());
        let readings = self
            .checks
            .iter()
            .map(|c| Reading {
                id: c.id(),
                labels: c.labels(),
                observation: Err(reason.clone()),
                remediation: None,
                duration: Duration::ZERO,
            })
            .collect();
        (self.report(health, block), readings)
    }

    fn report(&self, source: SourceHealth, block: Option<PinnedBlock>) -> NetworkReport {
        NetworkReport {
            network: self.network.clone(),
            chain_id: self.chain_id,
            source,
            block,
            watches: self.checks.len(),
        }
    }

    /// Stages 1 and 2 for this network's watches, given its verified (or failed) source.
    async fn read(
        &self,
        mut health: SourceHealth,
        block: BlockSpec,
    ) -> (NetworkReport, Vec<Reading>) {
        // Nothing readable means nothing judgeable. Every check reports Unknown naming the
        // cause, rather than the cycle failing wholesale and reporting nothing at all.
        if !health.is_up() {
            return self.all_unreadable(health, None);
        }

        // ---- pin the block --------------------------------------------------------------
        // `verify` already fetched the head; re-pin only when a specific height was asked for.
        let pinned = match block {
            BlockSpec::Latest => health.head,
            BlockSpec::Number(_) => match self.client.pin(block).await {
                Ok(b) => Some(b),
                Err(e) => {
                    health.fault = Some(SourceFault::Unreachable(e.to_string()));
                    return self.all_unreadable(health, None);
                }
            },
        };

        let Some(pinned) = pinned else {
            health.fault = Some(SourceFault::Unreachable(
                "endpoint returned no block".to_string(),
            ));
            return self.all_unreadable(health, None);
        };

        if self.checks.is_empty() {
            return (self.report(health, Some(pinned)), Vec::new());
        }

        // ---- stage 1: read (batched) ----------------------------------------------------
        let mut plan = ReadPlan::default();
        for check in &self.checks {
            check.plan(&mut plan);
        }
        tracing::debug!(
            network = %self.network,
            reads = plan.len(),
            checks = self.checks.len(),
            block = pinned.number,
            "prefetching"
        );
        let ctx = ReadCtx::prefetch(&self.client, pinned, &plan).await;

        // Failures that look like "this endpoint has no state at this block" are a source
        // problem, not N independent cell problems. Reporting them once, attributed to the
        // source, beats a table of unreadable cells that invites the reader to think some
        // wallets are fine and others are not.
        if let Some(reason) = ctx.state_fault() {
            health.fault = Some(SourceFault::StateUnavailable {
                block: pinned.number,
                reason: reason.to_string(),
            });
            return self.all_unreadable(health, Some(pinned));
        }

        // ---- stage 2: judge -------------------------------------------------------------
        let readings = futures::future::join_all(self.checks.iter().map(|c| c.run(&ctx))).await;
        (self.report(health, Some(pinned)), readings)
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceId;
    use crate::watch::{Labels, WatchId};

    fn outcome(readable: bool) -> Reading {
        Reading {
            id: WatchId::new("eth_balance", "w"),
            labels: Labels::default(),
            observation: if readable {
                Ok(crate::watch::Observation::new("read"))
            } else {
                Err("could not read".into())
            },
            remediation: None,
            duration: Duration::ZERO,
        }
    }

    fn report(readings: Vec<Reading>) -> CycleReport {
        CycleReport {
            networks: vec![network("arbitrum-one", true, 1)],
            subgraph_sources: Vec::new(),
            readings,
            subgraphs: Vec::new(),
            duration: Duration::ZERO,
        }
    }

    fn network(name: &str, up: bool, watches: usize) -> NetworkReport {
        NetworkReport {
            network: name.into(),
            chain_id: 1,
            source: SourceHealth {
                fault: (!up).then(|| SourceFault::Unreachable("down".into())),
                ..SourceHealth::new(SourceId::rpc(name), name)
            },
            block: None,
            watches,
        }
    }

    #[test]
    fn a_down_rpc_blinds_only_a_network_with_watches() {
        // Gnosis runs only for its subgraphs: its RPC failing leaves no watch unread.
        let mut r = report(vec![]);
        r.networks = vec![
            network("arbitrum-one", true, 20),
            network("gnosis", false, 0),
            network("mainnet", false, 3),
        ];
        let blind: Vec<&str> = r.blind_networks().map(|n| n.network.as_str()).collect();
        assert_eq!(blind, ["mainnet"]);
        assert!(r.read_anything());

        r.networks = vec![network("arbitrum-one", false, 20)];
        assert!(!r.read_anything());
    }

    #[test]
    fn counts_split_read_from_unreadable() {
        let r = report(vec![outcome(true), outcome(true), outcome(false)]);
        assert_eq!(r.counts(), (2, 1));
        assert_eq!(r.unreadable().count(), 1);
    }

    #[test]
    fn a_report_with_nothing_read_is_not_a_report_of_zeroes() {
        // Every check unreadable must stay distinguishable from every balance being zero.
        let r = report(vec![outcome(false), outcome(false)]);
        assert_eq!(r.counts(), (0, 2));
        assert!(r.readings.iter().all(|o| o.value("anything").is_none()));
    }

    #[test]
    fn remediations_are_counted_separately_from_reads() {
        let mut o = outcome(true);
        o.remediation = Some(crate::watch::RemediationIntent {
            asset: crate::watch::Asset::Eth,
            observed: crate::units::Amount::zero(18),
            below: crate::units::Amount::zero(18),
            top_up_by: crate::units::Amount::zero(18),
            from: None,
            from_address: None,
        });
        let r = report(vec![outcome(true), o]);
        assert_eq!(r.counts(), (2, 0));
        assert_eq!(r.remediations().count(), 1);
    }
}
