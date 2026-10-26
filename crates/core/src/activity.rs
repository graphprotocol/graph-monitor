//! When did this account last send a transaction?
//!
//! There is no RPC method for "last transaction", so this searches: the nonce at the head tells
//! you how many the account has ever sent, and the block where the nonce last changed is the
//! block of the last one. Finding it is a search over historical nonces.
//!
//! **This requires an archive endpoint.** A historical `eth_getTransactionCount` is a state
//! read, and a pruned node refuses it. When that happens the check reports unreadable — never
//! "inactive", which would be a false alarm caused by our own infrastructure.

use alloy::primitives::{Address, B256};

use crate::rpc::{BlockSpec, Client};

/// Ceiling on `eth_getLogs` requests for one activity search.
///
/// Window size is whatever the endpoint tolerates, so coverage is `budget × range`. With a
/// 10,000-block cap that is 600,000 blocks, about two days of Arbitrum. A provider allowing
/// wider spans covers the whole lookback in a handful of requests; one capped at 100 blocks
/// cannot cover anything useful, and the search says so rather than reporting silence.
const MAX_LOG_REQUESTS: usize = 60;

/// How many times one window may be narrowed before giving up. Halving from 10,000 reaches
/// 100 — Chainstack's Developer cap — in seven steps.
const MAX_RANGE_SHRINKS: usize = 10;

/// Transaction lookups issued at once when attributing logs to an account.
const TX_LOOKUP_CONCURRENCY: usize = 8;

/// Hard cap on probes, so a pathological search can never run away. Two phases of ~30 covers
/// the full height of any chain we read.
const MAX_PROBES: usize = 80;

/// What the search found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Nonce zero: this account has never sent a transaction.
    Never,
    /// The block of the most recent outbound transaction, and how long ago that was.
    Last { block: u64, idle_seconds: u64 },
}

/// Locate the most recent outbound transaction at or before `head`.
///
/// Two phases: step backwards in doubling strides until a block is found with a lower nonce,
/// then bisect. The doubling matters — an account that transacted minutes ago is found in a
/// handful of probes, and recently-active is the common case. A naive bisection from genesis
/// would pay ~29 archive reads every time regardless.
pub async fn find_last_activity(
    client: &Client,
    address: alloy::primitives::Address,
    head: u64,
    head_timestamp: u64,
) -> Result<Activity, String> {
    let nonce = client
        .nonce_at(address, head)
        .await
        .map_err(|e| format!("could not read nonce of {address}: {e}"))?;

    if nonce == 0 {
        return Ok(Activity::Never);
    }

    let mut probes = 0;
    let mut probe = |_: ()| {
        probes += 1;
        probes <= MAX_PROBES
    };

    // Phase 1: find a block with a lower nonce, doubling the stride each time.
    let lower;
    let mut stride = 1u64;
    loop {
        if !probe(()) {
            return Err(format!(
                "gave up locating the last transaction of {address} after {MAX_PROBES} probes"
            ));
        }
        let candidate = head.saturating_sub(stride);
        let at = client
            .nonce_at(address, candidate)
            .await
            .map_err(|e| format!("could not read historical nonce of {address}: {e}"))?;
        if at < nonce {
            lower = candidate;
            break;
        }
        if candidate == 0 {
            // Nonce unchanged all the way back to genesis. Impossible for a real account, but
            // treat it as "no bracket" rather than looping.
            lower = 0;
            break;
        }
        stride = stride.saturating_mul(2);
    }

    // Phase 2: bisect for the earliest block at which the nonce already reached its current
    // value. That block is the one containing the last transaction.
    let mut lo = lower;
    let mut hi = head;
    while lo + 1 < hi {
        if !probe(()) {
            return Err(format!(
                "gave up locating the last transaction of {address} after {MAX_PROBES} probes"
            ));
        }
        let mid = lo + (hi - lo) / 2;
        let at = client
            .nonce_at(address, mid)
            .await
            .map_err(|e| format!("could not read historical nonce of {address}: {e}"))?;
        if at < nonce {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let block = client
        .pin(BlockSpec::Number(hi))
        .await
        .map_err(|e| format!("could not read block {hi}: {e}"))?;

    Ok(Activity::Last {
        block: block.number,
        // Measured against the pinned head, not wall clock, so the answer belongs to the same
        // block as every other value in the cycle.
        idle_seconds: head_timestamp.saturating_sub(block.timestamp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "diagnostic"]
    fn print_candidate_topics() {
        for sig in [
            "TokensPulled(address,uint256)",
            "TokensAdded(address,uint256)",
            "TokensRemoved(address,address,uint256)",
            "TokensPulled(address[],uint256[])",
            "InsufficientBalanceForRemoval(address,uint256)",
            "TokensRescued(address,address,uint256)",
            "GatewayUpdated(address)",
            "Transfer(address,address,uint256)",
            "Approval(address,address,uint256)",
        ] {
            println!("{:?}  {sig}", alloy::primitives::keccak256(sig.as_bytes()));
        }
    }

    #[test]
    fn event_signatures_hash_to_the_topics_seen_on_chain() {
        // Observed topic0 values from Arbitrum One. A typo in a signature yields a filter that
        // silently matches nothing, which would read as "inactive" — the exact false alarm
        // this watch exists to avoid.
        for (sig, expected) in [
            (
                "Log(bytes)",
                "0xafabcf2dd47e06a477a89e49c03f8ebe8e0a7e94f775b25bbb24227c9d0110b2",
            ),
            (
                "TokensAdded(address,uint256)",
                "0x0eaac1064ee8d13f83091f388597ffb5ff2d2e59f6a16aae2a74ba7f2d13c8d9",
            ),
            (
                "TokensPulled(address,uint256)",
                "0x3c3f210f5fb914ad3522d4b423e3db16130d0409700cb7b1827ccc90f341ac2f",
            ),
        ] {
            let got = alloy::primitives::keccak256(sig.as_bytes());
            assert_eq!(format!("{got:?}"), expected, "signature {sig}");
        }
    }

    #[test]
    fn never_and_last_are_distinct_outcomes() {
        // "Never transacted" is not "idle for a very long time" — an account with nonce 0 has
        // no last-activity timestamp at all, and reporting one would be inventing data.
        assert_eq!(Activity::Never, Activity::Never);
        assert_ne!(
            Activity::Never,
            Activity::Last {
                block: 1,
                idle_seconds: 0
            }
        );
    }

    /// The bisection, extracted so it can be tested without a network.
    fn bisect(nonce_at: impl Fn(u64) -> u64, head: u64, nonce: u64) -> u64 {
        let mut lo = 0;
        let mut hi = head;
        let mut stride = 1u64;
        loop {
            let candidate = head.saturating_sub(stride);
            if nonce_at(candidate) < nonce {
                lo = candidate;
                break;
            }
            if candidate == 0 {
                break;
            }
            stride = stride.saturating_mul(2);
        }
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if nonce_at(mid) < nonce {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        hi
    }

    #[test]
    fn finds_the_block_where_the_nonce_last_changed() {
        // Nonce reaches 5 at block 900 and stays there.
        let chain = |b: u64| if b >= 900 { 5 } else { 4 };
        assert_eq!(bisect(chain, 1000, 5), 900);
    }

    #[test]
    fn finds_activity_in_the_most_recent_block() {
        let chain = |b: u64| if b >= 1000 { 7 } else { 6 };
        assert_eq!(bisect(chain, 1000, 7), 1000);
    }

    #[test]
    fn finds_very_old_activity() {
        // The doubling phase has to walk a long way back before bisecting.
        let chain = |b: u64| if b >= 3 { 2 } else { 1 };
        assert_eq!(bisect(chain, 1_000_000, 2), 3);
    }

    #[test]
    fn the_search_is_logarithmic_not_linear() {
        use std::cell::Cell;
        let probes = Cell::new(0);
        let chain = |b: u64| {
            probes.set(probes.get() + 1);
            if b >= 499_999_000 {
                42
            } else {
                41
            }
        };
        assert_eq!(bisect(chain, 500_000_000, 42), 499_999_000);
        // A linear scan would be a million reads; doubling plus bisection is a few dozen.
        assert!(probes.get() < MAX_PROBES, "{} probes", probes.get());
    }
}

/// Which side of a transaction identifies the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxMatch {
    /// The account signed the transaction. Right for EOAs.
    From,
    /// The account was called. Right for contract accounts such as a multisig, where the
    /// sender is whichever owner relayed the transaction, not the multisig itself.
    To,
}

impl TxMatch {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "from" => Ok(TxMatch::From),
            "to" => Ok(TxMatch::To),
            other => Err(format!("expected `from` or `to`, got {other:?}")),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TxMatch::From => "from",
            TxMatch::To => "to",
        }
    }
}

/// Find the most recent log from `emitter` matching `topic0` whose transaction involves
/// `account`, searching back from `head` no further than `lookback_seconds`.
///
/// Widening windows rather than one big range: an account that acted minutes ago resolves in a
/// single request, and only a genuinely silent one pays for the full lookback — which is the
/// account worth spending on, since silence is the thing being watched for.
#[allow(clippy::too_many_arguments)]
pub async fn find_last_event(
    client: &Client,
    account: Address,
    emitter: Address,
    topic0: B256,
    matches: TxMatch,
    head: u64,
    head_timestamp: u64,
    lookback_seconds: u64,
) -> Result<Activity, String> {
    let cutoff = head_timestamp.saturating_sub(lookback_seconds);

    let mut end = head;
    let mut scanned = 0u64;

    for _ in 0..MAX_LOG_REQUESTS {
        // Providers cap the span and the cap is not discoverable up front, so a rejection
        // narrows the window and retries the same window immediately. Learning happens on the
        // client, so every later request and every other watch benefits.
        let mut shrinks = 0;
        let (logs, start) = loop {
            let span = client.log_range();
            let start = end.saturating_sub(span);
            match client.logs(emitter, topic0, start, end).await {
                Ok(logs) => break (logs, start),
                Err(e) => {
                    let text = e.to_string();
                    let can_shrink = Client::is_range_error(&text)
                        && client.log_range() > 1
                        && shrinks < MAX_RANGE_SHRINKS;
                    if !can_shrink {
                        return Err(format!("could not read logs from {emitter}: {text}"));
                    }
                    client.shrink_log_range();
                    shrinks += 1;
                }
            }
        };
        scanned += end.saturating_sub(start);

        // Newest first, deduplicated: one transaction often emits several matching logs, and
        // looking it up once per log is pure waste.
        let mut candidates: Vec<(u64, alloy::primitives::TxHash)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for log in logs.iter().rev() {
            if let (Some(number), Some(hash)) = (log.block_number, log.transaction_hash) {
                if seen.insert(hash) {
                    candidates.push((number, hash));
                }
            }
        }

        for chunk in candidates.chunks(TX_LOOKUP_CONCURRENCY) {
            let parties =
                futures::future::join_all(chunk.iter().map(|(_, hash)| client.tx_parties(*hash)))
                    .await;

            for ((number, _), party) in chunk.iter().zip(parties) {
                let party = party.map_err(|e| format!("could not read transaction: {e}"))?;
                let involved = match (matches, party) {
                    (TxMatch::From, Some((from, _))) => from == account,
                    (TxMatch::To, Some((_, to))) => to == Some(account),
                    (_, None) => false,
                };
                if !involved {
                    continue;
                }
                let block = client
                    .pin(BlockSpec::Number(*number))
                    .await
                    .map_err(|e| format!("could not read block {number}: {e}"))?;
                return Ok(Activity::Last {
                    block: block.number,
                    idle_seconds: head_timestamp.saturating_sub(block.timestamp),
                });
            }
        }

        if start == 0 {
            return Ok(Activity::Never);
        }

        // Stop once the window has reached past the lookback horizon. Only here is "nothing
        // found" a real answer, because only here has the whole lookback been covered.
        let edge = client
            .pin(BlockSpec::Number(start))
            .await
            .map_err(|e| format!("could not read block {start}: {e}"))?;
        if edge.timestamp <= cutoff {
            return Ok(Activity::Never);
        }

        end = start.saturating_sub(1);
    }

    // Out of budget before reaching the horizon. Reporting `Never` here would render as
    // "no activity", which is a false negative caused by the endpoint rather than by the
    // account — exactly the confusion this watch exists to prevent.
    Err(format!(
        "searched {scanned} blocks without reaching the {}h lookback; this endpoint caps log \
         queries at {} blocks, so raise max_log_range, shorten the lookback, or use a provider \
         with a wider limit",
        lookback_seconds / 3600,
        client.log_range()
    ))
}
