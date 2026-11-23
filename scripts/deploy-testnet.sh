#!/usr/bin/env bash
# Deploys the Agama contracts to Stellar testnet and wires them together.
#
#   SOURCE=<stellar keys alias> USDC=<USDC SAC id> ./scripts/deploy-testnet.sh
#
# The source account becomes owner, curator, operator and reporter; move those
# roles to dedicated accounts (owner to a multisig) before real funds.
set -euo pipefail

NET=testnet
: "${SOURCE:?set SOURCE to a stellar keys alias}"
: "${USDC:?set USDC to the testnet USDC contract id}"
ME=$(stellar keys address "$SOURCE")
W=target/wasm32v1-none/release
DAY=86400

stellar contract build

deploy() { stellar contract deploy --network "$NET" --source "$SOURCE" --wasm "$W/$1.wasm" -- "${@:2}"; }
invoke() { stellar contract invoke --network "$NET" --source "$SOURCE" --id "$1" -- "${@:2}"; }

VAULT=$(deploy agama_vault --owner "$ME" --asset "$USDC" --name "Agama USD" --symbol agUSD)
ENGINE=$(deploy agama_allocation_engine --owner "$ME" --curator "$ME" --vault "$VAULT" --asset "$USDC")
ORACLE=$(deploy agama_oracle --owner "$ME" --base "{\"Stellar\":\"$USDC\"}" --decimals 7 --resolution $DAY)
BOND_POOL=$(deploy agama_mock_stablebond --admin "$ME" --usdc "$USDC" --price 10000000)

invoke "$ORACLE" set_reporter --reporter "$ME" --allowed true
invoke "$ORACLE" add_feed --asset '{"Other":"TKUSD"}' --max_deviation_bps 500
invoke "$ORACLE" add_feed --asset "{\"Stellar\":\"$BOND_POOL\"}" --max_deviation_bps 0

TENKA=$(deploy agama_private_credit_adapter --owner "$ME" --config "{
  \"engine\":\"$ENGINE\",\"asset\":\"$USDC\",\"operator\":\"$ME\",\"oracle\":\"$ORACLE\",
  \"feed\":{\"Other\":\"TKUSD\"},\"unit_decimals\":6,\"max_age\":$((14*DAY)),\"max_slippage_bps\":100}")
BONDS=$(deploy agama_etherfuse_adapter --owner "$ME" --config "{
  \"engine\":\"$ENGINE\",\"asset\":\"$USDC\",\"pool\":\"$BOND_POOL\",\"oracle\":\"$ORACLE\",
  \"feed\":{\"Stellar\":\"$BOND_POOL\"},\"unit_decimals\":7,\"max_age\":$((14*DAY)),\"max_slippage_bps\":50}")

invoke "$VAULT" set_engine --engine "$ENGINE"
invoke "$ENGINE" register_adapter --adapter "$TENKA" --originator TENKA --jurisdiction LU --cap_bps 7000
invoke "$ENGINE" register_adapter --adapter "$BONDS" --originator EFUSE --jurisdiction MX --cap_bps 5000
invoke "$ENGINE" set_reserve_floor --bps 1000

cat <<OUT
vault   $VAULT
engine  $ENGINE
oracle  $ORACLE
tenka   $TENKA
bonds   $BONDS
bondpool(mock) $BOND_POOL
Next: seed the vault with a first deposit that is never redeemed, then push TKUSD and bond prices.
OUT
