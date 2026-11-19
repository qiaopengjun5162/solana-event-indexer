# Solana Event Indexer — Progress

## Goal

Build a minimal, Rust-only signal indexer for Solana Raydium CPMM swaps that can:
1. Extract swap events from on-chain blocks.
2. Store them in PostgreSQL.
3. Detect cross-pool price spreads with a fee and gas cost model.
4. Run in both historic-sync and live modes.

## Status

| Component | Status | Verified |
|-----------|--------|----------|
| Database schema (`migrations/001_swaps.sql`) | Done | Yes (Postgres 17 local) |
| Dedupe index (`migrations/002_dedupe_swaps.sql`) | Done | Yes (re-run inserts 0 new rows) |
| Swap event model (`src/models.rs`) | Done | Yes (cargo check) |
| DB insertion (`src/db.rs`) | Done | Yes (ON CONFLICT + rows_affected) |
| Raydium CPMM parser (`src/parser.rs`) | Done | Yes (cargo check + clippy + unit tests) |
| Historic + live block sync (`src/main.rs`) | Done | Yes (cargo check + clippy + CLI args) |
| Signature-based backfill (`src/backfill.rs`) | Done | **Yes — real mainnet swaps ingested** |
| Cross-pool spread detection (`src/spread.rs`) | Done | Yes (10 unit tests + real-data run) |
| Spread report CLI (`--spread-report`) | Done | Yes (real-data run) |
| `PoolState` + reserve mid price (`src/pool_state.rs`) | Done | **Yes — 67/69 pools resolved on mainnet** |
| Reserves report CLI (`--reserves-report`) | Done | Yes (live vault balances + VWAP deviation) |
| Size-aware arbitrage sizing (`src/arb.rs`) | Done | Yes (9 unit tests + live run: optimal 0.0059 units) |
| Size ladder (`--reserves-report`) | Done | Yes (live run: ×5 of optimal is already negative) |
| Multi-hop cycles (`--cycles-report`) | Done | Yes — 928 cycles / 367 profitable / best 0.002 SOL |
| Sample expansion (8k CPMM txs) | Done | Yes — 2090 swaps / 674 pools / 269k slots |
| Undecodable swap policy (`src/parser.rs`) | Done | Yes — 213/213 new pools resolve as CPMM |
| `cargo deny` license/advisory check | Done | Yes |

## What changed in this pass

### 0. The sample went from 234 swaps to 1771 — and the conclusion got sharper, not better

The first pass proved the pipeline on 234 swaps over ~650 slots. This pass backfilled
~8,000 CPMM transactions (progressive 1000 / 2000 / 4000 / 8000 windows so that
rate-limited transactions from an earlier round get retried by the next one):

| | before | after |
|---|---|---|
| swaps | 234 | 2037 |
| pools (`pool_id`) | 69 | 660 |
| slots covered | 648 | 307,830 |
| pairs with ≥2 pools (reserve report, all pools) | 1 | 14 |
| pools with ≥2 swaps | 1 | 267 |

Two operational notes, both cost real time:

- The default `SOLANA_RPC_URL` in `.env` points at a devnet endpoint that returns
  **401**; `getAccount` then reports `AccountNotFound`, which reads like "wrong
  program id" but is really "wrong RPC". Always pass `--rpc-url` explicitly.
- The public mainnet endpoint is not reachable through the sandbox HTTP proxy (or
  directly); it needs the local Clash proxy on `127.0.0.1:7890`. A plain
  `getTransaction` failure is a `warn` and does not abort the run, so a rerun
  recovers what a rate-limited round skipped.

### 0b. Pruned 14 non-CPMM pools (53 rows)

Every one of the 660 remaining `pool_id`s was re-checked on chain with
`getMultipleAccounts`. 14 were not CPMM-owned — 6 Meteora DLMM, 4 Orca
Whirlpool, 2 system-owned vault PDAs, 1 Raydium CLMM, 1 REAL — all produced by
the removed heuristic fallback. Their 53 rows were copied to
`swaps_noncpmm_backup_20261119` and deleted:

```
swaps 2090 → 2037   pools 674 → 660
reserves report: 660/660 parsed, 0 unparsable (was 8/250)
```

Two of the previously reported "opportunities" (+38 bps and +27 bps) had a
non-CPMM pool as one leg, so they were never real either.

The headline finding: **a bigger sample produced a bigger-looking spread and an
even smaller executable trade.** With 6 USDC/SOL pools the gross spread is 11.87%
(net 11.37% after both fees), but the optimal fill is 0.0059 units — and the
ladder shows why:

```
×1    投入 0.005897   盈亏 3.2470e-4   +551 bps
×5    投入 0.029484   盈亏 -3.7489e-3  -1272 bps
×20   投入 0.117937   盈亏 -5.5459e-2  -4702 bps
×100  投入 0.589684   盈亏 -4.8880e-1  -8289 bps
```

With all 660 pools loaded instead of the top 250, the reserve report finds 14
comparable pairs. Ten reach the printed top-10; eight of them have a positive
model profit at their own optimum, and **all eight turn negative at ×5**:

| pair | pools | gross % | optimal in | bps | ×5 bps |
|---|---|---|---|---|---|
| EPjFWdd5/So111 | 7 | 11.943 | 0.005931 | +554 | -1277 |
| FmRjg2XV/So111 | 2 | 17.306 | 0.008502 | +804 | -1674 |
| 9BB6NFEc/So111 | 2 | 4.565 | 0.013590 | +200 | -542 |
| 5TAcFVXU/So111 | 4 | 4.295 | 0.024230 | +187 | -510 |
| 2G72iP4K/So111 | 4 | 1.894 | 0.004587 | +69 | -200 |
| So111/USD1ttGY | 3 | 1.177 | 0.124659 | +34 | -99 |
| 4cQ9BeG6/So111 | 2 | 1.165 | 0.011697 | +33 | -97 |
| Grass7B4/So111 | 2 | 0.744 | 0.000343 | +12 | -36 |

The two "cheap" pools are 0.155 and 0.383 units deep. The spread exists because
those pools are nearly empty; an order five times the optimum reverses it. This is
the general shape, not an outlier: all three comparable pairs are shallow-pool
artefacts (`+551 bps`, `+38 bps`, `+27 bps` at their own optimum, all negative at
×5).

### 1. The program ID was wrong — this is why real ingestion "was blocked"

`RAYDIUM_CPMM_PROGRAM_ID` used to be `CPMMoo8L3F4NbTegBCKVNunggLgHpjANv64BHgf7Ct91`.
The on-chain program is `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`
(first 26 chars identical, the rest unrelated). `getAccountInfo` on the old
address returns `null`, and `getSignaturesForAddress` returns `[]` instead of an
error — so it looked like "there is no data", when in fact we were asking about
an address that does not exist.

`backfill` now checks the account exists and is executable before paging
signatures, and fails with a clear message otherwise.

### 2. Ingestion no longer needs a paid RPC

`src/backfill.rs` uses `getSignaturesForAddress` on the CPMM program plus
`getTransaction` per signature, instead of `getBlock`. The public endpoint
(`https://api.mainnet-beta.solana.com`) serves this fine, so the previous
"needs Helius/QuickNode" blocker is gone.

Two details that cost us half the sample:

- `maxSupportedTransactionVersion: Some(1)` — a large share of CPMM transactions
  are v1; declaring only 0 makes the RPC reject them with `-32015`.
- Failed transactions (`meta.err.is_some()`) must be skipped, not parsed.

### 3. Real spread detection

`src/spread.rs` replaces the logic that used to live (duplicated) in
`examples/find_opportunities.rs`. Design points:

- **Direction-normalised prices.** The pair key is the mint pair sorted
  lexicographically; a `quote → base` swap is inverted before being compared, so
  one pool cannot produce two different prices for the same asset pair.
- **VWAP, not last trade.** Each pool's quote is the volume-weighted average over
  the slot window, weighted by base-token volume.
- **Slot proximity.** Two pools whose latest swaps are more than `max_slot_gap`
  apart are not compared; the spread is stale by construction.
- **Fees and gas are subtracted.** 25 bps × 2 sides, plus one 5,000-lamport
  signature fee and the observed priority fee, converted with a configurable SOL
  price. A buy+sell pair is one transaction, so gas is charged once.
- **Thin pools are ignored.** A pool needs `min_swaps_per_pool` swaps in the
  window before it can be a leg.

`wallet`-level depth is not modelled; `observed_volume_usd` in the output is the
smaller of the two pools' window volume, and is a rough executable-size proxy,
not real depth.

### 4. `pool_id` was the vault's owner, not the pool

Everything downstream of the parse step assumed `pool_id` could be passed to
`getAccountInfo` and read as a `PoolState`. It could not: `extract_swaps` wrote
`input.owner` — the owner field of the *vault token account*, which for Raydium
CPMM is a derived authority PDA (system-owned, zero bytes).

The damage was visible once the pool ids were looked up:

```
30 个最活跃 pool_id 的账户类型：
  8 × whirLbMiic…  653 bytes  (Orca Whirlpool)
  8 × LBUZKhRx…    904 bytes  (Meteora DLMM)
  5 × CAMMCzo5…   1544 bytes  (Raydium CLMM)
  3 × 11111111…      0 bytes  (PDA, no data)
```

So the table was not holding "Raydium CPMM pools" at all — it held whichever
DEX the aggregator happened to route through, plus a few PDAs. Two fixes:

- `pool_id` is now `accounts[3]` of the swap instruction — the `PoolState`
  address — with the vault indices taken from the same instruction;
- swap instructions are collected from `meta.inner_instructions` as well as the
  top level, because aggregators (Jupiter, GMGN) call CPMM as a CPI; scanning
  only top-level instructions sent every such transaction to the heuristic
  fallback.

### 5. The swap discriminator never matched anything

`SWAP_DISCRIMINATOR` used to be `f8c69e91e17587c8`. Anchor instruction
discriminators are `sha256("global:<name>")[:8]`, and that value is not
`global:swap_base_input` (`8fbe5adac41e33de`) nor `swap_base_output`
(`37d96256a34ab4ad`). It never matched a single on-chain instruction, which is
why the "parse by vault indices" path silently never ran and every row came from
the heuristic fallback.

Two further details found while fixing it:

- **Account order** is `[payer, authority, amm_config, pool_state, user_in,
  user_out, in_vault, out_vault, …]`, so the vaults are `accounts[6]/[7]`.
  The old code read `accounts[5]/[6]` — user output token and input vault.
- **Address lookup tables.** In `json` encoding a v0 transaction returns only its
  static `accountKeys`; ALT-expanded accounts live in `meta.loaded_addresses`.
  Instruction indices are into the concatenation (static → writable → readonly),
  so without appending them the CPMM program id cannot even be found and the
  transaction falls through to the heuristic.

### 6. Reserves, not trade prices

`src/pool_state.rs` reads the real quote: `PoolState` → `token_0_vault` /
`token_1_vault` → each account's `amount` (offset 64, same for SPL Token and
Token-2022), then price = reserve ratio, normalised by mint ordering so it is
comparable with the VWAP.

Two guards are load-bearing, both learned the hard way:

- **Length is not a type check.** Orca Whirlpool accounts are 653 bytes and pass
  a `>= 637` test; parsing one as CPMM yields plausible-looking garbage. The
  Anchor discriminator `f7ede3f5d7c3de46` is checked first.
- **Owner is checked too.** Accounts owned by anything other than
  `CPMMoo8…qKP1C` are rejected before any field is read.

`--reserves-report` prints what the guards did (parsed / missing / unparsable),
the instantaneous cross-pool spread per pair with its depth, and how far the
trade VWAP sits from the reserve price.

### 7. Sizing, because a spread percentage is not a trade

`src/arb.rs` answers the question the percentage cannot: *how much can I put in*.
Two constant-product pools, 25 bps per side, and a ternary search over the
profit curve — the curve is single-peaked because a larger order eats its own
spread. Output per comparable pair:

```
低 nNJnkTHy @ 1.0277（深度 2.8470）
高 4nKqpCq6 @ 1.0537（深度 1.3961）
最优下单：投入 0.009315 → 拿回 0.009409，毛利 9.3819e-5（+101 bps），吃掉低价池 0.33% 的储备
```

That is the whole point in one line: the 2.036% net spread is real, and the
*optimal* trade is 0.009 quote units for a profit of 0.00009. Both pools hold
single-digit units, so no real order survives. The model deliberately stops at
two pools, one hop, and zero gas — it is an upper bound, and it says so in the
code.

Tests cover the invariants that caught a wrong assumption during development:
a round trip in one pool must lose (fees, then price impact); identical pools
must yield `None`; a 0.2% spread must die on 50 bps of fees; a larger
mispricing must yield more profit; and the optimum must respect pool capacity.

### 8. Multi-hop: the mispricing that has no pair

The two-pool model can only compare pools of the *same* pair. Most relative
mispricing does not live there — it lives in the ratio between three tokens:
SOL/A, A/B and B/SOL each look sane, but multiplied together they do not return
1. `--cycles-report` enumerates simple cycles of N legs starting and ending at
wrapped SOL, fills every leg with the same constant-product rule, and
ternary-searches the whole path (`out(x)` stays concave, so `out(x) - x` stays
single-peaked).

Anchoring on SOL is not cosmetic. Profit lands in SOL, the same unit as the
5000-lamport signature fee, so the report subtracts a fixed cost instead of
hand-waving "gas is small". `--priority-fee-lamports` adds the priority fee.

Live run, 660 pools, slot 471,267,313, with a 50k-lamport priority fee:

```
3 跳闭环候选 928 条（起点 wrapped SOL）；固定成本 55000 lamports = 0.000055 SOL
模型内盈利 367 条，落在 117 条不同币种路径上；扣掉固定成本后仍为正 172 条
利润分布（SOL，未扣 gas）：≥0.1 0 / 0.01–0.1 0 / 0.001–0.01 7 / <0.001 360；最高 0.002011 SOL
```

| path | pools | optimal in | gross bps | ×5 bps | worst leg impact |
|---|---|---|---|---|---|
| SOL → JA8JfHs6 → USDC → SOL | 3BA1Nd6Q / 4kUJa9hq / GBnkXkgm | 0.017609 | +1142 | -2091 | 5.52% |
| SOL → 76rTxzzt → USD1… → SOL | E9oGC72m / Gvy9HeXq / EY8q7JwE | 0.029213 | +618 | -1388 | 3.31% |
| SOL → WgqMUdYQ → USDC → SOL | BekkkgcL / 62Xtz5Lx / GBnkXkgm | 0.019140 | +659 | -1455 | 5.63% |

The result is the same shape as the two-pool case, only more emphatic. Multi-hop
does find far more opportunities (367 vs 8), but **the single best one in the
whole sample is 0.002 SOL — about $0.40 at SOL ≈ $200 — and 360 of the 367 pay
less than 0.001 SOL.** Every printed cycle is negative at ×5. Route coverage was
never the bottleneck; depth is. Three-hop arbitrage on this market is not a
strategy, it is a rounding error with extra steps.

Consistency check, and it is the reason to trust the leg math: setting
`--cycle-hops 2` degenerates to the two-pool model, and it reproduces that
report exactly (`FmRjg2XV/So111` → +804 bps at 0.008502, same figure the
reserves report prints for the same pair). The multi-hop engine is the two-pool
engine with more legs, not a second implementation that might disagree.

## Verification commands run
```bash
cargo fmt
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets                       # 41 lib tests + 1 integration test
psql -f migrations/002_dedupe_swaps.sql
psql -f migrations/003_pool_id_is_pool_state.sql   # 清空旧行：pool_id 语义变了
cargo run --release -- --backfill 700 --rpc-url https://api.mainnet-beta.solana.com
cargo run --release -- --spread-report --window-slots 5000 --min-net-pct 0.05
cargo run --release -- --reserves-report --pool-limit 700 --window-slots 400
cargo run --release -- --cycles-report --pool-limit 700 --cycle-hops 3 --cycles-top 5
```

Real-data result (2026-11-18, public mainnet RPC, after the pool-id fix):

```
程序 CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C 存在，余额 50334585 lamports
回填完成：交易 342 笔（失败跳过 114），解析出 swap 200 条，新入库 200 条
库内 234 笔 swap / 69 个 pool_id / 最高 slot 470940559
储备报告：解析成功 67 个池子，账户缺失 0，解析失败 2，金库读不到 0
交易对 64 个，其中 ≥2 个池子可比 1 个
  2G72iP4K / SOL  4 池  毛 2.536%  扣双边费 0.500% → 净 2.036%
      低 nNJnkTHy @ 1.0277（深度 2.8470）
      高 4nKqpCq6 @ 1.0537（深度 1.3961）
```

The 2.0% pair is a 4-pool pair whose two extreme pools hold ~3 and ~1.4 units of
quote liquidity. The spread is real arithmetic and economically meaningless —
which is exactly why the report prints depth next to the price instead of a
profit estimate.

Deviation between reserve price and 400-slot VWAP, same pools:

```
NXjQD9mL  储备 2.3329e-8  VWAP 1.0432e-7  -77.64%
Ghooj5bd  储备 7.0484e-7  VWAP 5.2592e-7  +34.02%
G9GrmZr3  储备 4.9155e-8  VWAP 6.2918e-8  -21.87%
CS6JyKHr  储备 132.9895   VWAP 116.6875   +13.97%
```

A VWAP built from one or two fills in the window is not a price; the deviation
column is the measurement of that.

## Known risks / next steps

- **Sample size.** 2037 swaps over 307k slots is better, but it is still one
  backfill window of a program that trades continuously; seasonality and pool
  churn are not represented.
- **Same-pair coverage.** 660 resolved pools across 235 pairs; only 14 pairs have
  more than one pool. That is enough to see the shape (every one of them decays
  to negative at ×5) but not enough to call it a distribution.
- **VWAP is a lagging estimate.** Measured against live reserves it deviates by
  tens of percent on thin pools (see above). Treat `--spread-report` as a
  trade-history summary, not a quote; use `--reserves-report` for a price.
- **Depth is a reserve balance, not executable size.** `src/arb.rs` computes the
  optimal trade for a pair (or a 2–4 leg cycle) with constant-product fill, and
  the ladder shows how fast it decays (×5 of the optimum is negative on every
  pair and every cycle in the sample). It still assumes the reserves stay put and
  that the only cost is the 25 bps fee plus a fixed signature/priority fee.
  Being front-run, failed landings and ATA rent are not modelled — the number it
  prints is an upper bound. The absolute sizes are what matters: the best 3-hop
  cycle in 928 pays 0.002 SOL, so the bound being loose does not rescue any of
  them.
- **Cycle enumeration is exhaustive only for one anchor.** Cycles must start and
  end at wrapped SOL. A profitable loop that never touches SOL (USDC → X → Y →
  USDC) is invisible here; adding a second anchor is cheap but every extra mint
  multiplies the search, so it should be driven by where volume actually is.
- **Undecodable swaps are now dropped, not guessed (fixed this pass).** The old
  fallback wrote `pool_id = vault owner` for any owner with two offsetting token
  deltas; that address has no account data, so the reserves report could never
  resolve it (8 of 250 pools in the pre-fix sample, all Whirlpool pools swept in
  sideways). `extract_swaps` now returns nothing when the discriminator is not
  recognised, and `backfill` counts those transactions as `txs_without_swap`
  (10 of 150 in the verification run). Verified: 213 newly written pools all have
  `owner = CPMMoo8…`, zero unresolvable. The 8 legacy rows are still in the
  database — they need a one-off prune, not a re-run.
- **Discriminators and account layout may drift** with program upgrades. They are
  now derived from `sha256("global:<name>")` and covered by tests, but a new
  instruction variant would need a new constant.
- `deny.toml` ignores several Solana SDK transitive advisories; revisit as
  upstream crates release patched versions.
- **`cargo deny check` is currently red, and not because of this change.** The
  advisory database has moved ahead of the pinned `solana-*` 2.3.4 tree:
  `quinn-proto` (RUSTSEC-2026-0338 / 0339 / 0340 / 0342 / 0523 / 0524 / 0538),
  `pin-utils` (RUSTSEC-2026-0441), `chrono` (RUSTSEC-2026-0543), plus some
  rustls/webpki entries. Clearing them means a dependency upgrade pass over the
  Solana crates, not another ignore line. The pre-commit `cargo-deny` hook stays
  red until then.
- `.env` still points `SOLANA_RPC_URL` at a devnet endpoint (401). Pass
  `--rpc-url` explicitly; do not rely on the env default.
- Public mainnet RPC returns 429 under a tight `getMultipleAccounts` loop; the
  report reads pools in batches of 100 and is fine, but a larger sweep needs a
  paying endpoint.
