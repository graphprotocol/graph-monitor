//! Subgraphs as verifiable sources.
//!
//! A subgraph answers from whatever block it last indexed, and a stale one answers just as
//! confidently as a current one. So the first thing worth knowing about a subgraph is whether
//! it answers at all, and how far behind the chain it is. Both come from one `_meta` query.

use std::time::Duration;

use alloy::transports::http::reqwest;
use serde_json::Value;

use crate::rpc::{BlockSpec, Client, PinnedBlock};
use crate::source::{SourceFault, SourceHealth, SourceId};

/// One query, every field stage 0 needs.
const META_QUERY: &str =
    r#"{"query":"{ _meta { deployment hasIndexingErrors block { number timestamp } } }"}"#;

/// A gateway that takes longer than this is not answering in any useful sense.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A credential that must never reach a log line, a panic message, or `{:?}`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where a subgraph is queried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    /// Sent as `Authorization: Bearer`, which keeps the key out of the URL and so out of
    /// anything that prints one.
    pub bearer: Option<Secret>,
}

/// What `_meta` reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub deployment: String,
    pub has_indexing_errors: bool,
    pub block_number: u64,
    /// Nullable in the schema: older graph-node versions do not report it.
    pub block_timestamp: Option<u64>,
}

/// Parse a `_meta` response body. GraphQL reports most failures as a 200 with an `errors`
/// array, so a successful HTTP status says nothing on its own.
pub fn parse_meta(body: &str) -> Result<Meta, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("response is not JSON ({e})"))?;

    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
        let messages: Vec<&str> = errors
            .iter()
            .filter_map(|e| e.get("message").and_then(Value::as_str))
            .collect();
        if !messages.is_empty() {
            return Err(messages.join("; "));
        }
    }

    let meta = value
        .pointer("/data/_meta")
        .filter(|m| !m.is_null())
        .ok_or_else(|| "response has no data._meta".to_string())?;

    let block_number = meta
        .pointer("/block/number")
        .and_then(Value::as_u64)
        .ok_or_else(|| "_meta.block.number is missing".to_string())?;

    Ok(Meta {
        deployment: meta
            .get("deployment")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        has_indexing_errors: meta
            .get("hasIndexingErrors")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        block_number,
        block_timestamp: meta.pointer("/block/timestamp").and_then(Value::as_u64),
    })
}

/// A configured subgraph, verified once per cycle.
pub struct SubgraphSource {
    pub id: SourceId,
    pub network: String,
    /// The query endpoint, or why there is none. A missing API key makes the subgraph
    /// unreadable rather than failing config: the rest of the tool still has work to do.
    pub endpoint: Result<Endpoint, String>,
    /// Set when config pins a deployment, so serving any other deployment is a fault.
    pub expected_deployment: Option<String>,
    pub max_lag: Duration,
}

impl SubgraphSource {
    /// Reachable, the expected deployment, indexing cleanly, and current.
    ///
    /// `chain_head` is the RPC's head this cycle, or `None` when the RPC failed. Without it
    /// the block gap cannot be measured, but reachability and time lag still can.
    pub async fn verify(
        &self,
        http: &reqwest::Client,
        rpc: &Client,
        chain_head: Option<PinnedBlock>,
        now_unix: u64,
    ) -> SourceHealth {
        let mut health = SourceHealth::new(self.id.clone(), &self.network);

        let endpoint = match &self.endpoint {
            Ok(endpoint) => endpoint,
            Err(reason) => {
                health.fault = Some(SourceFault::Unconfigured(reason.clone()));
                return health;
            }
        };

        let meta = match query_meta(http, endpoint).await {
            Ok(meta) => meta,
            Err(e) => {
                health.fault = Some(SourceFault::Unreachable(e));
                return health;
            }
        };

        // Older graph-node versions leave the timestamp out. The block number is enough to
        // ask the chain for it, as long as the RPC is up.
        let timestamp = match (meta.block_timestamp, chain_head) {
            (Some(ts), _) => Some(ts),
            (None, Some(_)) => rpc
                .pin(BlockSpec::Number(meta.block_number))
                .await
                .ok()
                .map(|b| b.timestamp),
            (None, None) => None,
        };

        judge(
            &mut health,
            &meta,
            timestamp,
            self.expected_deployment.as_deref(),
            chain_head,
            now_unix,
            self.max_lag,
        );
        health
    }
}

/// The verdict, as a pure function of what was read.
fn judge(
    health: &mut SourceHealth,
    meta: &Meta,
    timestamp: Option<u64>,
    expected_deployment: Option<&str>,
    chain_head: Option<PinnedBlock>,
    now_unix: u64,
    max_lag: Duration,
) {
    health.deployment = Some(meta.deployment.clone());
    health.head = timestamp.map(|timestamp| PinnedBlock {
        number: meta.block_number,
        timestamp,
    });
    health.indexed_block = Some(meta.block_number);
    // Saturating: a subgraph a block or two *ahead* of our RPC is a different node's view of
    // the head, not negative lag.
    health.blocks_behind = chain_head.map(|h| h.number.saturating_sub(meta.block_number));
    health.lag = timestamp.map(|ts| Duration::from_secs(now_unix.saturating_sub(ts)));

    // Most specific first. A wrong deployment makes every other number about the wrong thing.
    if let Some(expected) = expected_deployment {
        if meta.deployment != expected {
            health.fault = Some(SourceFault::WrongDeployment {
                expected: expected.to_string(),
                actual: meta.deployment.clone(),
            });
            return;
        }
    }
    if meta.has_indexing_errors {
        health.fault = Some(SourceFault::IndexingErrors);
        return;
    }
    if let Some(lag) = health.lag {
        if lag > max_lag {
            health.fault = Some(SourceFault::Stale { lag, max_lag });
        }
    }
}

async fn query_meta(http: &reqwest::Client, endpoint: &Endpoint) -> Result<Meta, String> {
    let mut request = http
        .post(&endpoint.url)
        .header("content-type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .body(META_QUERY);
    if let Some(bearer) = &endpoint.bearer {
        request = request.bearer_auth(bearer.expose());
    }

    // The error text can carry the URL; the key is in a header, so the URL is safe to show.
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;

    // A gateway error body usually says more than the status code does.
    match parse_meta(&body) {
        Ok(meta) => Ok(meta),
        Err(reason) if status.is_success() => Err(reason),
        Err(_) => Err(format!("HTTP {status}: {}", truncate(&body, 200))),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEPLOYMENT: &str = "QmTestDeployment";

    fn meta(block: u64, timestamp: Option<u64>, errors: bool) -> Meta {
        Meta {
            deployment: DEPLOYMENT.into(),
            has_indexing_errors: errors,
            block_number: block,
            block_timestamp: timestamp,
        }
    }

    fn verdict(
        meta: &Meta,
        expected: Option<&str>,
        head: Option<u64>,
        now: u64,
        max_lag_secs: u64,
    ) -> SourceHealth {
        let mut health = SourceHealth::new(SourceId::subgraph("network"), "arbitrum-one");
        judge(
            &mut health,
            meta,
            meta.block_timestamp,
            expected,
            head.map(|number| PinnedBlock {
                number,
                timestamp: now,
            }),
            now,
            Duration::from_secs(max_lag_secs),
        );
        health
    }

    #[test]
    fn parses_a_meta_response() {
        let body = r#"{"data":{"_meta":{"deployment":"QmX","hasIndexingErrors":false,
            "block":{"number":519433355,"timestamp":1793000000}}}}"#;
        assert_eq!(
            parse_meta(body).unwrap(),
            Meta {
                deployment: "QmX".into(),
                has_indexing_errors: false,
                block_number: 519_433_355,
                block_timestamp: Some(1_793_000_000),
            }
        );
    }

    #[test]
    fn a_null_timestamp_is_absent_not_zero() {
        // Zero would read as fifty years of lag.
        let body = r#"{"data":{"_meta":{"deployment":"QmX","hasIndexingErrors":false,
            "block":{"number":5,"timestamp":null}}}}"#;
        assert_eq!(parse_meta(body).unwrap().block_timestamp, None);
    }

    #[test]
    fn graphql_errors_are_failures_even_on_http_200() {
        let body = r#"{"errors":[{"message":"subgraph not found: abc"}]}"#;
        assert_eq!(parse_meta(body).unwrap_err(), "subgraph not found: abc");
        assert!(parse_meta(r#"{"data":null}"#).is_err());
        assert!(parse_meta("<html>bad gateway</html>").is_err());
    }

    #[test]
    fn a_current_subgraph_is_up_and_reports_both_lags() {
        let h = verdict(
            &meta(990, Some(1_000), false),
            None,
            Some(1_000),
            1_030,
            600,
        );
        assert!(h.is_up(), "{:?}", h.fault);
        assert_eq!(h.blocks_behind, Some(10));
        assert_eq!(h.lag, Some(Duration::from_secs(30)));
        assert_eq!(h.indexed_block, Some(990));
    }

    #[test]
    fn behind_by_more_than_max_lag_is_stale() {
        let h = verdict(
            &meta(100, Some(1_000), false),
            None,
            Some(5_000),
            2_000,
            600,
        );
        assert!(
            matches!(h.fault, Some(SourceFault::Stale { .. })),
            "{:?}",
            h.fault
        );
        // The numbers are still reported: a stale subgraph's lag is exactly what to graph.
        assert_eq!(h.blocks_behind, Some(4_900));
    }

    #[test]
    fn indexing_errors_are_a_fault() {
        let h = verdict(&meta(990, Some(1_000), true), None, Some(1_000), 1_000, 600);
        assert_eq!(h.fault, Some(SourceFault::IndexingErrors));
    }

    #[test]
    fn a_pinned_deployment_must_match() {
        let h = verdict(
            &meta(990, Some(1_000), false),
            Some("QmOther"),
            None,
            1_000,
            600,
        );
        assert!(matches!(h.fault, Some(SourceFault::WrongDeployment { .. })));
        let ok = verdict(
            &meta(990, Some(1_000), false),
            Some(DEPLOYMENT),
            None,
            1_000,
            600,
        );
        assert!(ok.is_up());
    }

    #[test]
    fn without_a_chain_head_the_block_gap_is_unknown_not_zero() {
        let h = verdict(&meta(990, Some(1_000), false), None, None, 1_000, 600);
        assert_eq!(h.blocks_behind, None);
        assert!(h.is_up());
    }

    #[test]
    fn a_secret_never_prints() {
        let e = Endpoint {
            url: "https://gateway.example/api/subgraphs/id/abc".into(),
            bearer: Some(Secret::new("super-secret-key".into())),
        };
        assert!(!format!("{e:?}").contains("super-secret-key"));
    }
}
