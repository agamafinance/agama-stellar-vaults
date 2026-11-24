# Architecture

## One token: agUSD

agUSD is the share of a SEP-56 vault whose asset is USDC. Depositing USDC mints agUSD at the current share
price; the share price is `total_assets / total_supply` (with OpenZeppelin's virtual offset). Yield and losses
from the credit book move `total_assets`, so they move the price of every share at once.

```
total_assets  = liquid_assets + engine.total_value()
liquid_assets = USDC on the vault - reserved_assets
```

The SEP-56 entry points (`deposit`, `mint`, `withdraw`, `redeem`, `max_*`, `preview_*`, `convert_*`) are the
OpenZeppelin `FungibleVault` trait. Agama plugs its accounting into the library's `VaultOverrides` hook
(`contracts/vault/src/accounting.rs`). The conversion formula is OpenZeppelin's, restated against Agama's
`total_assets` because the library version reads the vault's own USDC balance.

## Exits

Private credit cannot be sold back within a block, so exits have two paths:

1. **Synchronous** (`withdraw` / `redeem`): bounded by liquid USDC, and closed for everyone while a
   redemption queue exists, so a later holder cannot jump the queue.
2. **Queued** (`request_redeem` → `process_redeems` → `claim`), ERC-7540 style:
   - `request_redeem` moves shares into the vault's custody. They stay in the supply and keep earning.
   - `process_redeems` (permissionless) walks the queue in order. Each request is priced at that moment, its
     shares are burned and its USDC is set aside in `reserved_assets`. Processing stops at the first request
     the liquidity cannot cover.
   - `claim` pays the holder. It works while the vault is paused.
   - `cancel_redeem` returns the shares of an unprocessed request.

While redemptions are pending, the vault refuses to `release` capital to the engine.

## Allocation Engine

Holds the registry of adapters (max 10) and moves capital:

- `allocate(adapter, amount)` (curator): the vault `release`s USDC to the adapter, then the adapter is notified.
  Checked on post-move values: reserve floor (liquid USDC ≥ `reserve_floor_bps` of total assets), adapter
  cap, originator cap, jurisdiction cap (all in bps of total assets).
- `deallocate(adapter, amount)` (curator): the adapter sends USDC back to the vault. Allowed while paused.
- `total_value()`: sum of the adapters' `total_value()`, read by the vault.

Call graph, with no path back into a contract that is already executing (Soroban forbids re-entry):
`vault → engine → adapters → oracle` for valuation, and `engine → vault.release`, `engine → adapter` for moves.

## Adapters

Every adapter implements `StrategyAdapter`: `asset`, `total_value`, `available`, `deposit`, `withdraw`
(the last two for the engine only).

### Private credit (Tenka)

Tenka's tkUSD is an ERC-4626 fund share on Ethereum, subscribed and redeemed through Tenka's KYC'd Matching
Engine, with a weekly fund NAV from the Tenka Oracle. The Agama operator bridges USDC (Circle CCTP) and holds
the tkUSD position for the vault.

```
idle USDC ──dispatch──▶ in_transit ──confirm_subscription──▶ units
    ▲                        │                                  │
    └──────cancel_transit────┘           settle_redemption ◀────┘  (USDC in + units out, one call)
```

`total_value = idle + in_transit + units × price(TKUSD)`, price from the SEP-40 oracle, refused if older than
`max_age` (14 days: Tenka treats a NAV as stale after one missed week). Every fill the operator reports is
checked against the oracle price within `max_slippage_bps`.

### Etherfuse Stablebonds

Stablebonds (CETES, USTRY, TESOURO) are classic Stellar assets with a Stellar Asset Contract, bought and sold
through Etherfuse's API by a KYB'd account, not through an on-chain pool. The adapter follows the same
operator-settled model, except the bonds sit on the adapter itself:

```
idle USDC ──dispatch──▶ usdc_in_transit ──confirm_purchase(usdc, bonds)──▶ bonds on the adapter
    ▲                        │                                                 │
    └─────cancel_transit─────┘                              dispatch_bonds ────┘
    └──────settle_sale(bonds, usdc) ◀── bonds_in_transit ◀────────────────────┘
```

`confirm_purchase` pulls the bonds from the operator and `settle_sale` pulls the USDC, in the same call as the
bookkeeping. `total_value = idle + usdc_in_transit + (bonds held + bonds_in_transit) × price`, price from a SEP-40
feed (Etherfuse publishes one on-chain: base USD, 14 decimals), refused when older than `max_age` (1 hour).

## Oracle

SEP-40 read interface (`lastprice`, `price`, `prices`, `decimals`, `resolution`, `base`, `assets`), so
adapters can read this contract or Reflector through the same client. Reporters push prices; a move beyond
the feed's `max_deviation_bps` needs the owner's `force_price`. Timestamps strictly increase and cannot be in
the future. Freshness is checked by each consumer.

## Roles

| Role | Contract | Can |
| --- | --- | --- |
| Owner (multisig) | all | configure, register adapters, caps, pause, force a price |
| Curator | engine | allocate / deallocate |
| Operator | private credit and Etherfuse adapters | dispatch, confirm purchases / subscriptions, settle sales / redemptions |
| Reporter | oracle | push prices within the deviation bound |
| Anyone | vault | process the redemption queue |

## Keeper

`scripts/keeper.py` runs the off-chain duties, each round (default every 5 minutes):

- relays the tkUSD share price (`MasterVault.convertToAssets(1e6)` on Ethereum) to the `TKUSD` feed;
- relays the Stablebond price from Etherfuse's SEP-40 aggregator, with Etherfuse's timestamp;
- processes the redemption queue when liquidity allows (simulated first, sent only if a request clears);
- checks that the vault values (no stale price) and alerts otherwise.

A price move beyond a feed's deviation bound is never pushed by the keeper: it alerts, and the owner decides
(`force_price`).
