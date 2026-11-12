//! Checks read through a network's RPC: ETH balance, GRT balance and activity (about an
//! account), and GRT allowance (about an owner → spender pair).
//!
//! Each is a thin `read` over the shared context, an `observe` that turns the reading into
//! measurements and a sentence, and — where configured — a remediation trigger. None of them
//! decide whether a value is acceptable; that is Grafana's job.

use alloy::primitives::Address;

use crate::activity::Activity;
use crate::check::{
    remediation_for, Asset, Check, CheckId, Measurement, Observation, RemediationIntent, Subject,
    Unit,
};
use crate::config::{RemediationSpec, ResolvedAccount, Spender};
use crate::read::{ReadCtx, ReadPlan, ETH_DECIMALS};
use crate::units::Amount;

/// Measurement names. Constants because the console renderer and the metrics exporter both key
/// off them, and a typo would silently produce an empty panel.
pub const M_ETH_BALANCE: &str = "account_eth_balance";
pub const M_GRT_BALANCE: &str = "account_grt_balance";
pub const M_ALLOWANCE: &str = "allowance_grt";
/// Exact 1/0 classifications of a value `f64` cannot represent faithfully.
pub const M_ALLOWANCE_UNLIMITED: &str = "allowance_unlimited";
pub const M_ALLOWANCE_EXACT_MAX: &str = "allowance_exact_max";
pub const M_IDLE_SECONDS: &str = "account_idle_seconds";
pub const M_EVER_TRANSACTED: &str = "account_ever_transacted";
/// 1 when the search found activity inside its window. Distinct from `ever_transacted`: an
/// event check finding nothing means "not in the lookback", not "never acted".
pub const M_ACTIVITY_SEEN: &str = "account_activity_seen";
/// The configured top-up trigger. Exported whenever remediation is configured — not only when
/// it fires — so a dashboard can draw the line even while the balance is healthy.
pub const M_REMEDIATION_BELOW: &str = "account_remediation_below";

fn subject_for(account: &ResolvedAccount) -> Subject {
    Subject::Account {
        group: account.group.clone(),
        account: account.label.clone(),
        address: account.address,
    }
}

// ---------------------------------------------------------------------------------------
// ETH balance
// ---------------------------------------------------------------------------------------

pub struct EthBalanceCheck {
    pub network: String,
    pub account: ResolvedAccount,
    pub remediate: Option<RemediationSpec>,
}

#[async_trait::async_trait]
impl Check for EthBalanceCheck {
    type Reading = Amount;

    fn id(&self) -> CheckId {
        CheckId::new("eth_balance", &self.account.label)
    }

    fn network(&self) -> &str {
        &self.network
    }

    fn subject(&self) -> Subject {
        subject_for(&self.account)
    }

    fn plan(&self, plan: &mut ReadPlan) {
        plan.eth_balance(self.account.address);
    }

    async fn read(&self, ctx: &ReadCtx<'_>) -> Result<Self::Reading, String> {
        let raw = ctx.eth_balance(self.account.address).await?;
        Ok(Amount::new(raw, ETH_DECIMALS))
    }

    fn observe(&self, balance: &Self::Reading) -> Observation {
        let observation = Observation::new(format!(
            "{} holds {} ETH",
            self.account.label,
            balance.formatted(4)
        ))
        .with(Measurement {
            name: M_ETH_BALANCE,
            value: balance.to_f64(),
            unit: Unit::Eth,
            subject: self.subject(),
        });
        with_trigger(
            observation,
            self.remediate.as_ref(),
            Unit::Eth,
            self.subject(),
        )
    }

    fn remediation(&self, balance: &Self::Reading) -> Option<RemediationIntent> {
        remediation_for(Asset::Eth, balance, self.remediate.as_ref())
    }
}

// ---------------------------------------------------------------------------------------
// GRT balance
// ---------------------------------------------------------------------------------------

pub struct GrtBalanceCheck {
    pub network: String,
    pub account: ResolvedAccount,
    pub token: Address,
    pub remediate: Option<RemediationSpec>,
}

#[async_trait::async_trait]
impl Check for GrtBalanceCheck {
    type Reading = Amount;

    fn id(&self) -> CheckId {
        CheckId::new("grt_balance", &self.account.label)
    }

    fn network(&self) -> &str {
        &self.network
    }

    fn subject(&self) -> Subject {
        subject_for(&self.account)
    }

    fn plan(&self, plan: &mut ReadPlan) {
        plan.token_balance(self.token, self.account.address);
    }

    async fn read(&self, ctx: &ReadCtx<'_>) -> Result<Self::Reading, String> {
        // Decimals are read rather than assumed, and from the same batch, so the formatted
        // number belongs to the same block as the balance.
        let decimals = ctx.token_decimals(self.token).await?;
        let raw = ctx.token_balance(self.token, self.account.address).await?;
        Ok(Amount::new(raw, decimals))
    }

    fn observe(&self, balance: &Self::Reading) -> Observation {
        let observation = Observation::new(format!(
            "{} holds {} GRT",
            self.account.label,
            balance.formatted(2)
        ))
        .with(Measurement {
            name: M_GRT_BALANCE,
            value: balance.to_f64(),
            unit: Unit::Grt,
            subject: self.subject(),
        });
        with_trigger(
            observation,
            self.remediate.as_ref(),
            Unit::Grt,
            self.subject(),
        )
    }

    fn remediation(&self, balance: &Self::Reading) -> Option<RemediationIntent> {
        remediation_for(Asset::Grt, balance, self.remediate.as_ref())
    }
}

// ---------------------------------------------------------------------------------------
// GRT allowance
// ---------------------------------------------------------------------------------------

pub struct AllowanceCheck {
    pub network: String,
    pub account: ResolvedAccount,
    pub token: Address,
    pub spender: Spender,
}

#[async_trait::async_trait]
impl Check for AllowanceCheck {
    type Reading = Amount;

    fn id(&self) -> CheckId {
        CheckId::new(
            "allowance",
            format!("{}->{}", self.account.label, self.spender.display),
        )
    }

    fn network(&self) -> &str {
        &self.network
    }

    fn subject(&self) -> Subject {
        Subject::Allowance {
            group: self.account.group.clone(),
            owner: self.account.label.clone(),
            spender: self.spender.display.clone(),
        }
    }

    fn plan(&self, plan: &mut ReadPlan) {
        plan.allowance(self.token, self.account.address, self.spender.address);
    }

    async fn read(&self, ctx: &ReadCtx<'_>) -> Result<Self::Reading, String> {
        let decimals = ctx.token_decimals(self.token).await?;
        let raw = ctx
            .allowance(self.token, self.account.address, self.spender.address)
            .await?;
        Ok(Amount::new(raw, decimals))
    }

    fn observe(&self, allowance: &Self::Reading) -> Observation {
        let subject = self.subject();

        // The value, plus the two facts `f64` cannot carry. An unlimited approval is ~1.157e59,
        // a magnitude at which a float has discarded forty digits — asking Grafana to compare
        // it against 2^255 would be asking for a wrong answer. These booleans are computed on
        // exact U256, so the classification survives the wire format.
        Observation::new(format!(
            "{} → {} allowance is {}",
            self.account.label,
            self.spender.display,
            describe_allowance(allowance)
        ))
        .with(Measurement {
            name: M_ALLOWANCE,
            value: allowance.to_f64(),
            unit: Unit::Grt,
            subject: subject.clone(),
        })
        .with(Measurement {
            name: M_ALLOWANCE_UNLIMITED,
            value: bool_value(allowance.is_unlimited()),
            unit: Unit::Boolean,
            subject: subject.clone(),
        })
        .with(Measurement {
            name: M_ALLOWANCE_EXACT_MAX,
            value: bool_value(allowance.is_max()),
            unit: Unit::Boolean,
            subject,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------------------

pub struct ActivityCheck {
    pub network: String,
    pub account: ResolvedAccount,
}

#[async_trait::async_trait]
impl Check for ActivityCheck {
    type Reading = Activity;

    fn id(&self) -> CheckId {
        CheckId::new("activity", &self.account.label)
    }

    fn network(&self) -> &str {
        &self.network
    }

    fn subject(&self) -> Subject {
        subject_for(&self.account)
    }

    // Deliberately no `plan`: the search is adaptive, so its reads cannot be declared up
    // front. It runs its own sequential probes through the undeclared-read path.

    async fn read(&self, ctx: &ReadCtx<'_>) -> Result<Self::Reading, String> {
        match &self.account.activity {
            // An event check says what the account is *supposed to be doing*, and reads logs
            // rather than historical state — so unlike the nonce search it works on a pruned
            // endpoint.
            Some(spec) => ctx.last_event(self.account.address, spec).await,
            None => ctx.last_activity(self.account.address).await,
        }
    }

    fn observe(&self, activity: &Self::Reading) -> Observation {
        let subject = self.subject();
        let what = match &self.account.activity {
            Some(spec) => spec.event.clone(),
            None => "a transaction".to_string(),
        };
        match activity {
            // No last-activity timestamp exists, so none is invented. `ever_transacted = 0`
            // is the fact; an idle duration would be a fabrication.
            // Nothing found. For an event check that means "not within the lookback"; for a
            // nonce check it means nonce zero. Either way no age exists, so none is reported —
            // inventing one would be fabricating an observation.
            Activity::Never => Observation::new(match &self.account.activity {
                Some(spec) => format!(
                    "{} has not emitted {} within the lookback",
                    self.account.label, spec.event
                ),
                None => format!("{} has never sent a transaction", self.account.label),
            })
            .with(Measurement {
                name: M_ACTIVITY_SEEN,
                value: 0.0,
                unit: Unit::Boolean,
                subject: subject.clone(),
            })
            // Only a nonce check can claim an account has *never* acted; an event check only
            // knows it saw nothing in its window.
            .with_if(
                self.account.activity.is_none(),
                Measurement {
                    name: M_EVER_TRANSACTED,
                    value: 0.0,
                    unit: Unit::Boolean,
                    subject,
                },
            ),
            Activity::Last {
                block,
                idle_seconds,
            } => Observation::new(format!(
                "{} last did {what} {} ago, at block {block}",
                self.account.label,
                humanize(*idle_seconds)
            ))
            .with(Measurement {
                name: M_IDLE_SECONDS,
                value: *idle_seconds as f64,
                unit: Unit::Seconds,
                subject: subject.clone(),
            })
            .with(Measurement {
                name: M_ACTIVITY_SEEN,
                // Seen means seen within the lookback. A subgraph can know of activity older
                // than that: its idle time is still reported, but it does not count as seen.
                value: match &self.account.activity {
                    Some(spec) if *idle_seconds > spec.lookback.as_secs() => 0.0,
                    _ => 1.0,
                },
                unit: Unit::Boolean,
                subject: subject.clone(),
            })
            .with_if(
                self.account.activity.is_none(),
                Measurement {
                    name: M_EVER_TRANSACTED,
                    value: 1.0,
                    unit: Unit::Boolean,
                    subject,
                },
            ),
        }
    }
}

/// Coarse duration for a console sentence. Metrics carry the exact seconds.
pub fn humanize(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// Attach the configured remediation trigger, if there is one.
fn with_trigger(
    observation: Observation,
    remediate: Option<&RemediationSpec>,
    unit: Unit,
    subject: Subject,
) -> Observation {
    let Some(spec) = remediate else {
        return observation;
    };
    // 18 decimals for both assets here: this is a config value, not a chain read, and it only
    // ever leaves as an f64.
    let Ok(raw) = spec.below.to_base_units(18) else {
        return observation;
    };
    observation.with(Measurement {
        name: M_REMEDIATION_BELOW,
        value: Amount::new(raw, 18).to_f64(),
        unit,
        subject,
    })
}

fn bool_value(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

/// How an allowance reads in a sentence. `~unlimited` distinguishes a max approval that has
/// been spent against from one that is untouched.
pub fn describe_allowance(amount: &Amount) -> String {
    if amount.is_max() {
        "unlimited".to_string()
    } else if amount.is_unlimited() {
        "~unlimited".to_string()
    } else if amount.is_zero() {
        "none".to_string()
    } else {
        format!("{} GRT", amount.formatted(2))
    }
}

/// Build every check a resolved account asks for.
pub fn for_account(
    network: &str,
    account: &ResolvedAccount,
    token: Address,
) -> Vec<Box<dyn crate::check::DynCheck>> {
    let mut checks: Vec<Box<dyn crate::check::DynCheck>> = Vec::new();

    if account.check_eth {
        checks.push(Box::new(EthBalanceCheck {
            network: network.to_string(),
            account: account.clone(),
            remediate: account.eth_remediate.clone(),
        }));
    }
    if account.check_grt {
        checks.push(Box::new(GrtBalanceCheck {
            network: network.to_string(),
            account: account.clone(),
            token,
            remediate: account.grt_remediate.clone(),
        }));
    }
    if account.check_activity {
        checks.push(Box::new(ActivityCheck {
            network: network.to_string(),
            account: account.clone(),
        }));
    }
    for spender in &account.allowances {
        checks.push(Box::new(AllowanceCheck {
            network: network.to_string(),
            account: account.clone(),
            token,
            spender: spender.clone(),
        }));
    }

    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Source;
    use crate::units::DecimalStr;
    use alloy::primitives::U256;

    fn account() -> ResolvedAccount {
        ResolvedAccount {
            label: "BillingWallet".into(),
            address: Address::repeat_byte(0x11),
            group: Some("billing".into()),
            check_eth: false,
            check_grt: false,
            check_activity: false,
            activity: None,
            eth_remediate: None,
            grt_remediate: None,
            allowances: Vec::new(),
            source: Source::Default,
        }
    }

    fn eth(raw: &str) -> Amount {
        Amount::new(U256::from_str_radix(raw, 10).unwrap(), 18)
    }

    fn spec(below: &str, top_up: &str) -> RemediationSpec {
        RemediationSpec {
            below: DecimalStr::parse(below).unwrap(),
            top_up_by: DecimalStr::parse(top_up).unwrap(),
            from: None,
        }
    }

    fn eth_check(remediate: Option<RemediationSpec>) -> EthBalanceCheck {
        EthBalanceCheck {
            network: "arbitrum-one".into(),
            account: account(),
            remediate,
        }
    }

    // ---- observation -------------------------------------------------------------------

    #[test]
    fn activity_older_than_the_lookback_keeps_its_age_but_is_not_seen() {
        let event = "OracleVote(bytes32,bool,uint256,uint256)";
        let mut acc = account();
        acc.activity = Some(crate::config::ActivitySpec {
            emitter: crate::config::Spender {
                display: "SubgraphAvailabilityManager".into(),
                address: Address::repeat_byte(0xaa),
            },
            event: event.into(),
            topic0: alloy::primitives::keccak256(event.as_bytes()),
            lookback: std::time::Duration::from_secs(7 * 86_400),
            matches: crate::activity::TxMatch::From,
        });
        let check = ActivityCheck {
            network: "arbitrum-one".into(),
            account: acc,
        };
        let recent = check.observe(&Activity::Last {
            block: 1,
            idle_seconds: 86_400,
        });
        assert_eq!(recent.value(M_ACTIVITY_SEEN), Some(1.0));

        // A subgraph knows of a vote 8 days ago, past the 7-day lookback.
        let old = check.observe(&Activity::Last {
            block: 1,
            idle_seconds: 8 * 86_400,
        });
        assert_eq!(old.value(M_ACTIVITY_SEEN), Some(0.0));
        assert_eq!(old.value(M_IDLE_SECONDS), Some(8.0 * 86_400.0));
    }

    #[test]
    fn an_observation_carries_the_value_and_a_sentence() {
        let obs = eth_check(None).observe(&eth("421000000000000000"));
        assert!(obs.summary.contains("0.4210"), "{}", obs.summary);
        let m = obs.measurement(M_ETH_BALANCE).expect("measurement present");
        assert_eq!(m.unit, Unit::Eth);
        assert_eq!(m.subject.name(), "BillingWallet");
        assert_eq!(m.subject.group(), Some("billing"));
        assert!((m.value - 0.421).abs() < 1e-12);
    }

    #[test]
    fn nothing_is_judged() {
        // A wallet at zero is reported, not condemned. Whether that is acceptable is a
        // question for Grafana.
        let obs = eth_check(None).observe(&eth("0"));
        assert!(obs.summary.contains("0.0000"));
        assert_eq!(obs.value(M_ETH_BALANCE), Some(0.0));
    }

    // ---- remediation triggers ----------------------------------------------------------

    #[test]
    fn remediation_fires_only_below_the_configured_trigger() {
        let check = eth_check(Some(spec("0.05", "0.1")));
        assert!(check.remediation(&eth("100000000000000000")).is_none());
        assert!(check.remediation(&eth("50000000000000000")).is_none());
        let intent = check.remediation(&eth("10000000000000000")).unwrap();
        assert_eq!(intent.asset, Asset::Eth);
        assert_eq!(intent.below.formatted(4), "0.0500");
        assert_eq!(intent.top_up_by.formatted(4), "0.1000");
    }

    #[test]
    fn an_account_without_remediation_config_never_asks_for_repair() {
        assert!(eth_check(None).remediation(&eth("0")).is_none());
    }

    // ---- allowance ---------------------------------------------------------------------

    fn allowance_check() -> AllowanceCheck {
        AllowanceCheck {
            network: "arbitrum-one".into(),
            account: account(),
            token: Address::repeat_byte(0x22),
            spender: Spender {
                display: "NetworkMonitor".into(),
                address: Address::repeat_byte(0x33),
            },
        }
    }

    #[test]
    fn allowance_exports_exact_classifications_alongside_the_lossy_value() {
        let check = allowance_check();
        let spent = U256::from_str_radix("3960100000000000000000000", 10).unwrap();

        // Untouched max.
        let obs = check.observe(&Amount::new(U256::MAX, 18));
        assert_eq!(obs.value(M_ALLOWANCE_UNLIMITED), Some(1.0));
        assert_eq!(obs.value(M_ALLOWANCE_EXACT_MAX), Some(1.0));

        // Max, drawn against: still inexhaustible, no longer exactly max. The gauge alone
        // cannot tell these apart — at 1e59 the two f64 values are indistinguishable.
        let obs = check.observe(&Amount::new(U256::MAX - spent, 18));
        assert_eq!(obs.value(M_ALLOWANCE_UNLIMITED), Some(1.0));
        assert_eq!(obs.value(M_ALLOWANCE_EXACT_MAX), Some(0.0));

        // Revoked.
        let obs = check.observe(&Amount::new(U256::ZERO, 18));
        assert_eq!(obs.value(M_ALLOWANCE_UNLIMITED), Some(0.0));
        assert_eq!(obs.value(M_ALLOWANCE_EXACT_MAX), Some(0.0));
    }

    #[test]
    fn allowance_descriptions_distinguish_max_from_spent() {
        let spent = U256::from_str_radix("3960100000000000000000000", 10).unwrap();
        assert_eq!(describe_allowance(&Amount::new(U256::MAX, 18)), "unlimited");
        assert_eq!(
            describe_allowance(&Amount::new(U256::MAX - spent, 18)),
            "~unlimited"
        );
        assert_eq!(describe_allowance(&Amount::new(U256::ZERO, 18)), "none");
        assert_eq!(
            describe_allowance(&Amount::new(
                U256::from_str_radix("1500000000000000000", 10).unwrap(),
                18
            )),
            "1.50 GRT"
        );
    }

    // ---- assembly ----------------------------------------------------------------------

    #[test]
    fn an_account_builds_only_the_checks_it_watches() {
        let mut acc = account();
        assert!(for_account("arbitrum-one", &acc, Address::ZERO).is_empty());

        acc.check_eth = true;
        acc.allowances = vec![Spender {
            display: "NetworkMonitor".into(),
            address: Address::repeat_byte(0x33),
        }];
        let checks = for_account("arbitrum-one", &acc, Address::ZERO);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].id().kind, "eth_balance");
        assert_eq!(checks[1].id().kind, "allowance");
        assert_eq!(checks[1].id().instance, "BillingWallet->NetworkMonitor");
    }

    #[test]
    fn checks_declare_their_reads_for_batching() {
        let mut acc = account();
        acc.check_eth = true;
        acc.check_grt = true;
        let checks = for_account("arbitrum-one", &acc, Address::repeat_byte(0x22));

        let mut plan = ReadPlan::default();
        for c in &checks {
            c.plan(&mut plan);
        }
        // eth balance + token balance + the token's decimals
        assert_eq!(plan.len(), 3);
    }
}
