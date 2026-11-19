use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::Parser;
use dotenvy::dotenv;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcBlockConfig;
use solana_transaction_status::{EncodedTransaction, EncodedTransactionWithStatusMeta};
use tracing::info;

use solana_event_indexer::parser::RAYDIUM_CPMM_PROGRAM_ID;
use solana_event_indexer::pool_state::{self, ReserveQuote};
use solana_event_indexer::{arb, backfill, config::Config, db, logger, parser, spread};

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

    /// Print an on-chain reserves report (instantaneous mid prices read from
    /// the two vault balances of each pool) and exit.
    #[arg(long)]
    reserves_report: bool,
    /// How many of the most active pools to load reserves for.
    #[arg(long, default_value_t = 200)]
    pool_limit: i64,

    /// Evaluate multi-hop cycles (SOL → A → B → SOL, or USDC → A → B → USDC)
    /// against live reserves and exit. Answers what the two-pool report cannot
    /// see: whether cross-token mispricing leaves money on the table after all
    /// three fees. Profits are converted to SOL before the fee floor is applied,
    /// so anchors are comparable.
    #[arg(long)]
    cycles_report: bool,
    /// Legs per cycle. 3 = triangle; 2 degenerates to the two-pool case.
    #[arg(long, default_value_t = 3)]
    cycle_hops: usize,
    /// Comma-separated cycle anchors. `SOL` alone only sees loops that touch
    /// wrapped SOL; adding `USDC` also sees USDC → A → B → USDC loops. Accepts
    /// SOL / USDC / a full mint address.
    #[arg(long, default_value = "SOL,USDC")]
    cycle_anchors: String,
    /// How many cycles to print, sorted by model profit.
    #[arg(long, default_value_t = 10)]
    cycles_top: usize,
    /// Priority fee in lamports, added to the 5000-lamport signature fee when
    /// checking whether a cycle clears its fixed cost.
    #[arg(long, default_value_t = 0)]
    priority_fee_lamports: u64,
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

fn short_key(key: &str) -> String {
    key.chars().take(8).collect()
}

/// 价格量级差得太多（1e-7 和 2000 都可能出现），统一成能看出区别的格式。
fn fmt_price(price: f64) -> String {
    if price >= 1.0 {
        format!("{price:.4}")
    } else if price >= 0.001 {
        format!("{price:.6}")
    } else {
        format!("{price:.4e}")
    }
}

/// 链上储备报告：中间价来自两个金库的当前余额，而不是历史成交。
///
/// 这份报告要回答三件事：
/// 1. 库里的 pool_id 到底能不能解析成 PoolState（旧数据里存的是金库 owner PDA）
/// 2. 同一个交易对的多个池子，此刻的瞬时价差是多少
/// 3. 用成交算出来的 VWAP 偏离真实中间价多少——这决定了 VWAP 能不能当报价用
async fn report_reserves(
    client: &solana_client::nonblocking::rpc_client::RpcClient,
    pool: &sqlx::PgPool,
    args: &Args,
) -> anyhow::Result<()> {
    let (swaps, pools, max_slot) = db::stats(pool).await?;
    if pools == 0 {
        println!("数据库里还没有 swap，先跑 `--backfill 200` 灌一批真实数据。");
        return Ok(());
    }

    let pool_ids = db::top_pools(pool, args.pool_limit).await?;
    println!(
        "库内 {} 笔 swap / {} 个 pool_id / 最高 slot {}",
        swaps, pools, max_slot
    );
    println!("读取前 {} 个最活跃池子的储备...", pool_ids.len());

    let snapshot = pool_state::fetch_reserves(client, &pool_ids).await?;
    println!(
        "解析成功 {} 个池子，账户缺失 {} 个，解析失败 {} 个，金库读不到 {} 个（查询 slot {}）",
        snapshot.quotes.len(),
        snapshot.missing.len(),
        snapshot.unparsable.len(),
        snapshot.missing_vaults,
        snapshot.slot
    );
    if !snapshot.missing.is_empty() {
        println!(
            "  ⚠ 账户缺失意味着这些 pool_id 在链上不是账户（旧版本写入的是金库 owner PDA），示例：{}",
            snapshot
                .missing
                .iter()
                .take(3)
                .map(|id| short_key(id))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !snapshot.unparsable.is_empty() {
        println!("  ⚠ 解析失败示例：{}", snapshot.unparsable[0]);
    }
    println!();

    if snapshot.quotes.is_empty() {
        println!("没有读到任何储备，价差无从谈起。");
        return Ok(());
    }

    // 同一个交易对聚到一起：只有 ≥2 个池子才存在「跨池」这回事
    let mut by_pair: HashMap<(String, String), Vec<&ReserveQuote>> = HashMap::new();
    for quote in &snapshot.quotes {
        by_pair
            .entry((quote.base_mint.clone(), quote.quote_mint.clone()))
            .or_default()
            .push(quote);
    }

    let fee_pct = spread::RAYDIUM_CPMM_FEE_BPS * 2.0 / 100.0;
    let mut comparable: Vec<_> = by_pair.iter().filter(|(_, q)| q.len() >= 2).collect();
    comparable.sort_by_key(|(_, quotes)| std::cmp::Reverse(quotes.len()));

    println!(
        "交易对 {} 个，其中 ≥2 个池子可比 {} 个",
        by_pair.len(),
        comparable.len()
    );
    for ((base, quote_mint), quotes) in comparable.iter().take(10) {
        let mut sorted: Vec<&ReserveQuote> = (*quotes).clone();
        sorted.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
        let cheap = sorted.first().unwrap();
        let dear = sorted.last().unwrap();
        let gross = (dear.price - cheap.price) / cheap.price * 100.0;
        let net = gross - fee_pct;
        println!(
            "  {} / {}  {} 池  毛 {:.3}%  扣双边费 {:.3}% → 净 {:.3}%",
            short_key(base),
            short_key(quote_mint),
            quotes.len(),
            gross,
            fee_pct,
            net
        );
        println!(
            "      低 {} @ {}（深度 {}）",
            short_key(&cheap.pool_id),
            fmt_price(cheap.price),
            fmt_price(cheap.depth_quote())
        );
        println!(
            "      高 {} @ {}（深度 {}）",
            short_key(&dear.pool_id),
            fmt_price(dear.price),
            fmt_price(dear.depth_quote())
        );
        match arb::best_two_pool_arb(cheap, dear) {
            Some(result) => {
                println!(
                    "      最优下单：投入 {} → 拿回 {}，毛利 {}（{:+.0} bps），吃掉低价池 {:.2}% 的储备",
                    fmt_price(result.amount_in),
                    fmt_price(result.amount_out),
                    fmt_price(result.profit),
                    result.profit_bps,
                    result.pool_impact * 100.0
                );
                if result.pool_impact > 0.05 {
                    println!(
                        "      ⚠ 这个规模要吃掉低价池 5% 以上的储备：恒定乘积假设下成立，\
                        真实成交会因为滑点与抢跑而缩水"
                    );
                }
                // 规模阶梯：把最优解放大几档，让「针尖之外的亏损」可见。
                // 单看「最优投入 0.0059」会让人以为可以再投一点，实际放大 20 倍就归零。
                print!("      规模阶梯：");
                for (multiplier, label) in
                    [(1.0, "×1"), (5.0, "×5"), (20.0, "×20"), (100.0, "×100")]
                {
                    let amount = result.amount_in * multiplier;
                    let profit = arb::profit_at_size(cheap, dear, amount);
                    if !profit.is_finite() {
                        continue;
                    }
                    let bps = profit / amount * 10_000.0;
                    print!(
                        "\n        {label:<5} 投入 {:<12} 盈亏 {:<14} {:+.0} bps",
                        fmt_price(amount),
                        fmt_price(profit),
                        bps
                    );
                }
                println!();
            }
            None => println!("      两池价差不足以覆盖双边手续费，没有可执行空间"),
        }
    }
    if comparable.is_empty() {
        println!("  没有交易对拥有两个以上池子，跨池价差无从检验。");
    }
    println!();

    // 储备中间价 vs 成交 VWAP：同一个池子、同一段窗口
    let raw = db::fetch_recent_swaps(pool, args.window_slots).await?;
    let vwaps = spread::build_quotes(&raw);
    let mut by_pool: HashMap<&str, f64> = HashMap::new();
    for list in vwaps.values() {
        for q in list {
            by_pool.insert(q.pool_id.as_str(), q.vwap);
        }
    }

    let mut deviations: Vec<(&ReserveQuote, f64, f64)> = Vec::new();
    for quote in &snapshot.quotes {
        if let Some(vwap) = by_pool.get(quote.pool_id.as_str())
            && *vwap > 0.0
        {
            let dev_pct = (quote.price - vwap) / vwap * 100.0;
            deviations.push((quote, *vwap, dev_pct));
        }
    }
    deviations
        .sort_by(|a, b| b.2.abs().partial_cmp(&a.2.abs()).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "储备价 vs 成交 VWAP（窗口 {} slots，可比 {} 个池子）：",
        args.window_slots,
        deviations.len()
    );
    if deviations.is_empty() {
        println!("  库里没有与储备池子匹配的成交，先回填一段数据。");
    }
    for (quote, vwap, dev_pct) in deviations.iter().take(8) {
        println!(
            "  {}  {}  储备 {}  VWAP {}  偏离 {:+.2}%",
            short_key(&quote.pool_id),
            short_key(&quote.base_mint),
            fmt_price(quote.price),
            fmt_price(*vwap),
            dev_pct
        );
    }

    Ok(())
}

/// 多跳闭环报告：SOL 出去转一圈回来，还剩多少 SOL。
///
/// 两池报告只能在「同一个币对的两个池子」之间找价差。真实的相对错价经常长在
/// 三个币的价格关系上：SOL/A、A/B、B/SOL 各自看都合理，乘起来却不等于 1。
/// 这类机会两池模型永远看不到，因为它没有任何一对池子是同一个币对。
///
/// 起点钉在 wrapped SOL 上不是图省事——只有这样利润才和 lamports 计价的签名费
/// 同量纲，才能回答「赚的钱够不够付那一笔固定成本」。
async fn report_cycles(
    client: &solana_client::nonblocking::rpc_client::RpcClient,
    pool: &sqlx::PgPool,
    args: &Args,
) -> anyhow::Result<()> {
    let (swaps, pools, max_slot) = db::stats(pool).await?;
    if pools == 0 {
        println!("数据库里还没有 swap，先跑 `--backfill 200` 灌一批真实数据。");
        return Ok(());
    }

    let pool_ids = db::top_pools(pool, args.pool_limit).await?;
    println!(
        "库内 {} 笔 swap / {} 个 pool_id / 最高 slot {}",
        swaps, pools, max_slot
    );
    println!("读取前 {} 个最活跃池子的储备...", pool_ids.len());
    let snapshot = pool_state::fetch_reserves(client, &pool_ids).await?;
    println!(
        "解析成功 {} 个池子，账户缺失 {} 个，解析失败 {} 个，金库读不到 {} 个（查询 slot {}）",
        snapshot.quotes.len(),
        snapshot.missing.len(),
        snapshot.unparsable.len(),
        snapshot.missing_vaults,
        snapshot.slot
    );
    if snapshot.quotes.is_empty() {
        println!("没有读到任何储备，闭环无从算起。");
        return Ok(());
    }

    let anchors: Vec<String> = args
        .cycle_anchors
        .split(',')
        .filter_map(|raw| {
            let name = raw.trim();
            if name.is_empty() {
                return None;
            }
            let resolved = arb::resolve_anchor(name);
            if resolved.is_none() {
                eprintln!("忽略无法识别的锚点 `{name}`（写 SOL / USDC 或完整 mint）");
            }
            resolved
        })
        .collect();
    if anchors.is_empty() {
        println!("没有可用锚点，闭环无从枚举。");
        return Ok(());
    }

    // 利润以各锚点自己的币计价，要折回 SOL 才能和 lamports 手续费相减。
    // 折不了的锚点（没有 SOL 配对池）直接跳过：宁可少报，不猜汇率。
    let mut unit_in_sol: HashMap<String, f64> = HashMap::new();
    for anchor in &anchors {
        match arb::sol_price_of(&snapshot.quotes, anchor) {
            Some(price) => {
                unit_in_sol.insert(anchor.clone(), price);
            }
            None => println!(
                "锚点 {} 找不到与 SOL 配对的池子，它的利润无法折成 SOL，本轮跳过",
                short_key(anchor)
            ),
        }
    }
    let anchors: Vec<String> =
        anchors.into_iter().filter(|a| unit_in_sol.contains_key(a)).collect();
    if anchors.is_empty() {
        println!("所有锚点都折不成 SOL，无法与固定成本比较。");
        return Ok(());
    }

    let cycles = arb::find_cycles_from_anchors(&snapshot.quotes, &anchors, args.cycle_hops);
    let fee_sol = arb::fee_floor_sol(args.priority_fee_lamports);
    let anchor_names = anchors.iter().map(|a| short_key(a)).collect::<Vec<_>>().join(" + ");
    println!(
        "{} 跳闭环候选 {} 条（起点 {anchor_names}，多锚点按有向腿去重）；固定成本 {} lamports = {:.6} SOL",
        args.cycle_hops,
        cycles.len(),
        5_000 + args.priority_fee_lamports,
        fee_sol
    );

    // 第三条是折算成 SOL 的毛利，排序与"扣固定成本"都用它。
    let mut dropped_unpriced = 0usize;
    let mut scored: Vec<(arb::CycleArb, &arb::Cycle, f64)> = cycles
        .iter()
        .filter_map(|cycle| {
            let result = arb::best_cycle_arb(cycle)?;
            let Some(rate) = unit_in_sol.get(&cycle.start_mint) else {
                dropped_unpriced += 1;
                return None;
            };
            let profit_sol = result.profit * rate;
            Some((result, cycle, profit_sol))
        })
        .collect();
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let clears_fee = scored.iter().filter(|(_, _, profit_sol)| *profit_sol > fee_sol).count();

    // 同一串币种往往对应多个池子组合（SOL→X→USDC→SOL 可能有七八种走法）。
    // 不归并的话，榜首会被同一类机会刷屏，看不出到底有几种错价。
    let mut by_path: HashMap<String, (arb::CycleArb, &arb::Cycle, f64, usize)> = HashMap::new();
    for (result, cycle, profit_sol) in &scored {
        let key = cycle.mints().join(">");
        match by_path.get_mut(&key) {
            Some(entry) if entry.2 >= *profit_sol => entry.3 += 1,
            Some(entry) => {
                let variants = entry.3 + 1;
                *entry = (result.clone(), cycle, *profit_sol, variants);
            }
            None => {
                by_path.insert(key, (result.clone(), cycle, *profit_sol, 1));
            }
        }
    }
    let mut paths: Vec<(arb::CycleArb, &arb::Cycle, f64, usize)> = by_path.into_values().collect();
    paths.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "模型内盈利 {} 条，落在 {} 条不同币种路径上；扣掉固定成本后仍为正 {} 条",
        scored.len(),
        paths.len(),
        clears_fee
    );
    // 分锚点看：第二锚点到底带来了什么，必须用数字回答，不能只看总榜。
    for anchor in &anchors {
        let candidates = cycles.iter().filter(|c| &c.start_mint == anchor).count();
        let mut best_here = 0.0f64;
        let mut profitable = 0usize;
        let mut clears = 0usize;
        for (_, cycle, profit_sol) in &scored {
            if &cycle.start_mint != anchor {
                continue;
            }
            profitable += 1;
            if *profit_sol > fee_sol {
                clears += 1;
            }
            if *profit_sol > best_here {
                best_here = *profit_sol;
            }
        }
        println!(
            "  起点 {}：候选 {candidates} 条，盈利 {profitable} 条，扣固定成本后 {clears} 条，最好 {:.6} SOL",
            short_key(anchor),
            best_here
        );
    }
    if dropped_unpriced > 0 {
        println!("另有 {dropped_unpriced} 条闭环的锚点折不成 SOL，未计入统计");
    }
    // 利润的绝对值比 bps 更能说明问题：+1136 bps 听着很多，投 0.0175 SOL 只赚 0.002 SOL。
    let bucket = |floor: f64| scored.iter().filter(|(_, _, p)| *p >= floor).count();
    let best = scored.first().map(|(_, _, p)| *p).unwrap_or(0.0);
    println!(
        "利润分布（SOL 计价，未扣 gas）：≥0.1 {} / 0.01–0.1 {} / 0.001–0.01 {} / <0.001 {}；最高 {:.6} SOL",
        bucket(0.1),
        bucket(0.01) - bucket(0.1),
        bucket(0.001) - bucket(0.01),
        bucket(0.0) - bucket(0.001),
        best
    );
    if paths.is_empty() {
        println!();
        println!("没有任何闭环能盖住每条腿 25 bps 的手续费——这些池子之间没有可利用的相对错价。");
        return Ok(());
    }
    println!();

    for (i, (result, cycle, profit_sol, variants)) in paths.iter().take(args.cycles_top).enumerate()
    {
        let path = cycle.mints().iter().map(|m| short_key(m)).collect::<Vec<_>>().join(" → ");
        let pools_path = cycle.pools().iter().map(|p| short_key(p)).collect::<Vec<_>>().join(" / ");
        let is_sol = cycle.start_mint == arb::WRAPPED_SOL;
        println!(
            "{:>2}) {path}（同路径 {variants} 种走法，这是最好的一条）",
            i + 1
        );
        println!("      池子：{pools_path}");
        if is_sol {
            println!(
                "      最优下单：投入 {} → 拿回 {}，毛利 {}（{:+.0} bps），扣固定成本后 {}",
                fmt_price(result.amount_in),
                fmt_price(result.amount_out),
                fmt_price(result.profit),
                result.profit_bps,
                fmt_price(result.profit - fee_sol)
            );
        } else {
            let unit = short_key(&cycle.start_mint);
            println!(
                "      最优下单：投入 {} {unit} → 拿回 {} {unit}，毛利 {} {unit} ≈ {} SOL（{:+.0} bps），扣固定成本后 {} SOL",
                fmt_price(result.amount_in),
                fmt_price(result.amount_out),
                fmt_price(result.profit),
                fmt_price(*profit_sol),
                result.profit_bps,
                fmt_price(profit_sol - fee_sol)
            );
        }
        println!(
            "      最紧的一条腿吃掉 {:.2}% 的储备",
            result.worst_impact * 100.0
        );
        print!("      规模阶梯：");
        for (multiplier, label) in [(1.0, "×1"), (5.0, "×5"), (20.0, "×20")] {
            let amount = result.amount_in * multiplier;
            let profit = arb::cycle_profit_at_size(cycle, amount);
            if !profit.is_finite() {
                continue;
            }
            print!(
                "\n        {label:<5} 投入 {:<12} 盈亏 {:<14} {:+.0} bps",
                fmt_price(amount),
                fmt_price(profit),
                profit / amount * 10_000.0
            );
        }
        println!();
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

    if args.reserves_report {
        let client = solana_client::nonblocking::rpc_client::RpcClient::new(config.rpc_url.clone());
        return report_reserves(&client, &pool, &args).await;
    }

    if args.cycles_report {
        let client = solana_client::nonblocking::rpc_client::RpcClient::new(config.rpc_url.clone());
        return report_cycles(&client, &pool, &args).await;
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
            "回填完成：交易 {} 笔（失败跳过 {}，未解析出 swap {}），解析出 swap {} 条，新入库 {} 条，slot {}..={}",
            stats.transactions,
            stats.skipped_failed,
            stats.txs_without_swap,
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
