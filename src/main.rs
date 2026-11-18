use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::Parser;
use dotenvy::dotenv;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcBlockConfig;
use solana_transaction_status::{EncodedTransaction, EncodedTransactionWithStatusMeta};
use tracing::info;

use solana_event_indexer::parser::RAYDIUM_CPMM_PROGRAM_ID;
use solana_event_indexer::{backfill, config::Config, db, logger, parser, spread};

#[derive(Parser)]
#[command(name = "solana-event-indexer")]
#[command(about = "Index Raydium CPMM swap events from Solana blocks")]
struct Args {
    /// RPC URL to use. Overrides config and SOLANA_RPC_URL.
    #[arg(long, env = "SOLANA_RPC_URL")]
    rpc_url: Option<String>,
    /// Start slot for historic sync. Overrides config and SOLANA_START_SLOT.
    #[arg(long, env = "SOLANA_START_SLOT")]
    start_slot: Option<u64>,
    /// Only fetch a single slot and exit (useful for testing the parser).
    #[arg(long)]
    single_slot: Option<u64>,

    /// Backfill recent transactions of the CPMM program via
    /// getSignaturesForAddress + getTransaction, then exit. Value = max txs.
    #[arg(long)]
    backfill: Option<usize>,
    /// Stop backfilling once this many new swaps are stored.
    #[arg(long, default_value_t = 200)]
    min_swaps: usize,
    /// Program to backfill from.
    #[arg(long, default_value_t = RAYDIUM_CPMM_PROGRAM_ID.to_string())]
    program_id: String,

    /// Print cross-pool spread report from stored swaps and exit.
    #[arg(long)]
    spread_report: bool,
    /// Slot window used by the spread report.
    #[arg(long, default_value_t = 600)]
    window_slots: i64,
    /// Minimum net spread (percent) to report.
    #[arg(long, default_value_t = 0.3)]
    min_net_pct: f64,
    /// Notional used to convert spread into profit.
    #[arg(long, default_value_t = 1000.0)]
    trade_size_usd: f64,
    /// SOL price used for the gas cost model.
    #[arg(long, default_value_t = 150.0)]
    sol_price_usd: f64,
    /// Max slot difference between the two pool quotes.
    #[arg(long, default_value_t = 20)]
    max_slot_gap: i64,
    /// Minimum swaps per pool before it can be compared.
    #[arg(long, default_value_t = 3)]
    min_swaps_per_pool: usize,
}

fn process_transaction(
    tx_with_meta: EncodedTransactionWithStatusMeta,
    slot: u64,
    block_time: Option<DateTime<Utc>>,
) -> Vec<solana_event_indexer::models::SwapEvent> {
    let signature = match &tx_with_meta.transaction {
        EncodedTransaction::Json(tx) => tx.signatures.first().cloned().unwrap_or_default(),
        _ => return Vec::new(),
    };

    let Some(meta) = tx_with_meta.meta else {
        return Vec::new();
    };

    parser::extract_swaps(
        &signature,
        slot,
        block_time,
        &meta,
        &tx_with_meta.transaction,
    )
}

async fn process_slot(
    client: &RpcClient,
    pool: &sqlx::PgPool,
    slot: u64,
    config: &RpcBlockConfig,
) -> anyhow::Result<()> {
    let block = client.get_block_with_config(slot, *config)?;
    let block_time = block.block_time.and_then(DateTime::from_timestamp_secs);

    if let Some(txs) = block.transactions {
        for tx_with_meta in txs {
            for swap in process_transaction(tx_with_meta, slot, block_time) {
                if let Err(e) = db::insert_swap(pool, &swap).await {
                    tracing::error!("Failed to insert swap: {}", e);
                }
            }
        }
    }

    Ok(())
}

async fn report_spread(pool: &sqlx::PgPool, args: &Args) -> anyhow::Result<()> {
    let (swaps, pools, max_slot) = db::stats(pool).await?;
    if swaps == 0 {
        println!("数据库里还没有 swap，先跑 `--backfill 200` 灌一批真实数据。");
        return Ok(());
    }

    let raw = db::fetch_recent_swaps(pool, args.window_slots).await?;
    let quotes = spread::build_quotes(&raw);
    let params = spread::Params {
        sol_price_usd: args.sol_price_usd,
        max_slot_gap: args.max_slot_gap,
        min_net_pct: args.min_net_pct,
        trade_size_usd: args.trade_size_usd,
        min_swaps_per_pool: args.min_swaps_per_pool,
        ..Default::default()
    };
    let opportunities = spread::find_opportunities(&quotes, &params);

    println!(
        "库内 {} 笔 swap / {} 个池子 / 最高 slot {}",
        swaps, pools, max_slot
    );
    println!(
        "窗口 {} slots 内 {} 笔成交，覆盖 {} 个交易对",
        args.window_slots,
        raw.len(),
        quotes.len()
    );
    println!(
        "门槛：净价差 ≥ {}%，双边手续费 {} bps，交易额 ${}，slot 差 ≤ {}",
        args.min_net_pct,
        spread::RAYDIUM_CPMM_FEE_BPS as i64,
        args.trade_size_usd,
        args.max_slot_gap
    );
    println!();

    if opportunities.is_empty() {
        println!("没有超过门槛的跨池价差。");
        return Ok(());
    }
    for o in &opportunities {
        println!("{}", o.render());
    }
    println!("\n合计 {} 个机会", opportunities.len());
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    logger::init_logger();

    let args = Args::parse();

    info!("Loading configuration...");
    let mut config = Config::load()?;
    if let Some(rpc_url) = args.rpc_url.clone() {
        config.rpc_url = rpc_url;
    }
    if let Some(start_slot) = args.start_slot {
        config.start_slot = start_slot;
    }
    info!("Configuration loaded");

    info!("Database URL: {}", config.database_url);

    let pool = db::init_pool(&config.database_url).await?;
    info!("Database pool initialized");

    if args.spread_report {
        return report_spread(&pool, &args).await;
    }

    if let Some(max_txs) = args.backfill {
        info!(
            "Backfilling up to {} transactions from {}",
            max_txs, args.program_id
        );
        let client = solana_client::nonblocking::rpc_client::RpcClient::new(config.rpc_url.clone());
        let stats =
            backfill::backfill_recent(&client, &pool, &args.program_id, max_txs, args.min_swaps)
                .await?;
        println!(
            "回填完成：交易 {} 笔（失败跳过 {}），解析出 swap {} 条，新入库 {} 条，slot {}..={}",
            stats.transactions,
            stats.skipped_failed,
            stats.swaps_parsed,
            stats.swaps_inserted,
            stats.slots.0,
            stats.slots.1
        );
        return Ok(());
    }

    info!("Start slot: {}", config.start_slot);
    info!("RPC URL: {}", config.rpc_url);

    let client = RpcClient::new(config.rpc_url.clone());
    info!("Connecting to Solana RPC at: {}", client.url());

    let latest_slot = client.get_slot()?;
    info!("Chain tip is at slot: {}", latest_slot);
    if latest_slot < config.start_slot {
        anyhow::bail!(
            "start slot {} is ahead of chain tip {} — choose a lower start_slot or use a synced endpoint",
            config.start_slot,
            latest_slot
        );
    }

    let rpc_block_config = RpcBlockConfig {
        max_supported_transaction_version: Some(0),
        ..Default::default()
    };

    if let Some(slot) = args.single_slot {
        info!("Single-slot mode: processing slot {}", slot);
        process_slot(&client, &pool, slot, &rpc_block_config).await?;
        info!("Single-slot mode finished");
        return Ok(());
    }

    let mut current_slot = config.start_slot;
    if current_slot <= latest_slot {
        info!(
            "Starting historic sync from slot {} up to {}",
            current_slot, latest_slot
        );
        while current_slot <= latest_slot {
            if let Err(e) = process_slot(&client, &pool, current_slot, &rpc_block_config).await {
                tracing::warn!("Could not fetch block {}: {}", current_slot, e);
            }
            current_slot += 1;
        }
        info!("Historic sync finished");
    }

    info!("Switching to live mode");
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    let mut last_processed = latest_slot;

    loop {
        interval.tick().await;

        match client.get_slot() {
            Ok(latest) => {
                if latest > last_processed {
                    for slot in (last_processed + 1)..=latest {
                        if let Err(e) = process_slot(&client, &pool, slot, &rpc_block_config).await
                        {
                            tracing::warn!("Could not fetch block {}: {}", slot, e);
                        }
                    }
                    last_processed = latest;
                }
            }
            Err(e) => {
                tracing::warn!("Failed to get latest slot: {}", e);
            }
        }
    }
}
