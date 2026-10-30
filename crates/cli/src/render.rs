//! Reporting: the console and JSON views of a cycle.
//!
//! Stdout carries data only. Notes about degraded reads go to stderr, so piping into `jq`
//! never breaks.

use std::collections::BTreeMap;

use alloy::primitives::Address;
use graph_monitor_core::engine::CycleReport;
use graph_monitor_core::source::SourceHealth;
use graph_monitor_core::watch::{Measurement, Reading};
use graph_monitor_core::watches::{
    humanize, M_ALLOWANCE, M_ALLOWANCE_UNLIMITED, M_ETH_BALANCE, M_GRT_BALANCE, M_IDLE_SECONDS,
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
/// Blank means "not configured" — no check asked about it.
const UNCONFIGURED: &str = "–";

/// The block-provenance header. Every human readout opens with this, so a pasted screenshot
/// says which network and height it came from.
pub fn header(report: &CycleReport) -> String {
    match report.block {
        Some(block) => format!(
            "{}  {}",
            style::bold(&report.network),
            style::dim(&format!(
                "chain {} · block {} · {}",
                report.chain_id,
                group_thousands(block.number),
                block.timestamp_utc()
            ))
        ),
        // Even with nothing read, the readout still says where it was looking.
        None => format!(
            "{}  {}",
            style::bold(&report.network),
            style::alarm(&format!(
                "chain {} · no block read · source {}",
                report.chain_id,
                report.source.summary()
            ))
        ),
    }
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

/// An activity watch has a third outcome balances do not: it read successfully and found
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

fn account_rows(report: &CycleReport) -> BTreeMap<String, AccountRow> {
    let mut rows: BTreeMap<String, AccountRow> = BTreeMap::new();

    for reading in &report.readings {
        let Some(account) = reading.labels.account.clone() else {
            continue;
        };
        let row = rows.entry(account).or_default();
        row.group = reading.labels.group.clone().or(row.group.take());
        row.address = reading.labels.address.or(row.address);
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

fn balance_row(label: &str, row: &AccountRow) -> Vec<String> {
    vec![
        format!("  {label}"),
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

pub fn grouped(report: &CycleReport) -> String {
    let mut out = String::new();
    out.push_str(&header(report));
    out.push_str("\n\n");

    let rows = account_rows(report);

    {
        let mut table = Table::new(&[
            ("Account", Align::Left),
            ("Address", Align::Left),
            ("ETH", Align::Right),
            ("GRT", Align::Right),
            ("Last activity", Align::Right),
            ("Remediation", Align::Left),
        ]);

        // Grouped, with ungrouped last. Groups are a display device here and nothing more.
        let mut by_group: BTreeMap<Option<String>, Vec<(&String, &AccountRow)>> = BTreeMap::new();
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

        if table.is_empty() {
            out.push_str("No balance watches configured.\n");
        } else {
            out.push_str(&table.render());
        }
    }

    {
        out.push('\n');
        out.push_str(&style::bold("GRT allowances"));
        out.push('\n');

        let mut table = Table::new(&[
            ("Owner", Align::Left),
            ("Spender", Align::Left),
            ("Allowance", Align::Right),
            ("Unlimited", Align::Left),
        ]);

        for reading in report.readings.iter().filter(|o| o.id.kind == "allowance") {
            let owner = reading.labels.account.clone().unwrap_or_default();
            let spender = reading.labels.spender.clone().unwrap_or_default();
            let value = match measurement_value(reading, M_ALLOWANCE) {
                Some(v) => describe_allowance_value(v),
                None => UNREADABLE.to_string(),
            };
            // The exact classification, not a float comparison against 2^255.
            let unlimited = match measurement_value(reading, M_ALLOWANCE_UNLIMITED) {
                Some(v) if v > 0.0 => "yes",
                Some(_) => "no",
                None => UNREADABLE,
            };
            table.push(vec![owner, format!("→ {spender}"), value, unlimited.into()]);
        }

        if table.is_empty() {
            out.push_str("(none configured)\n");
        } else {
            out.push_str(&table.render());
        }
    }

    if !report.subgraphs.is_empty() {
        out.push('\n');
        out.push_str(&style::bold("Subgraphs"));
        out.push('\n');
        out.push_str(&subgraph_table(&report.subgraphs));
    }

    out.push_str(&format!("\n{}\n", summary_line(report)));
    out
}

/// Nothing here is graded beyond what stage 0 already decided: up means reachable, the
/// expected deployment, no indexing errors, and inside `max_lag`.
fn subgraph_table(subgraphs: &[SourceHealth]) -> String {
    let mut table = Table::new(&[
        ("Subgraph", Align::Left),
        ("Up", Align::Left),
        ("Indexed block", Align::Right),
        ("Behind", Align::Right),
        ("Lag", Align::Right),
        ("Detail", Align::Left),
    ]);
    for sg in subgraphs {
        let label = sg
            .id
            .as_str()
            .strip_prefix("subgraph:")
            .unwrap_or(sg.id.as_str());
        // A deployment sits under the subgraph it was listed with.
        let label = match label.split_once('@') {
            Some((_, deployment)) => format!("  └ {deployment}"),
            None => label.to_string(),
        };
        // Unknown renders as `!` only when the subgraph could not be read at all. A gap
        // that could not be measured (RPC down, no timestamp) is blank, not a failure.
        let unknown = if sg.indexed_block.is_none() {
            style::alarm(UNREADABLE)
        } else {
            style::dim(UNCONFIGURED)
        };
        table.push(vec![
            label,
            if sg.is_up() {
                "yes".to_string()
            } else {
                style::alarm("no")
            },
            sg.indexed_block
                .map(group_thousands)
                .unwrap_or_else(|| unknown.clone()),
            sg.blocks_behind
                .map(|b| format!("{} blocks", group_thousands(b)))
                .unwrap_or_else(|| unknown.clone()),
            sg.lag
                .map(|l| humanize(l.as_secs()))
                .unwrap_or_else(|| unknown.clone()),
            match &sg.fault {
                Some(fault) => style::alarm(&fault.summary()),
                None => style::dim(sg.deployment.as_deref().unwrap_or_default()),
            },
        ]);
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

pub fn flat(report: &CycleReport, unreadable_only: bool) -> String {
    let mut out = String::new();
    out.push_str(&header(report));
    out.push_str("\n\n");

    let mut table = Table::new(&[
        ("Read", Align::Left),
        ("Watch", Align::Left),
        ("Detail", Align::Left),
    ]);

    // Unreadable first: the reason to run this command is usually to find what did not resolve.
    let mut sorted: Vec<&Reading> = report.readings.iter().collect();
    sorted.sort_by_key(|o| (!o.is_unreadable(), o.id.to_string()));

    for reading in sorted {
        if unreadable_only && !reading.is_unreadable() {
            continue;
        }
        let mut detail = reading.summary().to_string();
        if let Some(intent) = &reading.remediation {
            detail.push_str(&format!("  [{}]", intent.describe()));
        }
        table.push(vec![
            if reading.is_unreadable() { "!" } else { "ok" }.to_string(),
            reading.id.to_string(),
            detail,
        ]);
    }

    // Subgraphs are sources rather than watches, but in a flat list of "what did and did
    // not resolve" they belong beside everything else.
    for sg in &report.subgraphs {
        if unreadable_only && sg.is_up() {
            continue;
        }
        table.push(vec![
            if sg.is_up() { "ok" } else { "!" }.to_string(),
            sg.id.to_string(),
            subgraph_summary(sg),
        ]);
    }

    if table.is_empty() {
        out.push_str(if unreadable_only {
            "Every watch was read successfully.\n"
        } else {
            "No watches configured.\n"
        });
    } else {
        out.push_str(&table.render());
    }

    out.push_str(&format!("\n{}\n", summary_line(report)));
    out
}

/// Why each unreadable watch failed, for stderr.
///
/// A count alone sends the reader off to rerun with `--flat`. Watches failing for the same
/// reason share a line, so a provider limit hitting five accounts reads as one problem.
pub fn unreadable_notes(report: &CycleReport) -> Vec<String> {
    let mut by_reason: Vec<((&str, &str), Vec<&str>)> = Vec::new();
    for reading in report.unreadable() {
        let key = (reading.id.kind, reading.summary());
        let instance = reading.id.instance.as_str();
        match by_reason.iter_mut().find(|(k, _)| *k == key) {
            Some((_, instances)) => instances.push(instance),
            None => by_reason.push((key, vec![instance])),
        }
    }
    by_reason
        .into_iter()
        .map(|((kind, reason), instances)| format!("{kind} {}: {reason}", instances.join(", ")))
        .collect()
}

fn subgraph_summary(sg: &SourceHealth) -> String {
    match (&sg.fault, sg.indexed_block, sg.blocks_behind) {
        (Some(fault), _, _) => fault.summary(),
        (None, Some(block), Some(behind)) => {
            format!("indexed to block {block}, {behind} blocks behind the chain head")
        }
        (None, Some(block), None) => format!("indexed to block {block}"),
        (None, None, _) => "reachable".to_string(),
    }
}

pub fn summary_line(report: &CycleReport) -> String {
    let (read, unreadable) = report.counts();
    let mut parts = vec![style::dim(&format!("{read} read"))];
    if unreadable > 0 {
        parts.push(style::alarm(&format!("{unreadable} unreadable")));
    }
    let down = report.subgraphs_down().count();
    if down > 0 {
        parts.push(style::alarm(&format!("{down} subgraph(s) down")));
    }
    let pending = report.remediations().count();
    if pending > 0 {
        parts.push(style::action(&format!("{pending} to remediate")));
    }
    parts.join(&style::dim(" · "))
}

fn measurement_json(m: &Measurement) -> Value {
    json!({
        "name": m.name,
        "value": m.value,
        "unit": m.unit.as_str(),
    })
}

pub fn json(report: &CycleReport) -> Value {
    let readings: Vec<Value> = report
        .readings
        .iter()
        .map(|o| {
            json!({
                "check": o.id.kind,
                "instance": o.id.instance,
                "read": if o.is_unreadable() { "failed" } else { "ok" },
                "summary": o.summary(),
                "account": o.labels.account,
                "group": o.labels.group,
                "address": o.labels.address.map(|a| a.to_string()),
                "spender": o.labels.spender,
                "remediation": o.remediation.as_ref().map(|r| json!({
                    "asset": r.asset.as_str(),
                    "below": r.below.to_f64(),
                    "top_up_by": r.top_up_by.to_f64(),
                    "from": r.from,
                })),
                "measurements": o.observation.as_ref().ok()
                    .map(|obs| obs.measurements.iter().map(measurement_json).collect::<Vec<_>>())
                    .unwrap_or_default(),
            })
        })
        .collect();

    let (read, unreadable) = report.counts();

    // Block provenance comes first, and is present in every mode.
    json!({
        "network": report.network,
        "chain_id": report.chain_id,
        "block": report.block.map(|b| b.number),
        "block_timestamp": report.block.map(|b| b.timestamp_rfc3339()),
        "source": {
            "id": report.source.id.as_str(),
            "up": report.source.is_up(),
            "summary": report.source.summary(),
            "lag_seconds": report.source.lag.map(|l| l.as_secs()),
        },
        "counts": { "read": read, "unreadable": unreadable },
        "watches": readings,
        "subgraphs": report.subgraphs.iter().map(|sg| json!({
            "id": sg.id.as_str(),
            "up": sg.is_up(),
            "summary": subgraph_summary(sg),
            "deployment": sg.deployment,
            "indexed_block": sg.indexed_block,
            "blocks_behind": sg.blocks_behind,
            "lag_seconds": sg.lag.map(|l| l.as_secs()),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;
    use graph_monitor_core::rpc::PinnedBlock;
    use graph_monitor_core::source::{SourceFault, SourceId};
    use graph_monitor_core::units::Amount;
    use graph_monitor_core::watch::{Asset, Labels, Observation, RemediationIntent, Unit, WatchId};
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
        let labels = Labels {
            network: "arbitrum-one".into(),
            group: group.map(str::to_string),
            account: Some(account.to_string()),
            address: Some(Address::repeat_byte(0x11)),
            spender: None,
        };
        let mut obs = Observation::new("summary");
        if let Some((name, value, unit)) = measurement {
            obs = obs.with(Measurement {
                name,
                value,
                unit,
                labels: labels.clone(),
            });
        }
        Reading {
            id: WatchId::new(kind, account),
            labels,
            observation: Ok(obs),
            remediation: None,
            duration: Duration::ZERO,
        }
    }

    fn unreadable(kind: &'static str, account: &str) -> Reading {
        let mut o = outcome(kind, account, None, None);
        o.observation = Err("endpoint refused historical state".into());
        o
    }

    fn report(readings: Vec<Reading>) -> CycleReport {
        let head = PinnedBlock {
            number: 505_090_074,
            timestamp: 1_773_253_334,
        };
        CycleReport {
            network: "arbitrum-one".into(),
            chain_id: 42161,
            source: SourceHealth {
                chain_id: Some(42161),
                head: Some(head),
                lag: Some(Duration::from_secs(2)),
                ..SourceHealth::new(SourceId::rpc("arbitrum-one"), "arbitrum-one")
            },
            block: Some(head),
            readings,
            subgraphs: Vec::new(),
            duration: Duration::ZERO,
        }
    }

    fn subgraph(label: &str, indexed: Option<u64>, fault: Option<SourceFault>) -> SourceHealth {
        SourceHealth {
            indexed_block: indexed,
            blocks_behind: indexed.map(|b| 505_090_074 - b),
            lag: indexed.map(|_| Duration::from_secs(45)),
            deployment: indexed.map(|_| "QmDeployment".to_string()),
            fault,
            ..SourceHealth::new(SourceId::subgraph(label), "arbitrum-one")
        }
    }

    #[test]
    fn subgraphs_render_their_lag_and_failures() {
        let mut r = report(vec![]);
        r.subgraphs = vec![
            subgraph("network", Some(505_089_974), None),
            subgraph("network@QmNext", Some(505_000_000), None),
            subgraph(
                "ebo",
                None,
                Some(SourceFault::Unreachable("subgraph not found".into())),
            ),
        ];
        let out = grouped(&r);
        let row = |name: &str| out.lines().find(|l| l.contains(name)).unwrap().to_string();
        assert!(row("network").contains("100 blocks"), "{out}");
        assert!(row("network").contains("45s"), "{out}");
        assert!(row("ebo").contains("subgraph not found"), "{out}");
        // A listed deployment renders under its subgraph, not as a sibling.
        assert!(row("QmNext").contains("└ QmNext"), "{out}");
        assert!(summary_line(&r).contains("1 subgraph(s) down"));

        let flat_out = flat(&r, true);
        assert!(flat_out.contains("subgraph:ebo"), "{flat_out}");
        assert!(!flat_out.contains("subgraph:network"), "{flat_out}");

        let v = json(&r);
        assert_eq!(v["subgraphs"][0]["blocks_behind"], json!(100));
        assert_eq!(v["subgraphs"][2]["up"], json!(false));
    }

    #[test]
    fn unreadable_notes_say_why_and_group_shared_reasons() {
        let mut a = unreadable("activity", "SamOracleGraphOps");
        a.observation = Err("searched 600000 blocks without reaching the lookback".into());
        let mut b = unreadable("activity", "SamOracleSemiotic");
        b.observation = a.observation.clone();
        let c = unreadable("eth_balance", "BillingWallet");
        let notes = unreadable_notes(&report(vec![
            a,
            outcome("eth_balance", "Fine", None, None),
            b,
            c,
        ]));
        assert_eq!(
            notes,
            [
                "activity SamOracleGraphOps, SamOracleSemiotic: searched 600000 blocks without \
                 reaching the lookback",
                "eth_balance BillingWallet: endpoint refused historical state",
            ]
        );
    }

    #[test]
    fn no_subgraphs_means_no_subgraph_section() {
        assert!(!grouped(&report(vec![])).contains("Subgraphs"));
    }

    #[test]
    fn header_states_the_block() {
        let out = grouped(&report(vec![]));
        assert!(out.contains("block 505,090,074"), "{out}");
        assert!(out.contains("arbitrum-one"), "{out}");
        assert!(out.contains("chain 42161"), "{out}");
    }

    #[test]
    fn header_still_names_the_network_when_nothing_could_be_read() {
        let mut r = report(vec![]);
        r.block = None;
        let out = grouped(&r);
        assert!(out.contains("arbitrum-one"), "{out}");
        assert!(out.contains("no block read"), "{out}");
    }

    #[test]
    fn unconfigured_and_unreadable_render_differently() {
        // Blank = not watched. `!` = watched, and the read failed.
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
        // Regression: the grouped renderer matched only balance kinds, so activity watches ran
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
        o.labels.spender = Some("NetworkMonitor".into());
        o.observation = Ok(Observation::new("…")
            .with(Measurement {
                name: M_ALLOWANCE,
                value: 1.157e59,
                unit: Unit::Grt,
                labels: o.labels.clone(),
            })
            .with(Measurement {
                name: M_ALLOWANCE_UNLIMITED,
                value: 1.0,
                unit: Unit::Boolean,
                labels: o.labels.clone(),
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
        assert!(flat(&r, true).contains("Every watch was read successfully"));
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
        assert_eq!(keys[0], "network");
        assert!(
            keys.iter().position(|k| *k == "block").unwrap()
                < keys.iter().position(|k| *k == "watches").unwrap()
        );
        assert_eq!(value["block"], json!(505_090_074u64));
        assert_eq!(value["source"]["up"], json!(true));
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
        assert_eq!(v["watches"][0]["read"], json!("ok"));
        assert_eq!(v["watches"][1]["read"], json!("failed"));
        // A failed read contributes no measurements — never a zero.
        assert_eq!(v["watches"][1]["measurements"].as_array().unwrap().len(), 0);
        assert!(v.get("worst_severity").is_none());
        assert!(v["watches"][0].get("severity").is_none());
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
