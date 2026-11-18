//! 价差报告的命令行版本。
//!
//! 算法本体在 `src/spread.rs`，这里只负责读库和打印——早先这个文件里
//! 复制了一整套价差逻辑，导致示例和主程序两套实现各自演化。

use std::env;

use anyhow::Result;
use solana_event_indexer::{db, spread};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/solana_events".into());
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;

    // 用法：find_opportunities [window_slots] [min_net_pct] [trade_size_usd] [sol_price_usd]
    let window_slots: i64 = env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(600);
    let min_net_pct: f64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(0.3);
    let trade_size_usd: f64 = env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(1_000.0);
    let sol_price_usd: f64 = env::args().nth(4).and_then(|s| s.parse().ok()).unwrap_or(150.0);

    let raw = db::fetch_recent_swaps(&pool, window_slots).await?;
    if raw.is_empty() {
        println!("窗口内没有成交，先跑 `--backfill` 灌数据。");
        return Ok(());
    }

    let quotes = spread::build_quotes(&raw);
    let params = spread::Params {
        min_net_pct,
        trade_size_usd,
        sol_price_usd,
        ..Default::default()
    };
    let opportunities = spread::find_opportunities(&quotes, &params);

    println!(
        "窗口 {} slots：{} 笔成交，{} 个交易对，{} 个池子报价",
        window_slots,
        raw.len(),
        quotes.len(),
        quotes.values().map(|v| v.len()).sum::<usize>()
    );
    for o in &opportunities {
        println!("{}", o.render());
    }
    println!("\n合计 {} 个机会", opportunities.len());
    Ok(())
}
