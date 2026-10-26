//! What a check is, and what running one produces.
//!
//! A check reads state and reports it. It does **not** decide whether that state is
//! acceptable — thresholds and alerting belong to Grafana (see `specs/CHECKS_PLAN.md`,
//! decision 8). The one exception is remediation: a top-up needs a trigger to act on, and that
//! trigger lives here because repair must not depend on the metrics pipeline being up.

use std::fmt;
use std::time::Duration;

use alloy::primitives::Address;

use crate::config::RemediationSpec;
use crate::read::{ReadCtx, ReadPlan};
use crate::units::Amount;

/// Identifies a check instance: its kind, plus what it is reading.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatchId {
    /// Stable across instances — `eth_balance`, `grt_balance`, `allowance`.
    pub kind: &'static str,
    /// Which thing this instance watches — usually an account label.
    pub instance: String,
}

impl WatchId {
    pub fn new(kind: &'static str, instance: impl Into<String>) -> Self {
        Self {
            kind,
            instance: instance.into(),
        }
    }
}

impl fmt::Display for WatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.instance)
    }
}

/// Dimensions carried into output. Concrete fields rather than a map, because these are the
/// dimensions the domain actually has and Prometheus label sets should not be open-ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Labels {
    pub network: String,
    /// Free-form grouping label. Carries no behavior — see `specs/CHECKS_PLAN.md`.
    pub group: Option<String>,
    pub account: Option<String>,
    pub address: Option<Address>,
    pub spender: Option<String>,
}

impl Labels {
    pub fn for_account(
        network: &str,
        group: Option<&str>,
        account: &str,
        address: Address,
    ) -> Self {
        Self {
            network: network.to_string(),
            group: group.map(str::to_string),
            account: Some(account.to_string()),
            address: Some(address),
            spender: None,
        }
    }

    pub fn with_spender(mut self, spender: impl Into<String>) -> Self {
        self.spender = Some(spender.into());
        self
    }

    /// `ungrouped` rather than an absent label, so metric queries never have to handle both
    /// the present and missing cases.
    pub fn group_or_ungrouped(&self) -> &str {
        self.group.as_deref().unwrap_or("ungrouped")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Eth,
    Grt,
    Seconds,
    /// A 1/0 classification — exact, unlike the value it describes.
    Boolean,
}

impl Unit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Unit::Eth => "ETH",
            Unit::Grt => "GRT",
            Unit::Seconds => "s",
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
    pub labels: Labels,
}

/// What a check saw.
#[derive(Debug, Clone)]
pub struct Observation {
    /// One human sentence, for the console. Deliberately not a metric label — it would be
    /// unbounded cardinality.
    pub summary: String,
    pub measurements: Vec<Measurement>,
}

impl Observation {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            measurements: Vec::new(),
        }
    }

    pub fn with(mut self, measurement: Measurement) -> Self {
        self.measurements.push(measurement);
        self
    }

    /// Attach a measurement only when it is meaningful. Emitting a placeholder instead would
    /// publish a fact the watch cannot actually vouch for.
    pub fn with_if(self, condition: bool, measurement: Measurement) -> Self {
        if condition {
            self.with(measurement)
        } else {
            self
        }
    }

    pub fn measurement(&self, name: &str) -> Option<&Measurement> {
        self.measurements.iter().find(|m| m.name == name)
    }

    pub fn value(&self, name: &str) -> Option<f64> {
        self.measurement(name).map(|m| m.value)
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

/// One check, run once. Either it observed something, or it could not read.
///
/// "Could not read" is kept distinct from any value, including zero: an endpoint outage must
/// never be indistinguishable from an empty wallet.
#[derive(Debug, Clone)]
pub struct Reading {
    pub id: WatchId,
    pub labels: Labels,
    pub observation: Result<Observation, String>,
    pub remediation: Option<RemediationIntent>,
    pub duration: Duration,
}

impl Reading {
    pub fn is_unreadable(&self) -> bool {
        self.observation.is_err()
    }

    /// The human sentence, or the reason there isn't one.
    pub fn summary(&self) -> &str {
        match &self.observation {
            Ok(o) => &o.summary,
            Err(reason) => reason,
        }
    }

    pub fn value(&self, name: &str) -> Option<f64> {
        self.observation.as_ref().ok().and_then(|o| o.value(name))
    }
}

/// Implemented by each kind of check.
///
/// `observe` and `remediation` take only the reading — no context, no clock, no network — so
/// both are testable by calling them with a literal.
#[async_trait::async_trait]
pub trait Watch: Send + Sync {
    type Reading: Send + Sync;

    fn id(&self) -> WatchId;
    fn labels(&self) -> Labels;

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
pub trait DynWatch: Send + Sync {
    fn id(&self) -> WatchId;
    fn labels(&self) -> Labels;
    fn plan(&self, plan: &mut ReadPlan);
    async fn run(&self, ctx: &ReadCtx<'_>) -> Reading;
}

#[async_trait::async_trait]
impl<C> DynWatch for C
where
    C: Watch,
{
    fn id(&self) -> WatchId {
        Watch::id(self)
    }

    fn labels(&self) -> Labels {
        Watch::labels(self)
    }

    fn plan(&self, plan: &mut ReadPlan) {
        Watch::plan(self, plan)
    }

    async fn run(&self, ctx: &ReadCtx<'_>) -> Reading {
        let started = std::time::Instant::now();
        let (observation, remediation) = match self.read(ctx).await {
            Ok(reading) => (Ok(self.observe(&reading)), self.remediation(&reading)),
            // A failed read yields no observation and no remediation: acting on a balance we
            // could not read would be acting blind.
            Err(reason) => (Err(reason), None),
        };
        Reading {
            id: Watch::id(self),
            labels: Watch::labels(self),
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
            id: WatchId::new("eth_balance", "W"),
            labels: Labels::default(),
            observation: Err("endpoint refused historical state".into()),
            remediation: None,
            duration: Duration::ZERO,
        };
        assert!(outcome.is_unreadable());
        assert_eq!(outcome.summary(), "endpoint refused historical state");
        assert_eq!(outcome.value("anything"), None);
    }

    #[test]
    fn group_label_falls_back_to_ungrouped() {
        let mut labels = Labels {
            network: "arbitrum-one".into(),
            ..Default::default()
        };
        assert_eq!(labels.group_or_ungrouped(), "ungrouped");
        labels.group = Some("billing".into());
        assert_eq!(labels.group_or_ungrouped(), "billing");
    }
}
