//! Configuration loading, layering and resolution.
//!
//! Layers, later wins:
//!   1. the shipped `config/default.toml`, embedded at build time
//!   2. `$XDG_CONFIG_HOME/graph-network/config.toml`
//!   3. `./graph-network.toml`
//!   4. environment / flags (applied by the caller)
//!
//! User config *extends* the shipped layer rather than replacing it. `use_defaults = false`
//! (or `--no-defaults`) drops layer 1 entirely.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::primitives::Address;
use serde::Deserialize;

use crate::error::{ConfigError, Result};
use crate::source::default_max_lag;
use crate::units::DecimalStr;

/// How far back an activity search looks when config does not say. Generous on purpose: the
/// scan stops at the first hit, so it costs nothing for an account that is actually active.
const DEFAULT_ACTIVITY_LOOKBACK: Duration = Duration::from_secs(7 * 86_400);

/// The config shipped inside the binary. Committed at `config/default.toml`.
pub const DEFAULT_CONFIG: &str = include_str!("../../../config/default.toml");

/// Which layer a piece of config came from. Surfaced in `--json` so the effective account set
/// is inspectable rather than inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    User,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::User => "user",
        }
    }
}

// ---------------------------------------------------------------------------------------
// Raw shapes, as they appear in TOML
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    default_network: Option<String>,
    use_defaults: Option<bool>,
    #[serde(default)]
    networks: BTreeMap<String, RawNetwork>,
    /// Keyed by network name: `[[accounts.arbitrum-one]]`.
    #[serde(default)]
    accounts: BTreeMap<String, Vec<RawAccount>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    chain_id: Option<u64>,
    rpc_url: Option<String>,
    /// Private key for remediation. Never read from the shipped layer; `${ENV}` works here,
    /// so it need not be a literal.
    signer_key: Option<String>,
    /// How far behind wall clock the head block may be, e.g. `"60s"`.
    max_lag: Option<String>,
    /// Requests in flight against this endpoint at once.
    max_concurrency: Option<usize>,
    /// Largest `eth_getLogs` span to attempt. Narrowed automatically if the endpoint refuses.
    max_log_range: Option<u64>,
    /// The address book. Holds contracts *and* EOAs — nothing in the tool cares which.
    #[serde(default)]
    addresses: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccount {
    address: String,
    /// Optional. Omitted, the account is named by whatever `address` says — a book name in
    /// the common case.
    label: Option<String>,
    /// Free-form grouping label. No behavior attached.
    group: Option<String>,
    enabled: Option<bool>,
    /// Which assets to read: `["eth", "grt"]`. A `remediate` block implies its asset.
    #[serde(default)]
    watch: Vec<String>,
    eth: Option<RawAsset>,
    grt: Option<RawAsset>,
    /// Event-driven activity. Present, it supersedes nonce-based activity for this account.
    activity: Option<RawActivity>,
    #[serde(default)]
    allowances: Vec<RawAllowance>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawActivity {
    /// Contract emitting the event: an address-book name or a hex address.
    emitter: String,
    /// Human-readable signature, e.g. `Transfer(address,address,uint256)`.
    event: String,
    /// How far back to search before giving up. A bound on cost, not a threshold.
    lookback: Option<String>,
    /// `from` (default) or `to`.
    #[serde(rename = "match")]
    match_: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAsset {
    remediate: Option<RawRemediate>,
}

/// The only threshold the tool holds. It exists because a top-up needs a number to act on,
/// and repair must keep working when the metrics pipeline does not.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRemediate {
    below: String,
    top_up_by: String,
    from: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAllowance {
    spender: String,
}

// ---------------------------------------------------------------------------------------
// Merged config
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct Network {
    pub chain_id: Option<u64>,
    pub signer_key: Option<String>,
    /// Kept unexpanded until the network is selected: a config may legitimately describe
    /// networks whose `${RPC_URL}` env vars are not set in this shell, and only the one being
    /// queried needs to resolve.
    pub rpc_url: Option<String>,
    pub max_lag: Option<String>,
    pub max_concurrency: Option<usize>,
    pub max_log_range: Option<u64>,
    pub addresses: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct AssetSpec {
    pub remediate: Option<RemediateSpec>,
}

#[derive(Debug, Clone)]
pub struct RemediateSpec {
    pub below: String,
    pub top_up_by: String,
    pub from: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActivityConfig {
    pub emitter: String,
    pub event: String,
    pub lookback: Option<String>,
    pub match_: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub label: String,
    pub address: String,
    pub group: Option<String>,
    pub enabled: bool,
    pub watch: Vec<String>,
    pub eth: Option<AssetSpec>,
    pub grt: Option<AssetSpec>,
    pub activity: Option<ActivityConfig>,
    pub allowances: Vec<String>,
    pub source: Source,
}

#[derive(Debug, Default)]
pub struct Config {
    pub default_network: Option<String>,
    pub networks: BTreeMap<String, Network>,
    pub accounts: BTreeMap<String, Vec<Account>>,
    /// Human-readable list of the layers actually loaded, for error messages.
    pub sources: Vec<String>,
    pub used_defaults: bool,
}

/// Where to look for user config.
#[derive(Debug, Default, Clone)]
pub struct LoadOptions {
    /// Explicit `--config <path>`. When set, the XDG and cwd files are not consulted.
    pub explicit_path: Option<PathBuf>,
    /// `--no-defaults`: skip the shipped layer regardless of what config says.
    pub no_defaults: bool,
}

impl Config {
    pub fn load(opts: &LoadOptions) -> Result<Self> {
        let mut user_layers: Vec<(String, RawConfig)> = Vec::new();

        match &opts.explicit_path {
            Some(path) => user_layers.push((path.display().to_string(), read_toml(path)?)),
            None => {
                for path in default_user_paths() {
                    if path.is_file() {
                        user_layers.push((path.display().to_string(), read_toml(&path)?));
                    }
                }
            }
        }

        // `use_defaults` has to be answered before the shipped layer is loaded, so it is read
        // from the user layers only. A default config that could switch itself off would be a
        // loop with no fixed point.
        let use_defaults = !opts.no_defaults
            && user_layers
                .iter()
                .rev()
                .find_map(|(_, raw)| raw.use_defaults)
                .unwrap_or(true);

        let mut config = Config {
            used_defaults: use_defaults,
            ..Default::default()
        };

        if use_defaults {
            let raw: RawConfig =
                toml::from_str(DEFAULT_CONFIG).map_err(|source| ConfigError::Parse {
                    path: PathBuf::from("<shipped default.toml>"),
                    source,
                })?;
            config.merge(raw, Source::Default, "<shipped defaults>")?;
        }

        for (name, raw) in user_layers {
            config.merge(raw, Source::User, &name)?;
        }

        Ok(config)
    }

    /// Load from explicit TOML strings. Used by tests and by callers supplying their own
    /// layering; `sources` are labels for error messages only.
    pub fn from_layers(layers: &[(&str, &str, Source)]) -> Result<Self> {
        let mut config = Config {
            used_defaults: layers.iter().any(|(_, _, s)| *s == Source::Default),
            ..Default::default()
        };
        for (label, text, source) in layers {
            let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse {
                path: PathBuf::from(*label),
                source: e,
            })?;
            config.merge(raw, *source, label)?;
        }
        Ok(config)
    }

    fn merge(&mut self, raw: RawConfig, source: Source, label: &str) -> Result<()> {
        self.sources.push(label.to_string());

        if let Some(n) = raw.default_network {
            self.default_network = Some(n);
        }

        // Networks merge per key: setting one rpc_url in user config must not discard the
        // shipped address book for that network.
        for (name, rn) in raw.networks {
            let entry = self.networks.entry(name).or_default();
            if let Some(id) = rn.chain_id {
                entry.chain_id = Some(id);
            }
            if let Some(url) = rn.rpc_url {
                entry.rpc_url = Some(url);
            }
            if let Some(key) = rn.signer_key {
                // A key in the embedded default would ship inside the binary. Refusing it here
                // makes that impossible rather than merely discouraged.
                if source == Source::Default {
                    return Err(ConfigError::SignerKeyInDefaults.into());
                }
                entry.signer_key = Some(key);
            }
            if let Some(lag) = rn.max_lag {
                entry.max_lag = Some(lag);
            }
            if let Some(c) = rn.max_concurrency {
                entry.max_concurrency = Some(c);
            }
            if let Some(r) = rn.max_log_range {
                entry.max_log_range = Some(r);
            }
            for (k, v) in rn.addresses {
                entry.addresses.insert(k, v);
            }
        }

        // Accounts extend. A user account whose label matches a shipped one replaces that
        // entry in place, which is how expectations get retuned without restating the list.
        for (network, accounts) in raw.accounts {
            let mut seen_in_layer: Vec<String> = Vec::new();
            let existing = self.accounts.entry(network.clone()).or_default();

            for acc in accounts {
                // The effective label falls back to the `address` string as written — a book
                // name in the common case. Deriving it before the book is resolved keeps
                // account identity independent of address lookup, so merging works the same
                // whether or not a label was given.
                let label = acc.label.clone().unwrap_or_else(|| acc.address.clone());

                if seen_in_layer.contains(&label) {
                    return Err(ConfigError::DuplicateLabel { network, label }.into());
                }
                seen_in_layer.push(label.clone());

                let merged = Account {
                    label,
                    address: acc.address,
                    group: acc.group,
                    enabled: acc.enabled.unwrap_or(true),
                    watch: acc.watch,
                    eth: acc.eth.map(into_asset_spec),
                    grt: acc.grt.map(into_asset_spec),
                    activity: acc.activity.map(|a| ActivityConfig {
                        emitter: a.emitter,
                        event: a.event,
                        lookback: a.lookback,
                        match_: a.match_,
                    }),
                    allowances: acc.allowances.into_iter().map(|a| a.spender).collect(),
                    source,
                };

                match existing.iter_mut().find(|e| e.label == merged.label) {
                    Some(slot) => *slot = merged,
                    None => existing.push(merged),
                }
            }
        }

        Ok(())
    }

    /// Pick a network and turn strings into typed values.
    pub fn resolve(&self, network: Option<&str>, rpc_override: Option<&str>) -> Result<Resolved> {
        let name = network
            .map(str::to_string)
            .or_else(|| self.default_network.clone())
            .ok_or(ConfigError::NoNetworkSelected {
                no_defaults: !self.used_defaults,
            })?;

        let net = self
            .networks
            .get(&name)
            .ok_or_else(|| ConfigError::UnknownNetwork {
                name: name.clone(),
                available: self.networks.keys().cloned().collect(),
            })?;

        let chain_id = net.chain_id.ok_or_else(|| ConfigError::MissingChainId {
            network: name.clone(),
        })?;

        let rpc_url = match rpc_override {
            Some(url) => url.to_string(),
            None => {
                let raw = net
                    .rpc_url
                    .as_deref()
                    .ok_or_else(|| ConfigError::MissingRpcUrl {
                        network: name.clone(),
                    })?;
                expand_env(raw, &format!("networks.{name}.rpc_url"))?
            }
        };

        let max_lag = match &net.max_lag {
            Some(raw) => parse_duration(raw, &format!("networks.{name}.max_lag"))?,
            None => default_max_lag(chain_id),
        };

        // The address book resolves first, so everything downstream can refer to it by name.
        let mut addresses = BTreeMap::new();
        for (key, value) in &net.addresses {
            let field = format!("networks.{name}.addresses.{key}");
            let expanded = expand_env(value, &field)?;
            addresses.insert(key.clone(), parse_address(&expanded, &field)?);
        }

        let accounts = self.accounts.get(&name).cloned().unwrap_or_default();

        // Labels resolve to addresses too, so the full label->address map must exist before
        // any allowance entry is resolved. Disabled accounts still participate: naming one as
        // a spender is about where tokens can go, not about whether we report on it.
        let mut label_addresses: BTreeMap<String, Address> = BTreeMap::new();
        for acc in &accounts {
            let field = format!("accounts.{name}.{}.address", acc.label);
            let expanded = expand_env(&acc.address, &field)?;
            label_addresses.insert(
                acc.label.clone(),
                resolve_book_entry(&expanded, &name, &field, &addresses)?,
            );
        }

        let mut resolved_accounts = Vec::new();
        for acc in accounts.iter().filter(|a| a.enabled) {
            let address = label_addresses[&acc.label];
            let base = format!("accounts.{name}.{}", acc.label);

            let eth_remediate = resolve_remediate(
                acc.eth.as_ref().and_then(|a| a.remediate.as_ref()),
                &name,
                &format!("{base}.eth.remediate"),
                &addresses,
                &label_addresses,
            )?;
            let grt_remediate = resolve_remediate(
                acc.grt.as_ref().and_then(|a| a.remediate.as_ref()),
                &name,
                &format!("{base}.grt.remediate"),
                &addresses,
                &label_addresses,
            )?;

            // A remediate block implies its asset is watched: configuring repair for an asset
            // that is never read would be inert, and silently so.
            let mut watch_eth = eth_remediate.is_some();
            let mut watch_grt = grt_remediate.is_some();
            let mut watch_activity = false;
            for asset in &acc.watch {
                match asset.to_ascii_lowercase().as_str() {
                    "eth" => watch_eth = true,
                    "grt" => watch_grt = true,
                    "activity" => watch_activity = true,
                    other => {
                        return Err(ConfigError::UnknownWatchAsset {
                            field: format!("{base}.watch"),
                            value: other.to_string(),
                        }
                        .into())
                    }
                }
            }

            let activity = match &acc.activity {
                Some(a) => {
                    let field = format!("{base}.activity");
                    let emitter = resolve_name(
                        &expand_env(&a.emitter, &field)?,
                        &name,
                        &addresses,
                        &label_addresses,
                    )?;
                    let lookback = match &a.lookback {
                        Some(l) => parse_duration(l, &format!("{field}.lookback"))?,
                        None => DEFAULT_ACTIVITY_LOOKBACK,
                    };
                    let matches = match &a.match_ {
                        Some(m) => crate::activity::TxMatch::parse(m).map_err(|reason| {
                            ConfigError::InvalidValue {
                                field: format!("{field}.match"),
                                value: m.clone(),
                                reason,
                            }
                        })?,
                        None => crate::activity::TxMatch::From,
                    };
                    Some(ActivitySpec {
                        topic0: alloy::primitives::keccak256(a.event.as_bytes()),
                        emitter,
                        event: a.event.clone(),
                        lookback,
                        matches,
                    })
                }
                None => None,
            };
            // An activity block implies the watch, the same way a remediate block implies its
            // asset: configuring it and never reading it would be inert, and silently so.
            let watch_activity = watch_activity || activity.is_some();

            let mut allowances = Vec::new();
            for spender in &acc.allowances {
                let field = format!("{base}.allowances");
                let expanded = expand_env(spender, &field)?;
                allowances.push(resolve_name(
                    &expanded,
                    &name,
                    &addresses,
                    &label_addresses,
                )?);
            }

            resolved_accounts.push(ResolvedAccount {
                label: acc.label.clone(),
                address,
                group: acc.group.clone(),
                watch_eth,
                watch_grt,
                watch_activity,
                activity,
                eth_remediate,
                grt_remediate,
                allowances,
                source: acc.source,
            });
        }

        let signer_key = match &net.signer_key {
            Some(raw) => Some(expand_env(raw, &format!("networks.{name}.signer_key"))?),
            None => None,
        };

        Ok(Resolved {
            network: name,
            chain_id,
            rpc_url,
            signer_key,
            max_concurrency: net
                .max_concurrency
                .unwrap_or(crate::rpc::DEFAULT_MAX_IN_FLIGHT),
            max_log_range: net.max_log_range.unwrap_or(crate::rpc::DEFAULT_LOG_RANGE),
            max_lag,
            addresses,
            accounts: resolved_accounts,
            sources: self.sources.clone(),
            used_defaults: self.used_defaults,
        })
    }

    /// Network name for a chain id, used to make chain-id mismatches legible.
    pub fn network_name_for_chain(&self, chain_id: u64) -> Option<String> {
        self.networks
            .iter()
            .find(|(_, n)| n.chain_id == Some(chain_id))
            .map(|(name, _)| name.clone())
    }
}

// ---------------------------------------------------------------------------------------
// Resolved config
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Spender {
    /// What to print: an address-book name, an account label, or the address itself.
    pub display: String,
    pub address: Address,
}

/// Event-driven activity: the most recent log from `emitter` matching `topic0` whose
/// transaction involves this account.
#[derive(Debug, Clone)]
pub struct ActivitySpec {
    pub emitter: Spender,
    pub event: String,
    pub topic0: alloy::primitives::B256,
    pub lookback: Duration,
    pub matches: crate::activity::TxMatch,
}

/// A configured repair: below `below`, move `top_up_by` in, sourced from `from`.
///
/// The only threshold the tool holds. Alert thresholds live in Grafana; this one exists
/// because repair must keep working when the metrics pipeline does not.
#[derive(Debug, Clone)]
pub struct RemediationSpec {
    pub below: DecimalStr,
    pub top_up_by: DecimalStr,
    /// `None` means the signing wallet funds it from its own balance.
    pub from: Option<Spender>,
}

#[derive(Debug, Clone)]
pub struct ResolvedAccount {
    pub label: String,
    pub address: Address,
    pub group: Option<String>,
    pub watch_eth: bool,
    pub watch_grt: bool,
    /// Archive-only: locating the last transaction means reading historical nonces.
    pub watch_activity: bool,
    /// Event-driven activity. When set, it replaces the nonce search for this account.
    pub activity: Option<ActivitySpec>,
    pub eth_remediate: Option<RemediationSpec>,
    pub grt_remediate: Option<RemediationSpec>,
    pub allowances: Vec<Spender>,
    pub source: Source,
}

impl ResolvedAccount {
    /// True when anything about this account needs a token read.
    pub fn needs_token(&self) -> bool {
        self.watch_grt || !self.allowances.is_empty()
    }
}

#[derive(Clone)]
pub struct Resolved {
    pub network: String,
    pub chain_id: u64,
    pub rpc_url: String,
    /// Redacted everywhere it could be printed; see `Resolved`'s Debug impl.
    pub signer_key: Option<String>,
    /// Requests in flight against this endpoint at once.
    pub max_concurrency: usize,
    /// Largest `eth_getLogs` span to attempt against this endpoint.
    pub max_log_range: u64,
    pub max_lag: Duration,
    pub addresses: BTreeMap<String, Address>,
    pub accounts: Vec<ResolvedAccount>,
    pub sources: Vec<String>,
    pub used_defaults: bool,
}

/// Hand-written so a signer key can never reach a log line, a panic message, or `{:?}`.
impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("network", &self.network)
            .field("chain_id", &self.chain_id)
            .field("rpc_url", &self.rpc_url)
            .field(
                "signer_key",
                &self.signer_key.as_ref().map(|_| "<redacted>"),
            )
            .field("accounts", &self.accounts.len())
            .finish()
    }
}

impl Resolved {
    /// Look up an address-book entry by name.
    pub fn address_of(&self, name: &str) -> Option<Address> {
        self.addresses.get(name).copied()
    }

    /// GRT, which every token check needs.
    pub fn graph_token(&self) -> Result<Address> {
        self.address_of("GraphToken").ok_or_else(|| {
            ConfigError::MissingGraphToken {
                network: self.network.clone(),
                no_defaults: !self.used_defaults,
            }
            .into()
        })
    }

    pub fn require_accounts(&self) -> Result<&[ResolvedAccount]> {
        if self.accounts.is_empty() {
            return Err(ConfigError::NoAccounts {
                network: self.network.clone(),
                sources: self.sources.clone(),
            }
            .into());
        }
        Ok(&self.accounts)
    }

    /// Prefer a configured Multicall3 over the canonical deployment, so a chain that put it
    /// somewhere else stays usable.
    pub fn multicall3(&self) -> Address {
        self.address_of("Multicall3")
            .unwrap_or(crate::rpc::CANONICAL_MULTICALL3)
    }

    /// Groups present, in first-seen order, with ungrouped accounts last.
    pub fn groups(&self) -> Vec<Option<String>> {
        let mut out: Vec<Option<String>> = Vec::new();
        for acc in &self.accounts {
            if acc.group.is_some() && !out.contains(&acc.group) {
                out.push(acc.group.clone());
            }
        }
        if self.accounts.iter().any(|a| a.group.is_none()) {
            out.push(None);
        }
        out
    }
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

fn default_user_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = dirs::config_dir() {
        paths.push(dir.join("graph-network").join("config.toml"));
    }
    paths.push(PathBuf::from("graph-network.toml"));
    paths
}

fn read_toml(path: &Path) -> Result<RawConfig> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| {
        ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        }
        .into()
    })
}

/// Expand `${VAR}` references. A missing variable is an error rather than an empty string:
/// an RPC URL with a hole in it fails later and less clearly.
fn expand_env(input: &str, field: &str) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| ConfigError::MissingEnvVar {
            path: field.to_string(),
            var: after.to_string(),
        })?;
        let var = &after[..end];
        let value = std::env::var(var).map_err(|_| ConfigError::MissingEnvVar {
            path: field.to_string(),
            var: var.to_string(),
        })?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn parse_address(value: &str, field: &str) -> Result<Address> {
    value.parse::<Address>().map_err(|_| {
        ConfigError::InvalidAddress {
            field: field.to_string(),
            value: value.to_string(),
        }
        .into()
    })
}

/// `"60s"`, `"5m"`, `"24h"`, or bare seconds.
fn parse_duration(value: &str, field: &str) -> Result<Duration> {
    let v = value.trim();
    let invalid = || ConfigError::InvalidDuration {
        field: field.to_string(),
        value: value.to_string(),
    };

    let (digits, multiplier) = match v.chars().last() {
        Some('s') => (&v[..v.len() - 1], 1),
        Some('m') => (&v[..v.len() - 1], 60),
        Some('h') => (&v[..v.len() - 1], 3600),
        Some('d') => (&v[..v.len() - 1], 86400),
        Some(c) if c.is_ascii_digit() => (v, 1),
        _ => return Err(invalid().into()),
    };

    let n: u64 = digits.trim().parse().map_err(|_| invalid())?;
    Ok(Duration::from_secs(n * multiplier))
}

fn into_asset_spec(raw: RawAsset) -> AssetSpec {
    AssetSpec {
        remediate: raw.remediate.map(|r| RemediateSpec {
            below: r.below,
            top_up_by: r.top_up_by,
            from: r.from,
        }),
    }
}

fn resolve_remediate(
    raw: Option<&RemediateSpec>,
    network: &str,
    field: &str,
    addresses: &BTreeMap<String, Address>,
    labels: &BTreeMap<String, Address>,
) -> Result<Option<RemediationSpec>> {
    let Some(raw) = raw else { return Ok(None) };

    let below = DecimalStr::parse(&raw.below).map_err(|_| ConfigError::InvalidAmount {
        field: format!("{field}.below"),
        value: raw.below.clone(),
    })?;
    let top_up_by = DecimalStr::parse(&raw.top_up_by).map_err(|_| ConfigError::InvalidAmount {
        field: format!("{field}.top_up_by"),
        value: raw.top_up_by.clone(),
    })?;
    let from = match &raw.from {
        Some(f) => {
            let expanded = expand_env(f, &format!("{field}.from"))?;
            Some(resolve_name(&expanded, network, addresses, labels)?)
        }
        None => None,
    };

    Ok(Some(RemediationSpec {
        below,
        top_up_by,
        from,
    }))
}

/// Resolve an account's own `address`: a hex literal, or a name from the address book.
///
/// Account labels are deliberately *not* consulted here. An account defining itself in terms
/// of another account's label would be indirection with no upside, and a cycle waiting to
/// happen.
fn resolve_book_entry(
    value: &str,
    network: &str,
    field: &str,
    addresses: &BTreeMap<String, Address>,
) -> Result<Address> {
    if let Some(address) = addresses.get(value) {
        return Ok(*address);
    }
    match value.parse::<Address>() {
        Ok(address) => Ok(address),
        Err(_) => Err(ConfigError::UnresolvableAddress {
            network: network.to_string(),
            field: field.to_string(),
            value: value.to_string(),
            addresses: addresses.keys().cloned().collect(),
        }
        .into()),
    }
}

/// Resolve a spender: address-book name, then account label, then hex address.
///
/// A name that is both a book entry and a label is an error rather than a precedence win. The
/// same string silently meaning two addresses is exactly how a readout ends up lying.
fn resolve_name(
    name: &str,
    network: &str,
    addresses: &BTreeMap<String, Address>,
    labels: &BTreeMap<String, Address>,
) -> Result<Spender> {
    let in_book = addresses.get(name).copied();
    let as_label = labels.get(name).copied();

    match (in_book, as_label) {
        (Some(book), Some(label)) if book != label => Err(ConfigError::AmbiguousName {
            network: network.to_string(),
            name: name.to_string(),
            book,
            label,
        }
        .into()),
        (Some(address), _) | (None, Some(address)) => Ok(Spender {
            display: name.to_string(),
            address,
        }),
        (None, None) => match name.parse::<Address>() {
            Ok(address) => Ok(Spender {
                display: name.to_string(),
                address,
            }),
            Err(_) => Err(ConfigError::UnresolvableName {
                network: network.to_string(),
                name: name.to_string(),
                addresses: addresses.keys().cloned().collect(),
                labels: labels.keys().cloned().collect(),
            }
            .into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    const DEFAULTS: &str = r#"
default_network = "arbitrum-one"

[networks.arbitrum-one]
chain_id = 42161
rpc_url = "https://shipped.example/rpc"

[networks.arbitrum-one.addresses]
GraphToken = "0x9623063377AD1B27544C965cCd7342f7EA7e88C7"
PaymentsEscrow = "0x1111111111111111111111111111111111111111"
BillingWallet = "0x2222222222222222222222222222222222222222"

[[accounts.arbitrum-one]]
address = "BillingWallet"
group = "billing"
watch = ["eth"]
"#;

    fn load(user: &str) -> Config {
        Config::from_layers(&[
            ("<defaults>", DEFAULTS, Source::Default),
            ("<user>", user, Source::User),
        ])
        .expect("config should load")
    }

    #[test]
    fn account_takes_its_name_from_the_address_book() {
        let resolved = load("").resolve(None, None).unwrap();
        assert_eq!(resolved.accounts[0].label, "BillingWallet");
        assert_eq!(
            resolved.accounts[0].address,
            "0x2222222222222222222222222222222222222222"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(resolved.accounts[0].group.as_deref(), Some("billing"));
    }

    #[test]
    fn groups_carry_no_behavior() {
        // Same group as the shipped account; nothing is inherited, so it watches nothing.
        let config = load(
            r#"
[[accounts.arbitrum-one]]
address = "PaymentsEscrow"
group = "billing"
"#,
        );
        let resolved = config.resolve(None, None).unwrap();
        let escrow = resolved
            .accounts
            .iter()
            .find(|a| a.label == "PaymentsEscrow")
            .unwrap();
        assert!(!escrow.watch_eth && !escrow.watch_grt);
        let billing = resolved
            .accounts
            .iter()
            .find(|a| a.label == "BillingWallet")
            .unwrap();
        assert!(billing.watch_eth);
    }

    #[test]
    fn watch_selects_which_assets_are_read() {
        let config = load(
            r#"
[[accounts.arbitrum-one]]
address = "PaymentsEscrow"
watch = ["grt"]
"#,
        );
        let acc = config
            .resolve(None, None)
            .unwrap()
            .accounts
            .into_iter()
            .find(|a| a.label == "PaymentsEscrow")
            .unwrap();
        assert!(acc.watch_grt && !acc.watch_eth);
        assert!(acc.needs_token());
    }

    #[test]
    fn an_unknown_watch_asset_is_rejected_rather_than_ignored() {
        // Silently not reading something the operator asked for is the worst outcome.
        let config = load(
            r#"
[[accounts.arbitrum-one]]
address = "PaymentsEscrow"
watch = ["btc"]
"#,
        );
        let err = config.resolve(None, None).unwrap_err().to_string();
        assert!(err.contains("btc"), "{err}");
        assert!(err.contains("eth"), "{err}");
    }

    #[test]
    fn a_remediate_block_implies_its_asset_is_watched() {
        // Configuring repair for an asset that is never read would be inert, and silently so.
        let config = load(
            r#"
[[accounts.arbitrum-one]]
address = "PaymentsEscrow"

[accounts.arbitrum-one.grt.remediate]
below = "25000"
top_up_by = "30000"
from = "BillingWallet"
"#,
        );
        let acc = config
            .resolve(None, None)
            .unwrap()
            .accounts
            .into_iter()
            .find(|a| a.label == "PaymentsEscrow")
            .unwrap();
        assert!(acc.watch_grt);
        let r = acc.grt_remediate.expect("remediation configured");
        assert_eq!(r.below.as_str(), "25000");
        assert_eq!(r.from.unwrap().display, "BillingWallet");
    }

    #[test]
    fn remediation_is_absent_unless_configured() {
        let resolved = load("").resolve(None, None).unwrap();
        let acc = &resolved.accounts[0];
        assert!(acc.eth_remediate.is_none());
        assert!(acc.grt_remediate.is_none());
    }

    #[test]
    fn user_config_extends_and_overrides_by_label() {
        let config = load(
            r#"
[[accounts.arbitrum-one]]
address = "BillingWallet"
watch = ["eth", "grt"]

[[accounts.arbitrum-one]]
address = "PaymentsEscrow"
watch = ["grt"]
"#,
        );
        let resolved = config.resolve(None, None).unwrap();
        assert_eq!(resolved.accounts.len(), 2);
        let billing = &resolved.accounts[0];
        assert_eq!(billing.label, "BillingWallet");
        assert!(billing.watch_grt, "user layer replaced the shipped entry");
        assert_eq!(billing.source, Source::User);
    }

    #[test]
    fn per_key_merge_keeps_shipped_addresses() {
        let config = load(
            r#"
[networks.arbitrum-one]
rpc_url = "https://mine.example/rpc"
"#,
        );
        let resolved = config.resolve(None, None).unwrap();
        assert_eq!(resolved.rpc_url, "https://mine.example/rpc");
        assert!(resolved.address_of("GraphToken").is_some());
        assert!(resolved.address_of("PaymentsEscrow").is_some());
    }

    #[test]
    fn enabled_false_drops_one_account() {
        let config = load(
            r#"
[[accounts.arbitrum-one]]
address = "BillingWallet"
enabled = false
"#,
        );
        assert!(config.resolve(None, None).unwrap().accounts.is_empty());
    }

    #[test]
    fn max_lag_defaults_by_chain_and_can_be_overridden() {
        let resolved = load("").resolve(None, None).unwrap();
        assert_eq!(resolved.max_lag, Duration::from_secs(60)); // Arbitrum default

        let config = load(
            r#"
[networks.arbitrum-one]
max_lag = "5m"
"#,
        );
        assert_eq!(
            config.resolve(None, None).unwrap().max_lag,
            Duration::from_secs(300)
        );
    }

    #[test]
    fn parses_duration_units() {
        assert_eq!(parse_duration("60s", "f").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("5m", "f").unwrap(), Duration::from_secs(300));
        assert_eq!(
            parse_duration("24h", "f").unwrap(),
            Duration::from_secs(86400)
        );
        assert_eq!(parse_duration("90", "f").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("soon", "f").is_err());
        assert!(parse_duration("", "f").is_err());
    }

    #[test]
    fn ambiguous_name_is_an_error_not_a_precedence_win() {
        let config = load(
            r#"
[[accounts.arbitrum-one]]
label = "PaymentsEscrow"
address = "0x4444444444444444444444444444444444444444"

[[accounts.arbitrum-one]]
address = "0x5555555555555555555555555555555555555555"
label = "payer"
[[accounts.arbitrum-one.allowances]]
spender = "PaymentsEscrow"
"#,
        );
        assert!(matches!(
            config.resolve(None, None).unwrap_err(),
            Error::Config(ConfigError::AmbiguousName { .. })
        ));
    }

    #[test]
    fn unresolvable_spender_lists_what_was_available() {
        let config = load(
            r#"
[[accounts.arbitrum-one]]
address = "BillingWallet"
[[accounts.arbitrum-one.allowances]]
spender = "NoSuchThing"
"#,
        );
        let err = config.resolve(None, None).unwrap_err().to_string();
        assert!(err.contains("NoSuchThing"), "{err}");
        assert!(err.contains("GraphToken"), "{err}");
    }

    #[test]
    fn use_defaults_false_discards_shipped_layer() {
        let config = Config::from_layers(&[(
            "<user>",
            r#"
use_defaults = false
default_network = "local"
[networks.local]
chain_id = 1337
rpc_url = "http://127.0.0.1:8545"
"#,
            Source::User,
        )])
        .unwrap();
        assert!(!config.networks.contains_key("arbitrum-one"));
        let resolved = config.resolve(None, None).unwrap();
        let msg = resolved.graph_token().unwrap_err().to_string();
        assert!(msg.contains("GraphToken"), "{msg}");
        assert!(msg.contains("use_defaults"), "{msg}");
    }

    #[test]
    fn unused_network_with_unset_env_var_does_not_break_the_active_one() {
        let config = load(
            r#"
[networks.arbitrum-sepolia]
chain_id = 421614
rpc_url = "${GN_TEST_DEFINITELY_UNSET}"
"#,
        );
        assert_eq!(
            config.resolve(Some("arbitrum-one"), None).unwrap().chain_id,
            42161
        );
    }

    #[test]
    fn groups_list_puts_ungrouped_last() {
        let config = load(
            r#"
[[accounts.arbitrum-one]]
address = "PaymentsEscrow"

[[accounts.arbitrum-one]]
address = "0x7777777777777777777777777777777777777777"
label = "oracle-one"
group = "oracles"
"#,
        );
        let resolved = config.resolve(None, None).unwrap();
        assert_eq!(
            resolved.groups(),
            vec![
                Some("billing".to_string()),
                Some("oracles".to_string()),
                None
            ]
        );
    }

    #[test]
    fn no_accounts_error_names_the_network() {
        let config = Config::from_layers(&[(
            "<user>",
            r#"
default_network = "empty-net"
[networks.empty-net]
chain_id = 1
rpc_url = "http://localhost:8545"
"#,
            Source::User,
        )])
        .unwrap();
        let resolved = config.resolve(None, None).unwrap();
        let err = resolved.require_accounts().unwrap_err().to_string();
        assert!(err.contains("empty-net"), "{err}");
    }

    #[test]
    fn shipped_defaults_parse() {
        let raw: std::result::Result<RawConfig, _> = toml::from_str(DEFAULT_CONFIG);
        assert!(raw.is_ok(), "{:?}", raw.err());
    }
}
