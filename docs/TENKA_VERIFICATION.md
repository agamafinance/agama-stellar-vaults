# Tenka verification (2026-11-23)

What we checked: does the Tenka source gist shared on 2026-11-13 (snapshot `0788b794`) match (a) the
interfaces Agama already integrated against in `agama-vault/ethereum`, and (b) what is deployed on-chain?
All checks were read-only: JSON-RPC calls with `cast`, bytecode dispatcher scans, and the existing Foundry fork suite.

## 1. Gist vs the interfaces Agama vendored

| Contract | Vendored signatures | Missing from gist | Changed | Only in gist (not vendored) |
| --- | --- | --- | --- | --- |
| MatchingEngine | 28 | 0 | 0 | `MAX_STATIC_FEE`, `MAX_SPREAD_FEE`, `feesCollected`, `TIMELOCKED_ROLE`, `EMERGENCY_ROLE` |
| MasterVault (tkUSD) | 50 | 0 | 0 | MMF request queue (`requestDeposit/Redeem`, `process*`, `depositRequests`…), role constants |
| Oracle | 18 | 0 | 0 | role constants |
| TenkaKYC | 14 | 0 | 0 | role constants |

- The `Order`, `SignedOrder`, `MatchingOrder` and `ERC3009Data` structs and the `EngineState`, `OrderType` and
  `OrderStatus` enums are identical. `ORDER_TYPEHASH` is the same string.
- Every vendored event and error exists in the gist, or is a standard OpenZeppelin ERC20, ERC4626, ERC2612 or
  ERC721 event or error.
- **Verdict:** the gist is a superset of what we integrated against. No signature we call changed.

## 2. What is deployed where

| Deployment | Address book | State |
| --- | --- | --- |
| Polygon staging | `PolygonStaging.sol` (MV `0x499d…FdBa`, ME `0x7C7A…533f`, Oracle `0x2Ec2…0824`) | live; same proxies and implementations on Ethereum mainnet (staging) |
| **Ethereum Sepolia staging** (new, deployed 2026-11-17, after the gist) | Registry `0x13a79b893910CA43C3C1Dc9F3A7C788faeD0CBAA` | live, details below |
| Same `PolygonStaging` addresses on Sepolia | — | **not** Tenka's staging: uninitialized contracts (owner 0) or no code. Do not use them. |

Sepolia, resolved through `Registry.getContractAddress(n)`:

| Role | Address |
| --- | --- |
| Master Vault `tkUSD-STAGING` (6 dp) | `0x1EF979B3b1fFA92b86Ea10125a70462C4380eDab` |
| Asset: "Staging Agora USD" `AUSD` (6 dp) | `0xa0C87008bf34DcB0b0d03e12C6Fe29DDEaa5A728` |
| Matching Engine | `0xB767440b282667aFc54F2dB00D18003Da8ab2012` |
| Oracle | `0xba503703F84d9f55402e3F83Cd814288032d17B0` |
| KYC | `0x8Dc74A57d63Ab702F532518Cd4033Dbcee52D639` |
| Master Vault registry | `0x79e64B750C5838115D1d54393A82A849b3560236` |
| Treasury | `0x9e621E4c1c34d95a2354F7077cF9417c52c2Dc3e` |

Sepolia state when we read it: totalAssets = totalSupply = 1e6 (a 1 AUSD seed); `convertToAssets(1e6)` = 1e6;
cap = 5e12; ratio = 8000; engine in `PRE_WINDOW`; fees are 10 bps static and 10% spread;
`WEEK_DURATION` = 300 s; one NAV log, with NAV 0 and `getNonStaleLatestFundNav` reverting `ErrStaleData`.

### Deployed bytecode vs the gist

We checked whether each gist function selector appears in each implementation's dispatcher. UUPS helpers were
excluded: these are transparent proxies.

| Implementation | Gist selectors present | Extra selectors | Polygon vs Sepolia bytecode |
| --- | --- | --- | --- |
| MatchingEngine | 44/44 | none | identical (metadata excluded) |
| TenkaKYC | 30/30 | none | identical |
| MasterVault | 88/88 | none | **different** |
| Oracle | 25/25 | Polygon: `0x15a0f1a3` (unnamed); Sepolia: none | **different** |

- The extra Oracle selector on Polygon is almost certainly the testing-only week-duration setter that the gist
  README says was removed. Sepolia doesn't have it, so the **Sepolia Oracle matches the gist snapshot**.
  Polygon staging is the older version.
- Sepolia still runs with `WEEK_DURATION` = 300 s, while the gist's `initialize` hard-codes 7 days. So the
  staging build differs from the gist on this constant, and production should use 7 days.
- The MasterVault differs between Polygon and Sepolia, but no function was added or removed. The change is
  internal, consistent with the "finalization guards" the README mentions.
- The MatchingEngine is byte-identical on both chains. We can't tell from selectors alone whether the newer
  strategic-role restriction (`setEngineState` being `onlyStrategic`) is present.

### Fork tests

`forge test --match-path test/Tenka.t.sol` in `agama-vault/ethereum` passed **35/35** against the pinned Polygon
staging fork (cached state). It covers BID and ASK entry and exit, cancellation, the staleness rule, valuation and
the denied cases. The suite has not been ported to the Sepolia addresses yet.

## 3. Impact on the Stellar private-credit adapter

| Topic | What Tenka does | What our adapter assumes | Action |
| --- | --- | --- | --- |
| Asset | Master Vault asset is **AUSD** (6 dp), not USDC | USDC on Stellar (7 dp) | the operator must swap USDC↔AUSD on Ethereum; the swap cost falls inside `max_slippage_bps` and the peg is a risk to document |
| Share decimals | tkUSD has 6 dp | `unit_decimals: 6` | matches |
| What the `TKUSD` feed must carry | the Tenka Oracle publishes the **fund admin NAV** (assets held by the fund admin), not a per-share price | the price of one tkUSD in base units | the reporter must relay `MasterVault.convertToAssets(1e6)` (= (idle + fund NAV) / supply), not `getLatestFundNav` |
| Staleness | `getNonStaleLatestFundNav` reverts once current week − NAV week > 1, so data is good for up to about 2 periods | `max_age` = 14 days | matches production (7-day periods); on staging a period is 300 s |
| Entry and exit | no direct ERC-4626 entry or exit: a BID order (pay AUSD) or ASK order (sell tkUSD) is posted in `PRE_WINDOW`, then the Tenka operator settles with `matchOrders` in `SETTLING`; prices are bps of NAV (10000 = NAV) | `dispatch` → `confirm_subscription` / `settle_redemption` | the adapter's three steps fit; settlement is asynchronous and periodic |
| Fees | 10 bps static fee on the seller side; 10% spread fee (cap at `MAX_SPREAD_FEE`) | `max_slippage_bps` = 100 | to check: what the spread fee applies to and whether it can push a fill outside 1% |
| KYC | orders need a KYC'd payer and receiver (`TenkaKYC`) | the operator account | Tenka must KYC the Agama operator / Safe on Sepolia |

## Open points

- Ask Tenka for their official Sepolia address book (the one above was reconstructed from the Registry) and which
  source commit Sepolia runs.
- Port `Tenka.t.sol` to the Sepolia deployment once the Agama Safe is KYC'd.
- Confirm that the production `WEEK_DURATION` is 7 days and that the strategic-role restriction is live.
