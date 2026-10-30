# Component Inventory

Components and their health checks: ✅ implemented · ⬜ not implemented.

Scope: everything here is observable from **outside** — public RPC, public HTTP endpoints,
public subgraph queries. Checks that need a component's internal metrics or logs belong on that
component's own dashboard, not in this tool.

---

## Infra
- ✅ Account `NetworkMonitor`: The EOA that tops up watched accounts.
  - ✅ Balance watch: ETH.

---

## Billing

- ✅ Account `FoundationBillingMultisig`: Foundation treasury that tops up the billing wallet.
  - ✅ Balance watch: GRT.
  - ✅ Allowance watch: GRT to `NetworkMonitor`.
- ✅ Account `BillingWallet`: Collects consumer payments from the billing contract.
  - ✅ Balance watch: ETH.
  - ✅ Balance watch: GRT.
  - ✅ Allowance watch: GRT to `NetworkMonitor`.
  - ⬜ Activity watch: calls to Billing contract to pull tokens.
    - Billing Contract: `0x1b07d3344188908fb6deceac381f3ee63c48477a`
    - `event TokensPulled(address indexed user, uint256 amount);`
- ✅ Account `GraphTallyWallet`: Funds indexer escrow.
  - ✅ Balance watch: ETH.
  - ✅ Balance watch: GRT.
  - ✅ Activity watch: deposits into `PaymentsEscrow`.
    - PaymentsEscrow: `0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E`
    - `event Deposit(address indexed payer, address indexed collector, address indexed receiver, uint256 tokens);`

---

## On-chain contracts

- ⬜ (Alert) Implementation address changed. A proxy started pointing at different code.
- ⬜ (Alert) Controller address changed. The Controller a contract trusts was swapped.
- ⬜ (Alert) Owner / governance changed. Ownership of a core contract moved.
- ⬜ (Alert) Pause state. A core contract is paused.
- ⬜ L1 supply and issuance. Minted GRT on mainnet matches expected issuance.
- ⬜ L2 supply and issuance. Same for Arbitrum.
- ⬜ L1+L2 supply reconciliation. Total supply across both chains adds up.
- ⬜ Inflation rate. Issuance per block sits within the expected rate.
- ⬜ Issuance distribution staleness. The allocator has distributed issuance recently enough.
- ⬜ Issuance distribution keeper. Call the permissionless `distributeIssuance()` when it goes stale.
- ⬜ Indexing rewards calculation. Rewards the contracts would pay match an independent recomputation.
- ⬜ Escrow balance. GRT locked on L1 backs the supply minted on L2.

---

## EBO — Epoch Block Oracle

- ✅ Account `EpochBlockOracle`: Posts epoch block hashes on chain.
  - ✅ Balance watch: ETH.
  - ✅ Activity watch: epoch block hash submissions.
    - From: `EpochBlockOracle`
    - EBODataEdge: `0x633bb9790d7c4c59991cebd377c0ed6501a35ebe`
    - `event Log(bytes data);`
- ⬜ Epoch advancing on schedule. Subgraph epoch matches contract epoch.

---

## SAO — Subgraph Availability Oracle

- For each account:
  - contract: `0x1cb555359319a94280acf85372ac2323aae2f5fd`
  - from: each account
  - `event OracleVote(bytes32 indexed subgraphDeploymentID, bool deny, uint256 indexed oracleIndex, uint256 timestamp);`
- ✅ Account `SamOracleEdgeAndNode`: Availability oracle operated by Edge & Node.
  - ✅ Balance watch: ETH.
  - ⬜ Activity watch: wallet voting
- ✅ Account `SamOracleGraphOps`: Availability oracle operated by GraphOps.
  - ✅ Balance watch: ETH.
  - ⬜ Activity watch: wallet voting
- ✅ Account `SamOracleInfraDAO`: Availability oracle operated by InfraDAO.
  - ✅ Balance watch: ETH.
  - ⬜ Activity watch: wallet voting
- ✅ Account `SamOracleSemiotic`: Availability oracle operated by Semiotic.
  - ✅ Balance watch: ETH.
  - ⬜ Activity watch: wallet voting
- ⬜ SAM activity: vote consensus
  - contract: `0x971B9d3d0Ae3ECa029CAB5eA1fB0F72c85e6a525` (RewardsManager)
  - `event RewardsDenylistUpdated(bytes32 indexed subgraphDeploymentID, uint256 sinceBlock);`

---

## REO — Rewards Eligibility Oracle

- ✅ Account `RewardsEligibilityOracle`: Marks which indexers may earn indexing rewards.
  - ✅ Balance watch: ETH.
  - ⬜ Activity watch: eligibility updates.
    - contract: `0x02753BaE61C08AbD4351Bce7F48524935C2Cc78E`  (RewardsEligibilityOracle)
    - `event IndexerEligibilityRenewed(address indexed indexer, address indexed oracle);`

---

## QoSO — Quality of Service Oracle (leave out for now)

- ⬜ Account unknown: The wallet this oracle signs with has not been identified.
- ⬜ Activity. Fresh quality-of-service data is arriving in the QoS subgraph.

---

## Upgrade indexer

- ✅ Account `UpgradeIndexerCurationBot`: Signals curation so the upgrade indexer serves new subgraphs.
  - ✅ Balance watch: ETH.
  - ⬜ Activity watch: curation signals on newly published subgraphs.
- ⬜ Curation signals landing. Newly published subgraphs are actually being signalled.

---

## Subgraphs

For every specified subgraph deployment and deployment id:
- ✅ Reachable. The subgraph answers queries.
- ✅ Indexing lag vs chain head. How far behind the chain it is.

Important subgraphs: network, analytics, EBO, QoS
