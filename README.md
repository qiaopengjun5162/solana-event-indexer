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
You can use a devnet RPC URL from another local project to verify the full pipeline end-to-end (configuration → DB → RPC → block fetch → parse → insert). Devnet has no Raydium CPMM swaps, so the `swaps` table will remain empty.

```bash
./target/release/solana-event-indexer \
  --rpc-url YOUR_DEVNET_RPC_URL \
  --single-slot 480833640
```

### Getting real mainnet swaps without a paid RPC

The block-by-block sync above needs an endpoint that can serve `getBlock` at
volume. For a quick real-data run, use the signature-based backfill instead —
it only calls `getSignaturesForAddress` and `getTransaction`, which the public
endpoint serves fine:

```bash
cargo run --release -- --backfill 300 \
  --rpc-url https://api.mainnet-beta.solana.com
```

It walks the recent transactions of the Raydium CPMM program, parses swaps, and
writes them to Postgres. Safe to re-run: a unique index on
`(signature, pool_id, token_in)` keeps duplicates out.

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

Find cross-pool spreads over the last 600 slots, requiring at least 0.3% net spread after fees and gas for a $1,000 trade:

```bash
cargo run --release -- --spread-report \
  --window-slots 600 --min-net-pct 0.3 --trade-size-usd 1000
```

The same report from an example (positional args), if you prefer:

```bash
cargo run --example find_opportunities -- 600 0.3 1000 150
```

Arguments:
- `600`: slot window
- `0.3`: minimum net spread percentage after fees and gas
- `1000`: trade size in USD
- `150`: SOL price used for the gas model

How the number is produced: each pool's quote is the volume-weighted average
price over the window, direction-normalised (a `B→A` swap is inverted, so one
pool never yields two prices for the same pair); pairs whose latest swaps are
more than `--max-slot-gap` apart are skipped as stale; then 25 bps × 2 sides of
CPMM fee and one transaction's gas are subtracted. `--min-swaps-per-pool`
(default 3) keeps thin pools out of the comparison.

## Features

- Historic block sync from a configurable start slot, then live mode every 2 seconds.
- Signature-based backfill that works on the public mainnet endpoint.
- Raydium CPMM swap extraction using instruction discriminator and vault indices.
- PostgreSQL storage with indexed columns for pool, token pair, and block time.
- Idempotent ingestion: re-running a backfill never duplicates swaps.
- Cross-pool spread analysis with a fee and gas cost model, in Rust, unit-tested.

## Architecture

- `src/lib.rs`: exposes the modules so the binary and the examples share one implementation.
- `src/parser.rs`: extracts `SwapEvent`s from transaction metadata.
- `src/backfill.rs`: `getSignaturesForAddress` + `getTransaction` ingestion; verifies the program account first.
- `src/spread.rs`: direction-normalised VWAP quotes and the opportunity model (fees, gas, slot proximity).
- `src/db.rs`: inserts swap events (dedupe via unique index) and reads the analysis window.
- `src/main.rs`: CLI — block sync by default, plus `--backfill` and `--spread-report` modes.
- `examples/analyze_swaps.rs`: lists recent swaps and price stats.
- `examples/find_opportunities.rs`: thin CLI over `src/spread.rs`.

## Database

Run `migrations/001_swaps.sql` then `migrations/002_dedupe_swaps.sql` against your Postgres database. The default database name is `solana_events`.

## License

MIT
