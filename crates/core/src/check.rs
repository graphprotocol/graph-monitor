//! What a check is, and what running one produces.
//!
//! The tool has two parts: **sources** it reads from (verified every cycle, see `source.rs`),
//! and **checks** it runs against what they serve. Every check is about a **subject** — an
//! account, an allowance, a subgraph, an indexer, or the network as a whole — and the subject
//! decides how a check's results are labelled everywhere they appear.
//!
//! A check reads state and reports it. It does **not** decide whether a value is acceptable —
//! thresholds and alerting belong to Grafana (see `specs/CHECKS_PLAN.md`, decision 8). Two
//! exceptions: remediation needs a trigger to act on, and some checks judge health themselves
//! (a subgraph is down if it is stale), which they report as a [`Fault`].

use std::fmt;
use std::time::Duration;

use alloy::primitives::Address;

use crate::config::RemediationSpec;
use crate::read::{ReadCtx, ReadPlan};
use crate::units::Amount;

/// Identifies a check instance: its kind, plus which subject it is about.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckId {
    /// Stable across instances — `eth_balance`, `allowance`, `subgraph`, `solo_deployments`.
    pub kind: &'static str,
    /// The subject's name: an account label, `owner->spender`, a subgraph, a network.
    pub instance: String,
}

impl CheckId {
    pub fn new(kind: &'static str, instance: impl Into<String>) -> Self {
        Self {
            kind,
            instance: instance.into(),
        }
    }
}

impl fmt::Display for CheckId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.instance)
    }
}

/// What a check, or one of its measurements, is about.
///
/// Concrete variants rather than a label map: these are the subjects the domain has, and each
/// gives a fixed label set, so Prometheus series stay bounded and queries stay predictable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Subject {
    /// A watched account. `group` is a free-form label with no behaviour.
    Account {
        group: Option<String>,
        account: String,
        address: Address,
    },
    /// What `owner` (an account label) lets `spender` move.
    Allowance {
        group: Option<String>,
        owner: String,
        spender: String,
    },
    /// A subgraph's published version, or one listed deployment of it.
    Subgraph {
        subgraph: String,
        deployment: Option<String>,
    },
    /// An indexer, as a protocol participant.
    Indexer { indexer: Address },
    /// The network as a whole: checks about the protocol rather than any one participant.
    Network,
}

impl Subject {
    /// Which kind of subject, as used in metric names (`gm_<kind>_…`).
    pub fn kind(&self) -> &'static str {
        match self {
            Subject::Account { .. } => "account",
            Subject::Allowance { .. } => "allowance",
            Subject::Subgraph { .. } => "subgraph",
            Subject::Indexer { .. } => "indexer",
            Subject::Network => "network",
        }
    }

    /// A short human name, unique within its kind on one network.
    pub fn name(&self) -> String {
        match self {
            Subject::Account { account, .. } => account.clone(),
            Subject::Allowance { owner, spender, .. } => format!("{owner}->{spender}"),
            Subject::Subgraph {
                subgraph,
                deployment: Some(d),
            } => format!("{subgraph}@{d}"),
            Subject::Subgraph { subgraph, .. } => subgraph.clone(),
            Subject::Indexer { indexer } => format!("{indexer:#x}"),
            Subject::Network => "network".to_string(),
        }
    }

    /// The free-form group, for subjects that have one.
    pub fn group(&self) -> Option<&str> {
        match self {
            Subject::Account { group, .. } | Subject::Allowance { group, .. } => group.as_deref(),
            _ => None,
        }
    }

    /// `ungrouped` rather than an absent label, so metric queries never have to handle both
    /// the present and missing cases.
    pub fn group_or_ungrouped(&self) -> &str {
        self.group().unwrap_or("ungrouped")
    }

    pub fn account(&self) -> Option<(&str, Address)> {
        match self {
            Subject::Account {
                account, address, ..
            } => Some((account, *address)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Eth,
    Grt,
    Seconds,
    /// A plain count: blocks, deployments.
    Count,
    /// A 1/0 classification — exact, unlike the value it describes.
    Boolean,
}

impl Unit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Unit::Eth => "ETH",
            Unit::Grt => "GRT",
            Unit::Seconds => "s",
            Unit::Count => "",
            Unit::Boolean => "",
        }
    }
}

/// Which asset a check reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asset {
    Eth,
    Grt,
}

impl Asset {
    pub fn as_str(&self) -> &'static str {
        match self {
            Asset::Eth => "eth",
            Asset::Grt => "grt",
        }
    }
}

/// A numeric observation, suitable for a time series.
///
/// `f64` appears here and nowhere else — every internal amount stays an exact integer in base
/// units until it reaches this boundary. Where that lossiness would destroy meaning (an
/// unlimited allowance is ~1.157e59, well past `f64`'s precision), the check also emits an
/// exact `Boolean` classification alongside the value.
#[derive(Debug, Clone)]
pub struct Measurement {
    pub name: &'static str,
    pub value: f64,
    pub unit: Unit,
    /// What this value is about. Usually the check's own subject; a check about the network
    /// can report one measurement per indexer.
    pub subject: Subject,
}

/// Something wrong that a check judged itself, such as a subgraph past its `max_lag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// A fixed name from a small closed set, so it can be a metric label.
    pub kind: &'static str,
    pub summary: String,
}

/// What a check saw.
#[derive(Debug, Clone)]
pub struct Observation {
    /// One human sentence, for the console. Deliberately not a metric label — it would be
    /// unbounded cardinality.
    pub summary: String,
    pub measurements: Vec<Measurement>,
    /// Set when the check itself judged something wrong. A finding, unlike a plain value.
    pub fault: Option<Fault>,
    /// Text facts for display that are not numbers, e.g. which deployment answered.
    pub facts: Vec<(&'static str, String)>,
}

impl Observation {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            measurements: Vec::new(),
            fault: None,
            facts: Vec::new(),
        }
    }

    pub fn with(mut self, measurement: Measurement) -> Self {
        self.measurements.push(measurement);
        self
    }

    /// Attach a measurement only when it is meaningful. Emitting a placeholder instead would
    /// publish a fact the check cannot actually vouch for.
    pub fn with_if(self, condition: bool, measurement: Measurement) -> Self {
        if condition {
            self.with(measurement)
        } else {
            self
        }
    }

    pub fn with_fault(mut self, kind: &'static str, summary: impl Into<String>) -> Self {
        self.fault = Some(Fault {
            kind,
            summary: summary.into(),
        });
        self
    }

    pub fn with_fact(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.facts.push((name, value.into()));
        self
    }

    pub fn measurement(&self, name: &str) -> Option<&Measurement> {
        self.measurements.iter().find(|m| m.name == name)
    }

    pub fn value(&self, name: &str) -> Option<f64> {
        self.measurement(name).map(|m| m.value)
    }

    pub fn fact(&self, name: &str) -> Option<&str> {
        self.facts
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a check could not look. Kept apart from every value, including zero: an endpoint outage
/// must never be indistinguishable from an empty wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    /// A fixed name from a small closed set: `read_failed`, `source_unavailable`,
    /// `not_configured`, `key_rejected`.
    pub kind: &'static str,
    pub reason: String,
}

impl Unreadable {
    pub fn new(kind: &'static str, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }

    /// The common case: the read itself failed.
    pub fn read_failed(reason: impl Into<String>) -> Self {
        Self::new("read_failed", reason)
    }
}

/// A repair the tool intends to perform, derived from config the tool holds itself.
#[derive(Debug, Clone)]
pub struct RemediationIntent {
    pub asset: Asset,
    /// The balance that triggered it.
    pub observed: Amount,
    /// The configured trigger level.
    pub below: Amount,
    pub top_up_by: Amount,
    /// Where the funds come from, for display. `None` means the signer's own balance.
    pub from: Option<String>,
    /// The same funder, resolved. Carried separately so the executor never has to re-resolve
    /// a name at the moment it is about to move money.
    pub from_address: Option<Address>,
}

impl RemediationIntent {
    pub fn describe(&self) -> String {
        let places = match self.asset {
            Asset::Eth => 4,
            Asset::Grt => 2,
        };
        match &self.from {
            Some(from) => format!(
                "top up {} {} from {from}",
                self.top_up_by.formatted(places),
                self.asset.as_str().to_uppercase()
            ),
            None => format!(
                "top up {} {}",
                self.top_up_by.formatted(places),
                self.asset.as_str().to_uppercase()
            ),
        }
    }
}

/// One check, run once. Either it observed something, or it could not look.
#[derive(Debug, Clone)]
pub struct Reading {
    pub id: CheckId,
    pub network: String,
    pub subject: Subject,
    pub observation: Result<Observation, Unreadable>,
    pub remediation: Option<RemediationIntent>,
    pub duration: Duration,
}

impl Reading {
    pub fn is_unreadable(&self) -> bool {
        self.observation.is_err()
    }

    /// The fault the check judged, if it read fine and found one.
    pub fn fault(&self) -> Option<&Fault> {
        self.observation
            .as_ref()
            .ok()
            .and_then(|o| o.fault.as_ref())
    }

    /// The human sentence, or the reason there isn't one.
    pub fn summary(&self) -> &str {
        match &self.observation {
            Ok(o) => &o.summary,
            Err(u) => &u.reason,
        }
    }

    pub fn value(&self, name: &str) -> Option<f64> {
        self.observation.as_ref().ok().and_then(|o| o.value(name))
    }
}

/// Implemented by each kind of check that reads through a network's RPC at the pinned block.
///
/// `observe` and `remediation` take only the reading — no context, no clock, no network — so
/// both are testable by calling them with a literal.
#[async_trait::async_trait]
pub trait Check: Send + Sync {
    type Reading: Send + Sync;

    fn id(&self) -> CheckId;
    fn network(&self) -> &str;
    fn subject(&self) -> Subject;

    /// Declare reads so the engine can batch them. Purely an optimization: a check that
    /// declares nothing still works, it just costs its own round trips.
    fn plan(&self, _plan: &mut ReadPlan) {}

    /// READ — all I/O, at the pinned block.
    async fn read(&self, ctx: &ReadCtx<'_>) -> Result<Self::Reading, String>;

    /// REPORT — turn the reading into measurements and a sentence. Pure.
    fn observe(&self, reading: &Self::Reading) -> Observation;

    /// REMEDIATE — does this reading call for repair? Pure; the executor performs it.
    fn remediation(&self, _reading: &Self::Reading) -> Option<RemediationIntent> {
        None
    }
}

/// Type-erased check, so a heterogeneous set can be held in one collection.
///
/// Boxed futures (via `async_trait`) rather than native `async fn`, because a trait with
/// native async is not dyn-compatible and the whole point of this trait is `Box<dyn …>`. The
/// per-check allocation is irrelevant next to a network round trip.
#[async_trait::async_trait]
pub trait DynCheck: Send + Sync {
    fn id(&self) -> CheckId;
    fn network(&self) -> &str;
    fn subject(&self) -> Subject;
    fn plan(&self, plan: &mut ReadPlan);
    async fn run(&self, ctx: &ReadCtx<'_>) -> Reading;
}

#[async_trait::async_trait]
impl<C> DynCheck for C
where
    C: Check,
{
    fn id(&self) -> CheckId {
        Check::id(self)
    }

    fn network(&self) -> &str {
        Check::network(self)
    }

    fn subject(&self) -> Subject {
        Check::subject(self)
    }

    fn plan(&self, plan: &mut ReadPlan) {
        Check::plan(self, plan)
    }

    async fn run(&self, ctx: &ReadCtx<'_>) -> Reading {
        let started = std::time::Instant::now();
        let (observation, remediation) = match self.read(ctx).await {
            Ok(reading) => (Ok(self.observe(&reading)), self.remediation(&reading)),
            // A failed read yields no observation and no remediation: acting on a balance we
            // could not read would be acting blind.
            Err(reason) => (Err(Unreadable::read_failed(reason)), None),
        };
        Reading {
            id: Check::id(self),
            network: Check::network(self).to_string(),
            subject: Check::subject(self),
            observation,
            remediation,
            duration: started.elapsed(),
        }
    }
}

/// Build a remediation intent if `observed` is under the configured trigger.
pub fn remediation_for(
    asset: Asset,
    observed: &Amount,
    spec: Option<&RemediationSpec>,
) -> Option<RemediationIntent> {
    let spec = spec?;
    let below = spec.below.to_base_units(observed.decimals).ok()?;
    if observed.raw >= below {
        return None;
    }
    Some(RemediationIntent {
        asset,
        observed: *observed,
        below: Amount::new(below, observed.decimals),
        top_up_by: Amount::new(
            spec.top_up_by.to_base_units(observed.decimals).ok()?,
            observed.decimals,
        ),
        from: spec.from.as_ref().map(|s| s.display.clone()),
        from_address: spec.from.as_ref().map(|s| s.address),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Spender;
    use crate::units::DecimalStr;
    use alloy::primitives::U256;

    fn eth(raw: &str) -> Amount {
        Amount::new(U256::from_str_radix(raw, 10).unwrap(), 18)
    }

    fn spec(below: &str, top_up: &str, from: Option<&str>) -> RemediationSpec {
        RemediationSpec {
            below: DecimalStr::parse(below).unwrap(),
            top_up_by: DecimalStr::parse(top_up).unwrap(),
            from: from.map(|f| Spender {
                display: f.to_string(),
                address: Address::repeat_byte(0x99),
            }),
        }
    }

    #[test]
    fn no_remediation_config_means_no_intent() {
        assert!(remediation_for(Asset::Eth, &eth("0"), None).is_none());
    }

    #[test]
    fn remediation_triggers_only_below_the_line() {
        let s = spec("0.05", "0.1", Some("Funder"));
        // Above and exactly at the trigger: nothing to do.
        assert!(remediation_for(Asset::Eth, &eth("100000000000000000"), Some(&s)).is_none());
        assert!(remediation_for(Asset::Eth, &eth("50000000000000000"), Some(&s)).is_none());
        // Below by one wei: act.
        let intent =
            remediation_for(Asset::Eth, &eth("49999999999999999"), Some(&s)).expect("triggered");
        assert_eq!(intent.asset, Asset::Eth);
        assert_eq!(intent.from.as_deref(), Some("Funder"));
        assert!(
            intent.describe().contains("0.1000 ETH"),
            "{}",
            intent.describe()
        );
        assert!(intent.describe().contains("Funder"));
    }

    #[test]
    fn a_sub_cent_balance_still_triggers_though_it_renders_as_zero() {
        // 0.004 ETH displays as 0.00; the trigger compares base units, not the rendering.
        let s = spec("0.025", "0.05", None);
        assert!(remediation_for(Asset::Eth, &eth("4000000000000000"), Some(&s)).is_some());
    }

    #[test]
    fn remediation_without_a_funder_describes_itself_anyway() {
        let s = spec("0.05", "0.1", None);
        let intent = remediation_for(Asset::Eth, &eth("0"), Some(&s)).unwrap();
        assert_eq!(intent.describe(), "top up 0.1000 ETH");
    }

    #[test]
    fn unreadable_outcomes_expose_their_reason() {
        let outcome = Reading {
            id: CheckId::new("eth_balance", "W"),
            network: "arbitrum-one".into(),
            subject: Subject::Network,
            observation: Err(Unreadable::read_failed("endpoint refused historical state")),
            remediation: None,
            duration: Duration::ZERO,
        };
        assert!(outcome.is_unreadable());
        assert_eq!(outcome.summary(), "endpoint refused historical state");
        assert_eq!(outcome.value("anything"), None);
        assert!(outcome.fault().is_none());
    }

    #[test]
    fn subjects_name_themselves_and_group_falls_back_to_ungrouped() {
        let mut account = Subject::Account {
            group: None,
            account: "BillingWallet".into(),
            address: Address::repeat_byte(1),
        };
        assert_eq!(account.kind(), "account");
        assert_eq!(account.group_or_ungrouped(), "ungrouped");
        if let Subject::Account { group, .. } = &mut account {
            *group = Some("billing".into());
        }
        assert_eq!(account.group_or_ungrouped(), "billing");

        let allowance = Subject::Allowance {
            group: None,
            owner: "BillingWallet".into(),
            spender: "NetworkMonitor".into(),
        };
        assert_eq!(allowance.name(), "BillingWallet->NetworkMonitor");
        let pinned = Subject::Subgraph {
            subgraph: "network".into(),
            deployment: Some("QmX".into()),
        };
        assert_eq!(pinned.name(), "network@QmX");
        assert_eq!(Subject::Network.name(), "network");
    }
}
