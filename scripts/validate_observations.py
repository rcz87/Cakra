#!/usr/bin/env python3
"""
Hypothesis validator v2 — uses DexScreener API for truth.

Why v2: v1 used observation.spot_price as baseline, but that price was
computed by router::get_pumpfun_quote which has its own bug (uses creator
buy as virtual_sol_reserves). So v1 PnL numbers were comparing fake
prices to real prices.

v2 strategy:
  For each mint, query DexScreener:
    - Is token still alive? (has pair data)
    - What DEX is it on? (pumpfun = still bonding curve, raydium = migrated)
    - Current priceUsd / priceNative
    - 24h high (proxy for peak we could have caught)
    - 24h volume (proxy for activity)

  Classify outcome:
    1. DEAD     — no DexScreener data (rugged or zero activity)
    2. STAGNANT — alive but priceUsd ≈ creation price (no movement)
    3. PUMPED   — alive AND price movement happened (high - low > X%)
    4. MIGRATED — DEX = raydium-cpmm or similar (graduated)

  Compute hypothetical PnL using priceChange.h1 if available
  (price 1 hour ago vs now — closest proxy for "after observation")

NO TRADING. Read-only analysis.
"""

import json
import sqlite3
import sys
import time
import urllib.request
import urllib.error

DB_PATH = "/root/Cakra/data/ricoz-sniper.db"


def fetch_dexscreener(mint, retries=2):
    """Query DexScreener for a token. Returns dict or None."""
    url = f"https://api.dexscreener.com/latest/dex/tokens/{mint}"
    for attempt in range(retries):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
            with urllib.request.urlopen(req, timeout=10) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as e:
            if e.code == 429:
                time.sleep(2)
                continue
            return None
        except Exception:
            return None
    return None


def classify_token(mint, observed_at):
    """Returns dict with status, dex, current_price, peak_24h, etc."""
    data = fetch_dexscreener(mint)
    if not data:
        return {"status": "RPC_ERROR"}

    pairs = data.get("pairs") or []
    if not pairs:
        return {"status": "DEAD", "dex": None}

    # Take the most liquid pair
    p = sorted(pairs, key=lambda x: -(x.get("liquidity", {}).get("usd") or 0))[0]

    dex = p.get("dexId") or "?"
    price_usd = float(p.get("priceUsd") or 0)
    price_native = float(p.get("priceNative") or 0)  # SOL per token
    liquidity_usd = (p.get("liquidity") or {}).get("usd") or 0

    # Price change percentages from DexScreener
    price_change = p.get("priceChange") or {}
    change_5m = price_change.get("m5") or 0
    change_1h = price_change.get("h1") or 0
    change_6h = price_change.get("h6") or 0
    change_24h = price_change.get("h24") or 0

    # Volume
    volume = p.get("volume") or {}
    vol_h1 = volume.get("h1") or 0
    vol_h24 = volume.get("h24") or 0

    # Determine status
    if "raydium" in dex.lower() or "pump-amm" in dex.lower() or "pumpswap" in dex.lower():
        status = "MIGRATED"
    elif "pumpfun" in dex.lower() or "pump.fun" in dex.lower():
        if vol_h1 < 1:
            status = "STAGNANT"  # alive but no activity
        else:
            status = "ACTIVE"
    else:
        status = f"OTHER_DEX:{dex}"

    return {
        "status": status,
        "dex": dex,
        "price_usd": price_usd,
        "price_native": price_native,
        "liquidity_usd": liquidity_usd,
        "change_5m": change_5m,
        "change_1h": change_1h,
        "change_6h": change_6h,
        "change_24h": change_24h,
        "vol_h1": vol_h1,
        "vol_h24": vol_h24,
    }


def main():
    print(f"DB: {DB_PATH}")
    print(f"Source: DexScreener API")
    print()

    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()
    cur.execute("""
        SELECT id, mint, symbol, liquidity_sol, market_cap_sol, observed_at
        FROM observations
        ORDER BY observed_at ASC
    """)
    rows = cur.fetchall()
    conn.close()

    print(f"Validating {len(rows)} observations...")
    print()

    results = []
    for i, row in enumerate(rows):
        obs_id, mint, symbol, liq_obs, mcap_obs, observed_at = row
        print(f"[{i+1}/{len(rows)}] {(symbol or '?')[:15]:<15} {mint[:12]}... ", end="", flush=True)

        info = classify_token(mint, observed_at)
        info["mint"] = mint
        info["symbol"] = symbol
        info["liquidity_obs"] = liq_obs
        info["mcap_obs"] = mcap_obs
        info["observed_at"] = observed_at
        results.append(info)

        status = info["status"]
        if status in ("DEAD", "RPC_ERROR"):
            print(f"{status}")
        else:
            ch1h = info.get("change_1h", 0) or 0
            ch24h = info.get("change_24h", 0) or 0
            liq = info.get("liquidity_usd") or 0
            print(f"{status:<10} liq=${liq:>6.0f} 1h={ch1h:+5.1f}% 24h={ch24h:+5.1f}%")

        time.sleep(0.3)  # rate limit

    # ── Aggregate report ──
    print()
    print("=" * 65)
    print("VALIDATOR REPORT v2 (DexScreener-based)")
    print("=" * 65)
    print(f"Total observations: {len(results)}")
    print()

    # Status breakdown
    by_status = {}
    for r in results:
        by_status.setdefault(r["status"], []).append(r)

    print("Status breakdown:")
    for status in sorted(by_status.keys()):
        items = by_status[status]
        pct = len(items) / len(results) * 100
        print(f"  {status:<15} {len(items):>3} ({pct:>5.1f}%)")
    print()

    # For tokens we have price data on
    with_data = [r for r in results if r.get("price_usd")]

    if with_data:
        # Aggregate price changes
        changes_1h = [r["change_1h"] for r in with_data if r.get("change_1h") is not None]
        changes_24h = [r["change_24h"] for r in with_data if r.get("change_24h") is not None]

        if changes_1h:
            avg_1h = sum(changes_1h) / len(changes_1h)
            winners_1h = [c for c in changes_1h if c > 0]
            print(f"1h price change (n={len(changes_1h)}):")
            print(f"  Avg:      {avg_1h:+.1f}%")
            print(f"  Winners:  {len(winners_1h)} ({len(winners_1h)/len(changes_1h)*100:.0f}%)")
            print(f"  Best:     {max(changes_1h):+.1f}%")
            print(f"  Worst:    {min(changes_1h):+.1f}%")
            print()

        if changes_24h:
            avg_24h = sum(changes_24h) / len(changes_24h)
            winners_24h = [c for c in changes_24h if c > 0]
            print(f"24h price change (n={len(changes_24h)}):")
            print(f"  Avg:      {avg_24h:+.1f}%")
            print(f"  Winners:  {len(winners_24h)} ({len(winners_24h)/len(changes_24h)*100:.0f}%)")
            print(f"  Best:     {max(changes_24h):+.1f}%")
            print(f"  Worst:    {min(changes_24h):+.1f}%")
            print()

        # Top movers
        sorted_by_24h = sorted(with_data, key=lambda r: -(r.get("change_24h") or 0))
        print("Top 5 movers (24h):")
        for r in sorted_by_24h[:5]:
            print(f"  {(r['symbol'] or '?')[:15]:<15} {r['status']:<10} 24h={r.get('change_24h', 0):+.1f}%  liq=${r.get('liquidity_usd', 0):.0f}")
        print()

    # Profitability projection — modal-aware
    if with_data and changes_1h:
        # Realistic exit: 1h change * 0.6 (slippage drag)
        REALISTIC_FACTOR = 0.6
        realistic_pnl = avg_1h * REALISTIC_FACTOR

        print("Projection — modal 0.03 SOL/trade:")
        print(f"  Avg 1h price change:    {avg_1h:+.1f}%")
        print(f"  After 40% slippage drag: {realistic_pnl:+.1f}%")
        position = 0.03
        gross = position * (realistic_pnl / 100)
        fees = 0.011
        net = gross - fees
        print(f"  EV gross per trade:     {gross:+.6f} SOL")
        print(f"  Fees per trade:         -{fees:.6f} SOL")
        print(f"  EV net per trade:       {net:+.6f} SOL")
        print()
        if net > 0:
            print("  ✅ POSITIVE EV — strategy could work at modal 0.03")
        else:
            print("  ❌ NEGATIVE EV — fees > expected return at modal 0.03")
            if avg_1h * REALISTIC_FACTOR > 0:
                needed = fees / (avg_1h * REALISTIC_FACTOR / 100)
                print(f"     Would need position ≥ {needed:.4f} SOL to break even")

    # Save full results to JSON for further analysis
    out_path = "/tmp/validator_results.json"
    with open(out_path, "w") as f:
        json.dump(results, f, indent=2, default=str)
    print()
    print(f"Full results saved: {out_path}")


if __name__ == "__main__":
    main()
