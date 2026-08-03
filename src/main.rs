mod config;
mod db;
mod logger;
mod models;
mod parser;

use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::Parser;
use dotenvy::dotenv;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcBlockConfig};
use solana_transaction_status::{EncodedTransaction, EncodedTransactionWithStatusMeta};
use tracing::info;

use crate::{config::Config, models::SwapEvent};

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
}

fn process_transaction(
    tx_with_meta: EncodedTransactionWithStatusMeta,
    slot: u64,
    block_time: Option<DateTime<Utc>>,
) -> Vec<SwapEvent> {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    logger::init_logger();

    let args = Args::parse();

    info!("Loading configuration...");
    let mut config = Config::load()?;
    if let Some(rpc_url) = args.rpc_url {
        config.rpc_url = rpc_url;
    }
    if let Some(start_slot) = args.start_slot {
        config.start_slot = start_slot;
    }
    info!("Configuration loaded");

    info!("Database URL: {}", config.database_url);
    info!("Start slot: {}", config.start_slot);
    info!("RPC URL: {}", config.rpc_url);

    let pool = db::init_pool(&config.database_url).await?;
    info!("Database pool initialized");

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
