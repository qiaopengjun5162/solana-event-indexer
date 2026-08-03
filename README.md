# Solana event indexer

A Solana block indexer that extracts swap events from on-chain transactions.

## Quick start

### Option 1: Docker Compose (recommended for local testing)

```bash
cp config/local.toml.example config/local.toml
# Edit config/local.toml with your RPC endpoint

docker compose up --build
```

This starts Postgres and the indexer. Migrations run automatically on first boot.

### Option 2: Native

1. Copy the local config template:

```bash
cp config/local.toml.example config/local.toml
# Edit config/local.toml with your RPC endpoint and database URL
```

2. Initialize the database:

```bash
./scripts/init_db.sh
```

3. Run the indexer:

```bash
cargo run --release
```

## Analyze opportunities
### CLI overrides

You can override the RPC URL and start slot from the command line without editing `.env` or config files:

```bash
# Single-slot test mode (useful for verifying the parser without a long sync)
cargo run --release -- --rpc-url https://mainnet.helius-rpc.com/?api-key=YOUR_API_KEY --single-slot 320000000

# Override start slot for historic sync
cargo run --release -- --start-slot 320000000
```

CLI arguments are also read from environment variables: `SOLANA_RPC_URL` and `SOLANA_START_SLOT`.

All analysis tools are written in Rust as cargo examples.

List recent swaps:

```bash
cargo run --example analyze_swaps -- --recent
```

Analyze a specific pool or token:

```bash
cargo run --example analyze_swaps -- <pool_id_or_token_mint>
```

Find cross-pool spreads from the last 5 minutes, requiring at least 0.3% net spread after fees and gas for a $1,000 trade:

```bash
cargo run --example find_opportunities -- 5 0.3 1000
```

Arguments:
- `5`: lookback minutes
- `0.3`: minimum net spread percentage after fees and gas
- `1000`: trade size in USD

## Features

- Historic block sync from a configurable start slot, then live mode every 2 seconds.
- Raydium CPMM swap extraction using instruction discriminator and vault indices.
- PostgreSQL storage with indexed columns for pool, token pair, and block time.
- Cross-pool spread analysis with fee and gas cost estimation in Rust.

## Architecture

- `src/parser.rs`: extracts `SwapEvent`s from transaction metadata.
- `src/db.rs`: inserts swap events into Postgres.
- `src/main.rs`: loops over slots, fetches blocks, parses swaps, and writes them to the database.
- `examples/analyze_swaps.rs`: lists recent swaps and price stats.
- `examples/find_opportunities.rs`: scans recent swaps for cross-pool price discrepancies with cost model.

## Database

Run `migrations/001_swaps.sql` against your Postgres database. The default database name is `solana_events`.

## License

MIT
