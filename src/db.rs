use sqlx::PgPool;

use crate::models::SwapEvent;

pub async fn init_pool(database_url: &str) -> anyhow::Result<PgPool> {
    Ok(PgPool::connect(database_url).await?)
}

pub async fn insert_swap(pool: &PgPool, swap: &SwapEvent) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO swaps
        (signature, slot, block_time, program_id, pool_id, token_in, token_out,
         amount_in_raw, amount_out_raw, amount_in, amount_out, priority_fee_lamports)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        "#,
    )
    .bind(&swap.signature)
    .bind(swap.slot as i64)
    .bind(swap.block_time)
    .bind(&swap.program_id)
    .bind(&swap.pool_id)
    .bind(&swap.token_in)
    .bind(&swap.token_out)
    .bind(swap.amount_in_raw as i64)
    .bind(swap.amount_out_raw as i64)
    .bind(&swap.amount_in)
    .bind(&swap.amount_out)
    .bind(swap.priority_fee_lamports as i64)
    .execute(pool)
    .await?;
    Ok(())
}
