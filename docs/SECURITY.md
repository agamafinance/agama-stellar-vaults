# Security notes

## Invariants

1. `total_assets = (USDC on vault - reserved_assets) + Σ adapter.total_value()`
2. `reserved_assets` equals the sum of `claimable` over all holders, and is never released to the engine.
3. `pending_shares` equals the agUSD held by the vault and the sum of shares in open requests.
4. No capital leaves the vault while `pending_shares > 0`.
5. Synchronous exits never exceed liquid USDC, and are zero while `pending_shares > 0` or paused.
6. After every `allocate`: liquid USDC ≥ floor, and each adapter / originator / jurisdiction is within its cap.
7. A missing or stale price makes `total_assets` panic: no entry or exit happens at an unpriced NAV.

## Trust assumptions

- **Owner** (intended: multisig) can change the engine, register adapters, set caps, pause, change the
  adapter operator and force an oracle price. A compromised owner can redirect capital via a malicious adapter.
- **Curator** decides allocations, bounded by caps and the reserve floor.
- **Private credit operator** holds the off-chain tkUSD position and reports unit counts. Fills are bounded by
  the oracle price (`max_slippage_bps`); the position itself must be reconciled against Ethereum from events.
- **Reporters** set prices within the deviation bound; larger moves need the owner.

## Design choices

- Built on OpenZeppelin Stellar Contracts 0.8 (`stellar-tokens` vault and fungible, `stellar-access`
  ownable, `stellar-contract-utils` pausable and fixed-point math).
- Decimals offset 0: agUSD has USDC's 7 decimals. The OpenZeppelin virtual share still applies, and
  **deployment must seed the vault with a first deposit that is never redeemed** (inflation / donation attack;
  see `donation_attack_is_unprofitable_after_seed_deposit`).
- Queued redemptions are priced when processed, not when requested (the holder keeps the yield and the risk
  until then).
- No fees in this version.

## Open points before mainnet

- Confirm the `StablebondPool` interface against the deployed Etherfuse contracts.
- Exercise the Etherfuse `authorize_as_current_contract` subscription path on testnet (unit tests mock auth).
- Tenka integration: deployed contract versions on Sepolia, KYC of the Agama operator, CCTP route, and the
  reporter relaying the Tenka fund NAV into the `TKUSD` feed.
- Monitoring: reconciliation job between adapter events and the Ethereum position.
