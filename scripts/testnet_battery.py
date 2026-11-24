#!/usr/bin/env python3
"""End-to-end battery against the contracts deployed on Stellar testnet.

    python3 scripts/testnet_battery.py [--users 12]

Reads deployments/testnet.json. Needs the stellar-cli keys used by
scripts/deploy-testnet.sh: `agama` (owner), `agv-curator`, `agv-operator`,
`agv-reporter`, and `agv-issuer` (issuer of the test USDC and CETES assets, admin of their
SACs). Test users are generated and funded with friendbot.

The tkUSD price pushed to the oracle is read live from Tenka's Sepolia
staging Master Vault (`convertToAssets(1e6)`), the value the production
reporter will relay.

Every phase ends with the protocol invariants (docs/SECURITY.md). Expected
failures (access control, caps, floor, deviation, staleness, pause) are
asserted to fail. Exit code 0 means every check passed.
"""

import argparse
import json
import random
import subprocess
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

NET = "testnet"
USDC = 10_000_000  # 7 dp
TK = 1_000_000  # tkUSD 6 dp
ONE = 10_000_000  # oracle 7 dp
SEPOLIA_RPC = "https://ethereum-sepolia-rpc.publicnode.com"
TENKA_MASTER_VAULT = "0x1EF979B3b1fFA92b86Ea10125a70462C4380eDab"
MAINNET_RPC = "https://mainnet.sorobanrpc.com"
MAINNET_PASSPHRASE = "Public Global Stellar Network ; September 2015"
# Etherfuse SEP-40 aggregator and the CETES SAC, Stellar mainnet
ETHERFUSE_ORACLE = "CAVRP26CWW6IUEXBRA3Q2T2SHBUVBC2DF43M4E23LEZGW5ZEIB62HALS"
CETES_MAINNET = "CAL6ER2TI6CTRAY6BFXWNWA7WTYXUXTQCHUBCIBU5O6KM3HJFG6Z6VXV"

D = json.load(open("deployments/testnet.json"))
C = D["contracts"]
VAULT, ENGINE, ORACLE = C["vault"], C["allocationEngine"], C["oracle"]
TENKA, BONDS = C["privateCreditAdapter"], C["etherfuseAdapter"]
BOND = D["bond"]
TKUSD = '{"Other":"TKUSD"}'
BOND_FEED = json.dumps({"Stellar": BOND})

checks = {"pass": 0, "fail": 0}
log_lines = []


def log(msg):
    line = time.strftime("%H:%M:%S ") + msg
    print(line, flush=True)
    log_lines.append(line)


def check(cond, what):
    if cond:
        checks["pass"] += 1
        log(f"  ok   {what}")
    else:
        checks["fail"] += 1
        log(f"  FAIL {what}")


def sh(args, retries=4):
    last = None
    for attempt in range(retries):
        p = subprocess.run(args, capture_output=True, text=True)
        if p.returncode == 0:
            return p.stdout.strip()
        last = p.stderr.strip()
        # only network flakiness is retried
        if not any(s in last for s in ("timeout", "Timeout", "502", "503", "connection", "TRY_AGAIN")):
            break
        time.sleep(4 * (attempt + 1))
    raise RuntimeError(last)


def addr(key):
    return sh(["stellar", "keys", "address", key])


def invoke(source, contract, fn, *args, send=True):
    cmd = ["stellar", "contract", "invoke", "--quiet", "--network", NET, "--source", source,
           "--id", contract]
    if not send:
        cmd += ["--send", "no"]
    cmd += ["--", fn, *[str(a) for a in args]]
    out = sh(cmd)
    if out == "":
        return None
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        return out


def view(contract, fn, *args):
    v = invoke("agama", contract, fn, *args, send=False)
    if isinstance(v, str) and v.lstrip("-").isdigit():
        return int(v)
    return v


def fails(source, contract, fn, *args):
    try:
        invoke(source, contract, fn, *args)
        return False
    except RuntimeError:
        return True


def num(v):
    return int(v) if v is not None else 0


# ---------------------------------------------------------------- Tenka

def tenka_share_price():
    """tkUSD price in USD with 7 dp: convertToAssets(1e6) (AUSD 6 dp) * 10."""
    data = "0x07a2d13a" + (1_000_000).to_bytes(32, "big").hex()
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "eth_call",
                       "params": [{"to": TENKA_MASTER_VAULT, "data": data}, "latest"]}).encode()
    req = urllib.request.Request(SEPOLIA_RPC, body, {"Content-Type": "application/json"})
    res = json.load(urllib.request.urlopen(req, timeout=20))
    return int(res["result"], 16) * 10


def etherfuse_cetes_price():
    """CETES price from Etherfuse's mainnet SEP-40 aggregator, 14 dp -> 7 dp."""
    out = sh(["stellar", "contract", "invoke", "--quiet", "--rpc-url", MAINNET_RPC,
              "--network-passphrase", MAINNET_PASSPHRASE, "--source", "agama",
              "--id", ETHERFUSE_ORACLE, "--send", "no", "--", "lastprice",
              "--asset", json.dumps({"Stellar": CETES_MAINNET})])
    p = json.loads(out)
    age = int(time.time()) - int(p["timestamp"])
    return int(p["price"]) // 10**7, age


# ---------------------------------------------------------------- setup

def new_user(i, issuer):
    name = f"agv-user-{i}"
    p = subprocess.run(["stellar", "keys", "address", name], capture_output=True, text=True)
    if p.returncode != 0:
        sh(["stellar", "keys", "generate", name, "--network", NET, "--fund"])
    return name


def trust(key, issuer, code="USDC"):
    try:
        sh(["stellar", "tx", "new", "change-trust", "--quiet", "--network", NET, "--source", key,
            "--line", f"{code}:{issuer}"])
    except RuntimeError as e:
        if "already" not in str(e).lower():
            raise


def mint(to, amount):
    invoke("agv-issuer", D["usdc"], "mint", "--to", to, "--amount", amount)


def usdc_balance(a):
    return num(view(D["usdc"], "balance", "--id", a))


def invariants(users, tag):
    liquid = num(view(VAULT, "liquid_assets"))
    deployed = num(view(ENGINE, "total_value"))
    total = num(view(VAULT, "total_assets"))
    check(total == liquid + deployed, f"[{tag}] total_assets {total} = liquid {liquid} + deployed {deployed}")
    reserved = num(view(VAULT, "reserved_assets"))
    claim_sum = sum(num(view(VAULT, "claimable", "--owner", addr(u))) for u in users)
    check(reserved == claim_sum, f"[{tag}] reserved {reserved} = sum of claimable {claim_sum}")
    pending = num(view(VAULT, "pending_shares"))
    custody = num(view(VAULT, "balance", "--account", VAULT))
    check(pending == custody, f"[{tag}] pending shares {pending} = vault custody {custody}")
    vault_usdc = usdc_balance(VAULT)
    check(vault_usdc == liquid + reserved, f"[{tag}] vault USDC {vault_usdc} = liquid + reserved")
    supply = num(view(VAULT, "total_supply"))
    holders = sum(num(view(VAULT, "balance", "--account", addr(u))) for u in users) + custody \
        + num(view(VAULT, "balance", "--account", addr("agama")))
    check(supply == holders, f"[{tag}] supply {supply} = sum of balances {holders}")


# ---------------------------------------------------------------- battery

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--users", type=int, default=12)
    ap.add_argument("--seed", type=int, default=7)
    a = ap.parse_args()
    rng = random.Random(a.seed)

    issuer = addr("agv-issuer")
    owner, curator, operator, reporter = (addr(k) for k in
                                         ("agama", "agv-curator", "agv-operator", "agv-reporter"))

    log("== phase 0: accounts")
    with ThreadPoolExecutor(6) as ex:
        users = list(ex.map(lambda i: new_user(i, issuer), range(a.users)))
    with ThreadPoolExecutor(6) as ex:
        list(ex.map(lambda k: trust(k, issuer), users + ["agama", "agv-operator"]))
    trust("agv-operator", issuer, "CETES")
    for u in users:
        mint(addr(u), rng.randint(50, 2_000) * USDC)
    mint(owner, USDC)
    mint(operator, 10_000 * USDC)  # stands in for USDC bridged back from Ethereum
    log(f"   {len(users)} users funded")

    log("== phase 1: prices (tkUSD relayed from Tenka Sepolia)")
    tk_price = tenka_share_price()
    log(f"   Tenka Sepolia convertToAssets(1e6) -> tkUSD price {tk_price / ONE:.7f}")
    check(tk_price > 0, "Tenka Sepolia share price readable")
    now = int(time.time()) - 30
    invoke("agv-reporter", ORACLE, "set_price", "--reporter", reporter, "--asset", TKUSD,
           "--price", tk_price, "--timestamp", now)
    cetes, age = etherfuse_cetes_price()
    log(f"   Etherfuse mainnet oracle: CETES {cetes / ONE:.7f} USD, {age}s old")
    check(cetes > 0 and age < 3600, "Etherfuse SEP-40 CETES price readable and fresh")
    invoke("agv-reporter", ORACLE, "set_price", "--reporter", reporter, "--asset", BOND_FEED,
           "--price", cetes, "--timestamp", now)
    lp = view(ORACLE, "lastprice", "--asset", TKUSD)
    check(int(lp["price"]) == tk_price, "SEP-40 lastprice returns the relayed tkUSD price")
    check(fails("agv-reporter", ORACLE, "set_price", "--reporter", reporter, "--asset", TKUSD,
                "--price", tk_price * 2, "--timestamp", now + 1), "reporter cannot move tkUSD by 100%")
    check(fails("agv-curator", ORACLE, "set_price", "--reporter", curator, "--asset", TKUSD,
                "--price", tk_price, "--timestamp", now + 1), "non-reporter cannot push a price")

    log("== phase 2: seed deposit and user deposits")
    invoke("agama", VAULT, "deposit", "--assets", USDC, "--receiver", owner, "--from", owner,
           "--operator", owner)
    deposits = {}
    for u in users:
        amt = rng.randint(20, usdc_balance(addr(u)) // USDC) * USDC
        before = num(view(VAULT, "preview_deposit", "--assets", amt))
        got = num(invoke(u, VAULT, "deposit", "--assets", amt, "--receiver", addr(u),
                         "--from", addr(u), "--operator", addr(u)))
        deposits[u] = amt
        check(got == before, f"{u} deposit {amt / USDC:.0f} USDC -> {got} agUSD = preview")
    total0 = num(view(VAULT, "total_assets"))
    check(total0 == sum(deposits.values()) + USDC, "total assets = deposits + seed")
    check(fails(users[0], VAULT, "deposit", "--assets", 10**15, "--receiver", addr(users[0]),
                "--from", addr(users[0]), "--operator", addr(users[0])), "deposit above balance fails")
    invariants(users, "deposits")

    log("== phase 3: allocation (caps, floor, real Etherfuse auth path)")
    bonds_amt = total0 * 30 // 100
    tenka_amt = total0 * 55 // 100
    check(fails("agv-operator", ENGINE, "allocate", "--adapter", BONDS, "--amount", USDC),
          "non-curator cannot allocate")
    check(fails("agv-curator", ENGINE, "allocate", "--adapter", BONDS, "--amount", total0 * 51 // 100),
          "bonds cap 50% enforced")
    invoke("agv-curator", ENGINE, "allocate", "--adapter", BONDS, "--amount", bonds_amt)
    # the operator buys CETES through Etherfuse (stood in by the test issuer)
    invoke("agv-operator", BONDS, "dispatch", "--usdc", bonds_amt)
    bonds = bonds_amt * ONE // cetes
    invoke("agv-issuer", BOND, "mint", "--to", operator, "--amount", bonds)
    check(fails("agv-operator", BONDS, "confirm_purchase", "--usdc", bonds_amt, "--bonds", bonds * 95 // 100),
          "CETES purchase 5% short of the oracle price refused")
    invoke("agv-operator", BONDS, "confirm_purchase", "--usdc", bonds_amt, "--bonds", bonds)
    check(num(view(BONDS, "bonds")) == bonds, f"adapter holds {bonds / ONE:.2f} CETES on-chain")
    bv = num(view(BONDS, "total_value"))
    check(abs(bv - bonds_amt) <= bonds_amt // 10_000, f"CETES position value {bv} ~ {bonds_amt}")
    check(fails("agv-curator", ENGINE, "allocate", "--adapter", TENKA,
                "--amount", total0 - bonds_amt - total0 * 9 // 100), "10% reserve floor enforced")
    invoke("agv-curator", ENGINE, "allocate", "--adapter", TENKA, "--amount", tenka_amt)
    check(fails("agv-curator", VAULT, "release", "--amount", USDC, "--to", curator),
          "vault.release refuses anyone but the engine")
    invariants(users, "allocated")

    log("== phase 4: Tenka cycle (operator)")
    check(fails("agv-curator", TENKA, "dispatch", "--amount", USDC), "non-operator cannot dispatch")
    invoke("agv-operator", TENKA, "dispatch", "--amount", tenka_amt)
    check(num(view(TENKA, "in_transit")) == tenka_amt, "USDC in transit to Ethereum")
    units = tenka_amt * ONE // tk_price // 10  # 7dp USDC -> 6dp tkUSD at price
    check(fails("agv-operator", TENKA, "confirm_subscription", "--usdc", tenka_amt,
                "--units", units * 90 // 100), "fill 10% off the oracle price refused")
    invoke("agv-operator", TENKA, "confirm_subscription", "--usdc", tenka_amt, "--units", units)
    tv = num(view(TENKA, "total_value"))
    check(abs(tv - tenka_amt) <= tenka_amt // 1_000_000 + 10, f"Tenka position value {tv} ~ {tenka_amt}")
    invariants(users, "tenka subscribed")

    log("== phase 5: yield")
    before = num(view(VAULT, "convert_to_assets", "--shares", USDC))
    now = int(time.time()) - 5
    invoke("agv-reporter", ORACLE, "set_price", "--reporter", reporter, "--asset", TKUSD,
           "--price", tk_price * 1003 // 1000, "--timestamp", now)
    invoke("agv-reporter", ORACLE, "set_price", "--reporter", reporter, "--asset", BOND_FEED,
           "--price", cetes * 1001 // 1000, "--timestamp", now)
    after = num(view(VAULT, "convert_to_assets", "--shares", USDC))
    check(after > before, f"agUSD price rose {before} -> {after}")
    invariants(users, "yield")

    log("== phase 6: sync exits within liquidity")
    small = users[1]
    mw = num(view(VAULT, "max_withdraw", "--owner", addr(small)))
    take = min(mw, 5 * USDC)
    bal0 = usdc_balance(addr(small))
    invoke(small, VAULT, "withdraw", "--assets", take, "--receiver", addr(small), "--owner", addr(small),
           "--operator", addr(small))
    check(usdc_balance(addr(small)) == bal0 + take, f"{small} withdrew {take} synchronously")
    liquid = num(view(VAULT, "liquid_assets"))
    big = max(users, key=lambda u: num(view(VAULT, "balance", "--account", addr(u))))
    check(num(view(VAULT, "max_withdraw", "--owner", addr(big))) <= liquid, "max_withdraw <= liquidity")
    check(fails(big, VAULT, "withdraw", "--assets", liquid + 1, "--receiver", addr(big),
                "--owner", addr(big), "--operator", addr(big)), "withdraw above liquidity fails")
    check(fails(users[2], VAULT, "redeem", "--shares", 10, "--receiver", addr(users[2]),
                "--owner", addr(big), "--operator", addr(users[2])), "cannot redeem someone else's shares")
    invariants(users, "sync exits")

    log("== phase 7: queue (big exits)")
    queued = []
    for u in sorted(users, key=lambda u: -num(view(VAULT, "balance", "--account", addr(u))))[:4]:
        sh_ = num(view(VAULT, "balance", "--account", addr(u)))
        rid = num(invoke(u, VAULT, "request_redeem", "--shares", sh_, "--owner", addr(u)))
        queued.append((u, rid, sh_))
    log(f"   queued {[(q[0], q[1]) for q in queued]}")
    check(num(view(VAULT, "max_withdraw", "--owner", addr(users[-1]))) == 0, "queue closes sync exits")
    check(fails("agv-curator", ENGINE, "allocate", "--adapter", BONDS, "--amount", USDC),
          "no allocation while redemptions are pending")
    # cancel the last one and requeue it
    cu, cid, csh = queued.pop()
    check(fails(users[0] if users[0] != cu else users[1], VAULT, "cancel_redeem", "--id", cid),
          "only the owner cancels")
    invoke(cu, VAULT, "cancel_redeem", "--id", cid)
    check(num(view(VAULT, "balance", "--account", addr(cu))) == csh, f"{cu} cancel returned the shares")
    rid = num(invoke(cu, VAULT, "request_redeem", "--shares", csh, "--owner", addr(cu)))
    queued.append((cu, rid, csh))
    processed = num(invoke("agv-curator", VAULT, "process_redeems", "--max", 10))
    log(f"   processed before unwinding: {processed}")
    invariants(users, "queued")

    log("== phase 8: unwind (bonds on-chain, Tenka via operator) and process")
    held_bonds = num(view(BONDS, "bonds"))
    invoke("agv-operator", BONDS, "dispatch_bonds", "--bonds", held_bonds)
    proceeds = held_bonds * (cetes * 1001 // 1000) // ONE
    mint(operator, proceeds)  # Etherfuse pays the sale
    invoke("agv-operator", BONDS, "settle_sale", "--bonds", held_bonds, "--usdc", proceeds)
    invoke("agv-curator", ENGINE, "deallocate", "--adapter", BONDS, "--amount", proceeds)
    check(num(view(BONDS, "total_value")) == 0, "CETES sold and USDC returned to the vault")
    held = num(view(TENKA, "units"))
    price_now = int(view(ORACLE, "lastprice", "--asset", TKUSD)["price"])
    redeem_units = held * 8 // 10
    usdc_back = redeem_units * price_now * 10 // ONE
    invoke("agv-operator", TENKA, "settle_redemption", "--units", redeem_units, "--usdc", usdc_back)
    invoke("agv-curator", ENGINE, "deallocate", "--adapter", TENKA, "--amount", usdc_back)
    processed = num(invoke("agv-curator", VAULT, "process_redeems", "--max", 10))
    check(processed >= 1, f"processed {processed} queued redemptions")
    invariants(users, "processed")
    paid = 0
    for u, rid, _ in queued:
        c = num(view(VAULT, "claimable", "--owner", addr(u)))
        if c:
            b0 = usdc_balance(addr(u))
            invoke(u, VAULT, "claim", "--owner", addr(u))
            check(usdc_balance(addr(u)) == b0 + c, f"{u} claimed {c / USDC:.4f} USDC")
            gain = c - deposits[u]
            log(f"   {u}: deposited {deposits[u] / USDC:.2f}, got {c / USDC:.4f} ({gain / USDC:+.4f})")
            paid += 1
    check(paid >= 1, "queued holders paid")
    invariants(users, "claimed")

    log("== phase 9: pause")
    invoke("agama", VAULT, "pause", "--caller", owner)
    u = users[-1]
    check(num(view(VAULT, "max_deposit", "--receiver", addr(u))) == 0, "paused: max_deposit = 0")
    check(fails(u, VAULT, "deposit", "--assets", USDC, "--receiver", addr(u), "--from", addr(u),
                "--operator", addr(u)), "paused: deposit fails")
    check(fails("agv-curator", VAULT, "pause", "--caller", curator), "only owner pauses")
    invoke("agama", VAULT, "unpause", "--caller", owner)
    invoke(u, VAULT, "deposit", "--assets", USDC, "--receiver", addr(u), "--from", addr(u),
           "--operator", addr(u))
    check(True, "unpaused: deposit works")

    log("== phase 10: stale price freezes valuation")
    invoke("agama", TENKA, "set_limits", "--max_age", 1, "--max_slippage_bps", 100)
    time.sleep(8)
    check(fails(u, VAULT, "deposit", "--assets", USDC, "--receiver", addr(u), "--from", addr(u),
                "--operator", addr(u)), "stale tkUSD price: deposit refused")
    invoke("agama", TENKA, "set_limits", "--max_age", 14 * 86400, "--max_slippage_bps", 100)
    invoke(u, VAULT, "deposit", "--assets", USDC, "--receiver", addr(u), "--from", addr(u),
           "--operator", addr(u))
    check(True, "limits restored: deposit works")
    invariants(users, "final")

    log(f"== done: {checks['pass']} passed, {checks['fail']} failed")
    with open("deployments/testnet-battery.log", "w") as f:
        f.write("\n".join(log_lines) + "\n")
    return 0 if checks["fail"] == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
