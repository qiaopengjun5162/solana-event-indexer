use std::env;

use anyhow::Result;
use bigdecimal::BigDecimal;
use sqlx::{Row, postgres::PgPoolOptions};

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/solana_events".into());
    let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;

    let arg = env::args().nth(1).unwrap_or_else(|| "--recent".into());

    if arg == "--recent" {
        let rows = sqlx::query(
            "SELECT signature, slot, pool_id, token_in, token_out, amount_in, amount_out
             FROM swaps ORDER BY slot DESC LIMIT 50",
        )
        .fetch_all(&pool)
        .await?;

        for row in rows {
            println!(
                "slot={} | {} -> {} | pool={} | in={} out={} | sig={}",
                row.get::<i64, _>("slot"),
                row.get::<String, _>("token_in"),
                row.get::<String, _>("token_out"),
                row.get::<String, _>("pool_id"),
                row.get::<BigDecimal, _>("amount_in"),
                row.get::<BigDecimal, _>("amount_out"),
                row.get::<String, _>("signature"),
            );
        }
        return Ok(());
    }

    let rows = sqlx::query(
        "SELECT amount_in, amount_out, slot
         FROM swaps
         WHERE pool_id = $1 OR token_in = $1 OR token_out = $1
         ORDER BY slot DESC LIMIT 1000",
    )
    .bind(&arg)
    .fetch_all(&pool)
    .await?;

    if rows.is_empty() {
        println!("No swaps found for {}", arg);
        return Ok(());
    }

    let mut prices: Vec<(BigDecimal, i64)> = Vec::with_capacity(rows.len());
    for row in &rows {
        let amount_in: BigDecimal = row.get("amount_in");
        let amount_out: BigDecimal = row.get("amount_out");
        if amount_in != 0 {
            prices.push((&amount_out / &amount_in, row.get::<i64, _>("slot")));
        }
    }

    if prices.is_empty() {
        println!("No valid prices for {}", arg);
        return Ok(());
    }

    let min = prices.iter().min_by(|a, b| a.0.cmp(&b.0)).unwrap();
    let max = prices.iter().max_by(|a, b| a.0.cmp(&b.0)).unwrap();

    println!("Found {} swaps for {}", rows.len(), arg);
    println!("min price={} at slot {}", min.0, min.1);
    println!("max price={} at slot {}", max.0, max.1);

    Ok(())
}
