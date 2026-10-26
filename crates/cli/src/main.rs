//! `graph-network` — a CLI for gaining insight into Graph Protocol state.

mod cli;
mod metrics;
mod render;
mod style;
mod table;

use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::Parser;
use graph_network_core::config::{Config, LoadOptions, Resolved};
use graph_network_core::engine::{CycleReport, Engine};
use graph_network_core::remediate::Executor;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, ConfigCommand, Filter, RemediationArgs};

/// Exit codes. 2 is usage or config; 3 is the endpoint's fault. Nothing here exits non-zero
/// for a balance being low — the tool holds no opinion about that, so there is no failure to
/// report. Alerting is Grafana's.
const EXIT_USAGE: u8 = 2;
const EXIT_SOURCE: u8 = 3;

fn main() -> ExitCode {
    let args = Cli::parse();
    style::init(args.global.color);
    init_tracing(args.global.verbose);

    match run(args) {
        Ok(code) => code,
        Err(err) => {
            // Errors go to stderr so stdout stays pipeable even on failure.
            eprintln!("error: {err:#}");
            ExitCode::from(exit_code_for(&err))
        }
    }
}

/// Logs go to stderr, never stdout. `RUST_LOG` wins if set.
fn init_tracing(verbosity: u8) {
    let default = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!(
            "graph_network={default},graph_network_core={default}"
        ))
    });

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .init();
}

/// A core error carries its own exit code; anything else is a usage problem.
fn exit_code_for(err: &anyhow::Error) -> u8 {
    err.downcast_ref::<graph_network_core::Error>()
        .map(|e| e.exit_code() as u8)
        .unwrap_or(EXIT_USAGE)
}

#[tokio::main]
async fn run(args: Cli) -> Result<ExitCode> {
    let global = args.global.clone();

    let config = Config::load(&LoadOptions {
        explicit_path: global.config.clone(),
        no_defaults: global.no_defaults,
    })?;

    let mut resolved = config.resolve(global.network.as_deref(), global.rpc_url.as_deref())?;

    // Narrow the account set before building watches: there is no reason to read state for
    // rows the user filtered out.
    let (flat, unreadable_only, kinds, spenders) = match &args.command {
        Command::Status {
            flat,
            unreadable_only,
            filter,
        } => {
            resolved = filter_accounts(resolved, filter)?;
            (
                *flat,
                *unreadable_only,
                filter.kinds.clone(),
                filter.spenders.clone(),
            )
        }
        Command::Config(cmd) => {
            print_config(&resolved, cmd, global.json)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Serve {
            interval,
            metrics_port,
            filter,
        } => {
            resolved = filter_accounts(resolved, filter)?;
            return serve(
                resolved,
                *interval,
                *metrics_port,
                global.remediation.clone(),
            )
            .await;
        }
    };

    if !spenders.is_empty() {
        resolved = filter_spenders(resolved, &spenders)?;
    }

    let executor = build_executor(&engine_config(&resolved), &global.remediation)?;
    let engine = Engine::new(resolved)?;
    let report = engine.run_cycle(global.block).await;

    // Repairs run after every reading exists, serialised by the executor. See
    // `remediate::Executor::run`.
    let repairs = match &executor {
        Some(exec) => exec.run(&report.readings).await,
        None => Vec::new(),
    };

    // A kind filter applies after the cycle: kinds are a property of the built watches, not of
    // config, and skipping the reads is not worth a second filter path.
    let report = filter_kinds(report, &kinds)?;

    if global.json {
        println!("{}", serde_json::to_string_pretty(&render::json(&report))?);
    } else if flat {
        print!("{}", render::flat(&report, unreadable_only));
    } else {
        print!("{}", render::grouped(&report));
    }

    // Caveats go to stderr: they qualify the readout without polluting it.
    if !report.source.is_up() {
        eprintln!(
            "note: source {} is unavailable ({}); every watch reports unreadable",
            report.source.id,
            report.source.summary()
        );
    } else if report.unreadable().count() > 0 {
        eprintln!(
            "note: {} watch(es) could not be read at block {}",
            report.unreadable().count(),
            report.block.map(|b| b.number).unwrap_or_default()
        );
    }

    for repair in &repairs {
        eprintln!(
            "remediation: {} — {}",
            repair.account,
            repair.outcome.describe()
        );
    }

    // A dead source is an endpoint failure, not a clean run that happened to learn nothing.
    // The report is still printed — it says which network and why — but the exit code has to
    // reflect that no check was actually evaluated, or cron reads silence as health.
    if !report.source.is_up() {
        return Ok(ExitCode::from(EXIT_SOURCE));
    }

    Ok(ExitCode::SUCCESS)
}

/// Keep only the requested accounts, matching on label, address, or group.
///
/// A filter that matches nothing is an error: silently printing an empty table would let a
/// typo'd label read as "this account is fine".
fn filter_accounts(mut resolved: Resolved, filter: &Filter) -> Result<Resolved> {
    if filter.accounts.is_empty() && filter.groups.is_empty() {
        return Ok(resolved);
    }

    let in_group = |account: &graph_network_core::config::ResolvedAccount, want: &str| {
        account
            .group
            .as_deref()
            .is_some_and(|g| g.eq_ignore_ascii_case(want))
    };

    let mut unmatched = Vec::new();
    for want in &filter.accounts {
        if !resolved.accounts.iter().any(|a| matches_account(a, want)) {
            unmatched.push(format!("account {want:?}"));
        }
    }
    for want in &filter.groups {
        if !resolved.accounts.iter().any(|a| in_group(a, want)) {
            unmatched.push(format!("group {want:?}"));
        }
    }

    if !unmatched.is_empty() {
        let available: Vec<String> = resolved
            .accounts
            .iter()
            .map(|a| {
                format!(
                    "{} ({}){}",
                    a.label,
                    a.address,
                    a.group
                        .as_deref()
                        .map(|g| format!(" [{g}]"))
                        .unwrap_or_default()
                )
            })
            .collect();
        bail!(
            "nothing matches {} on network {:?}\nconfigured accounts:\n  {}",
            unmatched.join(", "),
            resolved.network,
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join("\n  ")
            }
        );
    }

    resolved.accounts.retain(|a| {
        filter.accounts.iter().any(|w| matches_account(a, w))
            || filter.groups.iter().any(|w| in_group(a, w))
    });
    Ok(resolved)
}

fn matches_account(account: &graph_network_core::config::ResolvedAccount, want: &str) -> bool {
    if account.label.eq_ignore_ascii_case(want) {
        return true;
    }
    // Compare parsed addresses so checksum casing never decides a match.
    want.parse::<alloy::primitives::Address>()
        .map(|addr| addr == account.address)
        .unwrap_or(false)
}

/// Keep only allowances to the requested spenders, dropping accounts left with none.
fn filter_spenders(mut resolved: Resolved, wanted: &[String]) -> Result<Resolved> {
    if wanted.is_empty() {
        return Ok(resolved);
    }

    let matches = |spec: &graph_network_core::config::Spender, want: &str| -> bool {
        spec.display.eq_ignore_ascii_case(want)
            || want
                .parse::<alloy::primitives::Address>()
                .map(|addr| addr == spec.address)
                .unwrap_or(false)
    };

    for account in &mut resolved.accounts {
        account
            .allowances
            .retain(|s| wanted.iter().any(|w| matches(s, w)));
    }

    if resolved.accounts.iter().all(|a| a.allowances.is_empty()) {
        bail!(
            "no configured allowance matches {} on network {:?}",
            wanted
                .iter()
                .map(|w| format!("{w:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            resolved.network
        );
    }

    resolved.accounts.retain(|a| !a.allowances.is_empty());
    Ok(resolved)
}

fn filter_kinds(mut report: CycleReport, kinds: &[String]) -> Result<CycleReport> {
    if kinds.is_empty() {
        return Ok(report);
    }
    let mut available: Vec<String> = report
        .readings
        .iter()
        .map(|o| o.id.kind.to_string())
        .collect();
    report
        .readings
        .retain(|o| kinds.iter().any(|k| k.eq_ignore_ascii_case(o.id.kind)));

    if report.readings.is_empty() {
        available.sort();
        available.dedup();
        bail!(
            "no checks of kind {} are configured; available kinds: {}",
            kinds
                .iter()
                .map(|k| format!("{k:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        );
    }
    Ok(report)
}

/// Config the executor needs, lifted before `Engine` takes ownership of the resolved config.
struct EngineConfig {
    network: String,
    rpc_url: String,
    signer_key: Option<String>,
    token: Option<alloy::primitives::Address>,
    armed: Vec<(String, String)>,
}

fn engine_config(resolved: &Resolved) -> EngineConfig {
    EngineConfig {
        network: resolved.network.clone(),
        rpc_url: resolved.rpc_url.clone(),
        signer_key: resolved.signer_key.clone(),
        token: resolved.address_of("GraphToken"),
        armed: resolved
            .accounts
            .iter()
            .flat_map(|a| {
                [
                    a.eth_remediate
                        .as_ref()
                        .map(|_| (a.label.clone(), "eth".to_string())),
                    a.grt_remediate
                        .as_ref()
                        .map(|_| (a.label.clone(), "grt".to_string())),
                ]
            })
            .flatten()
            .collect(),
    }
}

/// Build the executor, or explain why there isn't one.
///
/// Two independent gates: the flag and a configured key. Neither alone arms it — a key sitting
/// in config does nothing until someone passes `--remediate`, and the flag alone fails loudly
/// rather than quietly doing nothing.
fn build_executor(config: &EngineConfig, args: &RemediationArgs) -> Result<Option<Executor>> {
    if !args.remediate {
        return Ok(None);
    }
    let Some(key) = &config.signer_key else {
        bail!(
            "--remediate was passed but network {:?} has no signer_key configured.\n\
             Add it to your own config (never the shipped defaults):\n\n  \
             [networks.{}]\n  signer_key = \"${{GRAPH_NETWORK_SIGNER_KEY}}\"",
            config.network,
            config.network
        );
    };
    let token = config.token.unwrap_or(alloy::primitives::Address::ZERO);
    let executor = Executor::new(&config.rpc_url, key, token, args.dry_run)?;
    if args.dry_run {
        tracing::warn!(signer = %executor.signer_address(), "remediation armed in dry-run mode");
    } else {
        tracing::warn!(
            signer = %executor.signer_address(),
            "remediation ARMED - transactions will be sent"
        );
    }
    Ok(Some(executor))
}

/// Run cycles on an interval, exporting metrics.
async fn serve(
    resolved: Resolved,
    interval_secs: u64,
    metrics_port: u16,
    remediation: RemediationArgs,
) -> Result<ExitCode> {
    use tokio::time::{interval, Duration, MissedTickBehavior};

    let config = engine_config(&resolved);
    let executor = build_executor(&config, &remediation)?;
    let engine = Engine::new(resolved)?;

    let m = metrics::metrics();
    m.prime(&config.network, &config.armed);

    tokio::spawn(async move {
        if let Err(e) = metrics::serve(metrics_port).await {
            tracing::error!(error = %e, "metrics server stopped");
        }
    });

    let mut ticker = interval(Duration::from_secs(interval_secs.max(1)));
    // A cycle that overruns its interval skips the missed tick rather than queueing a backlog
    // it can never work off.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tracing::info!(
        interval_secs,
        metrics_port,
        checks = engine.watch_count(),
        "serving"
    );

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                return Ok(ExitCode::SUCCESS);
            }
        }

        let report = engine
            .run_cycle(graph_network_core::rpc::BlockSpec::Latest)
            .await;
        m.publish(&report);

        if let Some(exec) = &executor {
            let repairs = exec.run(&report.readings).await;
            m.publish_remediations(&config.network, &repairs);
        }

        let (read, unreadable) = report.counts();
        if report.source.is_up() {
            // Only a cycle that actually read something counts as success. Otherwise the
            // deadman is satisfied by a process failing briskly every two minutes.
            m.cycle_last_success.set(now_unix());
            tracing::info!(
                block = report.block.map(|b| b.number).unwrap_or_default(),
                read,
                unreadable,
                "cycle complete"
            );
        } else {
            tracing::warn!(source = %report.source.summary(), "cycle read nothing");
        }
    }
}

fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `config show` / `config addresses` — what the layering actually resolved to.
///
/// Config comes from up to three files plus environment expansion, and until now there was no
/// way to see the result. When an override does not take effect, this is the thing to look at.
fn print_config(resolved: &Resolved, cmd: &ConfigCommand, json: bool) -> Result<()> {
    match cmd {
        ConfigCommand::Addresses => {
            if json {
                let map: serde_json::Map<String, serde_json::Value> = resolved
                    .addresses
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::json!(v.to_string())))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                let mut table = table::Table::new(&[
                    ("Name", table::Align::Left),
                    ("Address", table::Align::Left),
                ]);
                for (name, address) in &resolved.addresses {
                    table.push(vec![name.clone(), address.to_string()]);
                }
                print!("{}", table.render());
            }
        }
        ConfigCommand::Show => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "network": resolved.network,
                        "chain_id": resolved.chain_id,
                        "rpc_url": resolved.rpc_url,
                        "max_lag_seconds": resolved.max_lag.as_secs(),
                        // Never the value. See `Resolved`'s Debug impl.
                        "signer_key": resolved.signer_key.as_ref().map(|_| "<redacted>"),
                        "layers": resolved.sources,
                        "used_defaults": resolved.used_defaults,
                        "addresses": resolved.addresses.len(),
                        "accounts": resolved.accounts.iter().map(|a| serde_json::json!({
                            "label": a.label,
                            "address": a.address.to_string(),
                            "group": a.group,
                            "source": a.source.as_str(),
                            "watch_eth": a.watch_eth,
                            "watch_grt": a.watch_grt,
                            "watch_activity": a.watch_activity,
                            "allowances": a.allowances.iter().map(|s| s.display.clone()).collect::<Vec<_>>(),
                            "remediates": a.eth_remediate.is_some() || a.grt_remediate.is_some(),
                        })).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                println!("Network:   {} ({})", resolved.network, resolved.chain_id);
                println!("RPC:       {}", resolved.rpc_url);
                println!("Max lag:   {}s", resolved.max_lag.as_secs());
                println!(
                    "Signer:    {}",
                    match resolved.signer_key {
                        Some(_) => "<redacted>",
                        None => "(none)",
                    }
                );
                println!("Layers:    {}", resolved.sources.join(", "));
                println!("Addresses: {}", resolved.addresses.len());
                println!();

                let mut table = table::Table::new(&[
                    ("Account", table::Align::Left),
                    ("Group", table::Align::Left),
                    ("Watches", table::Align::Left),
                    ("Remediates", table::Align::Left),
                    ("From", table::Align::Left),
                ]);
                for a in &resolved.accounts {
                    let mut watches = Vec::new();
                    if a.watch_eth {
                        watches.push("eth".to_string());
                    }
                    if a.watch_grt {
                        watches.push("grt".to_string());
                    }
                    if a.watch_activity {
                        watches.push("activity".to_string());
                    }
                    for s in &a.allowances {
                        watches.push(format!("allowance→{}", s.display));
                    }
                    let mut repairs = Vec::new();
                    if a.eth_remediate.is_some() {
                        repairs.push("eth");
                    }
                    if a.grt_remediate.is_some() {
                        repairs.push("grt");
                    }
                    table.push(vec![
                        a.label.clone(),
                        a.group.clone().unwrap_or_default(),
                        watches.join(", "),
                        repairs.join(", "),
                        a.source.as_str().to_string(),
                    ]);
                }
                print!("{}", table.render());
            }
        }
    }
    Ok(())
}
