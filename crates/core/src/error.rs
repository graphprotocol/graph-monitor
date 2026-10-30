//! Error types, and the exit codes they map to.

use std::path::PathBuf;

use alloy::primitives::Address;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Rpc(#[from] RpcError),
}

impl Error {
    /// Exit code for this error, per the spec's exit-code table.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Config(_) => 2,
            Error::Rpc(_) => 3,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    // The cause is not interpolated here: it is already attached as `#[source]`, and
    // anyhow's chain formatting would print it a second time.
    #[error("could not read config file {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("{path}: environment variable ${var} is referenced but not set")]
    MissingEnvVar { path: String, var: String },

    #[error(
        "{layer}: default_network has been removed. Every configured network now runs; pass \
         --network to run just one"
    )]
    DefaultNetworkRemoved { layer: String },

    #[error(
        "--rpc-url replaces one network's RPC, but {} networks would run ({}); pass --network \
         to pick one",
        networks.len(),
        join_or_none(networks)
    )]
    RpcOverrideNeedsNetwork { networks: Vec<String> },

    #[error(
        "unknown network {name:?}; configured networks: {}",
        join_or_none(available)
    )]
    UnknownNetwork {
        name: String,
        available: Vec<String>,
    },

    #[error(
        "network {network:?} is not one graph-monitor knows by name, so its chain id must be \
         set:\n\n  [sources.rpc.{network}]\n  chain_id = …"
    )]
    MissingChainId { network: String },

    #[error(
        "network {network:?} has no RPC configured; set url in [sources.rpc.{network}] or pass \
         --rpc-url"
    )]
    MissingRpcUrl { network: String },

    #[error(
        "{layer}: [networks.*] has been removed. Its parts moved:\n\n  \
         [networks.X.addresses] -> [addresses.X]\n  \
         networks.X.signer_key  -> [remediation.X] signer_key\n  \
         networks.X.chain_id    -> not needed for well-known networks; otherwise \
         [sources.rpc.X] chain_id\n  \
         networks.X.rpc_url     -> [sources.rpc.X] url"
    )]
    NetworksRemoved { layer: String },

    #[error(
        "network {network:?} has no GraphToken entry in [addresses.{network}], which \
         GRT balance and allowance checks require{}",
        if *no_defaults {
            " (note: --no-defaults / use_defaults = false is in effect, so the shipped \
              addresses are not loaded)"
        } else {
            ""
        }
    )]
    MissingGraphToken { network: String, no_defaults: bool },

    #[error("{field}: {value:?} is not a valid duration (try \"60s\", \"5m\", \"24h\")")]
    InvalidDuration { field: String, value: String },

    #[error(
        "a signer_key was found in the shipped default config. Remediation keys must come \
         from your own config file, never from the layer embedded in the binary."
    )]
    SignerKeyInDefaults,

    #[error("{field}: {value:?} is invalid — {reason}")]
    InvalidValue {
        field: String,
        value: String,
        reason: String,
    },

    #[error("{field}: {value:?} is not a watchable asset (expected \"eth\" or \"grt\")")]
    UnknownWatchAsset { field: String, value: String },

    #[error("{field}: {value:?} is not a valid address")]
    InvalidAddress { field: String, value: String },

    #[error("{field}: {value:?} is not a valid decimal amount")]
    InvalidAmount { field: String, value: String },

    #[error(
        "network {network:?}: {name:?} is not in the address book, is not an account label, \
         and is not an address.\n  address book: {}\n  account labels: {}",
        join_or_none(addresses),
        join_or_none(labels)
    )]
    UnresolvableName {
        network: String,
        name: String,
        addresses: Vec<String>,
        labels: Vec<String>,
    },

    #[error(
        "{field}: {value:?} is not in the address book for network {network:?} and is not an \
         address.\n  address book: {}",
        join_or_none(addresses)
    )]
    UnresolvableAddress {
        network: String,
        field: String,
        value: String,
        addresses: Vec<String>,
    },

    #[error(
        "network {network:?}: {name:?} is ambiguous — it is both an address-book entry ({book}) \
         and an account label ({label}). Rename one; the same name must not mean two addresses."
    )]
    AmbiguousName {
        network: String,
        name: String,
        book: Address,
        label: Address,
    },

    #[error(
        "network {network:?} has two accounts or two subgraphs labelled {label:?} in the same \
         config layer; labels must be unique within a network"
    )]
    DuplicateLabel { network: String, label: String },

    #[error(
        "nothing to watch: no network has accounts and no subgraphs are configured.\n\
         Config loaded from: {}\n\n\
         Add accounts to your config:\n\n\
         [[accounts.arbitrum-one]]\n\
         address = \"MyWallet\"   # a name from [addresses.arbitrum-one], or 0x…\n\
         watch   = [\"eth\"]",
        join_or_none(sources)
    )]
    NothingToWatch { sources: Vec<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("invalid rpc url {url:?}: {reason}")]
    InvalidUrl { url: String, reason: String },

    #[error("rpc request failed: {0}")]
    Transport(String),

    // Chain-id comparison lives in `source::RpcSource::verify`, not here: the client reports
    // what it sees and stage 0 decides whether that is acceptable.
    #[error(
        "block {number} is not available from this endpoint (it may be pruned or not yet \
         produced); balances were not read"
    )]
    BlockUnavailable { number: u64 },

    #[error("the endpoint returned no latest block")]
    NoLatestBlock,
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}
