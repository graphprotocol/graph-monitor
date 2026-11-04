//! `graph-monitor` — a CLI for gaining insight into Graph Protocol state.

mod cli;
mod metrics;
mod render;
mod style;
mod table;

use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::Parser;
use graph_monitor_core::config::{Config, LoadOptions, Resolved};
use graph_monitor_core::engine::{CycleReport, Engine};
use graph_monitor_core::remediate::Executor;
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
            "graph_monitor={default},graph_monitor_core={default}"
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
    err.downcast_ref::<graph_monitor_core::Error>()
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

    // Every configured network runs; --network narrows it to one.
    let mut resolved = config.resolve(global.network.as_deref(), global.rpc_url.as_deref())?;

    // A block number belongs to one chain. Applying 519,000,000 to Gnosis is not a question
    // with an answer.
    if global.block != graph_monitor_core::rpc::BlockSpec::Latest && resolved.networks.len() != 1 {
        bail!(
            "--block picks a height on one chain, but {} networks would run ({}); pass \
             --network to pick one",
            resolved.networks.len(),
            resolved
                .networks
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

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

    let repair_configs = repair_configs(&resolved);
    let executors = build_executors(&repair_configs, &global.remediation)?;
    let engine = Engine::new(resolved)?;
    let report = engine.run_cycle(global.block).await;

    // Repairs run after every reading exists, serialised by each network's executor. See
    // `remediate::Executor::run`.
    let mut repairs = Vec::new();
    for (network, exec) in &executors {
        repairs.extend(exec.run(&readings_on(&report, network)).await);
    }

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
    for net in &report.networks {
        if net.source.is_up() {
            continue;
        }
        if net.watches > 0 {
            eprintln!(
                "note: source {} is unavailable ({}); every {} watch reports unreadable",
                net.source.id,
                net.source.summary(),
                net.network
            );
        } else {
            eprintln!(
                "note: source {} is unavailable ({}); {} subgraphs have no block gap this run",
                net.source.id,
                net.source.summary(),
                net.network
            );
        }
    }
    for src in report.subgraph_sources.iter().filter(|s| !s.is_up()) {
        eprintln!("note: source {} is unavailable ({})", src.id, src.summary());
    }
    // Watches on a dead source are already explained above; list only the ones that failed
    // on their own.
    let unreadable = report
        .unreadable()
        .filter(|r| {
            report
                .network(&r.labels.network)
                .is_some_and(|n| n.source.is_up())
        })
        .count();
    if unreadable > 0 {
        eprintln!("note: {unreadable} watch(es) could not be read:");
        for line in render::unreadable_notes(&report) {
            eprintln!("  {line}");
        }
    }
    for sg in report.subgraphs_down() {
        eprintln!("note: subgraph {} is down: {}", sg.name(), sg.summary());
    }
    for sg in report.subgraphs_not_checked() {
        eprintln!("note: subgraph {} was {}", sg.name(), sg.summary());
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
    // reflect that some checks were not evaluated, or cron reads silence as health.
    if report.blind_networks().next().is_some() {
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

    let in_group = |account: &graph_monitor_core::config::ResolvedAccount, want: &str| {
        account
            .group
            .as_deref()
            .is_some_and(|g| g.eq_ignore_ascii_case(want))
    };

    let mut unmatched = Vec::new();
    for want in &filter.accounts {
        if !resolved.accounts().any(|(_, a)| matches_account(a, want)) {
            unmatched.push(format!("account {want:?}"));
        }
    }
    for want in &filter.groups {
        if !resolved.accounts().any(|(_, a)| in_group(a, want)) {
            unmatched.push(format!("group {want:?}"));
        }
    }

    if !unmatched.is_empty() {
        let available: Vec<String> = resolved
            .accounts()
            .map(|(n, a)| {
                format!(
                    "{} ({}) on {}{}",
                    a.label,
                    a.address,
                    n.name,
                    a.group
                        .as_deref()
                        .map(|g| format!(" [{g}]"))
                        .unwrap_or_default()
                )
            })
            .collect();
        bail!(
            "nothing matches {}\nconfigured accounts:\n  {}",
            unmatched.join(", "),
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join("\n  ")
            }
        );
    }

    for net in &mut resolved.networks {
        net.accounts.retain(|a| {
            filter.accounts.iter().any(|w| matches_account(a, w))
                || filter.groups.iter().any(|w| in_group(a, w))
        });
    }
    drop_emptied_networks(&mut resolved);
    Ok(resolved)
}

/// After a filter, drop networks it left with no accounts. A network that never had any runs
/// for its subgraphs, and stays.
fn drop_emptied_networks(resolved: &mut Resolved) {
    let has_subgraphs = |name: &str| resolved.subgraphs.iter().any(|s| s.network == name);
    let keep: Vec<bool> = resolved
        .networks
        .iter()
        .map(|n| !n.accounts.is_empty() || has_subgraphs(&n.name))
        .collect();
    let mut keep = keep.into_iter();
    resolved.networks.retain(|_| keep.next().unwrap_or(true));
}

fn matches_account(account: &graph_monitor_core::config::ResolvedAccount, want: &str) -> bool {
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

    let matches = |spec: &graph_monitor_core::config::Spender, want: &str| -> bool {
        spec.display.eq_ignore_ascii_case(want)
            || want
                .parse::<alloy::primitives::Address>()
                .map(|addr| addr == spec.address)
                .unwrap_or(false)
    };

    for net in &mut resolved.networks {
        for account in &mut net.accounts {
            account
                .allowances
                .retain(|s| wanted.iter().any(|w| matches(s, w)));
        }
    }

    if resolved.accounts().all(|(_, a)| a.allowances.is_empty()) {
        bail!(
            "no configured allowance matches {}",
            wanted
                .iter()
                .map(|w| format!("{w:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    for net in &mut resolved.networks {
        net.accounts.retain(|a| !a.allowances.is_empty());
    }
    drop_emptied_networks(&mut resolved);
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

/// One network's readings, for that network's executor.
fn readings_on(report: &CycleReport, network: &str) -> Vec<graph_monitor_core::watch::Reading> {
    report
        .readings
        .iter()
        .filter(|r| r.labels.network == network)
        .cloned()
        .collect()
}

/// What one network's executor needs, lifted before `Engine` takes ownership of the config.
struct RepairConfig {
    network: String,
    rpc_url: String,
    signer_key: Option<String>,
    token: Option<alloy::primitives::Address>,
    armed: Vec<(String, String)>,
}

fn repair_configs(resolved: &Resolved) -> Vec<RepairConfig> {
    resolved
        .networks
        .iter()
        .map(|n| RepairConfig {
            network: n.name.clone(),
            rpc_url: n.rpc.url.clone(),
            signer_key: n.signer_key.clone(),
            token: n.address_of("GraphToken"),
            armed: n
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
        })
        .collect()
}

/// One executor per network that has repairs configured, or an explanation of why not.
///
/// Two independent gates: the flag and a configured key. Neither alone arms anything — a key
/// sitting in config does nothing until someone passes `--remediate`, and the flag fails
/// loudly rather than quietly doing nothing.
fn build_executors(
    configs: &[RepairConfig],
    args: &RemediationArgs,
) -> Result<Vec<(String, Executor)>> {
    if !args.remediate {
        return Ok(Vec::new());
    }
    let mut executors = Vec::new();
    for config in configs.iter().filter(|c| !c.armed.is_empty()) {
        let Some(key) = &config.signer_key else {
            bail!(
                "--remediate was passed and network {:?} has repairs configured, but no \
                 signer_key.\nAdd it to your own config (never the shipped defaults):\n\n  \
                 [remediation.{}]\n  signer_key = \"${{GRAPH_MONITOR_SIGNER_KEY}}\"",
                config.network,
                config.network
            );
        };
        let token = config.token.unwrap_or(alloy::primitives::Address::ZERO);
        let executor = Executor::new(&config.rpc_url, key, token, args.dry_run)?;
        if args.dry_run {
            tracing::warn!(network = %config.network, signer = %executor.signer_address(), "remediation armed in dry-run mode");
        } else {
            tracing::warn!(
                network = %config.network,
                signer = %executor.signer_address(),
                "remediation ARMED - transactions will be sent"
            );
        }
        executors.push((config.network.clone(), executor));
    }
    if executors.is_empty() {
        bail!(
            "--remediate was passed but no account has a remediate block, so there is nothing \
             to repair. See the Remediation section of the README."
        );
    }
    Ok(executors)
}

/// Run cycles on an interval, exporting metrics.
async fn serve(
    resolved: Resolved,
    interval_secs: u64,
    metrics_port: u16,
    remediation: RemediationArgs,
) -> Result<ExitCode> {
    use tokio::time::{interval, Duration, MissedTickBehavior};

    let configs = repair_configs(&resolved);
    let executors = build_executors(&configs, &remediation)?;
    let engine = Engine::new(resolved)?;

    let m = metrics::metrics();
    for config in &configs {
        m.prime(&config.network, &config.armed);
    }

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
        networks = engine.network_count(),
        checks = engine.watch_count(),
        subgraphs = engine.subgraph_count(),
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
            .run_cycle(graph_monitor_core::rpc::BlockSpec::Latest)
            .await;
        m.publish(&report);

        for (network, exec) in &executors {
            let repairs = exec.run(&readings_on(&report, network)).await;
            m.publish_remediations(network, &repairs);
        }

        let (read, unreadable) = report.counts();
        for net in report.blind_networks() {
            tracing::warn!(source = %net.source.id, reason = %net.source.summary(), "network read nothing");
        }
        if report.read_anything() {
            // Only a cycle that actually read something counts as success. Otherwise the
            // deadman is satisfied by a process failing briskly every two minutes.
            m.cycle_last_success.set(now_unix());
            tracing::info!(read, unreadable, "cycle complete");
        } else {
            tracing::warn!("cycle read nothing");
        }
    }
}

/// The host of a URL, without the path or query where API keys tend to live.
fn url_host(url: &str) -> String {
    url.parse::<alloy::transports::http::reqwest::Url>()
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "<unparseable>".to_string())
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
                let by_network: serde_json::Map<String, serde_json::Value> = resolved
                    .networks
                    .iter()
                    .map(|n| {
                        let book: serde_json::Map<String, serde_json::Value> = n
                            .addresses
                            .iter()
                            .map(|(k, v)| (k.clone(), serde_json::json!(v.to_string())))
                            .collect();
                        (n.name.clone(), serde_json::Value::Object(book))
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&by_network)?);
            } else {
                let mut table = table::Table::new(&[
                    ("Chain", table::Align::Left),
                    ("Name", table::Align::Left),
                    ("Address", table::Align::Left),
                ]);
                for n in &resolved.networks {
                    for (name, address) in &n.addresses {
                        table.push(vec![n.name.clone(), name.clone(), address.to_string()]);
                    }
                }
                print!("{}", table.render());
            }
        }
        ConfigCommand::Show => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "layers": resolved.sources,
                        "used_defaults": resolved.used_defaults,
                        // URLs are left out: RPC providers commonly put the API key in the path.
                        "sources": {
                            "rpc": resolved.networks.iter().map(|n| serde_json::json!({
                                "network": n.name,
                                "chain_id": n.chain_id,
                                "host": url_host(&n.rpc.url),
                                "max_lag_seconds": n.rpc.max_lag.as_secs(),
                                "max_concurrency": n.rpc.max_concurrency,
                                "max_log_range": n.rpc.max_log_range,
                            })).collect::<Vec<_>>(),
                            "subgraph": resolved.subgraph_sources.iter().map(|s| serde_json::json!({
                                "name": s.name,
                                "network": s.network,
                                "url": s.endpoint.as_ref().ok().map(|e| e.url.clone()),
                                "unusable": s.endpoint.as_ref().err(),
                                "deployment": s.deployment,
                                "max_lag_seconds": s.max_lag.as_secs(),
                            })).collect::<Vec<_>>(),
                        },
                        "networks": resolved.networks.iter().map(|n| serde_json::json!({
                            "network": n.name,
                            "chain_id": n.chain_id,
                            // Never the value. See `ResolvedNetwork`'s Debug impl.
                            "signer_key": n.signer_key.as_ref().map(|_| "<redacted>"),
                            "addresses": n.addresses.len(),
                        })).collect::<Vec<_>>(),
                        "accounts": resolved.accounts().map(|(n, a)| serde_json::json!({
                            "network": n.name,
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
                        "subgraphs": resolved.subgraphs.iter().map(|s| serde_json::json!({
                            "network": s.network,
                            "subgraph": s.subgraph,
                            "url": s.endpoint.as_ref().ok().map(|e| e.url.clone()),
                            "unusable": s.endpoint.as_ref().err(),
                            "deployment": s.deployment,
                            "max_lag_seconds": s.max_lag.as_secs(),
                            "source": s.source.as_str(),
                        })).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                println!("Layers:    {}", resolved.sources.join(", "));
                println!();

                // Host only: RPC providers commonly put the API key in the path.
                let mut table = table::Table::new(&[
                    ("RPC source", table::Align::Left),
                    ("Chain id", table::Align::Right),
                    ("Host", table::Align::Left),
                    ("Max lag", table::Align::Right),
                    ("Signer", table::Align::Left),
                    ("Addresses", table::Align::Right),
                ]);
                for n in &resolved.networks {
                    table.push(vec![
                        format!("rpc:{}", n.name),
                        n.chain_id.to_string(),
                        url_host(&n.rpc.url),
                        format!("{}s", n.rpc.max_lag.as_secs()),
                        match n.signer_key {
                            Some(_) => "<redacted>",
                            None => "",
                        }
                        .to_string(),
                        n.addresses.len().to_string(),
                    ]);
                }
                print!("{}", table.render());
                println!();

                if !resolved.subgraph_sources.is_empty() {
                    let mut table = table::Table::new(&[
                        ("Subgraph source", table::Align::Left),
                        ("Chain", table::Align::Left),
                        ("Endpoint", table::Align::Left),
                        ("Max lag", table::Align::Right),
                    ]);
                    for s in &resolved.subgraph_sources {
                        table.push(vec![
                            format!("subgraph:{}", s.name),
                            s.network.clone(),
                            match &s.endpoint {
                                // The key travels in a header, so the URL is safe to print.
                                Ok(e) => e.url.clone(),
                                Err(reason) => format!("unusable: {reason}"),
                            },
                            format!("{}s", s.max_lag.as_secs()),
                        ]);
                    }
                    print!("{}", table.render());
                    println!();
                }

                let mut table = table::Table::new(&[
                    ("Account", table::Align::Left),
                    ("Chain", table::Align::Left),
                    ("Group", table::Align::Left),
                    ("Watches", table::Align::Left),
                    ("Remediates", table::Align::Left),
                    ("From", table::Align::Left),
                ]);
                for (n, a) in resolved.accounts() {
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
                        n.name.clone(),
                        a.group.clone().unwrap_or_default(),
                        watches.join(", "),
                        repairs.join(", "),
                        a.source.as_str().to_string(),
                    ]);
                }
                print!("{}", table.render());

                if !resolved.subgraphs.is_empty() {
                    println!();
                    let mut table = table::Table::new(&[
                        ("Subgraph", table::Align::Left),
                        ("Chain", table::Align::Left),
                        ("Endpoint", table::Align::Left),
                        ("Max lag", table::Align::Right),
                        ("From", table::Align::Left),
                    ]);
                    for s in &resolved.subgraphs {
                        table.push(vec![
                            s.name(),
                            s.network.clone(),
                            match &s.endpoint {
                                // The key travels in a header, so the URL is safe to print.
                                Ok(e) => e.url.clone(),
                                Err(reason) => format!("unusable: {reason}"),
                            },
                            format!("{}s", s.max_lag.as_secs()),
                            s.source.as_str().to_string(),
                        ]);
                    }
                    print!("{}", table.render());
                }
            }
        }
    }
    Ok(())
}
