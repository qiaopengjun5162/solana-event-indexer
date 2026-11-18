use bigdecimal::BigDecimal;
use num_traits::ToPrimitive;
use sqlx::{PgPool, Row};

use crate::models::SwapEvent;
use crate::spread::RawSwap;

pub async fn init_pool(database_url: &str) -> anyhow::Result<PgPool> {
    Ok(PgPool::connect(database_url).await?)
}

/// 幂等写入：靠 `ux_swaps_sig_pool_direction` 唯一索引做去重，
/// 返回 false 表示这条记录已经在库里（重复跑 backfill 不会再放大样本）。
pub async fn insert_swap(pool: &PgPool, swap: &SwapEvent) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"
        INSERT INTO swaps
        (signature, slot, block_time, program_id, pool_id, token_in, token_out,
         amount_in_raw, amount_out_raw, amount_in, amount_out, priority_fee_lamports)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (signature, pool_id, token_in) DO NOTHING
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
    Ok(result.rows_affected() > 0)
}

/// 取最近一段 slot 窗口内的成交，用于价差计算。
///
/// 用 slot 而不是 `block_time` 做窗口：老数据里 `block_time` 可能为空，
/// slot 一定有序且单调，不会出现「窗口里一条都没有」的假空。
pub async fn fetch_recent_swaps(pool: &PgPool, window_slots: i64) -> anyhow::Result<Vec<RawSwap>> {
    let rows = sqlx::query(
        r#"
        SELECT pool_id, token_in, token_out, amount_in, amount_out, slot, priority_fee_lamports
        FROM swaps
        WHERE slot >= (SELECT COALESCE(MAX(slot), 0) - $1 FROM swaps)
          AND amount_in > 0 AND amount_out > 0
        "#,
    )
    .bind(window_slots)
    .fetch_all(pool)
    .await?;

    let mut swaps = Vec::with_capacity(rows.len());
    for row in rows {
        let amount_in: BigDecimal = row.get("amount_in");
        let amount_out: BigDecimal = row.get("amount_out");
        let (Some(amount_in), Some(amount_out)) = (amount_in.to_f64(), amount_out.to_f64()) else {
            continue;
        };
        swaps.push(RawSwap {
            pool_id: row.get("pool_id"),
            token_in: row.get("token_in"),
            token_out: row.get("token_out"),
            amount_in,
            amount_out,
            slot: row.get::<i64, _>("slot"),
            priority_fee_lamports: row.get::<i64, _>("priority_fee_lamports"),
        });
    }
    Ok(swaps)
}

/// 库里的数据概况，报告前先看清楚样本有多大。
pub async fn stats(pool: &PgPool) -> anyhow::Result<(i64, i64, i64)> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS swaps, COUNT(DISTINCT pool_id) AS pools,
                COALESCE(MAX(slot), 0) AS max_slot FROM swaps",
    )
    .fetch_one(pool)
    .await?;
    Ok((row.get("swaps"), row.get("pools"), row.get("max_slot")))
}

/// 成交最多的池子地址，按成交笔数倒序。
///
/// 读储备是「一个池子三次账户查询」，不可能把全链几万个池子都拉一遍，
/// 所以按库里的活跃度取前 N 个。
pub async fn top_pools(pool: &PgPool, limit: i64) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT pool_id FROM swaps GROUP BY pool_id ORDER BY COUNT(*) DESC, pool_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|row| row.get("pool_id")).collect())
}
