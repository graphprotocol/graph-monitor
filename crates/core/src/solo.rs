//! Solo deployments: a check about the network, not any one participant.
//!
//! A deployment is solo when exactly one indexer has an open allocation on it, it has curation
//! signal, and it is not denied rewards. The network-monitoring subgraph keeps the count per
//! indexer (`Indexer.soloCount`), so this check is one query against the network's subgraph
//! source. It reports one measurement per indexer, each with that indexer as its subject.
//!
//! No indexer is treated specially here. Hiding one, such as the upgrade indexer, is a choice
//! for whoever reads the metrics.

use std::str::FromStr;
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use alloy::transports::http::reqwest;
use serde_json::Value;

use crate::check::{CheckId, Measurement, Observation, Reading, Subject, Unit, Unreadable};
use crate::subgraph::Endpoint;

pub const KIND: &str = "solo_deployments";
/// Deployments where the indexer is the only one allocated, with signal, not denied.
pub const M_SOLO_DEPLOYMENTS: &str = "indexer_solo_deployments";
/// GRT signalled on those deployments.
pub const M_SOLO_SIGNAL: &str = "indexer_solo_signal_grt";

/// Indexers with at least one solo deployment. Far fewer than 1000 on any network today.
const QUERY: &str =
    "{ indexers(first: 1000, orderBy: id, where: { soloCount_gt: 0 }) { id soloCount soloSignalledTokens } }";

/// One indexer's solo figures.
#[derive(Debug, Clone, PartialEq)]
pub struct Solo {
    pub indexer: Address,
    pub deployments: u64,
    pub signal_grt: f64,
}

/// Run the check. `source` is the network's verified subgraph source, or `None` when it has
/// none or it failed verification; then the check is unreadable, never zero.
pub async fn run(
    network: &str,
    http: &reqwest::Client,
    source: Option<&Endpoint>,
    source_name: String,
) -> Reading {
    let started = Instant::now();
    let observation = match source {
        None => Err(Unreadable::new(
            "source_unavailable",
            format!("source {source_name} is unavailable"),
        )),
        Some(endpoint) => {
            match crate::subgraph::query_data(
                http,
                endpoint,
                &serde_json::json!({ "query": QUERY }),
            )
            .await
            .and_then(|data| parse(&data))
            {
                Ok(rows) => Ok(observe(&rows)),
                Err(reason) if reason.contains("no field `indexers`") => {
                    Err(Unreadable::read_failed(format!(
                        "{source_name} does not track solo deployments yet ({reason})"
                    )))
                }
                Err(reason) => Err(Unreadable::read_failed(reason)),
            }
        }
    };
    reading(network, observation, started.elapsed())
}

pub fn reading(
    network: &str,
    observation: Result<Observation, Unreadable>,
    duration: Duration,
) -> Reading {
    Reading {
        id: CheckId::new(KIND, network),
        network: network.to_string(),
        subject: Subject::Network,
        observation,
        remediation: None,
        duration,
    }
}

/// Measurements and a sentence. Pure.
pub fn observe(rows: &[Solo]) -> Observation {
    let total: u64 = rows.iter().map(|r| r.deployments).sum();
    let mut observation = Observation::new(format!(
        "{total} solo deployments across {} indexers",
        rows.len()
    ));
    for row in rows {
        let subject = Subject::Indexer {
            indexer: row.indexer,
        };
        observation = observation
            .with(Measurement {
                name: M_SOLO_DEPLOYMENTS,
                value: row.deployments as f64,
                unit: Unit::Count,
                subject: subject.clone(),
            })
            .with(Measurement {
                name: M_SOLO_SIGNAL,
                value: row.signal_grt,
                unit: Unit::Grt,
                subject,
            });
    }
    observation
}

/// Parse the `indexers` response. `soloSignalledTokens` is a BigInt in wei, as a string.
fn parse(data: &Value) -> Result<Vec<Solo>, String> {
    let rows = data
        .get("indexers")
        .and_then(Value::as_array)
        .ok_or_else(|| "response has no indexers".to_string())?;
    rows.iter()
        .map(|row| {
            let id = row
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "indexer id is missing".to_string())?;
            let indexer =
                Address::from_str(id).map_err(|_| format!("indexer id {id} is not an address"))?;
            let deployments = row
                .get("soloCount")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .ok_or_else(|| "soloCount is missing".to_string())?;
            let signal_wei = row
                .get("soloSignalledTokens")
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .ok_or_else(|| "soloSignalledTokens is missing".to_string())?;
            let signal_grt = alloy::primitives::U256::from_str(&signal_wei)
                .map(|wei| crate::units::Amount::new(wei, 18).to_f64())
                .map_err(|_| format!("soloSignalledTokens {signal_wei} is not a number"))?;
            Ok(Solo {
                indexer,
                deployments,
                signal_grt,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_indexers_and_converts_wei() {
        let data = serde_json::json!({ "indexers": [
            { "id": "0x0a015d9e17d3978164486473a2ddbb654418db07", "soloCount": 420,
              "soloSignalledTokens": "12426000000000000000000" },
        ]});
        let rows = parse(&data).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].deployments, 420);
        assert!((rows[0].signal_grt - 12_426.0).abs() < 1e-6);
    }

    #[test]
    fn each_indexer_is_its_own_subject() {
        let rows = vec![
            Solo {
                indexer: Address::repeat_byte(1),
                deployments: 3,
                signal_grt: 10.0,
            },
            Solo {
                indexer: Address::repeat_byte(2),
                deployments: 5,
                signal_grt: 20.0,
            },
        ];
        let obs = observe(&rows);
        assert_eq!(obs.summary, "8 solo deployments across 2 indexers");
        let subjects: Vec<String> = obs
            .measurements
            .iter()
            .filter(|m| m.name == M_SOLO_DEPLOYMENTS)
            .map(|m| m.subject.name())
            .collect();
        assert_eq!(
            subjects,
            [
                format!("{:#x}", Address::repeat_byte(1)),
                format!("{:#x}", Address::repeat_byte(2))
            ]
        );
    }

    #[test]
    fn a_schema_without_solo_tracking_is_an_error_not_zero() {
        assert!(parse(&serde_json::json!({})).is_err());
    }
}
