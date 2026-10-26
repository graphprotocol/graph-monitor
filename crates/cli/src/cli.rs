//! Command tree and argument parsing.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};
use graph_monitor_core::rpc::BlockSpec;

#[derive(Debug, Parser)]
#[command(
    name = "graph-monitor",
    version,
    about = "Insight into Graph Protocol state",
    long_about = "A CLI for gaining insight into Graph Protocol state.\n\n\
                  Every readout is taken from a single block height, which is printed with the \
                  results and can be pinned with --block."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[command(flatten)]
    pub global: GlobalArgs,
}

#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Network to query. Defaults to `default_network` from config.
    #[arg(long, global = true, env = "GRAPH_MONITOR_NETWORK")]
    pub network: Option<String>,

    /// Override the RPC endpoint without touching config.
    #[arg(long, global = true, env = "GRAPH_MONITOR_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Block to read at: a number, 0x-prefixed hex, or `latest`.
    #[arg(long, global = true, default_value = "latest")]
    pub block: BlockSpec,

    /// Machine-readable output on stdout.
    #[arg(long, global = true)]
    pub json: bool,

    /// When to colour output. `auto` means only when stdout is a terminal.
    #[arg(long, global = true, value_enum, default_value_t = crate::style::ColorChoice::Auto)]
    pub color: crate::style::ColorChoice,

    /// Explicit config file. Skips the XDG and working-directory files.
    #[arg(long, global = true, env = "GRAPH_MONITOR_CONFIG")]
    pub config: Option<PathBuf>,

    /// Ignore the shipped config layer for this run.
    #[arg(long, global = true)]
    pub no_defaults: bool,

    #[command(flatten)]
    pub remediation: RemediationArgs,

    /// Increase log verbosity (stderr). Repeat for more.
    #[arg(short = 'v', long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
}

/// Remediation is a modifier on a cycle, not a separate operation: the same read and report
/// happens either way, with repair as an extra stage.
#[derive(Debug, Clone, Args)]
pub struct RemediationArgs {
    /// Perform configured repairs. Off by default — without it nothing is ever signed.
    #[arg(long, global = true)]
    pub remediate: bool,

    /// Form the repair and log it without broadcasting anything.
    #[arg(long, global = true)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Read every watch and show what it found
    Status {
        /// One line per watch instead of grouped tables.
        #[arg(long)]
        flat: bool,

        /// Only show watches that could not be read.
        #[arg(long)]
        unreadable_only: bool,

        #[command(flatten)]
        filter: Filter,
    },

    /// Inspect the effective configuration
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Run continuously, exporting Prometheus metrics
    Serve {
        /// Seconds between cycles.
        #[arg(long, default_value = "120")]
        interval: u64,

        /// Port for the metrics server.
        #[arg(long, default_value = "9090", env = "GRAPH_MONITOR_METRICS_PORT")]
        metrics_port: u16,

        #[command(flatten)]
        filter: Filter,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// What config resolved to after layering, env expansion and name resolution
    Show,
    /// The address book for the active network
    Addresses,
}

/// Shared by every command that runs a cycle.
#[derive(Debug, Clone, Args)]
pub struct Filter {
    /// Only these accounts, by label or address. Repeatable.
    #[arg(long = "account", value_name = "LABEL|ADDR")]
    pub accounts: Vec<String>,

    /// Only accounts in these groups. Repeatable.
    #[arg(long = "group", value_name = "GROUP")]
    pub groups: Vec<String>,

    /// Only watches of these kinds, e.g. `eth_balance`. Repeatable.
    #[arg(long = "kind", value_name = "KIND")]
    pub kinds: Vec<String>,

    /// Only allowances to these spenders. Repeatable.
    #[arg(long = "spender", value_name = "NAME|ADDR")]
    pub spenders: Vec<String>,
}
