# ibx vs ibapi (TWS API) — Order Types Gap Assessment

## Order Types

| Order Type | ibapi | ibx | Gap |
|---|---|---|---|
| Market (MKT) | Yes | Yes | — |
| Limit (LMT) | Yes | Yes | — |
| Stop (STP) | Yes | Yes | — |
| Stop Limit (STP LMT) | Yes | Yes | — |
| Trailing Stop (TRAIL) | Yes | Yes | — |
| Trailing Stop Limit (TRAIL LIMIT) | Yes | Yes | — |
| Trailing Stop Percent | Yes | Yes | — |
| Market on Close (MOC) | Yes | Yes | — |
| Limit on Close (LOC) | Yes | Yes | — |
| Market if Touched (MIT) | Yes | Yes | — |
| Limit if Touched (LIT) | Yes | Yes | — |
| Relative / Pegged to Primary (REL) | Yes | Yes | — |
| Market to Limit (MTL) | Yes | Yes | — (OrdType K, fills on paper) |
| Market with Protection (MKT PRT) | Yes | Yes* | Implemented, rejected on STK/SMART (futures-only?) |
| Stop with Protection (STP PRT) | Yes | Yes* | Implemented, rejected on STK/SMART (futures-only?) |
| Pegged to Market (PEG MKT) | Yes | Yes* | Implemented (OrdType E+ExecInst P), rejected — see ib-agent#44 |
| Pegged to Midpoint (PEG MID) | Yes | Yes* | Implemented (OrdType E+ExecInst M), rejected — see ib-agent#44 |
| Pegged to Stock (PEG STK) | Yes | **No** | Gap (options) |
| Pegged to Benchmark (PEG BENCH) | Yes | **No** | Gap (unknown companion tags, see ib-agent#39) |
| Mid-Price (MIDPRICE) | Yes | Yes* | Implemented (OrdType MIDPX), rejected — see ib-agent#44 |
| Volatility (VOL) | Yes | **No** | Gap (options, unknown FIX tags) |
| Auction (AUC) | Yes | **No** | Gap |
| Snap to Market | Yes | Yes* | Implemented (OrdType SMKT), rejected — see ib-agent#44 |
| Snap to Midpoint | Yes | Yes* | Implemented (OrdType SMID), rejected — see ib-agent#44 |
| Snap to Primary | Yes | Yes* | Implemented (OrdType SREL), rejected — see ib-agent#44 |
| Box Top | Yes | **No** | Gap (BOX exchange) |

## Time-in-Force

| TIF | ibapi | ibx | Gap |
|---|---|---|---|
| DAY | Yes | Yes | — |
| GTC | Yes | Yes (Limit, Stop, StopLimit) | — |
| IOC (Immediate or Cancel) | Yes | Yes (Limit) | — |
| FOK (Fill or Kill) | Yes | Yes (Limit) | — (IB rejects STK/SMART) |
| OPG (At the Opening) | Yes | Yes (Limit) | — |
| GTD (Good Til Date) | Yes | Yes (via SubmitLimitEx) | — |
| DTC (Day Til Cancelled) | Yes | Yes (via SubmitLimitEx) | — |

## Order Features / Attributes

| Feature | ibapi | ibx | Gap |
|---|---|---|---|
| Outside RTH | Yes | Yes (GTC Limit/Stop/StopLimit) | — |
| Order Modify (cancel/replace) | Yes | Yes | — |
| Order Cancel | Yes | Yes | — |
| Cancel All (per instrument) | Yes | Yes | — |
| Bracket Orders (parent+children) | Yes | Yes | — |
| OCA Groups (One-Cancels-All) | Yes | Yes (standalone + bracket) | — |
| Short Sell | Yes | Yes (Side::ShortSell) | — |
| Hidden Orders | Yes | Yes (via SubmitLimitEx) | — |
| Iceberg / Display Size | Yes | Yes (via SubmitLimitEx) | — |
| Good After Time | Yes | Yes (via SubmitLimitEx) | — |
| Good Til Date | Yes | Yes (via SubmitLimitEx) | — |
| Minimum Quantity | Yes | Yes (via SubmitLimitEx) | — |
| All or None | Yes | **No** | Gap (no confirmed FIX tag) |
| Discretionary Amount | Yes | **No** | Gap (no confirmed FIX tag) |
| Sweep to Fill | Yes | **No** | Gap (no confirmed FIX tag) |
| What-If (margin/commission preview) | Yes | **No** | Gap |
| Cash Quantity (notional orders) | Yes | **No** | Gap |
| Fractional Shares | Yes | **No** | Gap |
| Scale Orders | Yes | **No** | Gap |
| Hedge Orders (delta/beta/FX/pair) | Yes | **No** | Gap (options) |
| Adjustable Stops | Yes | **No** | Gap |
| Trigger Method (stop trigger logic) | Yes | **No** | Gap |
| Transmit Control (stage orders) | Yes | **No** | Gap |

## Conditional Orders

| Condition Type | ibapi | ibx |
|---|---|---|
| Price condition | Yes | **No** |
| Time condition | Yes | **No** |
| Volume condition | Yes | **No** |
| Margin condition | Yes | **No** |
| Execution condition | Yes | **No** |
| Percentage change condition | Yes | **No** |

## Algorithmic Orders

| Algo | ibapi | ibx |
|---|---|---|
| Adaptive | Yes | Yes |
| VWAP | Yes | **No** |
| TWAP | Yes | **No** |
| Arrival Price | Yes | **No** |
| Close Price | Yes | **No** |
| Dark Ice | Yes | **No** |
| % of Volume (+ variants) | Yes | **No** |
| Accumulate/Distribute | Yes | **No** |
| Balance Impact & Risk | Yes | **No** |
| Minimize Impact | Yes | **No** |
| Third-party algos (Fox River, QB) | Yes | **No** |

## Asset Class Coverage

| Asset | ibapi | ibx |
|---|---|---|
| Stocks (STK) | Yes | Yes |
| Options (OPT) | Yes | **No** |
| Futures (FUT) | Yes | **No** |
| Forex (CASH) | Yes | **No** |
| Bonds | Yes | **No** |
| Warrants | Yes | **No** |
| Combos/Spreads | Yes | **No** |

ibx is stock-only (hardcoded `167=STK`, exchange `SMART`, currency `USD`).

## Summary

| Category | ibapi | ibx | Coverage |
|---|---|---|---|
| Order types | 25+ | 22 (9 new, 8 rejected*) | ~88% |
| TIF options | 7 | 7 | **100%** |
| Order attributes | 20+ | 13 | ~65% |
| Algo strategies | 12+ | 1 | ~8% |
| Conditional orders | 6 types | 0 | 0% |
| Asset classes | 7+ | 1 | ~14% |

## Implementation History

### Round 1 — High-Priority Gaps (commits 703a667, 006fb0b, fdc3972)
1. Trailing Stop / Trailing Stop Limit
2. IOC / FOK
3. Bracket Orders + OCA Groups
4. MOC / LOC
5. MIT / LIT
6. GTC on Stop/StopLimit
7. Adaptive algo
8. Modify bug fix (preserve order type/TIF)

### Round 2 — Medium-Priority Gaps (commits c0083ed, cd7a6b0)
9. Relative (REL) — pegged-to-primary
10. OPG (At the Opening)
11. Extended limit (SubmitLimitEx): iceberg, hidden, GAT, GTD, outside RTH

### Round 3 — Remaining Equity Features (commits 7708060, 16b9daa, bf2ddfc)
12. Short sell (Side::ShortSell)
13. MinQty, DTC TIF
14. Trailing Stop Percent (tag 6268)
15. Standalone OCA groups (via OrderAttrs.oca_group)

### Round 4 — Tier 1 from issue #43 FIX tag mappings
16. Market to Limit (MTL, OrdType K) — **PASS (filled)**
17. Market with Protection (MKT PRT, OrdType U) — rejected (futures-only?)
18. Stop with Protection (STP PRT, OrdType SP) — rejected (futures-only?)
19. Mid-Price (MIDPX, OrdType MIDPX) — rejected, see ib-agent#44
20. Snap to Market (SMKT) — rejected, see ib-agent#44
21. Snap to Midpoint (SMID) — rejected, see ib-agent#44
22. Snap to Primary (SREL) — rejected, see ib-agent#44
23. Pegged to Market (OrdType E + ExecInst P) — rejected, see ib-agent#44
24. Pegged to Midpoint (OrdType E + ExecInst M) — rejected, see ib-agent#44
Note: Multi-char OrdType support via discriminant constants (ORD_STP_PRT, ORD_MIDPX, etc.)
Note: ord_type_fix_str() maps discriminants → FIX tag 40 strings for Modify handler
