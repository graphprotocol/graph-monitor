//! RPC access: chain-id verification, block pinning, batched reads.
//!
//! The one-block-one-truth rule lives here. `Client::pin` resolves whatever the user asked
//! for into a concrete height *once*, and every later read is issued against that number.
//! Nothing in this module accepts a "latest" after pinning.

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{address, Address, Bytes, TxHash, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use alloy::sol_types::SolCall;
use futures::future::join_all;

use crate::error::{Result, RpcError};

/// Multicall3, at the same address on every chain it is deployed to.
pub const CANONICAL_MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

/// Default requests in flight, across every watch sharing this client.
///
/// Low, because the shipped endpoint is a free public one that returns 429 well before a
/// paid provider would. Raise it in config when pointing at something that can take it —
/// activity searches are latency-bound and benefit almost linearly.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 6;

/// Default `eth_getLogs` block range. Conservative on purpose: providers cap this and the cap
/// varies wildly — Chainstack allows 100 blocks on Developer and 10,000 above it, while the
/// public Arbitrum endpoint served 345,000. Starting low and learning upward is safe; starting
/// high means every first request fails.
pub const DEFAULT_LOG_RANGE: u64 = 10_000;

/// Sub-calls per `aggregate3`. Large batches get rejected by providers with request-size
/// limits, so the batch is chunked rather than sent whole.
const MAX_CALLS_PER_BATCH: usize = 250;

sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }
}

/// What the user asked for on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSpec {
    Latest,
    Number(u64),
}

impl std::str::FromStr for BlockSpec {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("latest") {
            return Ok(BlockSpec::Latest);
        }
        let cleaned = s.replace('_', "");
        let parsed = if let Some(hex) = cleaned.strip_prefix("0x") {
            u64::from_str_radix(hex, 16)
        } else {
            cleaned.parse::<u64>()
        };
        parsed
            .map(BlockSpec::Number)
            .map_err(|_| format!("expected a block number or 'latest', got {s:?}"))
    }
}

/// A concrete height, with the timestamp that goes in every readout header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedBlock {
    pub number: u64,
    pub timestamp: u64,
}

impl PinnedBlock {
    pub fn block_id(&self) -> BlockId {
        BlockId::Number(BlockNumberOrTag::Number(self.number))
    }

    /// UTC, formatted for the readout header.
    pub fn timestamp_utc(&self) -> String {
        match chrono::DateTime::from_timestamp(self.timestamp as i64, 0) {
            Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            None => format!("timestamp {}", self.timestamp),
        }
    }

    /// RFC 3339, for `--json`.
    pub fn timestamp_rfc3339(&self) -> String {
        match chrono::DateTime::from_timestamp(self.timestamp as i64, 0) {
            Some(dt) => dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            None => self.timestamp.to_string(),
        }
    }
}

/// One sub-call in a batch.
#[derive(Debug, Clone)]
pub struct BatchCall {
    pub target: Address,
    pub calldata: Bytes,
}

/// A sub-call either returned data or reverted. A single failure must not void the readout,
/// so failures are values rather than errors.
#[derive(Debug, Clone)]
pub enum CallOutcome {
    Ok(Bytes),
    Failed,
}

impl CallOutcome {
    /// Decode a successful call, treating a decode failure as a failed read: returning a
    /// misparsed number would be worse than admitting the cell could not be read.
    pub fn decode<C: SolCall>(&self) -> Option<C::Return> {
        match self {
            CallOutcome::Ok(data) => C::abi_decode_returns(data).ok(),
            CallOutcome::Failed => None,
        }
    }
}

pub struct Client {
    provider: alloy::providers::DynProvider,
    multicall3: Address,
    /// Caps requests in flight. Watches run concurrently and some — the activity search
    /// especially — issue many small calls, which is enough to earn a 429 from a public
    /// endpoint. Throttling here beats every call site remembering to be polite.
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    /// Largest `eth_getLogs` span this endpoint has accepted. Shared across watches so the
    /// provider's limit is discovered once per process rather than per search.
    log_range: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Client {
    pub fn new(
        rpc_url: &str,
        multicall3: Address,
        max_in_flight: usize,
        log_range: u64,
    ) -> Result<Self> {
        let url = rpc_url
            .parse::<alloy::transports::http::reqwest::Url>()
            .map_err(|e| RpcError::InvalidUrl {
                url: rpc_url.to_string(),
                reason: e.to_string(),
            })?;

        let provider = ProviderBuilder::new().connect_http(url).erased();

        Ok(Self {
            provider,
            multicall3,
            permits: std::sync::Arc::new(tokio::sync::Semaphore::new(max_in_flight.max(1))),
            log_range: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(log_range.max(1))),
        })
    }

    /// Wait for a slot. The permit releases when the caller's guard drops.
    async fn permit(&self) -> tokio::sync::SemaphorePermit<'_> {
        self.permits
            .acquire()
            .await
            .expect("semaphore is never closed")
    }

    /// The chain id the endpoint reports.
    ///
    /// Comparing it against config is stage 0's job, not this layer's.
    pub async fn chain_id(&self) -> Result<u64> {
        let _permit = self.permit().await;
        self.provider
            .get_chain_id()
            .await
            .map_err(|e| RpcError::Transport(e.to_string()).into())
    }

    /// Resolve a block spec to a concrete height, once.
    pub async fn pin(&self, spec: BlockSpec) -> Result<PinnedBlock> {
        let _permit = self.permit().await;
        let tag = match spec {
            BlockSpec::Latest => BlockNumberOrTag::Latest,
            BlockSpec::Number(n) => BlockNumberOrTag::Number(n),
        };

        let block = self
            .provider
            .get_block_by_number(tag)
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;

        match block {
            Some(block) => Ok(PinnedBlock {
                number: block.header.number,
                timestamp: block.header.timestamp,
            }),
            None => match spec {
                BlockSpec::Number(n) => Err(RpcError::BlockUnavailable { number: n }.into()),
                BlockSpec::Latest => Err(RpcError::NoLatestBlock.into()),
            },
        }
    }

    /// Largest log span known to work on this endpoint.
    pub fn log_range(&self) -> u64 {
        self.log_range.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Halve the accepted span after the endpoint rejected one for being too wide.
    ///
    /// Learned rather than configured, because the caller cannot know a provider's limit and
    /// getting it wrong means every request fails. Returns the new span.
    pub fn shrink_log_range(&self) -> u64 {
        let next = (self.log_range() / 2).max(1);
        self.log_range
            .store(next, std::sync::atomic::Ordering::Relaxed);
        tracing::debug!(
            range = next,
            "narrowed log range after a provider rejection"
        );
        next
    }

    /// Does this error mean the requested block span was too wide?
    pub fn is_range_error(error: &str) -> bool {
        let e = error.to_ascii_lowercase();
        e.contains("range limit")
            || e.contains("block range")
            || e.contains("range is too large")
            || e.contains("query returned more than")
            || e.contains("exceed maximum block range")
    }

    /// Logs from one contract matching one event signature, over a block range.
    ///
    /// Unlike a historical state read, this works on a pruned node: endpoints generally retain
    /// receipts and logs far longer than state.
    pub async fn logs(
        &self,
        emitter: Address,
        topic0: B256,
        from: u64,
        to: u64,
    ) -> Result<Vec<alloy::rpc::types::Log>> {
        let _permit = self.permit().await;
        let filter = alloy::rpc::types::Filter::new()
            .address(emitter)
            .event_signature(topic0)
            .from_block(from)
            .to_block(to);
        self.provider
            .get_logs(&filter)
            .await
            .map_err(|e| RpcError::Transport(e.to_string()).into())
    }

    /// Sender and callee of a transaction.
    ///
    /// A log carries neither, so attributing activity to an account means one extra lookup per
    /// candidate. `to` is `None` for contract creation. Returns `None` when the endpoint does
    /// not know the transaction.
    pub async fn tx_parties(&self, hash: TxHash) -> Result<Option<(Address, Option<Address>)>> {
        let _permit = self.permit().await;
        use alloy::consensus::Transaction as _;
        let tx = self
            .provider
            .get_transaction_by_hash(hash)
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        Ok(tx.map(|t| (t.inner.signer(), t.to())))
    }

    /// Outbound transaction count at a specific height.
    ///
    /// A historical nonce is a *state* read, so this needs an archive endpoint for anything
    /// but the head — the same constraint that makes the activity check archive-only.
    pub async fn nonce_at(&self, address: Address, block: u64) -> Result<u64> {
        let _permit = self.permit().await;
        self.provider
            .get_transaction_count(address)
            .block_id(BlockId::Number(BlockNumberOrTag::Number(block)))
            .await
            .map_err(|e| RpcError::Transport(e.to_string()).into())
    }

    /// ETH balances at the pinned block. `eth_getBalance` is not a contract call, so it cannot
    /// ride in the multicall; these are issued concurrently instead.
    ///
    /// Errors are returned rather than logged and discarded: the caller needs the text to tell
    /// "this one address is odd" from "this endpoint cannot serve this block at all".
    pub async fn eth_balances(
        &self,
        addresses: &[Address],
        block: PinnedBlock,
    ) -> Vec<std::result::Result<U256, String>> {
        let futures = addresses.iter().map(|addr| {
            let provider = &self.provider;
            let addr = *addr;
            async move {
                provider
                    .get_balance(addr)
                    .block_id(block.block_id())
                    .await
                    .map_err(|e| e.to_string())
            }
        });
        join_all(futures).await
    }

    /// Batch contract reads through Multicall3 at the pinned block.
    pub async fn multicall(
        &self,
        calls: &[BatchCall],
        block: PinnedBlock,
    ) -> Result<Vec<CallOutcome>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }

        let contract = IMulticall3::new(self.multicall3, &self.provider);
        let mut outcomes = Vec::with_capacity(calls.len());

        for chunk in calls.chunks(MAX_CALLS_PER_BATCH) {
            let payload: Vec<IMulticall3::Call3> = chunk
                .iter()
                .map(|c| IMulticall3::Call3 {
                    target: c.target,
                    allowFailure: true,
                    callData: c.calldata.clone(),
                })
                .collect();

            let results = contract
                .aggregate3(payload)
                .block(block.block_id())
                .call()
                .await
                .map_err(|e| RpcError::Transport(e.to_string()))?;

            for r in results {
                outcomes.push(if r.success {
                    CallOutcome::Ok(r.returnData)
                } else {
                    CallOutcome::Failed
                });
            }
        }

        Ok(outcomes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_block_specs() {
        assert_eq!("latest".parse::<BlockSpec>().unwrap(), BlockSpec::Latest);
        assert_eq!("LATEST".parse::<BlockSpec>().unwrap(), BlockSpec::Latest);
        assert_eq!(
            "285431902".parse::<BlockSpec>().unwrap(),
            BlockSpec::Number(285_431_902)
        );
        assert_eq!(
            "285_431_902".parse::<BlockSpec>().unwrap(),
            BlockSpec::Number(285_431_902)
        );
        assert_eq!("0x10".parse::<BlockSpec>().unwrap(), BlockSpec::Number(16));
        assert!("tomorrow".parse::<BlockSpec>().is_err());
        assert!("-5".parse::<BlockSpec>().is_err());
    }

    #[test]
    fn formats_block_timestamps() {
        let block = PinnedBlock {
            number: 285_431_902,
            timestamp: 1_773_253_334,
        };
        assert_eq!(block.timestamp_utc(), "2026-03-11 18:22:14 UTC");
        assert_eq!(block.timestamp_rfc3339(), "2026-03-11T18:22:14Z");
    }

    #[test]
    fn pinned_block_yields_a_numeric_block_id() {
        // Never a tag: a numeric id is what makes two reads comparable.
        let block = PinnedBlock {
            number: 100,
            timestamp: 0,
        };
        assert_eq!(
            block.block_id(),
            BlockId::Number(BlockNumberOrTag::Number(100))
        );
    }

    #[test]
    fn rejects_malformed_rpc_urls() {
        let err = match Client::new(
            "not a url",
            CANONICAL_MULTICALL3,
            DEFAULT_MAX_IN_FLIGHT,
            DEFAULT_LOG_RANGE,
        ) {
            Ok(_) => panic!("expected a malformed url to be rejected"),
            Err(e) => e,
        };
        assert_eq!(err.exit_code(), 3);
    }
}
