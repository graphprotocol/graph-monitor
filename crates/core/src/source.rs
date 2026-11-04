use std::time::Duration;

use crate::rpc::{BlockSpec, Client, PinnedBlock};

/// Identifies a data source
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(pub String);

impl SourceId {
    pub fn rpc(network: &str) -> Self {
        SourceId(format!("rpc:{network}"))
    }

    pub fn subgraph(name: &str) -> Self {
        SourceId(format!("subgraph:{name}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFault {
    /// Could not reach the endpoint at all.
    Unreachable(String),
    /// The endpoint is a different chain than config claims. Every value it returns would be
    /// plausible and wrong.
    ChainIdMismatch { expected: u64, actual: u64 },
    /// Reachable and correct, but behind: its head is older than `max_lag`.
    Stale { lag: Duration, max_lag: Duration },
    /// Live, but cannot serve *state* at the requested block.
    StateUnavailable { block: u64, reason: String },
    /// A subgraph source answered badly: wrong deployment, indexing errors, stale, or no
    /// usable answer at all.
    Subgraph(crate::subgraph::SubgraphFault),
    /// A subgraph source could not be queried from our side, e.g. no API key.
    NotQueryable(String),
}

impl SourceFault {
    pub fn summary(&self) -> String {
        match self {
            SourceFault::Unreachable(e) => format!("endpoint unreachable: {e}"),
            SourceFault::ChainIdMismatch { expected, actual } => {
                format!("chain id mismatch: config expects {expected}, endpoint reports {actual}")
            }
            SourceFault::Stale { lag, max_lag } => format!(
                "head block is {}s behind the clock, over the {}s limit",
                lag.as_secs(),
                max_lag.as_secs()
            ),
            SourceFault::StateUnavailable { block, reason } => format!(
                "state at block {block} is not available from this endpoint — reading a \
                 historical block usually needs an archive node ({reason})"
            ),
            SourceFault::Subgraph(fault) => fault.summary(),
            SourceFault::NotQueryable(reason) => format!("cannot be queried: {reason}"),
        }
    }
}

/// The outcome of verifying one source.
#[derive(Debug, Clone)]
pub struct SourceHealth {
    pub id: SourceId,
    pub network: String,
    pub chain_id: Option<u64>,
    pub head: Option<PinnedBlock>,
    pub lag: Option<Duration>,
    pub fault: Option<SourceFault>,
}

impl SourceHealth {
    /// Nothing known yet.
    pub fn new(id: SourceId, network: &str) -> Self {
        Self {
            id,
            network: network.to_string(),
            chain_id: None,
            head: None,
            lag: None,
            fault: None,
        }
    }

    pub fn is_up(&self) -> bool {
        self.fault.is_none()
    }

    pub fn summary(&self) -> String {
        match &self.fault {
            None => match (self.head, self.lag) {
                (Some(head), Some(lag)) => {
                    format!("head {}, {}s behind", head.number, lag.as_secs())
                }
                _ => "reachable".to_string(),
            },
            Some(fault) => fault.summary(),
        }
    }
}

/// An RPC endpoint as a verifiable source.
pub struct RpcSource {
    pub id: SourceId,
    pub network: String,
    pub chain_id: u64,
    pub max_lag: Duration,
}

impl RpcSource {
    pub fn new(network: &str, chain_id: u64, max_lag: Duration) -> Self {
        Self {
            id: SourceId::rpc(network),
            network: network.to_string(),
            chain_id,
            max_lag,
        }
    }

    /// Reachable, the right chain, and current.
    pub async fn verify(&self, client: &Client, now_unix: u64) -> SourceHealth {
        let mut health = SourceHealth::new(self.id.clone(), &self.network);

        let actual = match client.chain_id().await {
            Ok(id) => id,
            Err(e) => {
                health.fault = Some(SourceFault::Unreachable(e.to_string()));
                return health;
            }
        };
        health.chain_id = Some(actual);

        if actual != self.chain_id {
            health.fault = Some(SourceFault::ChainIdMismatch {
                expected: self.chain_id,
                actual,
            });
            return health;
        }

        let head = match client.pin(BlockSpec::Latest).await {
            Ok(head) => head,
            Err(e) => {
                health.fault = Some(SourceFault::Unreachable(e.to_string()));
                return health;
            }
        };
        health.head = Some(head);

        // Saturating: a head timestamp slightly ahead of local clock is clock skew, not
        // negative lag, and must not wrap around into an enormous number.
        let lag = Duration::from_secs(now_unix.saturating_sub(head.timestamp));
        health.lag = Some(lag);

        if lag > self.max_lag {
            health.fault = Some(SourceFault::Stale {
                lag,
                max_lag: self.max_lag,
            });
        }

        health
    }
}

/// Chain ids of networks the tool knows by name, so config need not repeat them. The id is
/// what every RPC is checked against: a URL cannot be trusted to say which chain it serves.
pub fn known_chain_id(network: &str) -> Option<u64> {
    match network {
        "mainnet" => Some(1),
        "sepolia" => Some(11_155_111),
        "arbitrum-one" => Some(42161),
        "arbitrum-sepolia" => Some(421_614),
        "gnosis" => Some(100),
        _ => None,
    }
}

/// Default freshness allowance, chosen roughly an order of magnitude above block time so
/// ordinary jitter never trips it.
pub fn default_max_lag(chain_id: u64) -> Duration {
    match chain_id {
        // Arbitrum One / Sepolia — sub-second blocks.
        42161 | 421614 => Duration::from_secs(60),
        // Ethereum mainnet and everything else — 12s blocks or unknown.
        _ => Duration::from_secs(120),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health_with(lag: Option<Duration>, fault: Option<SourceFault>) -> SourceHealth {
        SourceHealth {
            chain_id: Some(42161),
            head: Some(PinnedBlock {
                number: 100,
                timestamp: 0,
            }),
            lag,
            fault,
            ..SourceHealth::new(SourceId::rpc("arbitrum-one"), "arbitrum-one")
        }
    }

    #[test]
    fn a_source_is_up_only_without_a_fault() {
        assert!(health_with(Some(Duration::from_secs(1)), None).is_up());
        assert!(!health_with(
            Some(Duration::from_secs(900)),
            Some(SourceFault::Stale {
                lag: Duration::from_secs(900),
                max_lag: Duration::from_secs(60)
            })
        )
        .is_up());
    }

    #[test]
    fn faults_explain_themselves() {
        let stale = SourceFault::Stale {
            lag: Duration::from_secs(900),
            max_lag: Duration::from_secs(60),
        };
        assert_eq!(
            stale.summary(),
            "head block is 900s behind the clock, over the 60s limit"
        );
        let mismatch = SourceFault::ChainIdMismatch {
            expected: 42161,
            actual: 1,
        };
        assert!(mismatch.summary().contains("42161"));
        assert!(mismatch.summary().contains('1'));
    }

    #[test]
    fn well_known_networks_need_no_chain_id() {
        assert_eq!(known_chain_id("arbitrum-one"), Some(42161));
        assert_eq!(known_chain_id("gnosis"), Some(100));
        assert_eq!(known_chain_id("my-devnet"), None);
    }

    #[test]
    fn default_lag_follows_block_time() {
        assert_eq!(default_max_lag(42161), Duration::from_secs(60));
        assert_eq!(default_max_lag(421614), Duration::from_secs(60));
        assert_eq!(default_max_lag(1), Duration::from_secs(120));
        assert_eq!(default_max_lag(999999), Duration::from_secs(120));
    }
}
