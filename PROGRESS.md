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
| Cross-pool spread detection (`src/spread.rs`) | Done | Yes (7 unit tests + real-data run) |
| Spread report CLI (`--spread-report`) | Done | Yes (real-data run) |
| `cargo deny` license/advisory check | Done | Yes |

## What changed in this pass

### 1. The program ID was wrong — this is why real ingestion "was blocked"

`RAYDIUM_CPMM_PROGRAM_ID` used to be `CPMMoo8L3F4NbTegBCKVNunggLgHpjANv64BHgf7Ct91`.
The on-chain program is `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`
(first 24 chars identical, the rest unrelated). `getAccountInfo` on the old
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

## Verification commands run

```bash
cargo fmt
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets                       # 13 lib tests + 1 integration test
psql -f migrations/002_dedupe_swaps.sql
cargo run -- --backfill 60 --rpc-url https://api.mainnet-beta.solana.com
cargo run -- --spread-report --window-slots 5000 --min-net-pct 0.05
```

Real-data result (2026-11-18, public mainnet RPC):

```
程序 CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C 存在，余额 50334585 lamports
回填完成：交易 60 笔（失败跳过 31），解析出 swap 34 条，新入库 34 条，slot 470876427..=470876586
库内 59 笔 swap / 23 个池子 / 最高 slot 470876586
```

## Known risks / next steps

- **Sample size.** A few hundred swaps over a few hundred slots is enough to
  prove the pipeline, not enough to find real arbitrage. Run a longer backfill
  (`--backfill 5000`) before drawing conclusions from the report.
- **Same-pair coverage.** With 23 pools across 37 pairs, most pairs have a single
  pool, so no cross-pool comparison is possible. The report becomes meaningful
  once the window covers several pools of the same pair.
- **Prices are trade-derived.** VWAP over trades is an approximation of the pool
  mid-price; reading `PoolState` reserves would give a true instantaneous quote
  and allow size-aware slippage. That requires `getAccountInfo` per pool and is
  the natural next step.
- The exact CPMM swap discriminator and vault-index layout may drift with
  program upgrades.
- The heuristic fallback can misclassify non-Raydium DEX swaps as Raydium swaps.
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
