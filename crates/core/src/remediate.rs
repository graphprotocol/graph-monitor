//! Stage 3: performing the repairs the checks asked for.
//!
//! This is the only part of the tool that writes. Everything here is off by default and needs
//! both an explicit flag and a configured signer, because the read path is what makes the tool
//! safe to hand to anyone.
//!
//! The operational rules are lifted from `network-monitor`, where they were learned the hard
//! way. They live in this one executor rather than in each check, so they are enforced by
//! construction instead of by code review.

use std::time::Duration;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;

use crate::error::{Result, RpcError};
use crate::watch::{Asset, Reading, RemediationIntent};

sol! {
    #[sol(rpc)]
    interface IERC20Transfer {
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
    }
}

/// Arbitrum prices the L1 calldata posting cost as extra L2 gas, derived from a *moving* L1
/// base-fee estimate. A limit that was exact at estimation time gets rejected at broadcast as
/// `intrinsic gas too low` the moment that estimate ticks up — which surfaces as a spurious
/// remediation failure. The limit is only a ceiling (transactions pay for gas used), so
/// doubling it costs nothing.
const GAS_LIMIT_MULTIPLIER: u64 = 2;

/// How long to wait for a receipt before giving up on it.
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemediationOutcome {
    /// Broadcast and mined.
    Sent { tx: TxHash },
    /// `--dry-run`: the plan was formed but nothing was sent.
    WouldSend { plan: String },
    /// Remediation was configured but could not run — no signer, usually.
    Skipped { reason: String },
    /// The transaction failed or reverted. This outranks the breach that prompted it: an
    /// auto-fix that is broken is more urgent than the condition it was meant to fix.
    Failed { reason: String },
}

impl RemediationOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            RemediationOutcome::Sent { .. } => "sent",
            RemediationOutcome::WouldSend { .. } => "would_send",
            RemediationOutcome::Skipped { .. } => "skipped",
            RemediationOutcome::Failed { .. } => "failed",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            RemediationOutcome::Sent { tx } => format!("sent {tx}"),
            RemediationOutcome::WouldSend { plan } => format!("dry run: would {plan}"),
            RemediationOutcome::Skipped { reason } => format!("skipped: {reason}"),
            RemediationOutcome::Failed { reason } => format!("FAILED: {reason}"),
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, RemediationOutcome::Failed { .. })
    }
}

/// One repair, performed.
#[derive(Debug, Clone)]
pub struct RemediationReport {
    pub account: String,
    pub asset: Asset,
    pub outcome: RemediationOutcome,
}

/// Sends the transactions. One per network, holding the signer.
pub struct Executor {
    signer_address: Address,
    provider: alloy::providers::DynProvider,
    token: Address,
    dry_run: bool,
}

impl Executor {
    pub fn new(rpc_url: &str, key: &str, token: Address, dry_run: bool) -> Result<Self> {
        let signer: PrivateKeySigner = key
            .parse()
            // Deliberately not including the parse error: some of them echo the input.
            .map_err(|_| RpcError::Transport("signer key is not a valid private key".into()))?;
        let signer_address = signer.address();

        let url = rpc_url
            .parse::<alloy::transports::http::reqwest::Url>()
            .map_err(|e| RpcError::InvalidUrl {
                url: rpc_url.to_string(),
                reason: e.to_string(),
            })?;

        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_http(url)
            .erased();

        Ok(Self {
            signer_address,
            provider,
            token,
            dry_run,
        })
    }

    pub fn signer_address(&self) -> Address {
        self.signer_address
    }

    /// Perform every requested repair, **one at a time**.
    ///
    /// Serial by construction, not by convention: one signer sending concurrently races its
    /// own nonce, and the failure mode is a stuck transaction rather than a clean error. This
    /// is why remediation runs after all judging rather than inside the per-check loop.
    pub async fn run(&self, readings: &[Reading]) -> Vec<RemediationReport> {
        let mut reports = Vec::new();
        for reading in readings {
            let Some(intent) = &reading.remediation else {
                continue;
            };
            let account = reading.labels.account.clone().unwrap_or_default();
            let Some(to) = reading.labels.address else {
                reports.push(RemediationReport {
                    account,
                    asset: intent.asset,
                    outcome: RemediationOutcome::Skipped {
                        reason: "no address on the check".into(),
                    },
                });
                continue;
            };

            let result = self.perform(to, intent).await;
            if result.is_failure() {
                tracing::error!(account = %account, outcome = %result.describe(), "remediation failed");
            } else {
                tracing::info!(account = %account, outcome = %result.describe(), "remediation");
            }
            reports.push(RemediationReport {
                account,
                asset: intent.asset,
                outcome: result,
            });
        }
        reports
    }

    async fn perform(&self, to: Address, intent: &RemediationIntent) -> RemediationOutcome {
        let amount = intent.top_up_by.raw;

        let request = match intent.asset {
            Asset::Eth => TransactionRequest::default().with_to(to).with_value(amount),
            Asset::Grt => {
                // With a funder, the signer pulls from it — which needs an allowance the
                // signer already holds. Without one, the signer spends its own balance.
                let calldata = match &intent.from {
                    Some(_) => {
                        let Some(from) = self.funder_address(intent) else {
                            return RemediationOutcome::Skipped {
                                reason: "funder address could not be resolved".into(),
                            };
                        };
                        IERC20Transfer::transferFromCall { from, to, amount }.abi_encode()
                    }
                    None => IERC20Transfer::transferCall { to, amount }.abi_encode(),
                };
                TransactionRequest::default()
                    .with_to(self.token)
                    .with_input(calldata)
            }
        };

        if self.dry_run {
            return RemediationOutcome::WouldSend {
                plan: intent.describe(),
            };
        }

        self.broadcast(request).await
    }

    /// The funder's address, carried on the intent as a display name only, so it has to be
    /// resolved by the caller. Returning `None` skips rather than guessing.
    fn funder_address(&self, intent: &RemediationIntent) -> Option<Address> {
        intent.from_address
    }

    async fn broadcast(&self, request: TransactionRequest) -> RemediationOutcome {
        // Estimate, then pad. Never the unpadded estimate — see GAS_LIMIT_MULTIPLIER.
        let estimate = match self.provider.estimate_gas(request.clone()).await {
            Ok(g) => g,
            Err(e) => {
                return RemediationOutcome::Failed {
                    reason: format!("gas estimation failed: {e}"),
                }
            }
        };
        let request = request.with_gas_limit(estimate.saturating_mul(GAS_LIMIT_MULTIPLIER));

        let pending = match self.provider.send_transaction(request).await {
            Ok(p) => p,
            Err(e) => {
                return RemediationOutcome::Failed {
                    reason: format!("broadcast failed: {e}"),
                }
            }
        };

        let tx = *pending.tx_hash();
        match tokio::time::timeout(RECEIPT_TIMEOUT, pending.get_receipt()).await {
            Ok(Ok(receipt)) if receipt.status() => RemediationOutcome::Sent { tx },
            // Mined but reverted. The transaction "succeeded" at the RPC layer and still did
            // nothing, which is exactly the case a naive implementation reports as success.
            Ok(Ok(_)) => RemediationOutcome::Failed {
                reason: format!("transaction {tx} reverted"),
            },
            Ok(Err(e)) => RemediationOutcome::Failed {
                reason: format!("receipt for {tx} failed: {e}"),
            },
            Err(_) => RemediationOutcome::Failed {
                reason: format!("timed out waiting for receipt of {tx}"),
            },
        }
    }
}

/// Amount helper for tests and callers that need a raw value.
pub fn raw(amount: U256) -> U256 {
    amount
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reverted_transaction_is_a_failure_not_a_success() {
        // The RPC call succeeded; the transaction did nothing. Reporting this as sent is how
        // an auto-fix silently stops working.
        let failed = RemediationOutcome::Failed {
            reason: "transaction 0xabc reverted".into(),
        };
        assert!(failed.is_failure());
        assert!(failed.describe().starts_with("FAILED"));
    }

    #[test]
    fn dry_run_and_skipped_are_not_failures() {
        assert!(!RemediationOutcome::WouldSend {
            plan: "top up 1 ETH".into()
        }
        .is_failure());
        assert!(!RemediationOutcome::Skipped {
            reason: "no signer configured".into()
        }
        .is_failure());
    }

    #[test]
    fn outcomes_have_stable_metric_labels() {
        // These strings become a Prometheus label value; changing one silently breaks a panel.
        assert_eq!(
            RemediationOutcome::Sent { tx: TxHash::ZERO }.as_str(),
            "sent"
        );
        assert_eq!(
            RemediationOutcome::WouldSend {
                plan: String::new()
            }
            .as_str(),
            "would_send"
        );
        assert_eq!(
            RemediationOutcome::Skipped {
                reason: String::new()
            }
            .as_str(),
            "skipped"
        );
        assert_eq!(
            RemediationOutcome::Failed {
                reason: String::new()
            }
            .as_str(),
            "failed"
        );
    }

    #[test]
    fn a_bad_key_is_rejected_without_echoing_it() {
        let msg = match Executor::new("http://localhost:8545", "not-a-key", Address::ZERO, true) {
            Ok(_) => panic!("a malformed key must be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(!msg.contains("not-a-key"), "key must never appear: {msg}");
    }
}
