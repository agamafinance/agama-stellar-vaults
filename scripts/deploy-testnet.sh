#!/usr/bin/env bash
# Deploys the Agama contracts to Stellar testnet and wires them together.
#
#   SOURCE=<stellar keys alias> USDC=<USDC SAC id> \
#   [CURATOR=<G..>] [OPERATOR=<G..>] [REPORTER=<G..>] ./scripts/deploy-testnet.sh
#
# SOURCE is the owner. Roles default to the owner; on mainnet the owner is a
# multisig and every role is a dedicated account. Addresses are written to
# deployments/testnet.json.
set -euo pipefail

NET=testnet
: "${SOURCE:?set SOURCE to a stellar keys alias}"
: "${USDC:?set USDC to the testnet USDC contract id}"
ME=$(stellar keys address "$SOURCE")
CURATOR=${CURATOR:-$ME}
OPERATOR=${OPERATOR:-$ME}
REPORTER=${REPORTER:-$ME}
W=target/wasm32v1-none/release
DAY=86400

stellar contract build --quiet 2>/dev/null || stellar contract build

# Testnet RPC submissions occasionally time out; retry, and treat a retry that
# fails only because the first attempt landed as success (checked by callers).
retry() { local n; for n in 1 2 3 4; do "$@" && return 0; echo "retry $n: $*" >&2; sleep 5; done; return 1; }
deploy() {
  local id
  id=$(retry stellar contract deploy --quiet --network "$NET" --source "$SOURCE" --wasm "$W/$1.wasm" -- "${@:2}")
  echo "$1 $id" >&2
  echo "$id"
}
invoke() { retry stellar contract invoke --quiet --network "$NET" --source "$SOURCE" --id "$1" -- "${@:2}" >/dev/null; }

VAULT=$(deploy agama_vault --owner "$ME" --asset "$USDC" --name "Agama USD" --symbol agUSD)
ENGINE=$(deploy agama_allocation_engine --owner "$ME" --curator "$CURATOR" --vault "$VAULT" --asset "$USDC")
ORACLE=$(deploy agama_oracle --owner "$ME" --base "{\"Stellar\":\"$USDC\"}" --decimals 7 --resolution $DAY)
BOND_POOL=$(deploy agama_mock_stablebond --admin "$ME" --usdc "$USDC" --price 10000000)

invoke "$ORACLE" set_reporter --reporter "$REPORTER" --allowed true
invoke "$ORACLE" add_feed --asset '{"Other":"TKUSD"}' --max_deviation_bps 500
invoke "$ORACLE" add_feed --asset "{\"Stellar\":\"$BOND_POOL\"}" --max_deviation_bps 0

TENKA=$(deploy agama_private_credit_adapter --owner "$ME" --config "{
  \"engine\":\"$ENGINE\",\"asset\":\"$USDC\",\"operator\":\"$OPERATOR\",\"oracle\":\"$ORACLE\",
  \"feed\":{\"Other\":\"TKUSD\"},\"unit_decimals\":6,\"max_age\":$((14*DAY)),\"max_slippage_bps\":100}")
BONDS=$(deploy agama_etherfuse_adapter --owner "$ME" --config "{
  \"engine\":\"$ENGINE\",\"asset\":\"$USDC\",\"pool\":\"$BOND_POOL\",\"oracle\":\"$ORACLE\",
  \"feed\":{\"Stellar\":\"$BOND_POOL\"},\"unit_decimals\":7,\"max_age\":$((14*DAY)),\"max_slippage_bps\":50}")

invoke "$VAULT" set_engine --engine "$ENGINE"
invoke "$ENGINE" register_adapter --adapter "$TENKA" --originator TENKA --jurisdiction LU --cap_bps 7000
invoke "$ENGINE" register_adapter --adapter "$BONDS" --originator EFUSE --jurisdiction MX --cap_bps 5000
invoke "$ENGINE" set_reserve_floor --bps 1000

mkdir -p deployments
cat > deployments/testnet.json <<JSON
{
  "network": "testnet",
  "usdc": "$USDC",
  "owner": "$ME",
  "curator": "$CURATOR",
  "operator": "$OPERATOR",
  "reporter": "$REPORTER",
  "contracts": {
    "vault": "$VAULT",
    "allocationEngine": "$ENGINE",
    "oracle": "$ORACLE",
    "privateCreditAdapter": "$TENKA",
    "etherfuseAdapter": "$BONDS",
    "mockStablebond": "$BOND_POOL"
  }
}
JSON
cat deployments/testnet.json
echo "Next: seed the vault with a first deposit that is never redeemed, then push TKUSD and bond prices."
