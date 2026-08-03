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
| Database schema (`migrations/001_swaps.sql`) | Done | Yes (Docker + init_db.sh) |
| Swap event model (`src/models.rs`) | Done | Yes (cargo check) |
| DB insertion (`src/db.rs`) | Done | Yes (cargo check) |
| Raydium CPMM parser (`src/parser.rs`) | Done | Yes (cargo check + clippy + unit tests) |
| Historic + live sync (`src/main.rs`) | Done | Yes (cargo check + clippy + CLI args) |
| Docker compose setup | Done | Yes (postgres up) |
| Swap listing example (`examples/analyze_swaps.rs`) | Done | Yes (cargo check + clippy) |
| Opportunity finder example (`examples/find_opportunities.rs`) | Done | Yes (cargo check + clippy) |
| `cargo deny` license/advisory check | Done | Yes |
| Real data ingestion | Blocked | Devnet flow verified; needs paid mainnet RPC for Raydium swaps |
| Real spread detection | Not yet | - |

## Verification commands run

```bash
cargo fmt
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo deny check
docker compose up -d postgres
./scripts/init_db.sh
# cargo run --release blocked by expired RPC URLs in .env
./target/release/solana-event-indexer --rpc-url YOUR_DEVNET_RPC_URL --single-slot 480833640  # devnet flow verified
```

All checks pass; the release binary builds. Real ingestion is blocked by RPC credentials.

## Known risks / next steps

- Parser unit tests cover exact-discriminator + vault-index parsing and the heuristic fallback. The release binary starts, but real ingestion is blocked by the lack of a working mainnet RPC endpoint.
- The exact Raydium CPMM swap discriminator and vault-index layout may drift with program upgrades.
- The heuristic fallback can misclassify non-Raydium DEX swaps as Raydium swaps.
- Gas cost model in `find_opportunities` uses hardcoded estimates (50k base + 3x priority fee lamports, $150 SOL price, 0.25% per side fee). These should be calibrated against real transactions.
- `deny.toml` now ignores several Solana SDK transitive advisories. These should be reviewed and removed as upstream crates release patched versions.
- All RPC URLs found in `.env` and Solana CLI config are either expired (401) or public endpoints that time out. Use `--rpc-url` to pass a Helius/QuickNode/Alchemy mainnet RPC URL directly.
