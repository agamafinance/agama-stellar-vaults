#!/usr/bin/env python3
"""Agama keeper: price relays and redemption-queue processing.

    python3 scripts/keeper.py [--once] [--interval 300] [--deployment deployments/testnet.json]

Each round:

1. tkUSD: reads Tenka's Master Vault `convertToAssets(1e6)` on Ethereum
   (the per-share price, not the Tenka Oracle's fund-admin NAV) and pushes
   it to the `TKUSD` feed when it changed or when the last push is older
   than `--refresh`.
2. Stablebond: reads the bond price from Etherfuse's SEP-40 aggregator on
   Stellar mainnet and pushes it, with Etherfuse's own timestamp, when newer.
3. Queue: if redemptions are pending, simulates `process_redeems` and sends
   it when at least one request can be processed.
4. Health: simulates `total_assets`; a failure (stale price, missing feed)
   is reported as an alert.

A move larger than the feed's deviation bound is not pushed: the oracle
would refuse it, and it needs a human (`force_price` by the owner). The
keeper alerts instead.

Signers (stellar-cli keys): `--reporter` pushes prices, `--caller` processes
the queue (any funded account: `process_redeems` is permissionless).
"""

import argparse
import json
import subprocess
import sys
import time
import urllib.request

ONE = 10_000_000
BPS = 10_000
MAINNET_RPC = "https://mainnet.sorobanrpc.com"
MAINNET_PASSPHRASE = "Public Global Stellar Network ; September 2015"
ETHERFUSE_ORACLE = "CAVRP26CWW6IUEXBRA3Q2T2SHBUVBC2DF43M4E23LEZGW5ZEIB62HALS"
ETHERFUSE_DECIMALS = 14
SEPOLIA_RPC = "https://ethereum-sepolia-rpc.publicnode.com"
TENKA_MASTER_VAULT = "0x1EF979B3b1fFA92b86Ea10125a70462C4380eDab"
TKUSD = '{"Other":"TKUSD"}'


def log(level, msg):
    print(f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {level:5} {msg}", flush=True)


def sh(args):
    for attempt in range(4):
        p = subprocess.run(args, capture_output=True, text=True)
        if p.returncode == 0:
            return p.stdout.strip()
        err = p.stderr.strip()
        if not any(s in err for s in ("timeout", "Timeout", "502", "503", "connection", "TRY_AGAIN")):
            raise RuntimeError(err.splitlines()[-1] if err else "failed")
        time.sleep(4 * (attempt + 1))
    raise RuntimeError(err)


class Keeper:
    def __init__(self, a):
        d = json.load(open(a.deployment))
        self.net = d["network"]
        self.c = d["contracts"]
        self.bond_feed = json.dumps({"Stellar": d["bond"]}) if "bond" in d else None
        self.bond_mainnet = a.bond_mainnet
        self.reporter_key = a.reporter
        self.reporter = sh(["stellar", "keys", "address", a.reporter])
        self.caller = a.caller
        self.refresh = a.refresh
        self.dev_cache = {}

    def call(self, source, contract, fn, *args, send=True):
        cmd = ["stellar", "contract", "invoke", "--quiet", "--network", self.net, "--source", source,
               "--id", contract]
        if not send:
            cmd += ["--send", "no"]
        out = sh(cmd + ["--", fn, *[str(x) for x in args]])
        try:
            return json.loads(out) if out else None
        except json.JSONDecodeError:
            return out

    def view(self, contract, fn, *args):
        return self.call(self.caller, contract, fn, *args, send=False)

    def max_dev(self, feed):
        if feed not in self.dev_cache:
            self.dev_cache[feed] = int(self.view(self.c["oracle"], "feed_config", "--asset", feed)
                                       ["max_deviation_bps"])
        return self.dev_cache[feed]

    def push(self, feed, name, price, ts):
        last = self.view(self.c["oracle"], "lastprice", "--asset", feed)
        if last:
            lp, lt = int(last["price"]), int(last["timestamp"])
            if ts <= lt:
                return
            if lp == price and time.time() - lt < self.refresh:
                return
            dev = self.max_dev(feed)
            if dev and abs(price - lp) * BPS > lp * dev:
                log("ALERT", f"{name}: {lp / ONE:.7f} -> {price / ONE:.7f} exceeds {dev} bps; "
                             "needs owner force_price")
                return
        self.call(self.reporter_key, self.c["oracle"], "set_price", "--reporter", self.reporter,
                  "--asset", feed, "--price", price, "--timestamp", ts)
        log("INFO", f"{name}: pushed {price / ONE:.7f} @ {ts}")

    def tenka(self):
        data = "0x07a2d13a" + (1_000_000).to_bytes(32, "big").hex()
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "eth_call",
                           "params": [{"to": TENKA_MASTER_VAULT, "data": data}, "latest"]}).encode()
        req = urllib.request.Request(SEPOLIA_RPC, body, {"Content-Type": "application/json"})
        res = json.load(urllib.request.urlopen(req, timeout=20))
        price = int(res["result"], 16) * 10  # AUSD 6 dp -> 7 dp
        self.push(TKUSD, "tkUSD", price, int(time.time()) - 5)

    def etherfuse(self):
        if not self.bond_feed:
            return
        out = sh(["stellar", "contract", "invoke", "--quiet", "--rpc-url", MAINNET_RPC,
                  "--network-passphrase", MAINNET_PASSPHRASE, "--source", self.caller,
                  "--id", ETHERFUSE_ORACLE, "--send", "no", "--", "lastprice",
                  "--asset", json.dumps({"Stellar": self.bond_mainnet})])
        p = json.loads(out)
        price = int(p["price"]) // 10 ** (ETHERFUSE_DECIMALS - 7)
        self.push(self.bond_feed, "stablebond", price, min(int(p["timestamp"]), int(time.time()) - 5))

    def queue(self):
        pending = int(self.view(self.c["vault"], "pending_shares"))
        if pending == 0:
            return
        n = int(self.call(self.caller, self.c["vault"], "process_redeems", "--max", 20, send=False))
        if n == 0:
            head, tail = self.view(self.c["vault"], "queue_bounds")
            log("INFO", f"queue: {pending} shares pending in {tail - head} requests, waiting for liquidity")
            return
        n = int(self.call(self.caller, self.c["vault"], "process_redeems", "--max", 20))
        log("INFO", f"queue: processed {n} requests")

    def health(self):
        try:
            ta = int(self.view(self.c["vault"], "total_assets"))
            liquid = int(self.view(self.c["vault"], "liquid_assets"))
            log("INFO", f"vault: total {ta / ONE:,.2f} USDC, liquid {liquid / ONE:,.2f}")
        except RuntimeError as e:
            log("ALERT", f"vault valuation failing: {e}")

    def round(self):
        for step in (self.tenka, self.etherfuse, self.queue, self.health):
            try:
                step()
            except Exception as e:  # keep the loop alive, report the step
                log("ALERT", f"{step.__name__}: {e}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--deployment", default="deployments/testnet.json")
    ap.add_argument("--reporter", default="agv-reporter")
    ap.add_argument("--caller", default="agv-curator")
    ap.add_argument("--bond-mainnet", default="CAL6ER2TI6CTRAY6BFXWNWA7WTYXUXTQCHUBCIBU5O6KM3HJFG6Z6VXV",
                    help="Stablebond SAC on mainnet whose Etherfuse price is relayed (default CETES)")
    ap.add_argument("--refresh", type=int, default=6 * 3600,
                    help="re-push an unchanged price after this many seconds")
    ap.add_argument("--interval", type=int, default=300)
    ap.add_argument("--once", action="store_true")
    a = ap.parse_args()
    k = Keeper(a)
    while True:
        k.round()
        if a.once:
            return 0
        time.sleep(a.interval)


if __name__ == "__main__":
    sys.exit(main())
