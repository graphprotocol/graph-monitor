//! Cycle orchestration: verify, pin, read, judge, report.
//!
//! One cycle produces one `CycleReport`, which every reporter — console, JSON, metrics,
//! renders from.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Resolved;
use crate::error::Result;
use crate::read::{ReadCtx, ReadPlan};
use crate::rpc::{BlockSpec, Client, PinnedBlock};
use crate::source::{RpcSource, SourceFault, SourceHealth};
use crate::watch::{DynWatch, Reading};
use crate::watches;

/// One pass over every configured check.
pub struct CycleReport {
    pub network: String,
    pub chain_id: u64,
    pub source: SourceHealth,
    /// Absent when the source failed verification and nothing could be read.
    pub block: Option<PinnedBlock>,
    pub readings: Vec<Reading>,
    pub duration: Duration,
}

impl CycleReport {
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

/// Everything needed to run cycles against one network.
pub struct Engine {
    pub config: Resolved,
    pub client: Client,
    pub source: RpcSource,
    checks: Vec<Box<dyn DynWatch>>,
}

impl Engine {
    pub fn new(config: Resolved) -> Result<Self> {
        let accounts = config.require_accounts()?.to_vec();

        // Only needed if some account actually checks a token. Resolving it lazily keeps a
        // pure-ETH config working on a network with no GraphToken entry.
        let needs_token = accounts.iter().any(|a| a.needs_token());
        let token = if needs_token {
            config.graph_token()?
        } else {
            alloy::primitives::Address::ZERO
        };

        let mut all = Vec::new();
        for account in &accounts {
            all.extend(watches::for_account(&config.network, account, token));
        }

        let client = Client::new(
            &config.rpc_url,
            config.multicall3(),
            config.max_concurrency,
            config.max_log_range,
        )?;
        let source = RpcSource::new(&config.network, config.chain_id, config.max_lag);

        Ok(Self {
            config,
            client,
            source,
            checks: all,
        })
    }

    pub fn watch_count(&self) -> usize {
        self.checks.len()
    }

    /// Every check reports unreadable, attributed to the source fault that prevented reading.
    fn all_unreadable(&self, health: SourceHealth, started: Instant) -> CycleReport {
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

        CycleReport {
            network: self.config.network.clone(),
            chain_id: self.config.chain_id,
            source: health,
            block: None,
            readings,
            duration: started.elapsed(),
        }
    }

    /// Run every stage once.
    ///
    /// `block` selects the height. `BlockSpec::Latest` is resolved to a concrete number once,
    /// here, and every read in the cycle uses that number — including reads that fail, which
    /// is what keeps a partial cycle internally consistent.
    pub async fn run_cycle(&self, block: BlockSpec) -> CycleReport {
        let started = Instant::now();

        // ---- stage 0: verify the source ------------------------------------------------
        let mut health = self.source.verify(&self.client, now_unix()).await;

        // Nothing readable means nothing judgeable. Every check reports Unknown naming the
        // cause, rather than the cycle failing wholesale and reporting nothing at all.
        if !health.is_up() {
            return self.all_unreadable(health, started);
        }

        // ---- pin the block --------------------------------------------------------------
        // `verify` already fetched the head; re-pin only when a specific height was asked for.
        let pinned = match block {
            BlockSpec::Latest => health.head,
            BlockSpec::Number(_) => match self.client.pin(block).await {
                Ok(b) => Some(b),
                Err(e) => {
                    health.fault = Some(SourceFault::Unreachable(e.to_string()));
                    return self.all_unreadable(health, started);
                }
            },
        };

        let Some(pinned) = pinned else {
            health.fault = Some(SourceFault::Unreachable(
                "endpoint returned no block".to_string(),
            ));
            return self.all_unreadable(health, started);
        };

        // ---- stage 1: read (batched) ----------------------------------------------------
        let mut plan = ReadPlan::default();
        for check in &self.checks {
            check.plan(&mut plan);
        }
        tracing::debug!(
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
            let mut report = self.all_unreadable(health, started);
            report.block = Some(pinned);
            return report;
        }

        // ---- stage 2: judge -------------------------------------------------------------
        let readings = futures::future::join_all(self.checks.iter().map(|c| c.run(&ctx))).await;

        CycleReport {
            network: self.config.network.clone(),
            chain_id: self.config.chain_id,
            source: health,
            block: Some(pinned),
            readings,
            duration: started.elapsed(),
        }
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
            network: "arbitrum-one".into(),
            chain_id: 42161,
            source: SourceHealth {
                id: SourceId::rpc("arbitrum-one"),
                network: "arbitrum-one".into(),
                chain_id: Some(42161),
                head: None,
                lag: None,
                fault: None,
            },
            block: None,
            readings,
            duration: Duration::ZERO,
        }
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
