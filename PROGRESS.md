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
| Database schema (`migrations/001_swaps.sql`) | Done | Not yet |
| Swap event model (`src/models.rs`) | Done | Yes (cargo check) |
| DB insertion (`src/db.rs`) | Done | Yes (cargo check) |
| Raydium CPMM parser (`src/parser.rs`) | Done | Yes (cargo check + clippy + unit tests) |
| Historic + live sync (`src/main.rs`) | Done | Yes (cargo check + clippy) |
| Docker compose setup | Done | Not yet |
| Swap listing example (`examples/analyze_swaps.rs`) | Done | Yes (cargo check + clippy) |
| Opportunity finder example (`examples/find_opportunities.rs`) | Done | Yes (cargo check + clippy) |
| `cargo deny` license/advisory check | Done | Yes (deny.toml updated for Solana transitive deps) |
| Real data ingestion | Not yet | - |
| Real spread detection | Not yet | - |

## Verification commands run

```bash
cargo fmt
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo deny check
```

All pass as of the latest commit.

## Known risks / next steps

- Parser unit tests cover exact-discriminator + vault-index parsing and the heuristic fallback. It has not yet been run against real Solana mainnet blocks.
- The exact Raydium CPMM swap discriminator and vault-index layout may drift with program upgrades.
- The heuristic fallback can misclassify non-Raydium DEX swaps as Raydium swaps.
- Gas cost model in `find_opportunities` uses hardcoded estimates (50k base + 3x priority fee lamports, $150 SOL price, 0.25% per side fee). These should be calibrated against real transactions.
- `deny.toml` now ignores several Solana SDK transitive advisories. These should be reviewed and removed as upstream crates release patched versions.
