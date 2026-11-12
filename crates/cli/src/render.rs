//! Reporting: the console and JSON views of a cycle.
//!
//! Stdout carries data only. Notes about degraded reads go to stderr, so piping into `jq`
//! never breaks.

use std::collections::BTreeMap;

use alloy::primitives::Address;
use graph_monitor_core::check::{Measurement, Reading, Subject};
use graph_monitor_core::checks::{
    humanize, M_ALLOWANCE, M_ALLOWANCE_UNLIMITED, M_ETH_BALANCE, M_GRT_BALANCE, M_IDLE_SECONDS,
};
use graph_monitor_core::engine::CycleReport;
use graph_monitor_core::solo::{M_SOLO_DEPLOYMENTS, M_SOLO_SIGNAL};
use graph_monitor_core::subgraph::{
    F_SERVED_DEPLOYMENT, M_SUBGRAPH_BLOCKS_BEHIND, M_SUBGRAPH_BLOCK_HEIGHT, M_SUBGRAPH_LAG,
    M_SUBGRAPH_UP,
};
use serde_json::{json, Value};

use crate::style;
use crate::table::{group_thousands, Align, Table};

/// Fractional digits per asset. ETH at 2 means gas balances under 0.01 render as `0.00`; the
/// threshold judgment compares base units, not this string, so the warning still fires.
const ETH_PLACES: usize = 2;
const GRT_PLACES: usize = 2;

/// `!` marks a value that could not be read. A zero is a real zero and renders as a number;
/// conflating the two would let a failing endpoint look like a healthy, empty wallet.
const UNREADABLE: &str = "!";
/// A dash means "not configured": no check asked about it.
const UNCONFIGURED: &str = "–";

/// The block-provenance header, one line per network. Every human readout opens with this,
/// so a pasted screenshot says which networks and heights it came from.
pub fn header(report: &CycleReport) -> String {
    let lines: Vec<String> = report
        .networks
        .iter()
        .map(|net| match net.block {
            Some(block) => format!(
                "{}  {}",
                style::bold(&net.network),
                style::dim(&format!(
                    "chain {} · block {} · {}",
                    net.chain_id,
                    group_thousands(block.number),
                    block.timestamp_utc()
                ))
            ),
            // Even with nothing read, the readout still says where it was looking.
            None => format!(
                "{}  {}",
                style::bold(&net.network),
                style::alarm(&format!(
                    "chain {} · no block read · source {}",
                    net.chain_id,
                    net.source.summary()
                ))
            ),
        })
        .collect();
    lines.join("\n")
}

/// Full address, dimmed.
///
/// Not truncated: an abbreviated address cannot be copied into a block explorer or grepped
/// against a config, which is most of what anyone wants one for. Dimming keeps it out of the
/// way without making it useless.
fn render_address(addr: &Address) -> String {
    style::dim(&addr.to_string())
}

fn measurement_value(reading: &Reading, name: &str) -> Option<f64> {
    reading.value(name)
}

/// One row of the account table, assembled from the checks that touched that account.
#[derive(Default)]
struct AccountRow {
    group: Option<String>,
    address: Option<Address>,
    eth: Option<Cell>,
    grt: Option<Cell>,
    activity: Option<Activity>,
    /// What the tool would repair this cycle. Empty is the normal case.
    remediation: Vec<String>,
}

enum Cell {
    Value(f64),
    Unreadable,
}

/// An activity check has a third outcome balances do not: it read successfully and found
/// nothing within the lookback. That is not a failed read, and must not render as one.
enum Activity {
    Since(u64),
    NoneWithinLookback,
    Unreadable,
}

impl Activity {
    fn render(&self) -> String {
        match self {
            Activity::Since(seconds) => humanize(*seconds),
            Activity::NoneWithinLookback => style::dim("none"),
            Activity::Unreadable => style::alarm(UNREADABLE),
        }
    }
}

impl Cell {
    fn render(&self, places: usize) -> String {
        match self {
            // A real zero is data, but a quiet kind: dimming it lets the eye skip to the
            // values that carry information.
            Cell::Value(v) if *v == 0.0 => style::dim(&format_float(*v, places)),
            Cell::Value(v) => format_float(*v, places),
            Cell::Unreadable => style::alarm(UNREADABLE),
        }
    }
}

/// Thousands-separated, truncated rather than rounded — rounding 0.9999 up to 1.00 in a
/// low-balance readout rounds in the direction that hides the problem.
fn format_float(v: f64, places: usize) -> String {
    let factor = 10f64.powi(places as i32);
    let truncated = (v * factor).floor() / factor;
    let s = format!("{truncated:.places$}");
    match s.split_once('.') {
        Some((int, frac)) => format!("{}.{}", group_thousands_str(int), frac),
        None => group_thousands_str(&s),
    }
}

fn group_thousands_str(digits: &str) -> String {
    let bytes = digits.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// (label, network): the same label on two chains is two accounts.
type AccountKey = (String, String);

fn account_rows(report: &CycleReport) -> BTreeMap<AccountKey, AccountRow> {
    let mut rows: BTreeMap<AccountKey, AccountRow> = BTreeMap::new();

    for reading in &report.readings {
        let Subject::Account {
            group,
            account,
            address,
        } = &reading.subject
        else {
            continue;
        };
        let row = rows
            .entry((account.clone(), reading.network.clone()))
            .or_default();
        row.group = group.clone().or(row.group.take());
        row.address = Some(*address);
        if let Some(intent) = &reading.remediation {
            row.remediation.push(intent.describe());
        }

        let cell = |v: Option<f64>| match v {
            Some(v) => Cell::Value(v),
            // The check ran but produced no measurement: the read failed.
            None => Cell::Unreadable,
        };

        match reading.id.kind {
            "eth_balance" => row.eth = Some(cell(measurement_value(reading, M_ETH_BALANCE))),
            "grt_balance" => row.grt = Some(cell(measurement_value(reading, M_GRT_BALANCE))),
            "activity" => {
                row.activity = Some(if reading.is_unreadable() {
                    Activity::Unreadable
                } else {
                    match measurement_value(reading, M_IDLE_SECONDS) {
                        Some(seconds) => Activity::Since(seconds as u64),
                        // Read fine; the account simply has not acted within the lookback.
                        None => Activity::NoneWithinLookback,
                    }
                });
            }
            _ => {}
        }
    }

    rows
}

/// There is no status column any more — nothing here judges a balance. What is worth showing
/// is whether the tool intends to act on it.
fn remediation_note(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    style::action(&notes.join("; "))
}

fn balance_row((label, network): &AccountKey, row: &AccountRow) -> Vec<String> {
    vec![
        format!("  {label}"),
        style::dim(network),
        row.address.map(|a| render_address(&a)).unwrap_or_default(),
        row.eth
            .as_ref()
            .map(|c| c.render(ETH_PLACES))
            .unwrap_or_else(|| style::dim(UNCONFIGURED)),
        row.grt
            .as_ref()
            .map(|c| c.render(GRT_PLACES))
            .unwrap_or_else(|| style::dim(UNCONFIGURED)),
        row.activity
            .as_ref()
            .map(|a| a.render())
            .unwrap_or_else(|| style::dim(UNCONFIGURED)),
        remediation_note(&row.remediation),
    ]
}

/// The grouped readout, in three parts: what the tool read from (**Inputs**), what it found
/// (**Checks**, one subsection per kind of check), and a one-line summary.
pub fn grouped(report: &CycleReport) -> String {
    let mut inputs = vec![chains_table(report), sources_table(report)];
    inputs.retain(|t| !t.is_empty());

    let mut checks: Vec<(String, String)> = Vec::new();
    checks.extend(accounts_section(report));
    checks.extend(allowances_section(report));
    // Subgraphs are part of the network being checked, so they sit with the other findings.
    let subgraphs: Vec<&Reading> = report.of_kind("subgraph").collect();
    if !subgraphs.is_empty() {
        checks.push((
            subsection_title("Subgraphs", &subgraphs, "checked"),
            subgraph_table(&subgraphs),
        ));
    }
    let solo: Vec<&Reading> = report.of_kind(graph_monitor_core::solo::KIND).collect();
    if !solo.is_empty() {
        checks.push((solo_title(&solo), solo_table(&solo)));
    }

    // Banners span the widest thing under them, so the parts read as blocks, up to a
    // terminal's width: one long error message must not stretch every rule.
    let width = inputs
        .iter()
        .chain(checks.iter().map(|(_, body)| body))
        .flat_map(|t| t.lines())
        .chain(checks.iter().map(|(title, _)| title.as_str()))
        .map(|l| crate::table::display_width(l) + INDENT.len())
        .max()
        .unwrap_or(0)
        .clamp(60, 80);

    let mut out = String::new();
    out.push_str(&title_line(report));
    out.push_str("\n\n");

    // How the tool sees, before what it saw. Everything below is only as good as these.
    out.push_str(&banner("INPUTS", width));
    out.push_str(
        &inputs
            .iter()
            .map(|t| indent(t))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    out.push('\n');
    out.push_str(&banner("CHECKS", width));
    if checks.is_empty() {
        out.push_str(&indent("No checks configured.\n"));
    }
    let sections: Vec<String> = checks
        .iter()
        .map(|(title, body)| indent(&format!("{title}\n{body}")))
        .collect();
    out.push_str(&sections.join("\n"));

    out.push('\n');
    out.push_str(&style::dim(&"━".repeat(width)));
    out.push('\n');
    out.push_str(&summary_line(report));
    out.push('\n');
    out
}

const INDENT: &str = "  ";

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{INDENT}{l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// `INPUTS ━━━━━…`, full width.
fn banner(name: &str, width: usize) -> String {
    let rule = width.saturating_sub(name.len() + 1).max(3);
    format!("{} {}\n", style::bold(name), style::dim(&"━".repeat(rule)))
}

/// What ran and as of when: the latest block time across the chains read.
fn title_line(report: &CycleReport) -> String {
    let mut line = style::bold(&format!("graph-monitor {}", env!("CARGO_PKG_VERSION")));
    if let Some(block) = report
        .networks
        .iter()
        .filter_map(|n| n.block)
        .max_by_key(|b| b.timestamp)
    {
        line.push_str(&style::dim(&format!(" · as of {}", block.timestamp_utc())));
    }
    line
}

/// `Accounts · 11 accounts · 1 unreadable`: the name, then how it went, so a glance down the
/// left edge says which section needs reading.
fn subsection_title(name: &str, readings: &[&Reading], noun: &str) -> String {
    let mut parts = vec![style::dim(&format!("{} {noun}", readings.len()))];
    parts.extend(health(readings));
    format!(
        "{}  {}",
        style::bold(&format!("▸ {name}")),
        parts.join(&style::dim(" · "))
    )
}

/// Unreadable, faulty and pending-repair counts. Nothing when there are none: the tool grades
/// no values, so there is no "ok" to claim.
fn health(readings: &[&Reading]) -> Vec<String> {
    let unreadable = readings.iter().filter(|r| r.is_unreadable()).count();
    let faults = readings.iter().filter(|r| r.fault().is_some()).count();
    let repairs = readings.iter().filter(|r| r.remediation.is_some()).count();
    let mut parts = Vec::new();
    if faults > 0 {
        parts.push(style::alarm(&format!("{faults} fault(s)")));
    }
    if unreadable > 0 {
        parts.push(style::alarm(&format!("{unreadable} unreadable")));
    }
    if repairs > 0 {
        parts.push(style::action(&format!("{repairs} to remediate")));
    }
    parts
}

fn accounts_section(report: &CycleReport) -> Option<(String, String)> {
    let rows = account_rows(report);
    if rows.is_empty() {
        return None;
    }
    let readings: Vec<&Reading> = report
        .readings
        .iter()
        .filter(|r| matches!(r.subject, Subject::Account { .. }))
        .collect();

    let mut table = Table::new(&[
        ("Account", Align::Left),
        ("Chain", Align::Left),
        ("Address", Align::Left),
        ("ETH", Align::Right),
        ("GRT", Align::Right),
        ("Last activity", Align::Right),
        ("Remediation", Align::Left),
    ]);

    // Grouped, with ungrouped last. Groups are a display device here and nothing more.
    let mut by_group: BTreeMap<Option<String>, Vec<(&AccountKey, &AccountRow)>> = BTreeMap::new();
    for (label, row) in &rows {
        by_group
            .entry(row.group.clone())
            .or_default()
            .push((label, row));
    }
    let ungrouped = by_group.remove(&None);
    let grouped_count = by_group.len();

    for (group, entries) in &by_group {
        if let Some(name) = group {
            table.section(style::bold(name));
        }
        for (label, row) in entries {
            table.push(balance_row(label, row));
        }
    }
    if let Some(entries) = ungrouped {
        if grouped_count > 0 {
            table.section(style::bold("ungrouped"));
        }
        for (label, row) in entries {
            table.push(balance_row(label, row));
        }
    }

    let mut parts = vec![style::dim(&format!(
        "{} accounts, {} checks",
        rows.len(),
        readings.len()
    ))];
    parts.extend(health(&readings));
    let title = format!(
        "{}  {}",
        style::bold("▸ Accounts"),
        parts.join(&style::dim(" · "))
    );
    Some((title, table.render()))
}

fn allowances_section(report: &CycleReport) -> Option<(String, String)> {
    let readings: Vec<&Reading> = report
        .readings
        .iter()
        .filter(|r| matches!(r.subject, Subject::Allowance { .. }))
        .collect();
    if readings.is_empty() {
        return None;
    }

    let mut table = Table::new(&[
        ("Owner", Align::Left),
        ("Chain", Align::Left),
        ("Spender", Align::Left),
        ("Allowance", Align::Right),
        ("Unlimited", Align::Left),
    ]);
    for reading in &readings {
        let Subject::Allowance { owner, spender, .. } = &reading.subject else {
            continue;
        };
        let value = match measurement_value(reading, M_ALLOWANCE) {
            Some(v) => describe_allowance_value(v),
            None => style::alarm(UNREADABLE),
        };
        // The exact classification, not a float comparison against 2^255.
        let unlimited = match measurement_value(reading, M_ALLOWANCE_UNLIMITED) {
            Some(v) if v > 0.0 => "yes".to_string(),
            Some(_) => "no".to_string(),
            None => style::alarm(UNREADABLE),
        };
        table.push(vec![
            owner.clone(),
            style::dim(&reading.network),
            format!("→ {spender}"),
            value,
            unlimited,
        ]);
    }
    Some((
        subsection_title("GRT allowances", &readings, "allowances"),
        table.render(),
    ))
}

/// Solo is one check per network, so its status is per network: unreadable, or the total.
fn solo_title(readings: &[&Reading]) -> String {
    let parts: Vec<String> = readings
        .iter()
        .map(|r| match &r.observation {
            Err(_) => style::alarm(&format!("{UNREADABLE} unreadable on {}", r.network)),
            Ok(obs) => {
                let total: f64 = obs
                    .measurements
                    .iter()
                    .filter(|m| m.name == M_SOLO_DEPLOYMENTS)
                    .map(|m| m.value)
                    .sum();
                let indexers = obs
                    .measurements
                    .iter()
                    .filter(|m| m.name == M_SOLO_DEPLOYMENTS)
                    .count();
                style::dim(&format!(
                    "{} across {indexers} indexers on {}",
                    group_thousands(total as u64),
                    r.network
                ))
            }
        })
        .collect();
    format!(
        "{}  {}",
        style::bold("▸ Solo deployments"),
        parts.join(&style::dim(" · "))
    )
}

/// Every chain the tool read, and the one block it read each at. Every value under CHECKS on
/// that chain is from this block.
fn chains_table(report: &CycleReport) -> String {
    let mut table = Table::new(&[
        ("Chain", Align::Left),
        ("Chain id", Align::Right),
        ("Read at block", Align::Right),
        ("Block time", Align::Left),
    ]);
    for net in &report.networks {
        let (block, time) = match net.block {
            Some(b) => (group_thousands(b.number), style::dim(&b.timestamp_utc())),
            // Even with nothing read, the readout still says where it was looking.
            None => (style::alarm(UNREADABLE), style::alarm("no block read")),
        };
        table.push(vec![
            style::bold(&net.network),
            net.chain_id.to_string(),
            block,
            time,
        ]);
    }
    table.render()
}

/// The tool's inputs: every network's RPC, then every subgraph it reads from. If one is
/// down, the tool is blind to what it serves.
fn sources_table(report: &CycleReport) -> String {
    let mut table = Table::new(&[
        ("Source", Align::Left),
        ("Chain", Align::Left),
        ("Up", Align::Left),
        ("Head", Align::Right),
        ("Lag", Align::Right),
        ("Detail", Align::Left),
    ]);
    let sources = report
        .networks
        .iter()
        .map(|n| &n.source)
        .chain(&report.subgraph_sources);
    for source in sources {
        // `!` only when nothing could be read at all.
        let unknown = || {
            if source.head.is_none() {
                style::alarm(UNREADABLE)
            } else {
                style::dim(UNCONFIGURED)
            }
        };
        let detail = match &source.fault {
            Some(fault) => style::alarm(&fault.summary()),
            None => String::new(),
        };
        table.push(vec![
            source.id.to_string(),
            style::dim(&source.network),
            yes_no(source.is_up()),
            source
                .head
                .map(|h| group_thousands(h.number))
                .unwrap_or_else(unknown),
            source
                .lag
                .map(|l| humanize(l.as_secs()))
                .unwrap_or_else(unknown),
            detail,
        ]);
    }
    table.render()
}

fn yes_no(up: bool) -> String {
    if up {
        "yes".to_string()
    } else {
        style::alarm("no")
    }
}

/// Nothing is graded here beyond the check itself: up means answering, the asked-for
/// deployment, no indexing errors, and inside `max_lag`.
fn subgraph_table(readings: &[&Reading]) -> String {
    let mut table = Table::new(&[
        ("Subgraph", Align::Left),
        ("Chain", Align::Left),
        ("Up", Align::Left),
        ("Indexed block", Align::Right),
        ("Behind", Align::Right),
        ("Lag", Align::Right),
        ("Detail", Align::Left),
    ]);
    for reading in readings {
        let Subject::Subgraph {
            subgraph,
            deployment,
        } = &reading.subject
        else {
            continue;
        };
        // A listed deployment sits under the subgraph it was listed with.
        let label = match deployment {
            Some(d) => format!("  └ {d}"),
            None => subgraph.clone(),
        };
        let block = reading.value(M_SUBGRAPH_BLOCK_HEIGHT);
        // `!` only when nothing could be read. A gap that could not be measured on a
        // subgraph that did answer (RPC down, no timestamp) is a dash, not a failure.
        let unknown = || {
            if block.is_none() {
                style::alarm(UNREADABLE)
            } else {
                style::dim(UNCONFIGURED)
            }
        };
        let (up, detail) = match &reading.observation {
            // Our problem, not the subgraph's: it must not read as "no".
            Err(why) => (style::action("not checked"), style::action(&why.reason)),
            Ok(obs) => match &obs.fault {
                Some(fault) => (style::alarm("no"), style::alarm(&fault.summary)),
                None if deployment.is_some() => ("yes".to_string(), String::new()),
                // For the published version, which deployment that currently is.
                None => (
                    "yes".to_string(),
                    style::dim(obs.fact(F_SERVED_DEPLOYMENT).unwrap_or_default()),
                ),
            },
        };
        debug_assert!(
            reading.is_unreadable() || reading.value(M_SUBGRAPH_UP).is_some(),
            "a read subgraph always reports up"
        );
        // A deployment row is on its parent's chain; saying it again is noise.
        let chain = if deployment.is_some() {
            String::new()
        } else {
            style::dim(&reading.network)
        };
        table.push(vec![
            label,
            chain,
            up,
            block
                .map(|b| group_thousands(b as u64))
                .unwrap_or_else(unknown),
            reading
                .value(M_SUBGRAPH_BLOCKS_BEHIND)
                .map(|b| format!("{} blocks", group_thousands(b as u64)))
                .unwrap_or_else(unknown),
            reading
                .value(M_SUBGRAPH_LAG)
                .map(|l| humanize(l as u64))
                .unwrap_or_else(unknown),
            detail,
        ]);
    }
    table.render()
}

/// How many rows of indexers to show per network. The metrics carry them all.
const SOLO_ROWS: usize = 10;

/// Indexers by solo deployments, most first. No indexer is hidden, the upgrade indexer
/// included: what counts as abuse is for the reader to judge.
fn solo_table(readings: &[&Reading]) -> String {
    let mut table = Table::new(&[
        ("Indexer", Align::Left),
        ("Chain", Align::Left),
        ("Solo", Align::Right),
        ("Signal GRT", Align::Right),
        ("Detail", Align::Left),
    ]);
    for reading in readings {
        let obs = match &reading.observation {
            Err(why) => {
                table.push(vec![
                    style::alarm(UNREADABLE),
                    style::dim(&reading.network),
                    style::alarm(UNREADABLE),
                    style::alarm(UNREADABLE),
                    style::alarm(&why.reason),
                ]);
                continue;
            }
            Ok(obs) => obs,
        };
        // (indexer, deployments, signal)
        let mut rows: Vec<(Address, f64, Option<f64>)> = obs
            .measurements
            .iter()
            .filter(|m| m.name == M_SOLO_DEPLOYMENTS)
            .filter_map(|m| match m.subject {
                Subject::Indexer { indexer } => {
                    let signal = obs
                        .measurements
                        .iter()
                        .find(|s| s.name == M_SOLO_SIGNAL && s.subject == m.subject)
                        .map(|s| s.value);
                    Some((indexer, m.value, signal))
                }
                _ => None,
            })
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        if rows.is_empty() {
            table.push(vec![
                style::dim("none"),
                style::dim(&reading.network),
                String::new(),
                String::new(),
                String::new(),
            ]);
        }
        let hidden = rows.len().saturating_sub(SOLO_ROWS);
        for (indexer, count, signal) in rows.into_iter().take(SOLO_ROWS) {
            table.push(vec![
                render_address(&indexer),
                style::dim(&reading.network),
                group_thousands(count as u64),
                signal
                    .map(|s| format_float(s, 0))
                    .unwrap_or_else(|| style::dim(UNCONFIGURED)),
                String::new(),
            ]);
        }
        if hidden > 0 {
            table.push(vec![
                style::dim(&format!("+{hidden} more")),
                style::dim(&reading.network),
                String::new(),
                String::new(),
                String::new(),
            ]);
        }
    }
    table.render()
}

/// An unlimited approval is ~1.15e59 GRT once scaled; never print that.
fn describe_allowance_value(v: f64) -> String {
    if v >= 5.7e58 {
        "~unlimited".to_string()
    } else if v == 0.0 {
        "none".to_string()
    } else {
        format_float(v, GRT_PLACES)
    }
}

/// What a reading came to, in one word: `ok`, `!` (could not read) or `fault` (read, and
/// found something wrong).
fn state(reading: &Reading) -> &'static str {
    if reading.is_unreadable() {
        "!"
    } else if reading.fault().is_some() {
        "fault"
    } else {
        "ok"
    }
}

/// `account BillingWallet`, `subgraph ebo@Qm…`; just `network` for a network-wide check.
fn subject_display(subject: &Subject) -> String {
    match subject {
        Subject::Network => "network".to_string(),
        _ => format!("{} {}", subject.kind(), subject.name()),
    }
}

/// How findings name a check: `activity EpochBlockOracle`, or just `solo_deployments` when
/// the instance is the network itself and saying it twice is noise.
fn check_display<'a>(network: &str, kind: &'a str, instance: &'a str) -> String {
    if instance == network {
        kind.to_string()
    } else {
        format!("{kind} {instance}")
    }
}

/// Every check and source, one row each. `problems_only` keeps the rows that are not `ok`.
pub fn flat(report: &CycleReport, problems_only: bool) -> String {
    let mut out = String::new();
    out.push_str(&header(report));
    out.push_str("\n\n");

    let mut table = Table::new(&[
        ("State", Align::Left),
        ("Chain", Align::Left),
        ("Check", Align::Left),
        ("Subject", Align::Left),
        ("Detail", Align::Left),
    ]);

    // Problems first: the reason to run this command is usually to find what did not resolve.
    let mut sorted: Vec<&Reading> = report.readings.iter().collect();
    sorted.sort_by_key(|r| (state(r) == "ok", r.id.to_string()));

    for reading in sorted {
        if problems_only && state(reading) == "ok" {
            continue;
        }
        let mut detail = reading.summary().to_string();
        if let Some(intent) = &reading.remediation {
            detail.push_str(&format!("  [{}]", intent.describe()));
        }
        table.push(vec![
            state(reading).to_string(),
            reading.network.clone(),
            reading.id.to_string(),
            subject_display(&reading.subject),
            detail,
        ]);
    }

    // Sources are not checks, but in a flat list of "what did and did not resolve" they
    // belong beside everything else.
    for source in report
        .networks
        .iter()
        .map(|n| &n.source)
        .chain(&report.subgraph_sources)
    {
        if problems_only && source.is_up() {
            continue;
        }
        table.push(vec![
            if source.is_up() { "ok" } else { "!" }.to_string(),
            source.network.clone(),
            source.id.to_string(),
            "source".to_string(),
            source.summary(),
        ]);
    }

    if table.is_empty() {
        out.push_str(if problems_only {
            "Every check read, and none found a fault.\n"
        } else {
            "No checks configured.\n"
        });
    } else {
        out.push_str(&table.render());
    }

    out.push_str(&format!("\n{}\n", summary_line(report)));
    out
}

/// Something wrong in a cycle, worded for a person.
///
/// One list serves both commands: `status` prints every finding as a note, `serve` logs one
/// when it appears or changes and again when it clears. `key` is stable while the same thing
/// is wrong, so a long outage is one log line, not one per cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub key: String,
    pub message: String,
    /// What to say once it is no longer found.
    pub cleared: String,
}

pub fn findings(report: &CycleReport) -> Vec<Finding> {
    let mut out = Vec::new();

    for net in report.networks.iter().filter(|n| !n.source.is_up()) {
        let effect = if net.checks > 0 {
            format!("every {} account check reports unreadable", net.network)
        } else {
            format!("{} subgraphs have no block gap", net.network)
        };
        out.push(Finding {
            key: format!("source:{}", net.source.id),
            message: format!(
                "source {} is unavailable ({}); {effect}",
                net.source.id,
                net.source.summary()
            ),
            cleared: format!("source {} is up again", net.source.id),
        });
    }

    for src in report.subgraph_sources.iter().filter(|s| !s.is_up()) {
        out.push(Finding {
            key: format!("source:{}", src.id),
            message: format!(
                "source {} is unavailable ({}); activity checks on {} search the chain instead, \
                 and checks that need it are unreadable",
                src.id,
                src.summary(),
                src.network
            ),
            cleared: format!("source {} is up again", src.id),
        });
    }

    // Checks on an unavailable source are covered by its finding; these failed on their own.
    // Grouped by reason, so a provider limit hitting five accounts reads as one problem.
    type Reason<'a> = (&'a str, &'a str, &'a str);
    let mut by_reason: Vec<(Reason, Vec<&str>)> = Vec::new();
    for reading in report.unreadable() {
        let Err(why) = &reading.observation else {
            continue;
        };
        if why.kind == "source_unavailable" {
            continue;
        }
        let key = (
            reading.network.as_str(),
            reading.id.kind,
            why.reason.as_str(),
        );
        let instance = reading.id.instance.as_str();
        match by_reason.iter_mut().find(|(k, _)| *k == key) {
            Some((_, instances)) => instances.push(instance),
            None => by_reason.push((key, vec![instance])),
        }
    }
    for ((network, kind, reason), instances) in by_reason {
        let who = check_display(network, kind, &instances.join(", "));
        out.push(Finding {
            key: format!("unreadable:{network}:{who}"),
            message: format!("{network} {who} could not be read: {reason}"),
            cleared: format!("{network} {who} can be read again"),
        });
    }

    for (reading, fault) in report.faults() {
        let network = &reading.network;
        let who = check_display(network, reading.id.kind, &reading.id.instance);
        out.push(Finding {
            key: format!("fault:{network}:{who}"),
            message: format!("{network} {who}: {}", fault.summary),
            cleared: format!("{network} {who} is healthy again"),
        });
    }

    out
}

/// Logs findings as they change, for a long-running process.
#[derive(Default)]
pub struct FindingLog {
    open: std::collections::BTreeMap<String, Finding>,
}

/// What a cycle's findings changed, in the order to report it.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    /// New, or the same problem with a different message.
    Raised(String),
    Cleared(String),
}

impl FindingLog {
    pub fn update(&mut self, current: Vec<Finding>) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut next = std::collections::BTreeMap::new();
        for finding in current {
            if self.open.get(&finding.key).map(|f| &f.message) != Some(&finding.message) {
                changes.push(Change::Raised(finding.message.clone()));
            }
            next.insert(finding.key.clone(), finding);
        }
        for (key, finding) in &self.open {
            if !next.contains_key(key) {
                changes.push(Change::Cleared(finding.cleared.clone()));
            }
        }
        self.open = next;
        changes
    }
}

pub fn summary_line(report: &CycleReport) -> String {
    let (read, unreadable) = report.counts();
    let mut parts = vec![style::dim(&format!("{read} read"))];
    if unreadable > 0 {
        parts.push(style::alarm(&format!("{unreadable} unreadable")));
    }
    let faults = report.faults().count();
    if faults > 0 {
        parts.push(style::alarm(&format!("{faults} fault(s)")));
    }
    let pending = report.remediations().count();
    if pending > 0 {
        parts.push(style::action(&format!("{pending} to remediate")));
    }
    parts.join(&style::dim(" · "))
}

fn subject_json(subject: &Subject) -> Value {
    let mut v = match subject {
        Subject::Account {
            group,
            account,
            address,
        } => json!({ "group": group, "account": account, "address": address.to_string() }),
        Subject::Allowance {
            group,
            owner,
            spender,
        } => json!({ "group": group, "owner": owner, "spender": spender }),
        Subject::Subgraph {
            subgraph,
            deployment,
        } => json!({ "subgraph": subgraph, "deployment": deployment }),
        Subject::Indexer { indexer } => json!({ "indexer": format!("{indexer:#x}") }),
        Subject::Network => json!({}),
    };
    v["kind"] = json!(subject.kind());
    v
}

/// A measurement names its subject only when it is not the reading's own, as for each
/// indexer under the network-wide solo check.
fn measurement_json(m: &Measurement, of: &Subject) -> Value {
    let mut v = json!({
        "name": m.name,
        "value": m.value,
        "unit": m.unit.as_str(),
    });
    if &m.subject != of {
        v["subject"] = subject_json(&m.subject);
    }
    v
}

pub fn json(report: &CycleReport) -> Value {
    let checks: Vec<Value> = report
        .readings
        .iter()
        .map(|r| {
            let obs = r.observation.as_ref().ok();
            json!({
                "network": r.network,
                "check": r.id.kind,
                "instance": r.id.instance,
                "subject": subject_json(&r.subject),
                "state": match state(r) { "!" => "unreadable", s => s },
                "summary": r.summary(),
                "unreadable": r.observation.as_ref().err().map(|u| json!({
                    "kind": u.kind,
                    "reason": u.reason,
                })),
                "fault": r.fault().map(|f| json!({ "kind": f.kind, "summary": f.summary })),
                "remediation": r.remediation.as_ref().map(|rem| json!({
                    "asset": rem.asset.as_str(),
                    "below": rem.below.to_f64(),
                    "top_up_by": rem.top_up_by.to_f64(),
                    "from": rem.from,
                })),
                "measurements": obs
                    .map(|o| o.measurements.iter().map(|m| measurement_json(m, &r.subject)).collect::<Vec<_>>())
                    .unwrap_or_default(),
                "facts": obs
                    .map(|o| o.facts.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>())
                    .unwrap_or_default(),
            })
        })
        .collect();

    let (read, unreadable) = report.counts();

    // Block provenance comes first, and is present in every mode: one entry per network.
    json!({
        "networks": report.networks.iter().map(|n| json!({
            "network": n.network,
            "chain_id": n.chain_id,
            "block": n.block.map(|b| b.number),
            "block_timestamp": n.block.map(|b| b.timestamp_rfc3339()),
            "source": {
                "id": n.source.id.as_str(),
                "up": n.source.is_up(),
                "summary": n.source.summary(),
                "lag_seconds": n.source.lag.map(|l| l.as_secs()),
            },
        })).collect::<Vec<_>>(),
        "subgraph_sources": report.subgraph_sources.iter().map(|s| json!({
            "id": s.id.as_str(),
            "network": s.network,
            "up": s.is_up(),
            "summary": s.summary(),
            "block": s.head.map(|h| h.number),
            "lag_seconds": s.lag.map(|l| l.as_secs()),
        })).collect::<Vec<_>>(),
        "counts": { "read": read, "unreadable": unreadable, "faults": report.faults().count() },
        "checks": checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;
    use graph_monitor_core::check::{
        Asset, CheckId, Observation, RemediationIntent, Unit, Unreadable,
    };
    use graph_monitor_core::engine::NetworkReport;
    use graph_monitor_core::rpc::PinnedBlock;
    use graph_monitor_core::source::{SourceHealth, SourceId};
    use graph_monitor_core::subgraph::{NotChecked, SubgraphFault, SubgraphHealth, SubgraphStatus};
    use graph_monitor_core::units::Amount;
    use std::time::Duration;

    fn amount(v: u64) -> Amount {
        Amount::new(U256::from(v), 0)
    }

    fn outcome(
        kind: &'static str,
        account: &str,
        group: Option<&str>,
        measurement: Option<(&'static str, f64, Unit)>,
    ) -> Reading {
        let subject = Subject::Account {
            group: group.map(str::to_string),
            account: account.to_string(),
            address: Address::repeat_byte(0x11),
        };
        let mut obs = Observation::new("summary");
        if let Some((name, value, unit)) = measurement {
            obs = obs.with(Measurement {
                name,
                value,
                unit,
                subject: subject.clone(),
            });
        }
        Reading {
            id: CheckId::new(kind, account),
            network: "arbitrum-one".into(),
            subject,
            observation: Ok(obs),
            remediation: None,
            duration: Duration::ZERO,
        }
    }

    fn unreadable(kind: &'static str, account: &str) -> Reading {
        let mut o = outcome(kind, account, None, None);
        o.observation = Err(Unreadable::read_failed("endpoint refused historical state"));
        o
    }

    fn report(readings: Vec<Reading>) -> CycleReport {
        let head = PinnedBlock {
            number: 505_090_074,
            timestamp: 1_773_253_334,
        };
        CycleReport {
            networks: vec![NetworkReport {
                network: "arbitrum-one".into(),
                chain_id: 42161,
                source: SourceHealth {
                    chain_id: Some(42161),
                    head: Some(head),
                    lag: Some(Duration::from_secs(2)),
                    ..SourceHealth::new(SourceId::rpc("arbitrum-one"), "arbitrum-one")
                },
                block: Some(head),
                checks: readings.len(),
            }],
            subgraph_sources: Vec::new(),
            readings,
            duration: Duration::ZERO,
        }
    }

    fn subgraph(
        name: &str,
        deployment: Option<&str>,
        indexed: Option<u64>,
        status: SubgraphStatus,
    ) -> SubgraphHealth {
        SubgraphHealth {
            network: "arbitrum-one".into(),
            subgraph: name.into(),
            deployment: deployment.map(str::to_string),
            served_deployment: indexed.map(|_| "QmServed".to_string()),
            indexed_block: indexed,
            block_timestamp: None,
            blocks_behind: indexed.map(|b| 505_090_074 - b),
            lag: indexed.map(|_| Duration::from_secs(45)),
            status,
        }
    }

    fn readings(healths: Vec<SubgraphHealth>) -> Vec<Reading> {
        healths
            .into_iter()
            .map(|h| h.into_reading(Duration::ZERO))
            .collect()
    }

    #[test]
    fn subgraphs_render_their_lag_and_failures() {
        let mut r = report(vec![]);
        r.readings = readings(vec![
            subgraph("network", None, Some(505_089_974), SubgraphStatus::Up),
            subgraph(
                "network",
                Some("QmNext"),
                Some(505_000_000),
                SubgraphStatus::Up,
            ),
            subgraph(
                "ebo",
                None,
                None,
                SubgraphStatus::Down(SubgraphFault::Unreachable("subgraph not found".into())),
            ),
            subgraph(
                "qos",
                None,
                None,
                SubgraphStatus::NotChecked(NotChecked::NotConfigured(
                    "no gateway.api_key configured".into(),
                )),
            ),
            // Another chain: answered, but the gap to our RPC's head is not measurable.
            SubgraphHealth {
                network: "gnosis".into(),
                blocks_behind: None,
                ..subgraph("gateway-qos", None, Some(42), SubgraphStatus::Up)
            },
        ]);
        let out = grouped(&r);
        let row = |name: &str| {
            out.lines()
                .find(|l| l.trim_start().starts_with(name))
                .unwrap_or_else(|| panic!("no row {name} in:\n{out}"))
                .to_string()
        };
        assert!(row("network").contains("100 blocks"), "{out}");
        assert!(row("network").contains("45s"), "{out}");
        assert!(row("network").contains("QmServed"), "{out}");
        // A listed deployment renders under its subgraph, not as a sibling.
        assert!(row("└ QmNext").contains("90,074 blocks"), "{out}");
        assert!(row("ebo").contains("subgraph not found"), "{out}");
        // Not checked is our problem and must not read as the subgraph being down.
        let gnosis = row("gateway-qos");
        assert!(gnosis.contains("gnosis"), "{out}");
        assert!(!gnosis.contains('!'), "{out}");
        let qos = row("qos");
        let up_column: Vec<&str> = qos.split_whitespace().skip(2).take(2).collect();
        assert_eq!(up_column, ["not", "checked"], "{out}");
        let line = summary_line(&r);
        assert!(line.contains("1 fault(s)"), "{line}");
        assert!(line.contains("1 unreadable"), "{line}");

        let flat_out = flat(&r, true);
        assert!(flat_out.contains("subgraph:ebo"), "{flat_out}");
        assert!(flat_out.contains("subgraph:qos"), "{flat_out}");
        assert!(!flat_out.contains("subgraph:network"), "{flat_out}");

        let v = json(&r);
        let behind = |i: usize| {
            v["checks"][i]["measurements"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["name"] == M_SUBGRAPH_BLOCKS_BEHIND)
                .map(|m| m["value"].clone())
        };
        assert_eq!(behind(0), Some(json!(100.0)));
        assert_eq!(
            v["checks"][0]["facts"]["served_deployment"],
            json!("QmServed")
        );
        assert_eq!(v["checks"][1]["subject"]["deployment"], json!("QmNext"));
        assert_eq!(v["checks"][2]["state"], json!("fault"));
        assert_eq!(v["checks"][2]["fault"]["kind"], json!("unreachable"));
        assert_eq!(v["checks"][3]["state"], json!("unreadable"));
        assert_eq!(
            v["checks"][3]["unreadable"]["kind"],
            json!("not_configured")
        );
    }

    #[test]
    fn subgraph_sources_are_listed_with_the_rpcs() {
        let mut r = report(vec![]);
        r.subgraph_sources = vec![SourceHealth {
            head: Some(PinnedBlock {
                number: 505_090_000,
                timestamp: 1_773_253_330,
            }),
            lag: Some(Duration::from_secs(4)),
            ..SourceHealth::new(SourceId::subgraph("monitor"), "arbitrum-one")
        }];
        let out = grouped(&r);
        let sources = &out[out.find("INPUTS").unwrap()..out.find("CHECKS").unwrap()];
        let row = sources
            .lines()
            .find(|l| l.contains("subgraph:monitor"))
            .unwrap_or_else(|| panic!("no subgraph source row in:\n{out}"));
        assert!(
            row.contains("arbitrum-one") && row.contains("505,090,000"),
            "{row}"
        );
        assert_eq!(json(&r)["subgraph_sources"][0]["up"], json!(true));
    }

    #[test]
    fn subgraphs_sit_under_checks() {
        let mut r = report(vec![]);
        r.readings = readings(vec![subgraph("network", None, Some(1), SubgraphStatus::Up)]);
        let out = grouped(&r);
        assert!(out.find("CHECKS").unwrap() < out.find("Subgraphs").unwrap());
        assert!(!grouped(&report(vec![])).contains("Subgraphs"));
    }

    #[test]
    fn unreadable_findings_say_why_and_group_shared_reasons() {
        let mut a = unreadable("activity", "SamOracleGraphOps");
        a.observation = Err(Unreadable::read_failed(
            "searched 600000 blocks without reaching the lookback",
        ));
        let mut b = unreadable("activity", "SamOracleSemiotic");
        b.observation = a.observation.clone();
        let c = unreadable("eth_balance", "BillingWallet");
        let messages: Vec<String> = findings(&report(vec![
            a,
            outcome("eth_balance", "Fine", None, None),
            b,
            c,
        ]))
        .into_iter()
        .map(|f| f.message)
        .collect();
        assert_eq!(
            messages,
            [
                "arbitrum-one activity SamOracleGraphOps, SamOracleSemiotic could not be read: \
                 searched 600000 blocks without reaching the lookback",
                "arbitrum-one eth_balance BillingWallet could not be read: endpoint refused \
                 historical state",
            ]
        );
    }

    #[test]
    fn every_kind_of_problem_is_a_finding() {
        let mut r = report(vec![]);
        r.subgraph_sources = vec![SourceHealth {
            fault: Some(graph_monitor_core::source::SourceFault::NotQueryable(
                NotChecked::KeyRejected("auth error: API key not found".into()),
            )),
            ..SourceHealth::new(SourceId::subgraph("graph-monitor"), "arbitrum-one")
        }];
        r.readings = readings(vec![
            subgraph(
                "ebo",
                None,
                None,
                SubgraphStatus::Down(SubgraphFault::IndexingErrors),
            ),
            subgraph(
                "qos",
                None,
                None,
                SubgraphStatus::NotChecked(NotChecked::NotConfigured(
                    "no gateway.api_key configured".into(),
                )),
            ),
        ]);
        let messages: Vec<String> = findings(&r).into_iter().map(|f| f.message).collect();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(
            messages[0].contains("subgraph:graph-monitor is unavailable"),
            "{}",
            messages[0]
        );
        assert!(
            messages[0].contains("refused our API key"),
            "{}",
            messages[0]
        );
        // Unreadable before faults, each worded the same way whatever the kind of check.
        assert_eq!(
            messages[1],
            "arbitrum-one subgraph qos could not be read: no gateway.api_key configured"
        );
        assert!(
            messages[2].starts_with("arbitrum-one subgraph ebo: "),
            "{}",
            messages[2]
        );
    }

    #[test]
    fn the_log_reports_changes_not_every_cycle() {
        let f = |key: &str, message: &str| Finding {
            key: key.into(),
            message: message.into(),
            cleared: format!("{key} is fine again"),
        };
        let mut log = FindingLog::default();
        assert_eq!(
            log.update(vec![f("a", "a is down")]),
            [Change::Raised("a is down".into())]
        );
        // Same problem, same words: nothing to say.
        assert!(log.update(vec![f("a", "a is down")]).is_empty());
        // Same problem, new words: say it again.
        assert_eq!(
            log.update(vec![f("a", "a is stale")]),
            [Change::Raised("a is stale".into())]
        );
        // Gone: say it cleared, once.
        assert_eq!(
            log.update(vec![]),
            [Change::Cleared("a is fine again".into())]
        );
        assert!(log.update(vec![]).is_empty());
    }

    #[test]
    fn sources_come_first() {
        let out = grouped(&report(vec![]));
        let row = out
            .lines()
            .find(|l| l.contains("rpc:arbitrum-one"))
            .unwrap_or_else(|| panic!("no rpc row in:\n{out}"));
        assert!(row.contains("505,090,074"), "{row}");
        assert!(row.contains("2s"), "{row}");
        // How the tool sees comes before what it saw.
        assert!(out.find("INPUTS").unwrap() < out.find("CHECKS").unwrap());
    }

    #[test]
    fn inputs_state_the_block_each_chain_was_read_at() {
        let out = grouped(&report(vec![]));
        let row = out
            .lines()
            .find(|l| l.contains("42161"))
            .unwrap_or_else(|| panic!("no chain row in:\n{out}"));
        assert!(row.contains("arbitrum-one"), "{row}");
        assert!(row.contains("505,090,074"), "{row}");
        assert!(row.contains("2026-03-11"), "{row}");
        assert!(
            out.lines().next().unwrap().contains("as of 2026-03-11"),
            "{out}"
        );
    }

    #[test]
    fn header_still_names_the_network_when_nothing_could_be_read() {
        let mut r = report(vec![]);
        r.networks[0].block = None;
        let out = grouped(&r);
        assert!(out.contains("arbitrum-one"), "{out}");
        assert!(out.contains("no block read"), "{out}");
    }

    #[test]
    fn unconfigured_and_unreadable_render_differently() {
        // A dash = not checked. `!` = checked, and the read failed.
        let r = report(vec![
            outcome(
                "eth_balance",
                "HasEth",
                None,
                Some((M_ETH_BALANCE, 0.42, Unit::Eth)),
            ),
            unreadable("eth_balance", "Unreadable"),
        ]);
        let out = grouped(&r);

        let has_eth = out.lines().find(|l| l.contains("HasEth")).unwrap();
        assert!(has_eth.contains("0.42"), "{has_eth}");
        // GRT is not watched for this account, so its cell is blank, not `!`.
        assert!(!has_eth.contains('!'), "{has_eth}");

        let failed = out.lines().find(|l| l.contains("Unreadable")).unwrap();
        assert!(failed.contains('!'), "{failed}");
    }

    #[test]
    fn nothing_is_graded() {
        // There is no Status column: a balance of zero is reported, not condemned.
        let r = report(vec![outcome(
            "eth_balance",
            "Empty",
            None,
            Some((M_ETH_BALANCE, 0.0, Unit::Eth)),
        )]);
        let out = grouped(&r);
        assert!(out.contains("0.00"), "{out}");
        assert!(
            !out.contains("P0") && !out.contains("P1") && !out.contains("P2"),
            "{out}"
        );
        assert!(!out.contains("Status"), "{out}");
    }

    #[test]
    fn a_pending_repair_is_shown_where_the_status_used_to_be() {
        let mut o = outcome(
            "eth_balance",
            "BillingWallet",
            None,
            Some((M_ETH_BALANCE, 0.001, Unit::Eth)),
        );
        o.remediation = Some(RemediationIntent {
            asset: Asset::Eth,
            observed: amount(0),
            below: amount(5),
            top_up_by: amount(10),
            from: Some("Funder".into()),
            from_address: None,
        });
        let out = grouped(&report(vec![o]));
        assert!(out.contains("Remediation"), "{out}");
        assert!(out.contains("top up"), "{out}");
        assert!(out.contains("Funder"), "{out}");
        assert!(summary_line(&report(vec![])).contains("0 read"));
    }

    #[test]
    fn grouped_view_shows_activity() {
        // Regression: the grouped renderer matched only balance kinds, so activity checks ran
        // — slowly, since they scan logs — and then rendered nothing at all.
        let recent = outcome(
            "activity",
            "Busy",
            None,
            Some((M_IDLE_SECONDS, 540.0, Unit::Seconds)),
        );

        // Read fine, found nothing in the window. Not a failed read.
        let quiet = outcome("activity", "Quiet", None, None);

        let blind = unreadable("activity", "Blind");

        let out = grouped(&report(vec![recent, quiet, blind]));
        assert!(out.contains("Last activity"), "{out}");

        let row = |name: &str| {
            out.lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("row {name} missing from:\n{out}"))
                .to_string()
        };
        assert!(row("Busy").contains("9m"), "{}", row("Busy"));
        assert!(row("Quiet").contains("none"), "{}", row("Quiet"));
        assert!(row("Blind").contains('!'), "{}", row("Blind"));
    }

    #[test]
    fn accounts_are_sectioned_by_group() {
        let r = report(vec![
            outcome(
                "eth_balance",
                "BillingWallet",
                Some("graph-foundation"),
                Some((M_ETH_BALANCE, 0.1, Unit::Eth)),
            ),
            outcome(
                "eth_balance",
                "NetworkMonitor",
                Some("infra"),
                Some((M_ETH_BALANCE, 0.4, Unit::Eth)),
            ),
        ]);
        let out = grouped(&r);
        assert!(out.contains("graph-foundation"), "{out}");
        assert!(out.contains("infra"), "{out}");
    }

    #[test]
    fn allowance_column_uses_the_exact_flag_not_the_float() {
        // The gauge for an unlimited approval is ~1.157e59; the yes/no comes from the exact
        // U256 classification travelling beside it.
        let mut o = outcome("allowance", "BillingWallet", None, None);
        o.subject = Subject::Allowance {
            group: None,
            owner: "BillingWallet".into(),
            spender: "NetworkMonitor".into(),
        };
        o.observation = Ok(Observation::new("…")
            .with(Measurement {
                name: M_ALLOWANCE,
                value: 1.157e59,
                unit: Unit::Grt,
                subject: o.subject.clone(),
            })
            .with(Measurement {
                name: M_ALLOWANCE_UNLIMITED,
                value: 1.0,
                unit: Unit::Boolean,
                subject: o.subject.clone(),
            }));
        let out = grouped(&report(vec![o]));
        assert!(out.contains("~unlimited"), "{out}");
        assert!(out.contains("yes"), "{out}");
        assert!(!out.contains("115792089"), "{out}");
    }

    #[test]
    fn check_list_puts_unreadable_first() {
        let r = report(vec![
            outcome("eth_balance", "AAA", None, None),
            unreadable("eth_balance", "ZZZ"),
        ]);
        let out = flat(&r, false);
        let order: Vec<&str> = out
            .lines()
            .filter_map(|l| ["AAA", "ZZZ"].iter().find(|n| l.contains(**n)))
            .copied()
            .collect();
        assert_eq!(order, vec!["ZZZ", "AAA"]);
    }

    #[test]
    fn unreadable_only_hides_successful_reads() {
        let r = report(vec![
            outcome("eth_balance", "Fine", None, None),
            unreadable("eth_balance", "Broken"),
        ]);
        let out = flat(&r, true);
        assert!(out.contains("Broken"), "{out}");
        assert!(!out.contains("Fine"), "{out}");
    }

    #[test]
    fn everything_readable_says_so() {
        let r = report(vec![outcome("eth_balance", "Fine", None, None)]);
        assert!(flat(&r, true).contains("Every check read, and none found a fault"));
    }

    #[test]
    fn summary_line_counts_reads_and_repairs() {
        let mut o = outcome("eth_balance", "B", None, None);
        o.remediation = Some(RemediationIntent {
            asset: Asset::Eth,
            observed: amount(0),
            below: amount(5),
            top_up_by: amount(10),
            from: None,
            from_address: None,
        });
        let r = report(vec![
            outcome("eth_balance", "A", None, None),
            o,
            unreadable("eth_balance", "C"),
        ]);
        let line = summary_line(&r);
        assert!(line.contains("2 read"), "{line}");
        assert!(line.contains("1 unreadable"), "{line}");
        assert!(line.contains("1 to remediate"), "{line}");
    }

    #[test]
    fn json_puts_provenance_before_the_data() {
        let value = json(&report(vec![]));
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys[0], "networks");
        assert!(
            keys.iter().position(|k| *k == "networks").unwrap()
                < keys.iter().position(|k| *k == "checks").unwrap()
        );
        assert_eq!(value["networks"][0]["block"], json!(505_090_074u64));
        assert_eq!(value["networks"][0]["source"]["up"], json!(true));
    }

    #[test]
    fn every_network_gets_a_chain_row_and_rows_name_their_chain() {
        let mut gnosis = outcome(
            "eth_balance",
            "BillingWallet",
            None,
            Some((M_ETH_BALANCE, 1.5, Unit::Eth)),
        );
        gnosis.network = "gnosis".into();
        let mut r = report(vec![
            outcome(
                "eth_balance",
                "BillingWallet",
                None,
                Some((M_ETH_BALANCE, 0.25, Unit::Eth)),
            ),
            gnosis,
        ]);
        r.networks.push(NetworkReport {
            network: "gnosis".into(),
            chain_id: 100,
            source: SourceHealth::new(SourceId::rpc("gnosis"), "gnosis"),
            block: Some(PinnedBlock {
                number: 49_021_050,
                timestamp: 1_773_253_330,
            }),
            checks: 1,
        });
        let out = grouped(&r);
        assert!(out.contains("505,090,074"), "{out}");
        assert!(out.contains("49,021,050"), "{out}");
        // The same label on two chains is two rows, each saying which chain.
        let rows: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("BillingWallet"))
            .collect();
        assert_eq!(rows.len(), 2, "{out}");
        assert!(rows
            .iter()
            .any(|l| l.contains("arbitrum-one") && l.contains("0.25")));
        assert!(rows
            .iter()
            .any(|l| l.contains("gnosis") && l.contains("1.50")));

        let v = json(&r);
        assert_eq!(v["networks"][1]["network"], json!("gnosis"));
        assert_eq!(v["checks"][1]["network"], json!("gnosis"));
    }

    #[test]
    fn json_carries_no_severity_and_marks_failed_reads() {
        let r = report(vec![
            outcome(
                "eth_balance",
                "A",
                None,
                Some((M_ETH_BALANCE, 1.0, Unit::Eth)),
            ),
            unreadable("eth_balance", "B"),
        ]);
        let v = json(&r);
        assert_eq!(v["checks"][0]["state"], json!("ok"));
        assert_eq!(v["checks"][0]["subject"]["kind"], json!("account"));
        assert_eq!(v["checks"][1]["state"], json!("unreadable"));
        assert_eq!(v["checks"][1]["unreadable"]["kind"], json!("read_failed"));
        // A failed read contributes no measurements — never a zero.
        assert_eq!(v["checks"][1]["measurements"].as_array().unwrap().len(), 0);
        assert!(v.get("worst_severity").is_none());
        assert!(v["checks"][0].get("severity").is_none());
    }

    #[test]
    fn solo_deployments_list_indexers_most_first_and_name_them_in_json() {
        let indexer = |b: u8, count: f64| {
            let subject = Subject::Indexer {
                indexer: Address::repeat_byte(b),
            };
            [
                Measurement {
                    name: M_SOLO_DEPLOYMENTS,
                    value: count,
                    unit: Unit::Count,
                    subject: subject.clone(),
                },
                Measurement {
                    name: M_SOLO_SIGNAL,
                    value: count * 100.0,
                    unit: Unit::Grt,
                    subject,
                },
            ]
        };
        let mut obs = Observation::new("9 solo deployments across 2 indexers");
        for m in indexer(0xaa, 2.0).into_iter().chain(indexer(0xbb, 7.0)) {
            obs = obs.with(m);
        }
        let solo = graph_monitor_core::solo::reading("arbitrum-one", Ok(obs), Duration::ZERO);
        let blind = graph_monitor_core::solo::reading(
            "gnosis",
            Err(Unreadable::new(
                "source_unavailable",
                "source subgraph:gnosis is unavailable",
            )),
            Duration::ZERO,
        );
        let out = grouped(&report(vec![solo, blind]));
        let section = out[out.find("Solo deployments").unwrap()..].to_lowercase();
        let first = section.find("0xbbbb").expect(&section);
        let second = section.find("0xaaaa").expect(&section);
        assert!(first < second, "{section}");
        assert!(section.contains("700"), "{section}");
        assert!(
            section.contains("subgraph:gnosis is unavailable"),
            "{section}"
        );

        let v = json(&report(vec![graph_monitor_core::solo::reading(
            "arbitrum-one",
            Ok(Observation::new("1").with(indexer(0xaa, 1.0)[0].clone())),
            Duration::ZERO,
        )]));
        assert_eq!(v["checks"][0]["subject"]["kind"], json!("network"));
        assert_eq!(
            v["checks"][0]["measurements"][0]["subject"]["indexer"],
            json!(format!("{:#x}", Address::repeat_byte(0xaa)))
        );
    }

    #[test]
    fn sections_say_how_they_went() {
        let r = report(vec![
            outcome(
                "eth_balance",
                "Fine",
                None,
                Some((M_ETH_BALANCE, 1.0, Unit::Eth)),
            ),
            unreadable("eth_balance", "Broken"),
        ]);
        let out = grouped(&r);
        let title = out
            .lines()
            .find(|l| l.trim_start().starts_with("▸ Accounts"))
            .unwrap();
        assert!(title.contains("2 accounts, 2 checks"), "{title}");
        assert!(title.contains("1 unreadable"), "{title}");
        // A section with nothing configured is left out rather than shown empty.
        assert!(!out.contains("GRT allowances"), "{out}");
        // The summary closes the readout.
        assert!(
            out.trim_end()
                .lines()
                .last()
                .unwrap()
                .contains("1 unreadable"),
            "{out}"
        );
    }

    #[test]
    fn formats_floats_with_separators_and_truncation() {
        assert_eq!(format_float(1_204_882.119, 2), "1,204,882.11");
        assert_eq!(format_float(0.0, 2), "0.00");
        assert_eq!(format_float(0.9999, 2), "0.99");
    }

    #[test]
    fn an_unlimited_allowance_never_prints_its_digits() {
        assert_eq!(describe_allowance_value(1.15e59), "~unlimited");
        assert_eq!(describe_allowance_value(0.0), "none");
        assert_eq!(describe_allowance_value(720_000.0), "720,000.00");
    }
}
