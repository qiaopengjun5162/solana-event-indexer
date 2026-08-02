use std::{collections::HashMap, env};

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::{Duration, Utc};
use num_traits::ToPrimitive;
use sqlx::{Row, postgres::PgPoolOptions};

const RAYDIUM_FEE_BPS: f64 = 0.0025;
const SOL_PRICE_USD: f64 = 150.0;
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

struct Swap {
    pool_id: String,
    token_in: String,
    token_out: String,
    price: BigDecimal,
    slot: i64,
    priority_fee_lamports: i64,
}

struct Opportunity {
    token_in: String,
    token_out: String,
    buy_pool: String,
    sell_pool: String,
    buy_price: BigDecimal,
    sell_price: BigDecimal,
    gross_spread_pct: f64,
    net_spread_pct_1000_usd: f64,
    estimated_profit_usd: f64,
    buy_slot: i64,
    sell_slot: i64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/solana_events".into());
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;

    let minutes: i64 = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(5);
    let min_net_spread_pct: f64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(0.3);
    let trade_size_usd: f64 = env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(1000.0);

    let since = Utc::now() - Duration::minutes(minutes);

    let rows = sqlx::query(
        "SELECT pool_id, token_in, token_out, amount_in, amount_out, slot, priority_fee_lamports
         FROM swaps
         WHERE block_time >= $1 AND amount_in > 0 AND amount_out > 0",
    )
    .bind(since)
    .fetch_all(&pool)
    .await?;

    let mut by_pair: HashMap<(String, String), Vec<Swap>> = HashMap::new();
    for row in rows {
        let amount_in: BigDecimal = row.get("amount_in");
        let amount_out: BigDecimal = row.get("amount_out");
        if amount_in == 0 {
            continue;
        }
        let price = &amount_out / &amount_in;
        let swap = Swap {
            pool_id: row.get("pool_id"),
            token_in: row.get("token_in"),
            token_out: row.get("token_out"),
            price,
            slot: row.get("slot"),
            priority_fee_lamports: row.get::<i64, _>("priority_fee_lamports"),
        };
        by_pair
            .entry((swap.token_in.clone(), swap.token_out.clone()))
            .or_default()
            .push(swap);
    }

    let mut opportunities = Vec::new();
    for ((token_in, token_out), swaps) in by_pair {
        if swaps.len() < 2 {
            continue;
        }

        let min = swaps.iter().min_by(|a, b| a.price.cmp(&b.price)).unwrap();
        let max = swaps.iter().max_by(|a, b| a.price.cmp(&b.price)).unwrap();

        let buy_price_f64 = min.price.to_f64().unwrap_or(0.0);
        let sell_price_f64 = max.price.to_f64().unwrap_or(0.0);

        if buy_price_f64 == 0.0 {
            continue;
        }

        let gross_spread_pct = (sell_price_f64 - buy_price_f64) / buy_price_f64 * 100.0;

        let avg_priority_fee_lamports =
            (min.priority_fee_lamports + max.priority_fee_lamports) as f64 / 2.0;
        let gas_lamports = 50_000.0 + 3.0 * avg_priority_fee_lamports;
        let gas_cost_usd = (gas_lamports / LAMPORTS_PER_SOL) * 2.0 * SOL_PRICE_USD;

        let fees_pct = RAYDIUM_FEE_BPS * 2.0 * 100.0;
        let net_spread_pct_1000_usd =
            gross_spread_pct - fees_pct - (gas_cost_usd / trade_size_usd * 100.0);

        if net_spread_pct_1000_usd < min_net_spread_pct {
            continue;
        }

        let estimated_profit_usd = trade_size_usd * net_spread_pct_1000_usd / 100.0;

        opportunities.push(Opportunity {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            buy_pool: min.pool_id.clone(),
            sell_pool: max.pool_id.clone(),
            buy_price: min.price.clone(),
            sell_price: max.price.clone(),
            gross_spread_pct,
            net_spread_pct_1000_usd,
            estimated_profit_usd,
            buy_slot: min.slot,
            sell_slot: max.slot,
        });
    }

    opportunities
        .sort_by(|a, b| b.net_spread_pct_1000_usd.partial_cmp(&a.net_spread_pct_1000_usd).unwrap());

    for opp in &opportunities {
        println!(
            "{} -> {} | net={:.3}% @ ${:.0} | profit=${:.2} | gross={:.3}% | buy={:.6}@{} slot={} | sell={:.6}@{} slot={}",
            opp.token_in,
            opp.token_out,
            opp.net_spread_pct_1000_usd,
            trade_size_usd,
            opp.estimated_profit_usd,
            opp.gross_spread_pct,
            opp.buy_price,
            &opp.buy_pool[..8.min(opp.buy_pool.len())],
            opp.buy_slot,
            opp.sell_price,
            &opp.sell_pool[..8.min(opp.sell_pool.len())],
            opp.sell_slot,
        );
    }

    println!("\nTotal opportunities: {}", opportunities.len());

    Ok(())
}
