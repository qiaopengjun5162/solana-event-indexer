//! 基于 `getSignaturesForAddress` + `getTransaction` 的回填。
//!
//! 为什么不用 `getBlock`：公共 RPC 对 getBlock 限得很死，而且一次要多兆数据；
//! 按程序地址拿签名再逐笔取交易，用的是免费额度能吃下的接口，
//! 在只有公共 endpoint 的情况下也能灌进真实数据。

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction_status::{EncodedTransactionWithStatusMeta, UiTransactionEncoding};
use sqlx::PgPool;
use tracing::{info, warn};

use crate::db;
use crate::parser;

#[derive(Debug, Default)]
pub struct BackfillStats {
    pub signatures_seen: usize,
    pub transactions: usize,
    pub skipped_failed: usize,
    pub swaps_parsed: usize,
    pub swaps_inserted: u64,
    /// 成功交易里一条 swap 都没解析出来的笔数。
    ///
    /// 这些交易要么压根不是在 swap（加/撤流动性、创建池子），要么是新的指令
    /// 变体（判别码还没覆盖）。以前它们会被启发式兜底写成 pool_id = 金库 owner
    /// 的脏行，现在只计数、不落库——这个数字变大就是「该补解析了」的信号。
    pub txs_without_swap: usize,
    pub slots: (u64, u64),
}

/// 从程序地址往回抓最近的交易，解析出 swap 写入数据库。
///
/// * `max_txs` —— 最多取多少笔交易（控制免费 RPC 的调用次数）
/// * `min_swaps` —— 攒够多少条 swap 就提前停止
pub async fn backfill_recent(
    client: &RpcClient,
    pool: &PgPool,
    program_id: &str,
    max_txs: usize,
    min_swaps: usize,
) -> Result<BackfillStats> {
    let program = Pubkey::from_str(program_id).context("invalid program id")?;

    // 先确认这个地址在链上真的存在且是可执行程序。
    // 地址写错时 getSignaturesForAddress 只会安静地返回空数组，
    // 看起来像「没有数据」，实际是「查错了地方」。
    match client.get_account(&program).await {
        Ok(account) if account.executable => {
            info!(
                "程序 {} 存在，余额 {} lamports",
                program_id, account.lamports
            );
        }
        Ok(_) => anyhow::bail!("地址 {} 存在但不是可执行程序，检查 program id", program_id),
        Err(e) => anyhow::bail!(
            "地址 {} 在链上查不到（{}）。先确认 program id 是否正确",
            program_id,
            e
        ),
    }
    // slots.0 用 MAX 起头，才能用 min() 求出真实最小 slot
    let mut stats = BackfillStats {
        slots: (u64::MAX, 0),
        ..Default::default()
    };
    let mut before: Option<Signature> = None;
    let mut seen: HashSet<String> = HashSet::new();

    'outer: loop {
        let page = client
            .get_signatures_for_address_with_config(
                &program,
                GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(50),
                    commitment: None,
                },
            )
            .await
            .context("getSignaturesForAddress failed")?;

        if page.is_empty() {
            break;
        }
        info!("拿到 {} 条签名", page.len());

        // 接口按时间倒序返回；翻成顺序处理，让 slot 窗口从旧到新增长
        for confirmed in page.iter().rev() {
            if stats.transactions >= max_txs {
                break 'outer;
            }
            if !seen.insert(confirmed.signature.clone()) {
                continue;
            }
            stats.signatures_seen += 1;
            stats.slots.0 = stats.slots.0.min(confirmed.slot);
            stats.slots.1 = stats.slots.1.max(confirmed.slot);

            let Ok(signature) = Signature::from_str(&confirmed.signature) else {
                continue;
            };
            let tx = match client
                .get_transaction_with_config(
                    &signature,
                    RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Json),
                        commitment: None,
                        // CPMM 的交易里有相当一部分是 v1 交易，只声明 0 会被
                        // RPC 以 -32015 拒绝，白白丢掉一半样本
                        max_supported_transaction_version: Some(1),
                    },
                )
                .await
            {
                Ok(tx) => tx,
                Err(e) => {
                    warn!("取交易 {} 失败: {}", confirmed.signature, e);
                    continue;
                }
            };
            stats.transactions += 1;

            let EncodedTransactionWithStatusMeta {
                transaction, meta, ..
            } = tx.transaction;
            let Some(meta) = meta else { continue };
            if meta.err.is_some() {
                stats.skipped_failed += 1;
                continue;
            }

            let block_time = tx.block_time.and_then(chrono::DateTime::from_timestamp_secs);
            let swaps = parser::extract_swaps(
                &confirmed.signature,
                tx.slot,
                block_time,
                &meta,
                &transaction,
            );
            stats.swaps_parsed += swaps.len();
            if swaps.is_empty() {
                stats.txs_without_swap += 1;
            }

            for swap in &swaps {
                match db::insert_swap(pool, swap).await {
                    Ok(true) => stats.swaps_inserted += 1,
                    Ok(false) => {}
                    Err(e) => warn!("写入失败: {}", e),
                }
            }

            if stats.swaps_inserted as usize >= min_swaps {
                break 'outer;
            }
        }

        before = page.last().and_then(|c| Signature::from_str(&c.signature).ok());
        if before.is_none() {
            break;
        }
    }

    if stats.slots.0 == u64::MAX {
        stats.slots = (0, 0);
    }
    Ok(stats)
}
