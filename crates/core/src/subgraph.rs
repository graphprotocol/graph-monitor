//! Subgraphs as monitored components.
//!
//! A subgraph is part of the network this tool watches, not an input to it: the RPC is how the
//! tool sees, and a subgraph is one of the things it looks at. So a subgraph that is down is a
//! finding about the network, reported alongside balances, while the RPC being down means the
//! tool itself is blind.
//!
//! A subgraph answers from whatever block it last indexed, and a stale one answers just as
//! confidently as a current one. So the first thing worth knowing is whether it answers at all,
//! and how far behind the chain it is. Both come from one `_meta` query.

use std::time::Duration;

use alloy::transports::http::reqwest;
use serde_json::Value;

use crate::rpc::{BlockSpec, Client, PinnedBlock};

/// One query, everything the check needs.
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

/// Why a subgraph that was checked is not healthy. Each is a finding about the subgraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubgraphFault {
    /// No usable answer: unknown subgraph, no indexer serving it, or the request failed.
    Unreachable(String),
    /// A listed deployment was asked for and a different one answered.
    WrongDeployment { expected: String, actual: String },
    /// graph-node reports indexing errors, so what it serves may be wrong or frozen.
    IndexingErrors,
    /// The latest indexed block is older than `max_lag`.
    Stale { lag: Duration, max_lag: Duration },
}

impl SubgraphFault {
    pub fn summary(&self) -> String {
        match self {
            SubgraphFault::Unreachable(e) => format!("no answer: {e}"),
            SubgraphFault::WrongDeployment { expected, actual } => {
                format!("asked for {expected}, {actual} answered")
            }
            SubgraphFault::IndexingErrors => "subgraph has indexing errors".to_string(),
            SubgraphFault::Stale { lag, max_lag } => format!(
                "latest indexed block is {}s old, over the {}s limit",
                lag.as_secs(),
                max_lag.as_secs()
            ),
        }
    }
}

/// The outcome of one subgraph check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubgraphStatus {
    Up,
    Down(SubgraphFault),
    /// Our side failed before the subgraph could be judged: no API key, or the gateway
    /// refused ours. Says nothing about the subgraph, so it must not read as "down".
    NotChecked(String),
}

/// One check: a subgraph's published version, or one listed deployment of it.
#[derive(Debug, Clone)]
pub struct SubgraphHealth {
    pub network: String,
    /// The configured label.
    pub subgraph: String,
    /// The listed deployment this check asked for. `None` for the published version.
    pub deployment: Option<String>,
    /// The deployment that answered.
    pub served_deployment: Option<String>,
    pub indexed_block: Option<u64>,
    /// Timestamp of the latest indexed block, when known.
    pub block_timestamp: Option<u64>,
    /// Blocks between the latest indexed block and the RPC's head. `None` when the RPC was
    /// not verified this cycle, or serves another chain, since there is no trustworthy head
    /// to measure against.
    pub blocks_behind: Option<u64>,
    /// Age of the latest indexed block against the clock.
    pub lag: Option<Duration>,
    pub status: SubgraphStatus,
}

impl SubgraphHealth {
    fn new(check: &SubgraphCheck) -> Self {
        Self {
            network: check.network.clone(),
            subgraph: check.subgraph.clone(),
            deployment: check.deployment.clone(),
            served_deployment: None,
            indexed_block: None,
            block_timestamp: None,
            blocks_behind: None,
            lag: None,
            status: SubgraphStatus::Up,
        }
    }

    pub fn is_up(&self) -> bool {
        self.status == SubgraphStatus::Up
    }

    pub fn is_down(&self) -> bool {
        matches!(self.status, SubgraphStatus::Down(_))
    }

    pub fn is_not_checked(&self) -> bool {
        matches!(self.status, SubgraphStatus::NotChecked(_))
    }

    /// `network` for the published version, `network@Qm…` for a listed deployment.
    pub fn name(&self) -> String {
        match &self.deployment {
            Some(d) => format!("{}@{d}", self.subgraph),
            None => self.subgraph.clone(),
        }
    }

    /// The same check, read as a verdict on a source. For a subgraph the tool reads from,
    /// "not checked" is not a separate state: either way the tool cannot see through it.
    pub fn into_source_health(self, id: crate::source::SourceId) -> crate::source::SourceHealth {
        use crate::source::{SourceFault, SourceHealth};
        SourceHealth {
            head: self
                .indexed_block
                .zip(self.block_timestamp)
                .map(|(number, timestamp)| PinnedBlock { number, timestamp }),
            lag: self.lag,
            fault: match self.status {
                SubgraphStatus::Up => None,
                SubgraphStatus::Down(fault) => Some(SourceFault::Subgraph(fault)),
                SubgraphStatus::NotChecked(reason) => Some(SourceFault::NotQueryable(reason)),
            },
            ..SourceHealth::new(id, &self.network)
        }
    }

    pub fn summary(&self) -> String {
        match &self.status {
            SubgraphStatus::Down(fault) => fault.summary(),
            SubgraphStatus::NotChecked(reason) => format!("not checked: {reason}"),
            SubgraphStatus::Up => match (self.indexed_block, self.blocks_behind) {
                (Some(block), Some(behind)) => {
                    format!("indexed to block {block}, {behind} blocks behind the chain head")
                }
                (Some(block), None) => format!("indexed to block {block}"),
                (None, _) => "answering".to_string(),
            },
        }
    }
}

/// A configured check, run once per cycle.
pub struct SubgraphCheck {
    pub network: String,
    pub subgraph: String,
    /// Set for a listed deployment: anything else answering is a fault.
    pub deployment: Option<String>,
    /// The query endpoint, or why there is none. A missing API key makes the check
    /// not-checked rather than failing config: the rest of the tool still has work to do.
    pub endpoint: Result<Endpoint, String>,
    pub max_lag: Duration,
}

impl SubgraphCheck {
    /// Answering, the expected deployment, indexing cleanly, and current.
    ///
    /// `rpc` and `chain_head` are the subgraph's own chain's RPC and its verified head this
    /// cycle, or `None` when there is no RPC for that chain or it failed verification. Without it the block gap cannot be
    /// measured, and the RPC is not asked for a missing timestamp either, since it could be the
    /// wrong chain. Whether the subgraph answers, and usually its time lag, still can be.
    pub async fn run(
        &self,
        http: &reqwest::Client,
        rpc: Option<&Client>,
        chain_head: Option<PinnedBlock>,
        now_unix: u64,
    ) -> SubgraphHealth {
        let mut health = SubgraphHealth::new(self);

        let endpoint = match &self.endpoint {
            Ok(endpoint) => endpoint,
            Err(reason) => {
                health.status = SubgraphStatus::NotChecked(reason.clone());
                return health;
            }
        };

        let meta = match query_meta(http, endpoint).await {
            Ok(meta) => meta,
            Err(QueryError::Auth(reason)) => {
                health.status = SubgraphStatus::NotChecked(format!(
                    "the gateway refused our API key ({reason})"
                ));
                return health;
            }
            Err(QueryError::Failed(reason)) => {
                health.status = SubgraphStatus::Down(SubgraphFault::Unreachable(reason));
                return health;
            }
        };

        // Older graph-node versions leave the timestamp out. The block number is enough to
        // ask the chain for it, as long as the RPC is up.
        let timestamp = match (meta.block_timestamp, rpc, chain_head) {
            (Some(ts), _, _) => Some(ts),
            (None, Some(rpc), Some(_)) => rpc
                .pin(BlockSpec::Number(meta.block_number))
                .await
                .ok()
                .map(|b| b.timestamp),
            _ => None,
        };

        judge(
            &mut health,
            &meta,
            timestamp,
            chain_head,
            now_unix,
            self.max_lag,
        );
        health
    }
}

/// The verdict, as a pure function of what was read.
fn judge(
    health: &mut SubgraphHealth,
    meta: &Meta,
    timestamp: Option<u64>,
    chain_head: Option<PinnedBlock>,
    now_unix: u64,
    max_lag: Duration,
) {
    health.served_deployment = Some(meta.deployment.clone());
    health.indexed_block = Some(meta.block_number);
    health.block_timestamp = timestamp;
    // Saturating: a subgraph a block or two *ahead* of our RPC is a different node's view of
    // the head, not negative lag.
    health.blocks_behind = chain_head.map(|h| h.number.saturating_sub(meta.block_number));
    health.lag = timestamp.map(|ts| Duration::from_secs(now_unix.saturating_sub(ts)));

    // Most specific first. A wrong deployment makes every other number about the wrong thing.
    if let Some(expected) = &health.deployment {
        if &meta.deployment != expected {
            health.status = SubgraphStatus::Down(SubgraphFault::WrongDeployment {
                expected: expected.clone(),
                actual: meta.deployment.clone(),
            });
            return;
        }
    }
    if meta.has_indexing_errors {
        health.status = SubgraphStatus::Down(SubgraphFault::IndexingErrors);
        return;
    }
    if let Some(lag) = health.lag {
        if lag > max_lag {
            health.status = SubgraphStatus::Down(SubgraphFault::Stale { lag, max_lag });
        }
    }
}

enum QueryError {
    /// Our credential was the problem, not the subgraph.
    Auth(String),
    Failed(String),
}

/// The gateway answers key problems with HTTP 200 and an `auth error: …` message, so the
/// status code alone cannot tell "our key is wrong" from "this subgraph is broken".
fn classify(status: u16, reason: String) -> QueryError {
    if status == 401 || status == 403 || reason.to_ascii_lowercase().starts_with("auth error") {
        QueryError::Auth(reason)
    } else {
        QueryError::Failed(reason)
    }
}

async fn query_meta(http: &reqwest::Client, endpoint: &Endpoint) -> Result<Meta, QueryError> {
    let mut request = http
        .post(&endpoint.url)
        .header("content-type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .body(META_QUERY);
    if let Some(bearer) = &endpoint.bearer {
        request = request.bearer_auth(bearer.expose());
    }

    // The error text can carry the URL; the key is in a header, so the URL is safe to show.
    let response = request
        .send()
        .await
        .map_err(|e| QueryError::Failed(e.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| QueryError::Failed(e.to_string()))?;

    // A gateway error body usually says more than the status code does.
    match parse_meta(&body) {
        Ok(meta) => Ok(meta),
        Err(reason) if status.is_success() => Err(classify(status.as_u16(), reason)),
        Err(_) => Err(classify(
            status.as_u16(),
            format!("HTTP {status}: {}", truncate(&body, 200)),
        )),
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

    fn check(deployment: Option<&str>, max_lag_secs: u64) -> SubgraphCheck {
        SubgraphCheck {
            network: "arbitrum-one".into(),
            subgraph: "network".into(),
            deployment: deployment.map(str::to_string),
            endpoint: Err("unused".into()),
            max_lag: Duration::from_secs(max_lag_secs),
        }
    }

    fn verdict(
        meta: &Meta,
        deployment: Option<&str>,
        head: Option<u64>,
        now: u64,
        max_lag_secs: u64,
    ) -> SubgraphHealth {
        let check = check(deployment, max_lag_secs);
        let mut health = SubgraphHealth::new(&check);
        judge(
            &mut health,
            meta,
            meta.block_timestamp,
            head.map(|number| PinnedBlock {
                number,
                timestamp: now,
            }),
            now,
            check.max_lag,
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
    fn a_key_problem_is_ours_not_the_subgraphs() {
        // What the gateway actually sends for a bad key: HTTP 200, auth error message.
        assert!(matches!(
            classify(200, "auth error: API key not found".into()),
            QueryError::Auth(_)
        ));
        assert!(matches!(classify(401, "nope".into()), QueryError::Auth(_)));
        assert!(matches!(
            classify(200, "subgraph not found: abc".into()),
            QueryError::Failed(_)
        ));
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
        assert!(h.is_up(), "{:?}", h.status);
        assert_eq!(h.blocks_behind, Some(10));
        assert_eq!(h.lag, Some(Duration::from_secs(30)));
        assert_eq!(h.indexed_block, Some(990));
        assert_eq!(h.served_deployment.as_deref(), Some(DEPLOYMENT));
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
            matches!(h.status, SubgraphStatus::Down(SubgraphFault::Stale { .. })),
            "{:?}",
            h.status
        );
        // The numbers are still reported: a stale subgraph's lag is exactly what to graph.
        assert_eq!(h.blocks_behind, Some(4_900));
    }

    #[test]
    fn indexing_errors_are_a_fault() {
        let h = verdict(&meta(990, Some(1_000), true), None, Some(1_000), 1_000, 600);
        assert_eq!(
            h.status,
            SubgraphStatus::Down(SubgraphFault::IndexingErrors)
        );
    }

    #[test]
    fn a_listed_deployment_must_be_the_one_that_answers() {
        let h = verdict(
            &meta(990, Some(1_000), false),
            Some("QmOther"),
            None,
            1_000,
            600,
        );
        assert!(matches!(
            h.status,
            SubgraphStatus::Down(SubgraphFault::WrongDeployment { .. })
        ));
        assert_eq!(h.name(), "network@QmOther");
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
    fn as_a_source_not_checked_means_down() {
        // For a subgraph the tool reads from, "we could not ask" still means "we cannot see".
        let mut h = verdict(&meta(990, Some(1_000), false), None, None, 1_030, 600);
        let up = h
            .clone()
            .into_source_health(crate::source::SourceId::subgraph("monitor"));
        assert!(up.is_up());
        assert_eq!(up.head.map(|b| (b.number, b.timestamp)), Some((990, 1_000)));
        assert_eq!(up.id.as_str(), "subgraph:monitor");

        h.status = SubgraphStatus::NotChecked("no key".into());
        let down = h.into_source_health(crate::source::SourceId::subgraph("monitor"));
        assert!(!down.is_up());
        assert!(down.summary().contains("no key"), "{}", down.summary());
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
