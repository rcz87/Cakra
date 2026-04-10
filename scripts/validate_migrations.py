#!/usr/bin/env python3
"""
Migration sniping hypothesis validator.

For each migration observation in DB:
  1. Query DexScreener for current Raydium pool data
  2. Compute hypothetical PnL across multiple windows:
     - 5min after migration
     - 1h after migration
     - Current
  3. Aggregate by filter_passed (true/false) to see if filter is signal
  4. Output decision matrix

Goal: answer "if we had bought this migration, would we be in profit?"
NO TRADING. Read-only.
"""

import json
import sqlite3
import time
import urllib.request
from datetime import datetime, timezone, timedelta

DB_PATH = "/root/Cakra/data/ricoz-sniper.db"


def fetch_dexscreener(mint):
    url = f"https://api.dexscreener.com/latest/dex/tokens/{mint}"
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read())
    except Exception:
        return None


def classify(mint):
    data = fetch_dexscreener(mint)
    if not data:
        return None
    pairs = data.get("pairs") or []
    if not pairs:
        return {"status": "DEAD"}

    p = sorted(pairs, key=lambda x: -(x.get("liquidity", {}).get("usd") or 0))[0]
    dex = (p.get("dexId") or "").lower()
    pc = p.get("priceChange") or {}
    vol = p.get("volume") or {}
    liq = (p.get("liquidity") or {}).get("usd") or 0

    return {
        "status": "ALIVE",
        "dex": dex,
        "price_usd": float(p.get("priceUsd") or 0),
        "price_native": float(p.get("priceNative") or 0),
        "liquidity_usd": liq,
        "change_5m": pc.get("m5") or 0,
        "change_1h": pc.get("h1") or 0,
        "change_6h": pc.get("h6") or 0,
        "change_24h": pc.get("h24") or 0,
        "vol_h1": vol.get("h1") or 0,
        "vol_h24": vol.get("h24") or 0,
        "tx_h1_buys": (p.get("txns", {}).get("h1") or {}).get("buys") or 0,
        "tx_h1_sells": (p.get("txns", {}).get("h1") or {}).get("sells") or 0,
    }


def main():
    print(f"DB: {DB_PATH}")
    print()

    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()
    cur.execute("""
        SELECT id, mint, symbol, migration_pool, liquidity_sol,
               market_cap_sol, observed_at, filter_passed, filter_reason
        FROM observations
        WHERE is_migration = 1
        ORDER BY observed_at ASC
    """)
    rows = cur.fetchall()
    conn.close()

    if not rows:
        print("No migration observations yet.")
        print("Bot needs to run for a while in observe-only mode first.")
        print("Migrations happen ~1-5 per hour on PumpFun.")
        return

    print(f"Validating {len(rows)} migration observations...")
    print()

    results = []
    for i, row in enumerate(rows):
        obs_id, mint, symbol, pool, liq, mcap, observed_at, filter_passed, reason = row
        print(f"[{i+1}/{len(rows)}] {(symbol or '?')[:15]:<15} {mint[:12]}... ", end="", flush=True)

        info = classify(mint)
        if info is None:
            print("API error")
            continue

        info.update({
            "mint": mint,
            "symbol": symbol,
            "pool": pool,
            "liquidity_obs": liq,
            "mcap_obs": mcap,
            "observed_at": observed_at,
            "filter_passed": bool(filter_passed),
            "filter_reason": reason,
        })
        results.append(info)

        if info["status"] == "DEAD":
            print("DEAD")
        else:
            ch1h = info["change_1h"]
            ch5m = info["change_5m"]
            print(f"{info['dex']:<12} 5m={ch5m:+.1f}% 1h={ch1h:+.1f}% liq=${info['liquidity_usd']:.0f}")

        time.sleep(0.3)

    print()
    print("=" * 70)
    print("MIGRATION VALIDATOR REPORT")
    print("=" * 70)

    passed = [r for r in results if r["filter_passed"]]
    rejected = [r for r in results if not r["filter_passed"]]

    print(f"Total migrations observed: {len(results)}")
    print(f"  Passed our filter:    {len(passed)} ({len(passed)/len(results)*100:.0f}%)")
    print(f"  Rejected by filter:   {len(rejected)} ({len(rejected)/len(results)*100:.0f}%)")
    print()

    def stats(group, label):
        if not group:
            print(f"  {label}: no data")
            return
        alive = [r for r in group if r["status"] == "ALIVE"]
        dead = [r for r in group if r["status"] == "DEAD"]
        if not alive:
            print(f"  {label}: all dead ({len(dead)} tokens)")
            return
        ch1h = [r["change_1h"] for r in alive]
        ch5m = [r["change_5m"] for r in alive]
        winners_1h = [c for c in ch1h if c > 0]
        winners_5m = [c for c in ch5m if c > 0]
        avg_1h = sum(ch1h) / len(ch1h)
        avg_5m = sum(ch5m) / len(ch5m)

        print(f"  {label}:")
        print(f"    Alive:           {len(alive)}/{len(group)}")
        print(f"    5m winrate:      {len(winners_5m)}/{len(ch5m)} = {len(winners_5m)/len(ch5m)*100:.0f}%")
        print(f"    5m avg change:   {avg_5m:+.1f}%")
        print(f"    1h winrate:      {len(winners_1h)}/{len(ch1h)} = {len(winners_1h)/len(ch1h)*100:.0f}%")
        print(f"    1h avg change:   {avg_1h:+.1f}%")
        print(f"    Best 1h:         {max(ch1h):+.1f}%")
        print(f"    Worst 1h:        {min(ch1h):+.1f}%")

    print("Comparison: filter passed vs rejected")
    print()
    stats(passed, "FILTER PASSED")
    print()
    stats(rejected, "FILTER REJECTED")
    print()

    # Filter quality: signal-to-noise
    if passed and rejected:
        passed_alive = [r for r in passed if r["status"] == "ALIVE"]
        rejected_alive = [r for r in rejected if r["status"] == "ALIVE"]
        if passed_alive and rejected_alive:
            avg_p = sum(r["change_1h"] for r in passed_alive) / len(passed_alive)
            avg_r = sum(r["change_1h"] for r in rejected_alive) / len(rejected_alive)
            edge = avg_p - avg_r
            print(f"Filter edge (passed avg - rejected avg): {edge:+.1f}%")
            if abs(edge) < 2:
                print("⚠️  Filter doesn't differentiate (no signal)")
            elif edge > 0:
                print("✅ Filter selects better-than-random tokens")
            else:
                print("❌ Filter is anti-signal — rejected tokens do BETTER")
    print()

    # Save raw
    with open("/tmp/migration_validator.json", "w") as f:
        json.dump(results, f, indent=2, default=str)
    print(f"Full results: /tmp/migration_validator.json")


if __name__ == "__main__":
    main()
