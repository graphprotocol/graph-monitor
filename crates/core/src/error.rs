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
        "no network selected: pass --network, or set default_network in config{}",
        if *no_defaults {
            " (note: --no-defaults / use_defaults = false is in effect, so the shipped \
              networks and default_network are not loaded)"
        } else {
            ""
        }
    )]
    NoNetworkSelected { no_defaults: bool },

    #[error(
        "unknown network {name:?}; configured networks: {}",
        join_or_none(available)
    )]
    UnknownNetwork {
        name: String,
        available: Vec<String>,
    },

    #[error("network {network:?} has no chain_id configured")]
    MissingChainId { network: String },

    #[error("network {network:?} has no rpc_url configured; set it in config or pass --rpc-url")]
    MissingRpcUrl { network: String },

    #[error(
        "network {network:?} has no GraphToken entry in [networks.{network}.addresses], which \
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
        "network {network:?} has two accounts labelled {label:?} in the same config layer; \
         labels must be unique within a network"
    )]
    DuplicateLabel { network: String, label: String },

    #[error(
        "no accounts configured for network {network:?}.\n\
         Config loaded from: {}\n\n\
         Add accounts to your config:\n\n\
         [[accounts.{network}]]\n\
         address = \"MyWallet\"        # a name from [networks.{network}.addresses], or 0x…\n\
         group   = \"billing\"         # optional label\n\
         eth.min = [[\"0.05\", \"P2\"]]  # warn below 0.05 ETH",
        join_or_none(sources)
    )]
    NoAccounts {
        network: String,
        sources: Vec<String>,
    },
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
