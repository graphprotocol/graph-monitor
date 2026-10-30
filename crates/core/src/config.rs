//! Configuration loading, layering and resolution.
//!
//! Layers, later wins:
//!   1. the shipped `config/default.toml`, embedded at build time
//!   2. `$XDG_CONFIG_HOME/graph-monitor/config.toml`
//!   3. `./graph-monitor.toml`
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

/// How far behind wall clock a subgraph's latest block may be before it is called stale.
/// Looser than an RPC's allowance: indexing trails the head by design, and a gateway picks
/// among indexers that are each a little behind.
const DEFAULT_SUBGRAPH_MAX_LAG: Duration = Duration::from_secs(15 * 60);

/// The decentralized network's gateway. Queries carry the API key as a bearer token.
const DEFAULT_GATEWAY_URL: &str = "https://gateway.thegraph.com/api";

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
    /// Removed: every configured network runs. Accepted only to say so.
    default_network: Option<toml::Value>,
    use_defaults: Option<bool>,
    /// Removed: its parts moved to `[addresses.*]`, `[sources.rpc.*]` and `[remediation.*]`.
    /// Accepted only to say so.
    networks: Option<toml::Value>,
    /// Address books, keyed by network: `[addresses.arbitrum-one]`. Contracts *and* EOAs —
    /// nothing in the tool cares which.
    #[serde(default)]
    addresses: BTreeMap<String, BTreeMap<String, String>>,
    /// Remediation settings, keyed by network: `[remediation.arbitrum-one]`.
    #[serde(default)]
    remediation: BTreeMap<String, RawRemediation>,
    /// Keyed by network name: `[[accounts.arbitrum-one]]`.
    #[serde(default)]
    accounts: BTreeMap<String, Vec<RawAccount>>,
    /// What the tool reads from, as opposed to what it watches.
    sources: Option<RawSources>,
    gateway: Option<RawGateway>,
    /// Keyed by network name: `[[subgraphs.arbitrum-one]]`. The network is the chain the
    /// subgraph indexes, which is what its lag is measured against.
    #[serde(default)]
    subgraphs: BTreeMap<String, Vec<RawSubgraph>>,
}

/// Inputs. RPCs today; subgraphs the tool reads from (the Graph Monitor subgraph) later.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSources {
    /// Keyed by network name: `[sources.rpc.arbitrum-one]`.
    #[serde(default)]
    rpc: BTreeMap<String, RawRpc>,
    /// Keyed by the network the subgraph indexes: `[sources.subgraph.arbitrum-one]`.
    #[serde(default)]
    subgraph: BTreeMap<String, RawSubgraphSource>,
}

/// A subgraph the tool reads from. Unlike `[[subgraphs.*]]`, which are watched, a source
/// failing makes the tool blind to what it serves.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubgraphSource {
    /// Names the source in output: `subgraph:<label>`. Defaults to the network.
    label: Option<String>,
    id: Option<String>,
    deployment: Option<String>,
    url: Option<String>,
    max_lag: Option<String>,
    enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRpc {
    url: Option<String>,
    /// The chain the endpoint must report. Only needed for a network the tool does not know.
    chain_id: Option<u64>,
    /// How far behind wall clock the head block may be, e.g. `"60s"`.
    max_lag: Option<String>,
    /// Requests in flight against this endpoint at once.
    max_concurrency: Option<usize>,
    /// Largest `eth_getLogs` span to attempt. Narrowed automatically if the endpoint refuses.
    max_log_range: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGateway {
    url: Option<String>,
    /// `${ENV}` works here, so it need not be a literal.
    api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubgraph {
    label: String,
    /// Subgraph id: follows whichever deployment is currently published.
    id: Option<String>,
    /// Deployment ids (`Qm…`) of this subgraph, each checked on its own, e.g. the version
    /// being upgraded to while the published one still serves.
    #[serde(default)]
    deployments: Vec<String>,
    /// Any GraphQL endpoint, e.g. a graph-node directly. No API key is sent.
    url: Option<String>,
    max_lag: Option<String>,
    enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRemediation {
    /// Private key that signs repairs on this network. Never read from the shipped layer;
    /// `${ENV}` works here, so it need not be a literal.
    signer_key: Option<String>,
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

/// A subgraph source, as configured.
#[derive(Debug, Default, Clone)]
pub struct SubgraphSource {
    pub label: Option<String>,
    pub id: Option<String>,
    pub deployment: Option<String>,
    pub url: Option<String>,
    pub max_lag: Option<String>,
    pub enabled: Option<bool>,
}

/// An RPC source, as configured.
#[derive(Debug, Default, Clone)]
pub struct Rpc {
    /// Kept unexpanded until needed: a config may legitimately describe networks whose
    /// `${RPC_URL}` env vars are not set in this shell, and only the ones in use must resolve.
    pub url: Option<String>,
    pub chain_id: Option<u64>,
    pub max_lag: Option<String>,
    pub max_concurrency: Option<usize>,
    pub max_log_range: Option<u64>,
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

#[derive(Debug, Default, Clone)]
pub struct Gateway {
    pub url: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Subgraph {
    pub label: String,
    pub id: Option<String>,
    pub deployments: Vec<String>,
    pub url: Option<String>,
    pub max_lag: Option<String>,
    pub enabled: bool,
    pub source: Source,
}

#[derive(Debug, Default)]
pub struct Config {
    /// Address books, keyed by network.
    pub addresses: BTreeMap<String, BTreeMap<String, String>>,
    /// Remediation signer keys, keyed by network. Kept unexpanded until resolved.
    pub signer_keys: BTreeMap<String, String>,
    pub accounts: BTreeMap<String, Vec<Account>>,
    /// RPC sources, keyed by network.
    pub rpcs: BTreeMap<String, Rpc>,
    /// Subgraph sources, keyed by the network they index.
    pub subgraph_sources: BTreeMap<String, SubgraphSource>,
    pub gateway: Gateway,
    pub subgraphs: BTreeMap<String, Vec<Subgraph>>,
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

        if raw.default_network.is_some() {
            return Err(ConfigError::DefaultNetworkRemoved {
                layer: label.to_string(),
            }
            .into());
        }

        if raw.networks.is_some() {
            return Err(ConfigError::NetworksRemoved {
                layer: label.to_string(),
            }
            .into());
        }

        // Address books merge per entry: adding one name in user config must not discard the
        // shipped book for that network.
        for (network, book) in raw.addresses {
            self.addresses.entry(network).or_default().extend(book);
        }

        for (network, r) in raw.remediation {
            if let Some(key) = r.signer_key {
                // A key in the embedded default would ship inside the binary. Refusing it here
                // makes that impossible rather than merely discouraged.
                if source == Source::Default {
                    return Err(ConfigError::SignerKeyInDefaults.into());
                }
                self.signer_keys.insert(network, key);
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

        let raw_sources = raw.sources.unwrap_or_default();

        // Subgraph sources merge per key as well.
        for (name, rs) in raw_sources.subgraph {
            let entry = self.subgraph_sources.entry(name).or_default();
            // Setting one target replaces the others: a user pointing a shipped source at
            // their own graph-node must not end up with two targets and an error.
            if rs.id.is_some() || rs.deployment.is_some() || rs.url.is_some() {
                entry.id = rs.id;
                entry.deployment = rs.deployment;
                entry.url = rs.url;
            }
            if let Some(l) = rs.label {
                entry.label = Some(l);
            }
            if let Some(lag) = rs.max_lag {
                entry.max_lag = Some(lag);
            }
            if let Some(e) = rs.enabled {
                entry.enabled = Some(e);
            }
        }

        // RPC sources merge per key too: overriding one url keeps the shipped max_lag.
        for (name, rr) in raw_sources.rpc {
            let entry = self.rpcs.entry(name).or_default();
            if let Some(url) = rr.url {
                entry.url = Some(url);
            }
            if let Some(id) = rr.chain_id {
                entry.chain_id = Some(id);
            }
            if let Some(lag) = rr.max_lag {
                entry.max_lag = Some(lag);
            }
            if let Some(c) = rr.max_concurrency {
                entry.max_concurrency = Some(c);
            }
            if let Some(r) = rr.max_log_range {
                entry.max_log_range = Some(r);
            }
        }

        if let Some(g) = raw.gateway {
            if let Some(url) = g.url {
                self.gateway.url = Some(url);
            }
            if let Some(key) = g.api_key {
                self.gateway.api_key = Some(key);
            }
        }

        // Subgraphs extend and override by label, the same way accounts do.
        for (network, subgraphs) in raw.subgraphs {
            let mut seen_in_layer: Vec<String> = Vec::new();
            let existing = self.subgraphs.entry(network.clone()).or_default();

            for sg in subgraphs {
                if seen_in_layer.contains(&sg.label) {
                    return Err(ConfigError::DuplicateLabel {
                        network,
                        label: sg.label,
                    }
                    .into());
                }
                seen_in_layer.push(sg.label.clone());

                let merged = Subgraph {
                    label: sg.label,
                    id: sg.id,
                    deployments: sg.deployments,
                    url: sg.url,
                    max_lag: sg.max_lag,
                    enabled: sg.enabled.unwrap_or(true),
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

    /// Turn strings into typed values, for every network that has something to watch.
    ///
    /// There is no main network: every network with accounts runs, and so does every network
    /// whose subgraphs have an RPC to be measured against. `only` narrows the run to one
    /// network, and `rpc_override` replaces that one network's RPC url.
    pub fn resolve(&self, only: Option<&str>, rpc_override: Option<&str>) -> Result<Resolved> {
        let has_rpc = |n: &str| self.rpcs.get(n).is_some_and(|r| r.url.is_some());

        let mut names: std::collections::BTreeSet<String> = self
            .accounts
            .iter()
            .filter(|(_, accounts)| accounts.iter().any(|a| a.enabled))
            .map(|(n, _)| n.clone())
            .collect();
        for (network, list) in &self.subgraphs {
            if list.iter().any(|s| s.enabled) && has_rpc(network) {
                names.insert(network.clone());
            }
        }

        if let Some(want) = only {
            let known = self.addresses.contains_key(want)
                || self.accounts.contains_key(want)
                || self.subgraphs.contains_key(want)
                || self.rpcs.contains_key(want);
            if !known {
                let mut available: std::collections::BTreeSet<String> =
                    self.rpcs.keys().cloned().collect();
                available.extend(names.iter().cloned());
                return Err(ConfigError::UnknownNetwork {
                    name: want.to_string(),
                    available: available.into_iter().collect(),
                }
                .into());
            }
            names = [want.to_string()].into();
        }

        if rpc_override.is_some() && names.len() != 1 {
            return Err(ConfigError::RpcOverrideNeedsNetwork {
                networks: names.into_iter().collect(),
            }
            .into());
        }

        let mut networks = Vec::new();
        for name in &names {
            networks.push(self.resolve_network(name, rpc_override)?);
        }

        let mut subgraphs = self.resolve_subgraphs()?;
        let mut subgraph_sources = self.resolve_subgraph_sources()?;
        if let Some(want) = only {
            subgraphs.retain(|s| s.network == want);
            subgraph_sources.retain(|s| s.network == want);
        }

        if networks.iter().all(|n| n.accounts.is_empty()) && subgraphs.is_empty() {
            return Err(ConfigError::NothingToWatch {
                sources: self.sources.clone(),
            }
            .into());
        }

        Ok(Resolved {
            networks,
            subgraph_sources,
            subgraphs,
            sources: self.sources.clone(),
            used_defaults: self.used_defaults,
        })
    }

    /// One network: its chain, its RPC, its address book and its accounts.
    fn resolve_network(&self, name: &str, rpc_override: Option<&str>) -> Result<ResolvedNetwork> {
        let name = name.to_string();
        let rpc = self.resolve_rpc(&name, rpc_override)?;
        let chain_id = rpc.chain_id;

        // The address book resolves first, so everything downstream can refer to it by name.
        let mut addresses = BTreeMap::new();
        for (key, value) in self.addresses.get(&name).into_iter().flatten() {
            let field = format!("addresses.{name}.{key}");
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

        let signer_key = match self.signer_keys.get(&name) {
            Some(raw) => Some(expand_env(raw, &format!("remediation.{name}.signer_key"))?),
            None => None,
        };

        Ok(ResolvedNetwork {
            name,
            chain_id,
            rpc,
            signer_key,
            addresses,
            accounts: resolved_accounts,
            used_defaults: self.used_defaults,
        })
    }

    /// The RPC source for `network`, with the chain id it must report.
    fn resolve_rpc(&self, network: &str, url_override: Option<&str>) -> Result<ResolvedRpc> {
        let rpc = self.rpcs.get(network).cloned().unwrap_or_default();
        // Configured wins, so a well-known name can still be pointed somewhere unusual.
        let chain_id = rpc
            .chain_id
            .or_else(|| crate::source::known_chain_id(network))
            .ok_or_else(|| ConfigError::MissingChainId {
                network: network.to_string(),
            })?;
        let field = format!("sources.rpc.{network}");

        let url = match url_override {
            Some(url) => url.to_string(),
            None => {
                let raw = rpc
                    .url
                    .as_deref()
                    .ok_or_else(|| ConfigError::MissingRpcUrl {
                        network: network.to_string(),
                    })?;
                expand_env(raw, &format!("{field}.url"))?
            }
        };
        let max_lag = match &rpc.max_lag {
            Some(raw) => parse_duration(raw, &format!("{field}.max_lag"))?,
            None => default_max_lag(chain_id),
        };

        Ok(ResolvedRpc {
            network: network.to_string(),
            chain_id,
            url,
            max_lag,
            max_concurrency: rpc
                .max_concurrency
                .unwrap_or(crate::rpc::DEFAULT_MAX_IN_FLIGHT),
            max_log_range: rpc.max_log_range.unwrap_or(crate::rpc::DEFAULT_LOG_RANGE),
        })
    }

    /// Every enabled subgraph, whichever chain it indexes.
    ///
    /// Not just the selected network's: a subgraph is a component of The Graph network, and
    /// checking it needs only the gateway. The chain matters for one number, blocks behind,
    /// which is measured only when that chain's RPC is configured.
    ///
    /// An unset `${GRAPH_API_KEY}` is not a config error here. The shipped config may list
    /// subgraphs, and someone checking balances without a key should still be able to; those
    /// subgraphs report unreadable and say why.
    fn resolve_subgraphs(&self) -> Result<Vec<ResolvedSubgraph>> {
        let gateway = self.gateway_endpoints()?;

        let mut out = Vec::new();
        for (network, sg) in self
            .subgraphs
            .iter()
            .flat_map(|(network, list)| list.iter().map(move |sg| (network, sg)))
        {
            if !sg.enabled {
                continue;
            }
            let field = format!("subgraphs.{network}.{}", sg.label);

            if sg.id.is_some() == sg.url.is_some() {
                return Err(ConfigError::InvalidValue {
                    field,
                    value: "id and url".to_string(),
                    reason: "set exactly one of id or url".to_string(),
                }
                .into());
            }
            if sg.url.is_some() && !sg.deployments.is_empty() {
                return Err(ConfigError::InvalidValue {
                    field: format!("{field}.deployments"),
                    value: sg.deployments.join(", "),
                    reason: "deployments are queried through the gateway, so they need `id`, \
                             not `url`"
                        .to_string(),
                }
                .into());
            }
            for (i, d) in sg.deployments.iter().enumerate() {
                let reason = if !d.starts_with("Qm") {
                    Some("a deployment id starts with Qm; the subgraph id goes in `id`")
                } else if sg.deployments[..i].contains(d) {
                    Some("listed twice")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return Err(ConfigError::InvalidValue {
                        field: format!("{field}.deployments"),
                        value: d.clone(),
                        reason: reason.to_string(),
                    }
                    .into());
                }
            }

            let max_lag = match &sg.max_lag {
                Some(raw) => parse_duration(raw, &format!("{field}.max_lag"))?,
                None => DEFAULT_SUBGRAPH_MAX_LAG,
            };

            let gateway = |path: String| gateway.endpoint(&path);

            // The subgraph itself: whatever version is published, or a direct endpoint.
            let endpoint = match (&sg.id, &sg.url) {
                (Some(id), _) => gateway(format!("subgraphs/id/{id}")),
                (_, Some(url)) => expand_env(url, &format!("{field}.url"))
                    .map(|url| crate::subgraph::Endpoint { url, bearer: None })
                    .map_err(|e| e.to_string()),
                _ => unreachable!("exactly one target was checked above"),
            };
            out.push(ResolvedSubgraph {
                network: network.clone(),
                subgraph: sg.label.clone(),
                deployment: None,
                endpoint,
                max_lag,
                source: sg.source,
            });

            // Then each listed deployment, checked on its own under the same umbrella.
            for d in &sg.deployments {
                out.push(ResolvedSubgraph {
                    network: network.clone(),
                    subgraph: sg.label.clone(),
                    deployment: Some(d.clone()),
                    endpoint: gateway(format!("deployments/id/{d}")),
                    max_lag,
                    source: sg.source,
                });
            }
        }
        Ok(out)
    }

    /// The gateway's base URL and key, shared by every subgraph queried through it.
    fn gateway_endpoints(&self) -> Result<GatewayEndpoints> {
        let base = self
            .gateway
            .url
            .clone()
            .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string());
        let base = expand_env(&base, "gateway.url")?
            .trim_end_matches('/')
            .to_string();
        let api_key = match &self.gateway.api_key {
            Some(raw) => expand_env(raw, "gateway.api_key")
                .map(crate::subgraph::Secret::new)
                .map_err(|e| e.to_string()),
            None => Err("no gateway.api_key configured".to_string()),
        };
        Ok(GatewayEndpoints { base, api_key })
    }

    /// Subgraphs the tool reads from.
    ///
    /// Like watched subgraphs, a missing API key is not a config error: the source reports
    /// itself down with the reason, and everything that does not depend on it still runs.
    fn resolve_subgraph_sources(&self) -> Result<Vec<ResolvedSubgraphSource>> {
        let gateway = self.gateway_endpoints()?;
        let mut out = Vec::new();
        for (network, src) in &self.subgraph_sources {
            if src.enabled == Some(false) {
                continue;
            }
            let field = format!("sources.subgraph.{network}");
            let targets = [&src.id, &src.deployment, &src.url]
                .iter()
                .filter(|t| t.is_some())
                .count();
            if targets != 1 {
                return Err(ConfigError::InvalidValue {
                    field,
                    value: format!("{targets} targets"),
                    reason: "set exactly one of id, deployment or url".to_string(),
                }
                .into());
            }
            if let Some(d) = &src.deployment {
                if !d.starts_with("Qm") {
                    return Err(ConfigError::InvalidValue {
                        field: format!("{field}.deployment"),
                        value: d.clone(),
                        reason: "a deployment id starts with Qm; the subgraph id goes in `id`"
                            .to_string(),
                    }
                    .into());
                }
            }
            let endpoint = match (&src.id, &src.deployment, &src.url) {
                (Some(id), _, _) => gateway.endpoint(&format!("subgraphs/id/{id}")),
                (_, Some(d), _) => gateway.endpoint(&format!("deployments/id/{d}")),
                (_, _, Some(url)) => expand_env(url, &format!("{field}.url"))
                    .map(|url| crate::subgraph::Endpoint { url, bearer: None })
                    .map_err(|e| e.to_string()),
                _ => unreachable!("exactly one target was checked above"),
            };
            let max_lag = match &src.max_lag {
                Some(raw) => parse_duration(raw, &format!("{field}.max_lag"))?,
                None => DEFAULT_SUBGRAPH_MAX_LAG,
            };
            out.push(ResolvedSubgraphSource {
                name: src.label.clone().unwrap_or_else(|| network.clone()),
                network: network.clone(),
                endpoint,
                deployment: src.deployment.clone(),
                max_lag,
            });
        }
        Ok(out)
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

/// The gateway, ready to build endpoints from.
struct GatewayEndpoints {
    base: String,
    api_key: std::result::Result<crate::subgraph::Secret, String>,
}

impl GatewayEndpoints {
    fn endpoint(&self, path: &str) -> std::result::Result<crate::subgraph::Endpoint, String> {
        self.api_key
            .clone()
            .map(|key| crate::subgraph::Endpoint {
                url: format!("{}/{path}", self.base),
                bearer: Some(key),
            })
            .map_err(|reason| format!("{reason} (needed for the gateway)"))
    }
}

/// A subgraph the tool reads from, resolved.
#[derive(Debug, Clone)]
pub struct ResolvedSubgraphSource {
    /// The label, or the network when none is set. Shown as `subgraph:<name>`.
    pub name: String,
    /// The chain it indexes.
    pub network: String,
    /// The query endpoint, or why there is none.
    pub endpoint: std::result::Result<crate::subgraph::Endpoint, String>,
    /// Set when pinned: anything else answering is a fault.
    pub deployment: Option<String>,
    pub max_lag: Duration,
}

#[derive(Debug, Clone)]
pub struct ResolvedSubgraph {
    /// The chain the subgraph indexes.
    pub network: String,
    /// The configured label.
    pub subgraph: String,
    /// A listed deployment this check asks for. `None` checks the published version.
    pub deployment: Option<String>,
    /// The query endpoint, or why there is none.
    pub endpoint: std::result::Result<crate::subgraph::Endpoint, String>,
    pub max_lag: Duration,
    pub source: Source,
}

impl ResolvedSubgraph {
    /// `network` for the published version, `network@Qm…` for a listed deployment.
    pub fn name(&self) -> String {
        match &self.deployment {
            Some(d) => format!("{}@{d}", self.subgraph),
            None => self.subgraph.clone(),
        }
    }
}

/// An RPC source, resolved: the endpoint and the chain it must prove it serves.
#[derive(Clone)]
pub struct ResolvedRpc {
    pub network: String,
    pub chain_id: u64,
    pub url: String,
    pub max_lag: Duration,
    /// Requests in flight against this endpoint at once.
    pub max_concurrency: usize,
    /// Largest `eth_getLogs` span to attempt against this endpoint.
    pub max_log_range: u64,
}

/// Hand-written: RPC URLs often carry an API key in the path.
impl std::fmt::Debug for ResolvedRpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedRpc")
            .field("network", &self.network)
            .field("chain_id", &self.chain_id)
            .finish()
    }
}

/// One network, resolved: everything needed to read and watch it.
#[derive(Clone)]
pub struct ResolvedNetwork {
    pub name: String,
    pub chain_id: u64,
    /// The RPC everything on this network is read through.
    pub rpc: ResolvedRpc,
    /// Redacted everywhere it could be printed; see the Debug impl.
    pub signer_key: Option<String>,
    pub addresses: BTreeMap<String, Address>,
    /// Empty for a network that runs only so its subgraphs can be measured.
    pub accounts: Vec<ResolvedAccount>,
    used_defaults: bool,
}

/// Hand-written so a signer key can never reach a log line, a panic message, or `{:?}`.
impl std::fmt::Debug for ResolvedNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedNetwork")
            .field("name", &self.name)
            .field("chain_id", &self.chain_id)
            .field("rpc", &self.rpc)
            .field(
                "signer_key",
                &self.signer_key.as_ref().map(|_| "<redacted>"),
            )
            .field("accounts", &self.accounts.len())
            .finish()
    }
}

impl ResolvedNetwork {
    /// Look up an address-book entry by name.
    pub fn address_of(&self, name: &str) -> Option<Address> {
        self.addresses.get(name).copied()
    }

    /// GRT, which every token check needs.
    pub fn graph_token(&self) -> Result<Address> {
        self.address_of("GraphToken").ok_or_else(|| {
            ConfigError::MissingGraphToken {
                network: self.name.clone(),
                no_defaults: !self.used_defaults,
            }
            .into()
        })
    }

    /// Prefer a configured Multicall3 over the canonical deployment, so a chain that put it
    /// somewhere else stays usable.
    pub fn multicall3(&self) -> Address {
        self.address_of("Multicall3")
            .unwrap_or(crate::rpc::CANONICAL_MULTICALL3)
    }
}

#[derive(Debug, Clone)]
pub struct Resolved {
    /// Every network that runs, in name order.
    pub networks: Vec<ResolvedNetwork>,
    /// Subgraphs the tool reads from.
    pub subgraph_sources: Vec<ResolvedSubgraphSource>,
    pub subgraphs: Vec<ResolvedSubgraph>,
    /// The config layers loaded, for messages.
    pub sources: Vec<String>,
    pub used_defaults: bool,
}

impl Resolved {
    pub fn network(&self, name: &str) -> Option<&ResolvedNetwork> {
        self.networks.iter().find(|n| n.name == name)
    }

    /// Every account on every network.
    pub fn accounts(&self) -> impl Iterator<Item = (&ResolvedNetwork, &ResolvedAccount)> {
        self.networks
            .iter()
            .flat_map(|n| n.accounts.iter().map(move |a| (n, a)))
    }
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

fn default_user_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = dirs::config_dir() {
        paths.push(dir.join("graph-monitor").join("config.toml"));
    }
    paths.push(PathBuf::from("graph-monitor.toml"));
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
[sources.rpc.arbitrum-one]
url = "https://shipped.example/rpc"

[addresses.arbitrum-one]
GraphToken = "0x9623063377AD1B27544C965cCd7342f7EA7e88C7"
PaymentsEscrow = "0x1111111111111111111111111111111111111111"
BillingWallet = "0x2222222222222222222222222222222222222222"

[[accounts.arbitrum-one]]
address = "BillingWallet"
group = "billing"
watch = ["eth"]
"#;

    /// The Arbitrum network from a resolved config, for tests about one network.
    fn arb(config: &Config) -> ResolvedNetwork {
        config
            .resolve(None, None)
            .unwrap()
            .network("arbitrum-one")
            .expect("arbitrum-one runs")
            .clone()
    }

    fn load(user: &str) -> Config {
        Config::from_layers(&[
            ("<defaults>", DEFAULTS, Source::Default),
            ("<user>", user, Source::User),
        ])
        .expect("config should load")
    }

    #[test]
    fn account_takes_its_name_from_the_address_book() {
        let resolved = arb(&load(""));
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
        let resolved = arb(&config);
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
        let acc = arb(&config)
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
        let acc = arb(&config)
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
        let resolved = arb(&load(""));
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
        let resolved = arb(&config);
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
[sources.rpc.arbitrum-one]
url = "https://mine.example/rpc"
"#,
        );
        let resolved = arb(&config);
        assert_eq!(resolved.rpc.url, "https://mine.example/rpc");
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
        // The only account was disabled, so there is nothing left to watch at all.
        let err = config.resolve(None, None).unwrap_err().to_string();
        assert!(err.contains("nothing to watch"), "{err}");
    }

    #[test]
    fn max_lag_defaults_by_chain_and_can_be_overridden() {
        let resolved = arb(&load(""));
        assert_eq!(resolved.rpc.max_lag, Duration::from_secs(60)); // Arbitrum default

        let config = load(
            r#"
[sources.rpc.arbitrum-one]
max_lag = "5m"
"#,
        );
        let rpc = arb(&config).rpc;
        assert_eq!(rpc.max_lag, Duration::from_secs(300));
        // Per-key: overriding max_lag keeps the shipped url.
        assert_eq!(rpc.url, "https://shipped.example/rpc");
    }

    #[test]
    fn a_networks_block_says_where_its_parts_went() {
        // The old shape must fail loudly, naming the new places, not be ignored.
        for body in [
            "[networks.arbitrum-one]\nchain_id = 42161\n",
            "[networks.arbitrum-one.addresses]\nX = \"0x7777777777777777777777777777777777777777\"\n",
            "[networks.arbitrum-one]\nrpc_url = \"http://x\"\n",
        ] {
            let err = Config::from_layers(&[
                ("<defaults>", DEFAULTS, Source::Default),
                ("<user>", body, Source::User),
            ])
            .unwrap_err()
            .to_string();
            assert!(err.contains("[addresses.X]"), "{err}");
            assert!(err.contains("[remediation.X]"), "{err}");
            assert!(err.contains("<user>"), "{err}");
        }
    }

    #[test]
    fn chain_ids_are_built_in_for_known_networks_and_configurable_for_others() {
        // Known: nothing to configure beyond the url.
        let known = load("[sources.rpc.gnosis]\nurl = \"http://g\"\n[[accounts.gnosis]]\naddress = \"0x7777777777777777777777777777777777777777\"\nwatch = [\"eth\"]\n");
        let resolved = known.resolve(Some("gnosis"), None).unwrap();
        assert_eq!(resolved.networks[0].chain_id, 100);

        // Unknown: an error that says what to add, then it works once added.
        let devnet = "[sources.rpc.devnet]\nurl = \"http://d\"\n[[accounts.devnet]]\naddress = \"0x7777777777777777777777777777777777777777\"\nwatch = [\"eth\"]\n";
        let err = load(devnet)
            .resolve(Some("devnet"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("chain_id"), "{err}");
        let fixed =
            load(&devnet.replace("url = \"http://d\"", "url = \"http://d\"\nchain_id = 31337"));
        assert_eq!(
            fixed.resolve(Some("devnet"), None).unwrap().networks[0].chain_id,
            31337
        );

        // Configured wins over built in.
        let odd = load("[sources.rpc.arbitrum-one]\nchain_id = 999\n");
        assert_eq!(arb(&odd).chain_id, 999);
    }

    #[test]
    fn signer_keys_live_under_remediation() {
        let config = load("[remediation.arbitrum-one]\nsigner_key = \"0xabc\"\n");
        assert_eq!(arb(&config).signer_key.as_deref(), Some("0xabc"));
        assert!(!format!("{:?}", arb(&config)).contains("0xabc"));

        let shipped = Config::from_layers(&[(
            "<defaults>",
            &format!("{DEFAULTS}\n[remediation.arbitrum-one]\nsigner_key = \"0xabc\"\n"),
            Source::Default,
        )]);
        assert!(shipped.is_err());
    }

    #[test]
    fn another_chains_rpc_is_resolved_only_for_its_subgraphs() {
        let with_subgraph = r#"
[sources.rpc.gnosis]
url = "https://gnosis.example"
[sources.rpc.unused]
url = "${GN_TEST_DEFINITELY_UNSET}"
[[subgraphs.gnosis]]
label = "qos"
url = "https://graph-node.example/qos"
"#;
        let resolved = load(with_subgraph).resolve(None, None).unwrap();
        let networks: Vec<(&str, u64)> = resolved
            .networks
            .iter()
            .map(|n| (n.name.as_str(), n.chain_id))
            .collect();
        // `unused` has neither accounts nor subgraphs, so its unset env var never matters.
        assert_eq!(networks, [("arbitrum-one", 42161), ("gnosis", 100)]);
        // Gnosis's default max_lag comes from its chain id, not Arbitrum's.
        let gnosis = resolved.network("gnosis").unwrap();
        assert_eq!(gnosis.rpc.max_lag, Duration::from_secs(120));
        assert!(gnosis.accounts.is_empty());

        // A chain with subgraphs but no RPC is fine: those subgraphs just have no block gap.
        let no_rpc = load(
            "[[subgraphs.gnosis]]\nlabel = \"qos\"\nurl = \"https://graph-node.example/qos\"\n",
        )
        .resolve(None, None)
        .unwrap();
        assert!(no_rpc.network("gnosis").is_none());
        assert_eq!(no_rpc.subgraphs.len(), 1);
    }

    #[test]
    fn rpc_debug_output_never_shows_the_url() {
        // Providers put API keys in the path.
        let resolved =
            load("[sources.rpc.arbitrum-one]\nurl = \"https://rpc.example/secret-key-123\"\n")
                .resolve(None, None)
                .unwrap();
        assert!(!format!("{resolved:?}").contains("secret-key-123"));
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
[sources.rpc.local]
url = "http://127.0.0.1:8545"
chain_id = 1337
[[accounts.local]]
address = "0x7777777777777777777777777777777777777777"
watch = ["grt"]
"#,
            Source::User,
        )])
        .unwrap();
        assert!(!config.addresses.contains_key("arbitrum-one"));
        let resolved = config.resolve(None, None).unwrap();
        let msg = resolved.networks[0].graph_token().unwrap_err().to_string();
        assert!(msg.contains("GraphToken"), "{msg}");
        assert!(msg.contains("use_defaults"), "{msg}");
    }

    #[test]
    fn unused_network_with_unset_env_var_does_not_break_the_active_one() {
        let config = load(
            r#"
[sources.rpc.arbitrum-sepolia]
url = "${GN_TEST_DEFINITELY_UNSET}"
"#,
        );
        let resolved = config.resolve(None, None).unwrap();
        let names: Vec<&str> = resolved.networks.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["arbitrum-one"]);
    }

    #[test]
    fn every_network_with_something_to_watch_runs() {
        let config = load(
            r#"
[sources.rpc.gnosis]
url = "https://gnosis.example"
[addresses.gnosis]
Wallet = "0x7777777777777777777777777777777777777777"
[[accounts.gnosis]]
address = "Wallet"
watch = ["eth"]
"#,
        );
        let resolved = config.resolve(None, None).unwrap();
        let names: Vec<&str> = resolved.networks.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["arbitrum-one", "gnosis"]);
        // Accounts resolve against their own network's address book.
        let accounts: Vec<(&str, &str)> = resolved
            .accounts()
            .map(|(n, a)| (n.name.as_str(), a.label.as_str()))
            .collect();
        assert_eq!(
            accounts,
            [("arbitrum-one", "BillingWallet"), ("gnosis", "Wallet")]
        );

        // --network narrows to one.
        let only = config.resolve(Some("gnosis"), None).unwrap();
        assert_eq!(only.networks.len(), 1);
        assert!(config.resolve(Some("nope"), None).is_err());

        // --rpc-url replaces one network's RPC, so it needs --network once two run.
        let err = config
            .resolve(None, Some("http://x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--network"), "{err}");
        let one = config.resolve(Some("gnosis"), Some("http://x")).unwrap();
        assert_eq!(one.networks[0].rpc.url, "http://x");
    }

    #[test]
    fn default_network_says_it_was_removed() {
        let err = Config::from_layers(&[
            ("<defaults>", DEFAULTS, Source::Default),
            (
                "<user>",
                "default_network = \"arbitrum-one\"\n",
                Source::User,
            ),
        ])
        .unwrap_err()
        .to_string();
        assert!(err.contains("default_network has been removed"), "{err}");
        assert!(err.contains("<user>"), "{err}");
    }

    #[test]
    fn nothing_to_watch_is_an_error() {
        let config = Config::from_layers(&[(
            "<user>",
            r#"
[sources.rpc.empty-net]
url = "http://localhost:8545"
chain_id = 1
"#,
            Source::User,
        )])
        .unwrap();
        let err = config.resolve(None, None).unwrap_err().to_string();
        assert!(err.contains("nothing to watch"), "{err}");
    }

    #[test]
    fn a_subgraph_id_queries_the_gateway_with_the_key_as_a_bearer() {
        let config = load(
            r#"
[gateway]
api_key = "literal-key"

[[subgraphs.arbitrum-one]]
label = "network"
id = "SubgraphIdAbc"

[[subgraphs.arbitrum-one]]
label = "ebo"
id = "EboId"
deployments = ["QmDeploymentXyz"]
max_lag = "5m"

[[subgraphs.arbitrum-one]]
label = "local"
url = "http://graph-node:8000/subgraphs/name/x"
"#,
        );
        let subgraphs = config.resolve(None, None).unwrap().subgraphs;
        assert_eq!(subgraphs.len(), 4);

        let network = subgraphs[0].endpoint.as_ref().unwrap();
        assert_eq!(
            network.url,
            "https://gateway.thegraph.com/api/subgraphs/id/SubgraphIdAbc"
        );
        // The key never goes into the URL.
        assert!(!network.url.contains("literal-key"));
        assert!(network.bearer.is_some());
        assert_eq!(subgraphs[0].max_lag, DEFAULT_SUBGRAPH_MAX_LAG);

        let deployment = &subgraphs[2];
        assert!(deployment
            .endpoint
            .as_ref()
            .unwrap()
            .url
            .ends_with("/deployments/id/QmDeploymentXyz"));
        assert_eq!(deployment.deployment.as_deref(), Some("QmDeploymentXyz"));
        assert_eq!(deployment.max_lag, Duration::from_secs(300));

        // A direct endpoint gets no key: it is not the gateway's to receive.
        assert!(subgraphs[3].endpoint.as_ref().unwrap().bearer.is_none());
    }

    #[test]
    fn a_missing_api_key_makes_gateway_subgraphs_unusable_not_the_config() {
        let config = load(
            r#"
[gateway]
api_key = "${GRAPH_MONITOR_TEST_UNSET_KEY}"

[[subgraphs.arbitrum-one]]
label = "network"
id = "SubgraphIdAbc"
"#,
        );
        // Balances must still be checkable without a gateway key.
        let resolved = config.resolve(None, None).unwrap();
        let err = resolved.subgraphs[0].endpoint.as_ref().unwrap_err();
        assert!(err.contains("GRAPH_MONITOR_TEST_UNSET_KEY"), "{err}");
    }

    #[test]
    fn a_subgraph_needs_exactly_one_target() {
        for body in [
            "label = \"x\"",
            "label = \"x\"\nid = \"a\"\nurl = \"http://b\"",
        ] {
            let config = load(&format!("[[subgraphs.arbitrum-one]]\n{body}\n"));
            let err = config.resolve(None, None).unwrap_err().to_string();
            assert!(err.contains("exactly one"), "{err}");
        }
    }

    #[test]
    fn deployments_expand_under_their_subgraph() {
        let config = load(
            r#"
[[subgraphs.arbitrum-one]]
label = "network"
id = "NetworkId"
deployments = ["QmOne", "QmTwo"]
"#,
        );
        let subgraphs = config.resolve(None, None).unwrap().subgraphs;
        let names: Vec<String> = subgraphs.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["network", "network@QmOne", "network@QmTwo"]);
        assert!(subgraphs.iter().all(|s| s.subgraph == "network"));
        // The subgraph id follows whatever is published, so nothing is pinned for it.
        assert_eq!(subgraphs[0].deployment, None);
        assert_eq!(subgraphs[1].deployment.as_deref(), Some("QmOne"));
    }

    #[test]
    fn bad_deployment_lists_are_rejected() {
        for (body, expect) in [
            // A subgraph id pasted into the deployment list.
            (
                "id = \"N\"\ndeployments = [\"DZz4kDTdmzWLWsV373w2bSmoar3umKKH9y82SUKr5qmp\"]",
                "starts with Qm",
            ),
            (
                "id = \"N\"\ndeployments = [\"QmA\", \"QmA\"]",
                "listed twice",
            ),
            ("url = \"http://n\"\ndeployments = [\"QmA\"]", "need `id`"),
        ] {
            let config = load(&format!(
                "[[subgraphs.arbitrum-one]]\nlabel = \"network\"\n{body}\n"
            ));
            let err = config.resolve(None, None).unwrap_err().to_string();
            assert!(err.contains(expect), "{err}");
        }
    }

    #[test]
    fn user_subgraphs_override_by_label_and_can_be_disabled() {
        let config = Config::from_layers(&[
            (
                "<defaults>",
                &format!(
                    "{DEFAULTS}\n[[subgraphs.arbitrum-one]]\nlabel = \"network\"\nid = \"Shipped\"\n\
                     [[subgraphs.arbitrum-one]]\nlabel = \"qos\"\nid = \"Q\"\n"
                ),
                Source::Default,
            ),
            (
                "<user>",
                "[[subgraphs.arbitrum-one]]\nlabel = \"network\"\nid = \"Mine\"\n\
                 [[subgraphs.arbitrum-one]]\nlabel = \"qos\"\nid = \"Q\"\nenabled = false\n",
                Source::User,
            ),
        ])
        .unwrap();
        let subgraphs = config.resolve(None, None).unwrap().subgraphs;
        assert_eq!(subgraphs.len(), 1);
        assert_eq!(subgraphs[0].source, Source::User);
        assert!(subgraphs[0]
            .endpoint
            .as_ref()
            .unwrap_err()
            .contains("api_key"));
    }

    #[test]
    fn subgraphs_on_other_chains_are_checked_too() {
        // Regression: a subgraph listed under a network other than the selected one was
        // dropped without a word.
        let config = load(
            r#"
[[subgraphs.arbitrum-one]]
label = "network"
id = "NetworkId"

[[subgraphs.gnosis]]
label = "qos"
id = "QosId"
"#,
        );
        let resolved = config.resolve(None, None).unwrap();
        let chains: Vec<(&str, &str)> = resolved
            .subgraphs
            .iter()
            .map(|s| (s.network.as_str(), s.subgraph.as_str()))
            .collect();
        assert_eq!(chains, [("arbitrum-one", "network"), ("gnosis", "qos")]);
    }

    #[test]
    fn subgraph_sources_are_keyed_by_network() {
        let config = load(
            r#"
[gateway]
api_key = "k"
[sources.subgraph.arbitrum-one]
label = "monitor"
id = "MonitorId"
"#,
        );
        let resolved = config.resolve(None, None).unwrap();
        let src = &resolved.subgraph_sources[0];
        assert_eq!(
            (src.name.as_str(), src.network.as_str()),
            ("monitor", "arbitrum-one")
        );
        assert!(src
            .endpoint
            .as_ref()
            .unwrap()
            .url
            .ends_with("/subgraphs/id/MonitorId"));
        assert_eq!(src.max_lag, DEFAULT_SUBGRAPH_MAX_LAG);

        // Without a label it is named after its network, like an RPC.
        let unlabelled = load("[sources.subgraph.arbitrum-one]\nid = \"X\"\n")
            .resolve(None, None)
            .unwrap();
        assert_eq!(unlabelled.subgraph_sources[0].name, "arbitrum-one");

        // --network narrows sources too.
        assert!(config
            .resolve(Some("gnosis"), None)
            .map(|r| r.subgraph_sources.is_empty())
            .unwrap_or(true));

        for (body, expect) in [
            ("label = \"m\"", "exactly one"),
            ("id = \"X\"\nurl = \"http://y\"", "exactly one"),
            ("deployment = \"NotQm\"", "starts with Qm"),
        ] {
            let err = load(&format!("[sources.subgraph.arbitrum-one]\n{body}\n"))
                .resolve(None, None)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{body}: {err}");
        }
    }

    #[test]
    fn a_user_can_repoint_a_shipped_subgraph_source() {
        let shipped = format!(
            "{DEFAULTS}\n[sources.subgraph.arbitrum-one]\nlabel = \"monitor\"\nid = \"Shipped\"\n"
        );
        // Setting `url` must replace the shipped `id`, not collide with it.
        let config = Config::from_layers(&[
            ("<defaults>", &shipped, Source::Default),
            (
                "<user>",
                "[sources.subgraph.arbitrum-one]\nurl = \"http://graph-node/monitor\"\n",
                Source::User,
            ),
        ])
        .unwrap();
        let src = &config.resolve(None, None).unwrap().subgraph_sources[0];
        assert_eq!(src.name, "monitor");
        assert_eq!(
            src.endpoint.as_ref().unwrap().url,
            "http://graph-node/monitor"
        );

        let off = Config::from_layers(&[
            ("<defaults>", &shipped, Source::Default),
            (
                "<user>",
                "[sources.subgraph.arbitrum-one]\nenabled = false\n",
                Source::User,
            ),
        ])
        .unwrap();
        assert!(off.resolve(None, None).unwrap().subgraph_sources.is_empty());
    }

    #[test]
    fn shipped_defaults_parse() {
        let raw: std::result::Result<RawConfig, _> = toml::from_str(DEFAULT_CONFIG);
        assert!(raw.is_ok(), "{:?}", raw.err());
    }
}
