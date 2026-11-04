//! Prometheus export.
//!
//! The tool publishes what it saw; Grafana decides what is acceptable. There are deliberately
//! no `*_breach` or `*_threshold` series for alerting — see `specs/CHECKS_PLAN.md`, decision 8.

use std::net::SocketAddr;
use std::sync::OnceLock;

use axum::{http::StatusCode, routing::get, Router};
use graph_monitor_core::engine::CycleReport;
use graph_monitor_core::remediate::RemediationReport;
use graph_monitor_core::watches::{
    M_ACTIVITY_SEEN, M_ALLOWANCE, M_ALLOWANCE_EXACT_MAX, M_ALLOWANCE_UNLIMITED, M_ETH_BALANCE,
    M_EVER_TRANSACTED, M_GRT_BALANCE, M_IDLE_SECONDS, M_REMEDIATION_BELOW,
};
use prometheus::{
    register_gauge, register_gauge_vec, register_histogram, register_histogram_vec,
    register_int_counter_vec, register_int_gauge_vec, Gauge, GaugeVec, Histogram, HistogramVec,
    IntCounterVec, IntGaugeVec, TextEncoder,
};

/// A cycle spans seconds to minutes — activity watches dominate and scale with how far back
/// they have to look. Prometheus' default buckets top out at 10s, which would put every real
/// cycle in `+Inf` and make the histogram useless.
const CYCLE_BUCKETS: &[f64] = &[0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0];

/// A single watch: a batched balance read is milliseconds, an exhaustive activity search is
/// tens of seconds.
const READ_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];
use tokio::net::TcpListener;

/// Labels carried by every per-account series. Order matters — it must match the order values
/// are supplied in.
const ACCOUNT_LABELS: &[&str] = &["network", "group", "account", "address"];
const ALLOWANCE_LABELS: &[&str] = &["network", "group", "owner", "spender"];
const WATCH_LABELS: &[&str] = &["network", "group", "account", "watch"];
/// `deployment` is empty for the published version, and the `Qm…` id for a listed deployment.
const SUBGRAPH_LABELS: &[&str] = &["network", "subgraph", "deployment"];

pub struct Metrics {
    // Observed state.
    pub eth_balance: GaugeVec,
    pub grt_balance: GaugeVec,
    pub allowance: GaugeVec,
    pub idle_seconds: GaugeVec,

    // Exact classifications a float cannot carry.
    pub allowance_unlimited: IntGaugeVec,
    pub allowance_exact_max: IntGaugeVec,
    pub ever_transacted: IntGaugeVec,
    pub activity_seen: IntGaugeVec,

    // Readability, not judgment.
    pub read_ok: IntGaugeVec,
    pub read_unknown: IntGaugeVec,
    pub read_duration: HistogramVec,
    pub read_errors: IntCounterVec,

    // Source health.
    pub source_up: IntGaugeVec,
    pub source_lag_seconds: GaugeVec,
    pub source_block_height: IntGaugeVec,

    // Subgraphs, as monitored components of the network. Not sources: see `subgraph.rs`.
    pub subgraph_up: IntGaugeVec,
    pub subgraph_not_checked: IntGaugeVec,
    pub subgraph_block_height: IntGaugeVec,
    pub subgraph_blocks_behind: IntGaugeVec,
    pub subgraph_lag_seconds: GaugeVec,

    // Remediation.
    pub remediation_threshold: GaugeVec,
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
                "ETH balance of a watched account",
                ACCOUNT_LABELS
            )
            .unwrap(),
            grt_balance: register_gauge_vec!(
                "gm_account_grt_balance",
                "GRT balance of a watched account",
                ACCOUNT_LABELS
            )
            .unwrap(),
            allowance: register_gauge_vec!(
                "gm_account_allowance_grt",
                "GRT allowance granted by owner to spender. Lossy above 2^53 — use \
                 gm_account_allowance_unlimited to test for an inexhaustible approval",
                ALLOWANCE_LABELS
            )
            .unwrap(),
            idle_seconds: register_gauge_vec!(
                "gm_account_idle_seconds",
                "Seconds since the account last sent a transaction, at the pinned block",
                ACCOUNT_LABELS
            )
            .unwrap(),
            allowance_unlimited: register_int_gauge_vec!(
                "gm_account_allowance_unlimited",
                "1 when the allowance is effectively inexhaustible. Computed on exact integers, \
                 because the gauge cannot represent 2^256 faithfully",
                ALLOWANCE_LABELS
            )
            .unwrap(),
            allowance_exact_max: register_int_gauge_vec!(
                "gm_account_allowance_exact_max",
                "1 when the allowance is exactly uint256 max — an infinite approval never drawn \
                 against",
                ALLOWANCE_LABELS
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
                 watch, 0 means nothing within the lookback — not that the account never acted",
                ACCOUNT_LABELS
            )
            .unwrap(),
            read_duration: register_histogram_vec!(
                prometheus::HistogramOpts::new(
                    "gm_read_duration_seconds",
                    "Wall time of one watch, by kind"
                )
                .buckets(READ_BUCKETS.to_vec()),
                &["network", "watch"]
            )
            .unwrap(),
            read_errors: register_int_counter_vec!(
                "gm_read_errors_total",
                "Watches that could not read, by kind. Rising without gm_source_up dropping \
                 usually means a provider limit rather than an outage",
                &["network", "watch"]
            )
            .unwrap(),
            build_info: register_int_gauge_vec!(
                "gm_build_info",
                "Always 1. Carries the running version so a change in behaviour can be lined \
                 up against a deploy",
                &["version"]
            )
            .unwrap(),
            read_ok: register_int_gauge_vec!(
                "gm_read_ok",
                "1 when this watch read its value successfully",
                WATCH_LABELS
            )
            .unwrap(),
            read_unknown: register_int_gauge_vec!(
                "gm_read_unknown",
                "1 when this watch could not read its value. Distinct from any value, including \
                 zero",
                WATCH_LABELS
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
            subgraph_up: register_int_gauge_vec!(
                "gm_subgraph_up",
                "1 when the subgraph answered, from the expected deployment, without indexing \
                 errors and within max_lag. Absent when it was not checked — see \
                 gm_subgraph_not_checked",
                SUBGRAPH_LABELS
            )
            .unwrap(),
            subgraph_not_checked: register_int_gauge_vec!(
                "gm_subgraph_not_checked",
                "1 when the subgraph could not be checked for a reason on our side, such as a \
                 missing or rejected API key. Says nothing about the subgraph itself",
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
            remediation_threshold: register_gauge_vec!(
                "gm_account_remediation_below",
                "Balance below which the tool tops this account up. Exported whenever \
                 remediation is configured, not only when it fires, so a dashboard can draw \
                 the line while the balance is still healthy. Not an alert threshold — \
                 alerting lives in Grafana",
                &["network", "group", "account", "asset"]
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
        }
    }

    /// Publish one cycle.
    ///
    /// Every labelled family is reset first. Without it, an account removed from config keeps
    /// exporting its last value forever, and a dashboard shows a wallet that is no longer
    /// watched as though it were healthy.
    pub fn publish(&self, report: &CycleReport) {
        self.eth_balance.reset();
        self.grt_balance.reset();
        self.allowance.reset();
        self.idle_seconds.reset();
        self.allowance_unlimited.reset();
        self.allowance_exact_max.reset();
        self.ever_transacted.reset();
        self.activity_seen.reset();
        self.read_ok.reset();
        self.read_unknown.reset();
        // Reset too: a trigger removed from config must stop drawing a line on a dashboard.
        self.remediation_threshold.reset();

        self.build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);

        // Reset with the rest: a network removed from config must stop exporting.
        self.source_up.reset();
        self.source_lag_seconds.reset();
        self.source_block_height.reset();
        self.block_height.reset();

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

        // Reset so a subgraph removed from config stops exporting, and so a number that could
        // not be measured this cycle is absent rather than last cycle's.
        self.subgraph_up.reset();
        self.subgraph_not_checked.reset();
        self.subgraph_block_height.reset();
        self.subgraph_blocks_behind.reset();
        self.subgraph_lag_seconds.reset();
        for sg in &report.subgraphs {
            // The chain the subgraph indexes, which need not be the one this process reads.
            let labels = [
                sg.network.as_str(),
                sg.subgraph.as_str(),
                sg.deployment.as_deref().unwrap_or(""),
            ];
            self.subgraph_not_checked
                .with_label_values(&labels)
                .set(i64::from(sg.is_not_checked()));
            // Not checked publishes no verdict: a missing key must not read as a down subgraph.
            if sg.is_not_checked() {
                continue;
            }
            self.subgraph_up
                .with_label_values(&labels)
                .set(i64::from(sg.is_up()));
            if let Some(block) = sg.indexed_block {
                self.subgraph_block_height
                    .with_label_values(&labels)
                    .set(block as i64);
            }
            if let Some(behind) = sg.blocks_behind {
                self.subgraph_blocks_behind
                    .with_label_values(&labels)
                    .set(behind as i64);
            }
            if let Some(lag) = sg.lag {
                self.subgraph_lag_seconds
                    .with_label_values(&labels)
                    .set(lag.as_secs_f64());
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
            // Each reading under its own network.
            let net = reading.labels.network.as_str();
            let group = reading.labels.group_or_ungrouped();
            let account = reading.labels.account.as_deref().unwrap_or("");
            let address = reading
                .labels
                .address
                .map(|a| a.to_string())
                .unwrap_or_default();
            let watch_labels = [net, group, account, reading.id.kind];

            self.read_duration
                .with_label_values(&[net, reading.id.kind])
                .observe(reading.duration.as_secs_f64());

            if reading.is_unreadable() {
                self.read_unknown.with_label_values(&watch_labels).set(1);
                self.read_ok.with_label_values(&watch_labels).set(0);
                self.read_errors
                    .with_label_values(&[net, reading.id.kind])
                    .inc();
                // No value is published for a failed read. Exporting a zero here is exactly
                // how an endpoint outage becomes indistinguishable from an empty wallet.
                continue;
            }
            self.read_ok.with_label_values(&watch_labels).set(1);
            self.read_unknown.with_label_values(&watch_labels).set(0);

            let Ok(observation) = &reading.observation else {
                continue;
            };
            let account_labels = [net, group, account, address.as_str()];
            let spender = reading.labels.spender.as_deref().unwrap_or("");
            let allowance_labels = [net, group, account, spender];

            for m in &observation.measurements {
                match m.name {
                    M_ETH_BALANCE => self
                        .eth_balance
                        .with_label_values(&account_labels)
                        .set(m.value),
                    M_GRT_BALANCE => self
                        .grt_balance
                        .with_label_values(&account_labels)
                        .set(m.value),
                    M_IDLE_SECONDS => self
                        .idle_seconds
                        .with_label_values(&account_labels)
                        .set(m.value),
                    M_ALLOWANCE => self
                        .allowance
                        .with_label_values(&allowance_labels)
                        .set(m.value),
                    M_ALLOWANCE_UNLIMITED => self
                        .allowance_unlimited
                        .with_label_values(&allowance_labels)
                        .set(m.value as i64),
                    M_ALLOWANCE_EXACT_MAX => self
                        .allowance_exact_max
                        .with_label_values(&allowance_labels)
                        .set(m.value as i64),
                    M_ACTIVITY_SEEN => self
                        .activity_seen
                        .with_label_values(&account_labels)
                        .set(m.value as i64),
                    M_REMEDIATION_BELOW => self
                        .remediation_threshold
                        .with_label_values(&[net, group, account, unit_asset(m.unit)])
                        .set(m.value),
                    M_EVER_TRANSACTED => self
                        .ever_transacted
                        .with_label_values(&account_labels)
                        .set(m.value as i64),
                    other => {
                        debug_assert!(false, "unmapped measurement {other}");
                        tracing::warn!(measurement = other, "measurement has no metric");
                    }
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
        for kind in ["eth_balance", "grt_balance", "allowance", "activity"] {
            self.read_errors
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

/// Metric label for the asset a measurement describes.
fn unit_asset(unit: graph_monitor_core::watch::Unit) -> &'static str {
    use graph_monitor_core::watch::Unit;
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
