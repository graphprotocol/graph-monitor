//! The read stage: batched reads at one pinned block.
//!
//! Checks declare what they need, the engine fetches it in as few round trips as possible, and
//! checks then read from the result as if it were free.

use std::collections::{BTreeSet, HashMap};

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

use crate::rpc::{BatchCall, Client, PinnedBlock};

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function decimals() external view returns (uint8);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

/// ETH is 18 decimals on every EVM chain; unlike a token's, this is not something to read.
pub const ETH_DECIMALS: u8 = 18;

/// What a set of checks needs read. Deduplicated by construction: two checks watching the same
/// wallet cost one call.
#[derive(Debug, Default, Clone)]
pub struct ReadPlan {
    eth_balances: BTreeSet<Address>,
    decimals: BTreeSet<Address>,
    token_balances: BTreeSet<(Address, Address)>,
    allowances: BTreeSet<(Address, Address, Address)>,
}

impl ReadPlan {
    pub fn eth_balance(&mut self, holder: Address) {
        self.eth_balances.insert(holder);
    }

    pub fn token_balance(&mut self, token: Address, holder: Address) {
        self.decimals.insert(token);
        self.token_balances.insert((token, holder));
    }

    pub fn allowance(&mut self, token: Address, owner: Address, spender: Address) {
        self.decimals.insert(token);
        self.allowances.insert((token, owner, spender));
    }

    pub fn has_eth_balance(&self, holder: Address) -> bool {
        self.eth_balances.contains(&holder)
    }

    pub fn has_decimals(&self, token: Address) -> bool {
        self.decimals.contains(&token)
    }

    pub fn has_token_balance(&self, token: Address, holder: Address) -> bool {
        self.token_balances.contains(&(token, holder))
    }

    pub fn has_allowance(&self, token: Address, owner: Address, spender: Address) -> bool {
        self.allowances.contains(&(token, owner, spender))
    }

    pub fn is_empty(&self) -> bool {
        self.eth_balances.is_empty() && self.token_balances.is_empty() && self.allowances.is_empty()
    }

    /// Total reads, for logging how much the batch saved.
    pub fn len(&self) -> usize {
        self.eth_balances.len()
            + self.decimals.len()
            + self.token_balances.len()
            + self.allowances.len()
    }
}

/// Reads available to a check, all taken at `block`.
pub struct ReadCtx<'a> {
    client: &'a Client,
    block: PinnedBlock,
    eth_balances: HashMap<Address, U256>,
    decimals: HashMap<Address, u8>,
    token_balances: HashMap<(Address, Address), U256>,
    allowances: HashMap<(Address, Address, Address), U256>,
    /// What the prefetch attempted.
    planned: ReadPlan,
    /// Set when failures look like the endpoint cannot serve state at this block at all,
    /// rather than one address being odd.
    state_fault: Option<String>,
}

impl<'a> ReadCtx<'a> {
    /// Execute a plan: one concurrent sweep of `eth_getBalance`, one Multicall3 batch for
    /// everything else. Failed individual reads are simply absent from the cache, and are
    /// retried live if a check asks for them.
    pub async fn prefetch(client: &'a Client, block: PinnedBlock, plan: &ReadPlan) -> Self {
        let mut ctx = Self {
            client,
            block,
            eth_balances: HashMap::new(),
            decimals: HashMap::new(),
            token_balances: HashMap::new(),
            allowances: HashMap::new(),
            planned: plan.clone(),
            state_fault: None,
        };

        // Call layout is fixed so flat results map back without bookkeeping.
        let decimals: Vec<Address> = plan.decimals.iter().copied().collect();
        let token_balances: Vec<(Address, Address)> = plan.token_balances.iter().copied().collect();
        let allowances: Vec<(Address, Address, Address)> =
            plan.allowances.iter().copied().collect();

        let mut calls: Vec<BatchCall> = Vec::with_capacity(plan.len());
        for token in &decimals {
            calls.push(BatchCall {
                target: *token,
                calldata: Bytes::from(IERC20::decimalsCall {}.abi_encode()),
            });
        }
        for (token, holder) in &token_balances {
            calls.push(BatchCall {
                target: *token,
                calldata: Bytes::from(IERC20::balanceOfCall { account: *holder }.abi_encode()),
            });
        }
        for (token, owner, spender) in &allowances {
            calls.push(BatchCall {
                target: *token,
                calldata: Bytes::from(
                    IERC20::allowanceCall {
                        owner: *owner,
                        spender: *spender,
                    }
                    .abi_encode(),
                ),
            });
        }

        let eth_holders: Vec<Address> = plan.eth_balances.iter().copied().collect();

        // Both halves read the same pinned block, so they are safe to run together.
        let (contract_results, eth_results) = futures::future::join(
            client.multicall(&calls, block),
            client.eth_balances(&eth_holders, block),
        )
        .await;

        for (holder, outcome) in eth_holders.iter().zip(eth_results) {
            match outcome {
                Ok(value) => {
                    ctx.eth_balances.insert(*holder, value);
                }
                Err(e) => {
                    if is_state_unavailable(&e) {
                        ctx.state_fault.get_or_insert(e.clone());
                    }
                    tracing::debug!(address = %holder, error = %e, "eth_getBalance failed");
                }
            }
        }

        let contract_results = match contract_results {
            Ok(results) => results,
            Err(e) => {
                // The whole batch failed. Every planned contract read will report its own
                // failure; if the cause looks systemic the engine turns that into one clear
                // source fault instead of a table of unreadable cells.
                let text = e.to_string();
                if is_state_unavailable(&text) {
                    ctx.state_fault.get_or_insert(text.clone());
                }
                tracing::debug!(error = %text, "multicall batch failed");
                return ctx;
            }
        };

        let mut cursor = 0;
        for token in &decimals {
            if let Some(v) = contract_results
                .get(cursor)
                .and_then(|r| r.decode::<IERC20::decimalsCall>())
            {
                ctx.decimals.insert(*token, v);
            }
            cursor += 1;
        }
        for key in &token_balances {
            if let Some(v) = contract_results
                .get(cursor)
                .and_then(|r| r.decode::<IERC20::balanceOfCall>())
            {
                ctx.token_balances.insert(*key, v);
            }
            cursor += 1;
        }
        for key in &allowances {
            if let Some(v) = contract_results
                .get(cursor)
                .and_then(|r| r.decode::<IERC20::allowanceCall>())
            {
                ctx.allowances.insert(*key, v);
            }
            cursor += 1;
        }

        ctx
    }

    pub fn block(&self) -> PinnedBlock {
        self.block
    }

    /// A failure that looks like the endpoint cannot serve state at this block at all.
    pub fn state_fault(&self) -> Option<&str> {
        self.state_fault.as_deref()
    }

    pub async fn eth_balance(&self, holder: Address) -> Result<U256, String> {
        if let Some(v) = self.eth_balances.get(&holder) {
            return Ok(*v);
        }
        if self.planned.has_eth_balance(holder) {
            return Err(self.planned_read_failed(&format!("ETH balance of {holder}")));
        }
        self.client
            .eth_balances(&[holder], self.block)
            .await
            .into_iter()
            .next()
            .unwrap_or_else(|| Err("no response".to_string()))
            .map_err(|e| format!("could not read ETH balance of {holder}: {e}"))
    }

    /// A read the prefetch already attempted and failed. Retrying here would make the result
    /// depend on which attempt happened to succeed.
    fn planned_read_failed(&self, what: &str) -> String {
        match &self.state_fault {
            Some(reason) => format!(
                "could not read {what} at block {}: {reason}",
                self.block.number
            ),
            None => format!("could not read {what} at block {}", self.block.number),
        }
    }

    pub async fn token_decimals(&self, token: Address) -> Result<u8, String> {
        if let Some(v) = self.decimals.get(&token) {
            return Ok(*v);
        }
        if self.planned.has_decimals(token) {
            return Err(self.planned_read_failed(&format!("decimals() of token {token}")));
        }
        self.live_call::<IERC20::decimalsCall>(token, IERC20::decimalsCall {}.abi_encode())
            .await
            .ok_or_else(|| format!("could not read decimals() of token {token}"))
    }

    pub async fn token_balance(&self, token: Address, holder: Address) -> Result<U256, String> {
        if let Some(v) = self.token_balances.get(&(token, holder)) {
            return Ok(*v);
        }
        if self.planned.has_token_balance(token, holder) {
            return Err(self.planned_read_failed(&format!("GRT balance of {holder}")));
        }
        self.live_call::<IERC20::balanceOfCall>(
            token,
            IERC20::balanceOfCall { account: holder }.abi_encode(),
        )
        .await
        .ok_or_else(|| format!("could not read balanceOf({holder}) on token {token}"))
    }

    pub async fn allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256, String> {
        if let Some(v) = self.allowances.get(&(token, owner, spender)) {
            return Ok(*v);
        }
        if self.planned.has_allowance(token, owner, spender) {
            return Err(self.planned_read_failed(&format!("allowance({owner}, {spender})")));
        }
        self.live_call::<IERC20::allowanceCall>(
            token,
            IERC20::allowanceCall { owner, spender }.abi_encode(),
        )
        .await
        .ok_or_else(|| format!("could not read allowance({owner}, {spender}) on token {token}"))
    }

    /// The block of this account's most recent outbound transaction, searched at the pinned
    /// head. Not batchable — the search is adaptive — and archive-only.
    pub async fn last_activity(
        &self,
        address: Address,
    ) -> Result<crate::activity::Activity, String> {
        crate::activity::find_last_activity(
            self.client,
            address,
            self.block.number,
            self.block.timestamp,
        )
        .await
    }

    /// The most recent matching event involving this account, searched back from the pinned
    /// block. Not batchable, and unlike the nonce search it needs no archive endpoint.
    pub async fn last_event(
        &self,
        address: Address,
        spec: &crate::config::ActivitySpec,
    ) -> Result<crate::activity::Activity, String> {
        crate::activity::find_last_event(
            self.client,
            address,
            spec.emitter.address,
            spec.topic0,
            spec.matches,
            self.block.number,
            self.block.timestamp,
            spec.lookback.as_secs(),
        )
        .await
    }

    async fn live_call<C: SolCall>(&self, target: Address, calldata: Vec<u8>) -> Option<C::Return> {
        let call = BatchCall {
            target,
            calldata: Bytes::from(calldata),
        };
        self.client
            .multicall(&[call], self.block)
            .await
            .ok()?
            .first()?
            .decode::<C>()
    }
}

/// Does this error mean "this endpoint has no state at this block"?
///
/// Node implementations disagree on wording and none of them use a distinct error code, so
/// this is substring matching — unpleasant, but the alternative is treating a whole-endpoint
/// outage as a scatter of unrelated per-cell failures.
fn is_state_unavailable(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "historical state",
        "missing trie node",
        "state not available",
        "state is not available",
        "metadata is not found",
        "header not found",
        "no historical rpc",
    ]
    .iter()
    .any(|needle| e.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_deduplicates_reads() {
        let token = Address::repeat_byte(1);
        let holder = Address::repeat_byte(2);

        let mut plan = ReadPlan::default();
        plan.token_balance(token, holder);
        plan.token_balance(token, holder);
        plan.eth_balance(holder);
        plan.eth_balance(holder);

        // Two wallets' worth of requests collapse to one balance + one decimals + one ETH.
        assert_eq!(plan.len(), 3);
    }

    #[test]
    fn requesting_a_token_read_implies_reading_its_decimals() {
        let token = Address::repeat_byte(1);
        let mut plan = ReadPlan::default();
        plan.allowance(token, Address::repeat_byte(2), Address::repeat_byte(3));
        assert_eq!(plan.decimals.len(), 1);
    }

    #[test]
    fn empty_plan_is_empty() {
        assert!(ReadPlan::default().is_empty());
        let mut plan = ReadPlan::default();
        plan.eth_balance(Address::ZERO);
        assert!(!plan.is_empty());
    }

    #[test]
    fn recognises_state_unavailability_across_node_wordings() {
        // Real messages seen from Arbitrum public endpoints.
        assert!(is_state_unavailable(
            "server returned an error response: error code -32000: historical state d2ffa3 is not available"
        ));
        assert!(is_state_unavailable(
            "error code -32000: metadata is not found, 481880764"
        ));
        assert!(is_state_unavailable("missing trie node abcdef"));
        // A revert or a bad address is not an endpoint problem.
        assert!(!is_state_unavailable("execution reverted"));
        assert!(!is_state_unavailable(
            "invalid argument 0: hex string too long"
        ));
    }

    #[test]
    fn plan_membership_is_queryable() {
        let token = Address::repeat_byte(1);
        let holder = Address::repeat_byte(2);
        let spender = Address::repeat_byte(3);
        let mut plan = ReadPlan::default();
        plan.eth_balance(holder);
        plan.allowance(token, holder, spender);

        // Knowing what was planned is what lets a failed read stay failed instead of being
        // silently retried into a different answer.
        assert!(plan.has_eth_balance(holder));
        assert!(plan.has_allowance(token, holder, spender));
        assert!(plan.has_decimals(token));
        assert!(!plan.has_eth_balance(spender));
        assert!(!plan.has_token_balance(token, holder));
    }
}
