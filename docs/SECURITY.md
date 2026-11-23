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
- Tenka integration (see `docs/TENKA_VERIFICATION.md`): official Sepolia address book and source commit, KYC of the
  Agama operator, USDC↔AUSD swap and CCTP route, spread fee vs `max_slippage_bps`, and a production reporter relaying
  `MasterVault.convertToAssets(1e6)` (not `getLatestFundNav`) into the `TKUSD` feed.
- Monitoring: reconciliation job between adapter events and the Ethereum position.

## Testnet verification

`scripts/testnet_battery.py` runs the whole system on Stellar testnet with separate owner / curator / operator /
reporter accounts and generated users, relaying the live tkUSD share price from Tenka's Sepolia Master Vault. It
checks every invariant above after each phase and asserts the expected refusals (access control, caps, reserve
floor, oracle deviation, fill bounds, queue ordering, pause, stale price). Last run: `deployments/testnet-battery.log`.
The Etherfuse `authorize_as_current_contract` subscription path is exercised there with real authorization.
