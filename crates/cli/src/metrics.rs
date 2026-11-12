//! Prometheus export.
//!
//! The tool publishes what it saw; Grafana decides what is acceptable. There are deliberately
//! no `*_breach` or `*_threshold` series for alerting — see `specs/CHECKS_PLAN.md`, decision 8.

use std::net::SocketAddr;
use std::sync::OnceLock;

use axum::{http::StatusCode, routing::get, Router};
use graph_monitor_core::check::{Reading, Subject, Unit};
use graph_monitor_core::checks::{
    M_ACTIVITY_SEEN, M_ALLOWANCE, M_ALLOWANCE_EXACT_MAX, M_ALLOWANCE_UNLIMITED, M_ETH_BALANCE,
    M_EVER_TRANSACTED, M_GRT_BALANCE, M_IDLE_SECONDS, M_REMEDIATION_BELOW,
};
use graph_monitor_core::engine::CycleReport;
use graph_monitor_core::remediate::RemediationReport;
use graph_monitor_core::solo::{M_SOLO_DEPLOYMENTS, M_SOLO_SIGNAL};
use graph_monitor_core::subgraph::{
    M_SUBGRAPH_BLOCKS_BEHIND, M_SUBGRAPH_BLOCK_HEIGHT, M_SUBGRAPH_LAG, M_SUBGRAPH_UP,
};
use prometheus::{
    register_gauge, register_gauge_vec, register_histogram, register_histogram_vec,
    register_int_counter_vec, register_int_gauge_vec, Gauge, GaugeVec, Histogram, HistogramVec,
    IntCounterVec, IntGaugeVec, TextEncoder,
};

/// A cycle spans seconds to minutes — activity checks dominate and scale with how far back
/// they have to look. Prometheus' default buckets top out at 10s, which would put every real
/// cycle in `+Inf` and make the histogram useless.
const CYCLE_BUCKETS: &[f64] = &[0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0];

/// A single check: a batched balance read is milliseconds, an exhaustive activity search is
/// tens of seconds.
const CHECK_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];
use tokio::net::TcpListener;

// Every series is labelled by its subject, so the labels are the subject's identity. Order
// matters: it must match `subject_labels`.
const ACCOUNT_LABELS: &[&str] = &["network", "group", "account", "address"];
const ALLOWANCE_LABELS: &[&str] = &["network", "group", "owner", "spender"];
/// `deployment` is empty for the published version, and the `Qm…` id for a listed deployment.
const SUBGRAPH_LABELS: &[&str] = &["network", "subgraph", "deployment"];
const INDEXER_LABELS: &[&str] = &["network", "indexer"];
const NETWORK_LABELS: &[&str] = &["network"];

/// The values of `labels_for(subject.kind())`, in order.
fn subject_labels(network: &str, subject: &Subject) -> Vec<String> {
    let mut out = vec![network.to_string()];
    match subject {
        Subject::Account {
            account, address, ..
        } => out.extend([
            subject.group_or_ungrouped().to_string(),
            account.clone(),
            address.to_string(),
        ]),
        Subject::Allowance { owner, spender, .. } => out.extend([
            subject.group_or_ungrouped().to_string(),
            owner.clone(),
            spender.clone(),
        ]),
        Subject::Subgraph {
            subgraph,
            deployment,
        } => out.extend([subgraph.clone(), deployment.clone().unwrap_or_default()]),
        Subject::Indexer { indexer } => out.push(format!("{indexer:#x}")),
        Subject::Network => {}
    }
    out
}

fn labels_for(kind: &str) -> &'static [&'static str] {
    match kind {
        "account" => ACCOUNT_LABELS,
        "allowance" => ALLOWANCE_LABELS,
        "subgraph" => SUBGRAPH_LABELS,
        "indexer" => INDEXER_LABELS,
        _ => NETWORK_LABELS,
    }
}

/// Whether one subject kind's checks could read, and what they found wrong. Per subject kind
/// so the series join on the same labels as that subject's values.
pub struct CheckHealth {
    /// `gm_<kind>_readable{<subject>, check}`
    pub readable: IntGaugeVec,
    /// `gm_<kind>_fault{<subject>, check, kind}`
    pub fault: IntGaugeVec,
}

impl CheckHealth {
    fn new(kind: &str) -> Self {
        let mut labels = labels_for(kind).to_vec();
        labels.push("check");
        let readable = register_int_gauge_vec!(
            format!("gm_{kind}_readable"),
            format!(
                "1 when the check read this {kind}, 0 when it could not. Unreadable is distinct \
                 from any value, including zero: no value series is published for it"
            ),
            &labels
        )
        .unwrap();
        labels.push("kind");
        let fault = register_int_gauge_vec!(
            format!("gm_{kind}_fault"),
            format!(
                "1 for what is wrong with this {kind}: why the check could not read it \
                 (read_failed, source_unavailable, not_configured, key_rejected) or what it \
                 judged wrong once it did (e.g. unreachable, stale, indexing_errors). Absent \
                 while healthy"
            ),
            &labels
        )
        .unwrap();
        Self { readable, fault }
    }

    fn reset(&self) {
        self.readable.reset();
        self.fault.reset();
    }
}

pub struct Metrics {
    // Account checks.
    pub eth_balance: GaugeVec,
    pub grt_balance: GaugeVec,
    pub idle_seconds: GaugeVec,
    pub ever_transacted: IntGaugeVec,
    pub activity_seen: IntGaugeVec,
    pub remediation_threshold: GaugeVec,

    // Allowance checks. The flags are exact classifications a float cannot carry.
    pub allowance: GaugeVec,
    pub allowance_unlimited: IntGaugeVec,
    pub allowance_exact_max: IntGaugeVec,

    // Subgraph checks.
    pub subgraph_up: IntGaugeVec,
    pub subgraph_block_height: IntGaugeVec,
    pub subgraph_blocks_behind: IntGaugeVec,
    pub subgraph_lag_seconds: GaugeVec,

    // Network checks, measured per indexer.
    pub solo_deployments: IntGaugeVec,
    pub solo_signal: GaugeVec,

    // Readability and faults, per subject kind.
    pub account_health: CheckHealth,
    pub allowance_health: CheckHealth,
    pub subgraph_health: CheckHealth,
    pub network_health: CheckHealth,
    pub check_duration: HistogramVec,
    pub check_errors: IntCounterVec,

    // Source health.
    pub source_up: IntGaugeVec,
    pub source_lag_seconds: GaugeVec,
    pub source_block_height: IntGaugeVec,
    pub source_fault: IntGaugeVec,

    // Remediation.
    pub remediation_total: IntCounterVec,

    // Liveness and provenance.
    pub block_height: IntGaugeVec,
    pub cycle_last_success: Gauge,
    pub cycle_duration: Histogram,
    pub build_info: IntGaugeVec,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn metrics() -> &'static Metrics {
    METRICS.get_or_init(Metrics::new)
}

impl Metrics {
    fn new() -> Self {
        Self {
            eth_balance: register_gauge_vec!(
                "gm_account_eth_balance",
                "ETH balance of an account",
                ACCOUNT_LABELS
            )
            .unwrap(),
            grt_balance: register_gauge_vec!(
                "gm_account_grt_balance",
                "GRT balance of an account",
                ACCOUNT_LABELS
            )
            .unwrap(),
            idle_seconds: register_gauge_vec!(
                "gm_account_idle_seconds",
                "Seconds since the account last acted, at the pinned block",
                ACCOUNT_LABELS
            )
            .unwrap(),
            ever_transacted: register_int_gauge_vec!(
                "gm_account_ever_transacted",
                "1 when the account has ever sent a transaction",
                ACCOUNT_LABELS
            )
            .unwrap(),
            activity_seen: register_int_gauge_vec!(
                "gm_account_activity_seen",
                "1 when the activity search found something inside its window. For an event \
                 check, 0 means nothing within the lookback — not that the account never acted",
                ACCOUNT_LABELS
            )
            .unwrap(),
            remediation_threshold: register_gauge_vec!(
                "gm_account_remediation_below",
                "Balance below which the tool tops this account up. Exported whenever \
                 remediation is configured, not only when it fires, so a dashboard can draw \
                 the line while the balance is still healthy. Not an alert threshold — \
                 alerting lives in Grafana",
                &["network", "group", "account", "asset"]
            )
            .unwrap(),
            allowance: register_gauge_vec!(
                "gm_allowance_grt",
                "GRT allowance granted by owner to spender. Lossy above 2^53 — use \
                 gm_allowance_unlimited to test for an inexhaustible approval",
                ALLOWANCE_LABELS
            )
            .unwrap(),
            allowance_unlimited: register_int_gauge_vec!(
                "gm_allowance_unlimited",
                "1 when the allowance is effectively inexhaustible. Computed on exact integers, \
                 because the gauge cannot represent 2^256 faithfully",
                ALLOWANCE_LABELS
            )
            .unwrap(),
            allowance_exact_max: register_int_gauge_vec!(
                "gm_allowance_exact_max",
                "1 when the allowance is exactly uint256 max — an infinite approval never drawn \
                 against",
                ALLOWANCE_LABELS
            )
            .unwrap(),
            subgraph_up: register_int_gauge_vec!(
                "gm_subgraph_up",
                "1 when the subgraph answered, from the expected deployment, without indexing \
                 errors and within max_lag. Absent when it could not be checked — see \
                 gm_subgraph_readable",
                SUBGRAPH_LABELS
            )
            .unwrap(),
            subgraph_block_height: register_int_gauge_vec!(
                "gm_subgraph_block_height",
                "Latest block the subgraph has indexed",
                SUBGRAPH_LABELS
            )
            .unwrap(),
            subgraph_blocks_behind: register_int_gauge_vec!(
                "gm_subgraph_blocks_behind",
                "Blocks between the subgraph's latest indexed block and the RPC's head. Absent \
                 when the RPC failed verification, since there is no head to measure against",
                SUBGRAPH_LABELS
            )
            .unwrap(),
            subgraph_lag_seconds: register_gauge_vec!(
                "gm_subgraph_lag_seconds",
                "Age of the subgraph's latest indexed block against wall clock",
                SUBGRAPH_LABELS
            )
            .unwrap(),
            solo_deployments: register_int_gauge_vec!(
                "gm_indexer_solo_deployments",
                "Deployments this indexer is the only one allocated to, that have curation \
                 signal and are not denied rewards. Indexers with none are absent",
                INDEXER_LABELS
            )
            .unwrap(),
            solo_signal: register_gauge_vec!(
                "gm_indexer_solo_signal_grt",
                "GRT signalled on this indexer's solo deployments",
                INDEXER_LABELS
            )
            .unwrap(),
            account_health: CheckHealth::new("account"),
            allowance_health: CheckHealth::new("allowance"),
            subgraph_health: CheckHealth::new("subgraph"),
            network_health: CheckHealth::new("network"),
            check_duration: register_histogram_vec!(
                prometheus::HistogramOpts::new(
                    "gm_check_duration_seconds",
                    "Wall time of one check, by kind"
                )
                .buckets(CHECK_BUCKETS.to_vec()),
                &["network", "check"]
            )
            .unwrap(),
            check_errors: register_int_counter_vec!(
                "gm_check_errors_total",
                "Checks that could not read, by kind. Rising without gm_source_up dropping \
                 usually means a provider limit rather than an outage",
                &["network", "check"]
            )
            .unwrap(),
            source_up: register_int_gauge_vec!(
                "gm_source_up",
                "1 when the data source passed verification",
                &["network", "source"]
            )
            .unwrap(),
            source_lag_seconds: register_gauge_vec!(
                "gm_source_lag_seconds",
                "How far the source's head trails wall clock",
                &["network", "source"]
            )
            .unwrap(),
            source_block_height: register_int_gauge_vec!(
                "gm_source_block_height",
                "Head block height reported by the source",
                &["network", "source"]
            )
            .unwrap(),
            source_fault: register_int_gauge_vec!(
                "gm_source_fault",
                "1 for the kind of fault that has a source down: unreachable, chain_id_mismatch, \
                 stale, state_unavailable, wrong_deployment, indexing_errors, not_configured or \
                 key_rejected. Absent while the source is up",
                &["network", "source", "kind"]
            )
            .unwrap(),
            remediation_total: register_int_counter_vec!(
                "gm_remediation_total",
                "Remediation attempts by outcome",
                &["network", "account", "asset", "outcome"]
            )
            .unwrap(),
            block_height: register_int_gauge_vec!(
                "gm_block_height",
                "Block every value in the last cycle was read at",
                &["network"]
            )
            .unwrap(),
            cycle_last_success: register_gauge!(
                "gm_cycle_last_success_timestamp",
                "Unix time of the last cycle that read anything. Alert on time() - this: it \
                 catches a process that is alive but wedged, which `up` cannot"
            )
            .unwrap(),
            cycle_duration: register_histogram!(prometheus::HistogramOpts::new(
                "gm_cycle_duration_seconds",
                "Wall time of one full cycle"
            )
            .buckets(CYCLE_BUCKETS.to_vec()))
            .unwrap(),
            build_info: register_int_gauge_vec!(
                "gm_build_info",
                "Always 1. Carries the running version so a change in behaviour can be lined \
                 up against a deploy",
                &["version"]
            )
            .unwrap(),
        }
    }

    /// Publish one cycle.
    ///
    /// Every labelled family is reset first. Without it, a subject removed from config keeps
    /// exporting its last value forever, and a dashboard shows a wallet that is no longer
    /// checked as though it were healthy. It also makes a value that could not be measured
    /// this cycle absent rather than last cycle's.
    pub fn publish(&self, report: &CycleReport) {
        for family in [
            &self.eth_balance,
            &self.grt_balance,
            &self.idle_seconds,
            &self.remediation_threshold,
            &self.allowance,
            &self.subgraph_lag_seconds,
            &self.solo_signal,
            &self.source_lag_seconds,
        ] {
            family.reset();
        }
        for family in [
            &self.ever_transacted,
            &self.activity_seen,
            &self.allowance_unlimited,
            &self.allowance_exact_max,
            &self.subgraph_up,
            &self.subgraph_block_height,
            &self.subgraph_blocks_behind,
            &self.solo_deployments,
            &self.source_up,
            &self.source_block_height,
            &self.source_fault,
            &self.block_height,
        ] {
            family.reset();
        }
        for health in self.healths() {
            health.reset();
        }

        self.build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);

        let sources = report
            .networks
            .iter()
            .map(|n| &n.source)
            .chain(&report.subgraph_sources);
        for source in sources {
            // Each under the chain it serves.
            let labels = [source.network.as_str(), source.id.as_str()];
            self.source_up
                .with_label_values(&labels)
                .set(i64::from(source.is_up()));
            if let Some(fault) = &source.fault {
                self.source_fault
                    .with_label_values(&[labels[0], labels[1], fault.kind()])
                    .set(1);
            }
            if let Some(lag) = source.lag {
                self.source_lag_seconds
                    .with_label_values(&labels)
                    .set(lag.as_secs_f64());
            }
            if let Some(head) = source.head {
                self.source_block_height
                    .with_label_values(&labels)
                    .set(head.number as i64);
            }
        }

        for net in &report.networks {
            if let Some(block) = net.block {
                self.block_height
                    .with_label_values(&[net.network.as_str()])
                    .set(block.number as i64);
            }
        }
        self.cycle_duration.observe(report.duration.as_secs_f64());

        for reading in &report.readings {
            self.publish_reading(reading);
        }
    }

    fn healths(&self) -> [&CheckHealth; 4] {
        [
            &self.account_health,
            &self.allowance_health,
            &self.subgraph_health,
            &self.network_health,
        ]
    }

    fn health_for(&self, subject: &Subject) -> &CheckHealth {
        match subject {
            Subject::Account { .. } => &self.account_health,
            Subject::Allowance { .. } => &self.allowance_health,
            Subject::Subgraph { .. } => &self.subgraph_health,
            Subject::Indexer { .. } | Subject::Network => &self.network_health,
        }
    }

    fn publish_reading(&self, reading: &Reading) {
        let net = reading.network.as_str();
        let check = reading.id.kind;
        self.check_duration
            .with_label_values(&[net, check])
            .observe(reading.duration.as_secs_f64());

        let health = self.health_for(&reading.subject);
        let mut labels = subject_labels(net, &reading.subject);
        labels.push(check.to_string());

        let observation = match &reading.observation {
            Err(unreadable) => {
                health.readable.with_label_values(&as_refs(&labels)).set(0);
                labels.push(unreadable.kind.to_string());
                health.fault.with_label_values(&as_refs(&labels)).set(1);
                self.check_errors.with_label_values(&[net, check]).inc();
                // No value is published for a failed read. Exporting a zero here is exactly
                // how an endpoint outage becomes indistinguishable from an empty wallet.
                return;
            }
            Ok(observation) => observation,
        };
        health.readable.with_label_values(&as_refs(&labels)).set(1);
        if let Some(fault) = &observation.fault {
            labels.push(fault.kind.to_string());
            health.fault.with_label_values(&as_refs(&labels)).set(1);
        }

        for m in &observation.measurements {
            let values = subject_labels(net, &m.subject);
            let l = as_refs(&values);
            match m.name {
                M_ETH_BALANCE => self.eth_balance.with_label_values(&l).set(m.value),
                M_GRT_BALANCE => self.grt_balance.with_label_values(&l).set(m.value),
                M_IDLE_SECONDS => self.idle_seconds.with_label_values(&l).set(m.value),
                M_EVER_TRANSACTED => self
                    .ever_transacted
                    .with_label_values(&l)
                    .set(m.value as i64),
                M_ACTIVITY_SEEN => self.activity_seen.with_label_values(&l).set(m.value as i64),
                M_REMEDIATION_BELOW => self
                    .remediation_threshold
                    .with_label_values(&[l[0], l[1], l[2], unit_asset(m.unit)])
                    .set(m.value),
                M_ALLOWANCE => self.allowance.with_label_values(&l).set(m.value),
                M_ALLOWANCE_UNLIMITED => self
                    .allowance_unlimited
                    .with_label_values(&l)
                    .set(m.value as i64),
                M_ALLOWANCE_EXACT_MAX => self
                    .allowance_exact_max
                    .with_label_values(&l)
                    .set(m.value as i64),
                M_SUBGRAPH_UP => self.subgraph_up.with_label_values(&l).set(m.value as i64),
                M_SUBGRAPH_BLOCK_HEIGHT => self
                    .subgraph_block_height
                    .with_label_values(&l)
                    .set(m.value as i64),
                M_SUBGRAPH_BLOCKS_BEHIND => self
                    .subgraph_blocks_behind
                    .with_label_values(&l)
                    .set(m.value as i64),
                M_SUBGRAPH_LAG => self.subgraph_lag_seconds.with_label_values(&l).set(m.value),
                M_SOLO_DEPLOYMENTS => self
                    .solo_deployments
                    .with_label_values(&l)
                    .set(m.value as i64),
                M_SOLO_SIGNAL => self.solo_signal.with_label_values(&l).set(m.value),
                other => {
                    debug_assert!(false, "unmapped measurement {other}");
                    tracing::warn!(measurement = other, "measurement has no metric");
                }
            }
        }
    }

    /// Record what the executor actually did.
    pub fn publish_remediations(&self, network: &str, reports: &[RemediationReport]) {
        for r in reports {
            self.remediation_total
                .with_label_values(&[network, &r.account, r.asset.as_str(), r.outcome.as_str()])
                .inc();
        }
    }

    /// Materialise counter series so a panel shows zero rather than "No data".
    ///
    /// `inc_by(0)` rather than `inc()`: incrementing would start every counter at one, which
    /// skews `rate()` and makes "no failures yet" read as one failure.
    /// `armed` is the (account, asset) pairs that actually have remediation configured.
    /// Priming every account would publish 0-valued counters implying repairs exist where none
    /// are configured — worse than a panel reading "No data".
    pub fn prime(&self, network: &str, armed: &[(String, String)]) {
        for kind in CHECK_KINDS {
            self.check_errors
                .with_label_values(&[network, kind])
                .inc_by(0);
        }
        for (account, asset) in armed {
            for outcome in ["sent", "would_send", "skipped", "failed"] {
                self.remediation_total
                    .with_label_values(&[network, account, asset, outcome])
                    .inc_by(0);
            }
        }
    }
}

fn as_refs(values: &[String]) -> Vec<&str> {
    values.iter().map(String::as_str).collect()
}

/// Every check kind, for priming counters.
pub const CHECK_KINDS: &[&str] = &[
    "eth_balance",
    "grt_balance",
    "allowance",
    "activity",
    "subgraph",
    graph_monitor_core::solo::KIND,
];

/// Metric label for the asset a measurement describes.
fn unit_asset(unit: Unit) -> &'static str {
    match unit {
        Unit::Eth => "eth",
        Unit::Grt => "grt",
        _ => "",
    }
}

async fn handler() -> (StatusCode, String) {
    match TextEncoder::new().encode_to_string(&prometheus::gather()) {
        Ok(body) => (StatusCode::OK, body),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not encode metrics: {e}"),
        ),
    }
}

/// Serve `/metrics` on its own port, separate from anything else this process does.
pub async fn serve(port: u16) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/metrics", get(handler))
        .fallback(|| async { (StatusCode::NOT_FOUND, "not found") });
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "metrics server listening");
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}
