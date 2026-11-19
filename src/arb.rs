//! 规模感知的套利测算：两池同币对，以及 SOL 起点的多跳闭环。
//!
//! 中间价只能回答「有没有价差」，回答不了「值不值得做」。价差 2% 听起来很美，
//! 但如果只有在某个池子里砸进去 3 个 token 才成立，那这笔机会在手续费和 gas
//! 面前等于零。这里用恒定乘积公式把「投多少、换多少、剩多少」算出来。
//!
//! 多跳部分回答另一个问题：**跨币种的价差不在同一个池子里**。SOL → A → B → SOL
//! 这样一圈回来如果还剩下 SOL，说明三块池子之间存在真实的相对错价——两池模型
//! 永远看不到这种机会，因为没有任何一对池子是同一个币对。
//!
//! 模型假设（都会让结果**偏乐观**，因此报告里必须一起交代）：
//!
//! - 每边扣 25 bps（Raydium CPMM 的 `trade_fee_rate`），从投入额里先扣
//! - 不含 gas、优先费、报价与成交之间的 slot 漂移（调用方可用 `fee_floor_sol` 折抵）
//! - 储备在交易过程中不变（真实情况是自打自跌，所以实际利润只会更低）
//! - 多跳只**枚举**长度固定的简单闭环，不做拆单与跨程序路由

use std::collections::{HashMap, HashSet};

use crate::pool_state::ReserveQuote;
use crate::spread::RAYDIUM_CPMM_FEE_BPS;

/// wrapped SOL 的 mint。多跳闭环以它为起点，利润才和 lamports 计价的手续费可比。
pub const WRAPPED_SOL: &str = "So11111111111111111111111111111111111111112";

/// USDC 的 mint。作为第二个锚点：不含 SOL 的闭环（USDC → A → B → USDC）
/// 在只锚定 SOL 的枚举里是完全不可见的。
pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// 把命令行里写的锚点换成 mint。支持 `SOL` / `USDC` 简写，其他一律当 mint 原样用。
pub fn resolve_anchor(name: &str) -> Option<String> {
    match name.trim().to_ascii_uppercase().as_str() {
        "" => None,
        "SOL" | "WSOL" => Some(WRAPPED_SOL.to_string()),
        "USDC" => Some(USDC.to_string()),
        _ => {
            let raw = name.trim();
            // base58 的 mint 至少 32 个字符，太短的只可能是拼错的简写
            (raw.len() >= 32).then(|| raw.to_string())
        }
    }
}

/// 一个单位的 `mint` 值多少 SOL，用于把任意锚点的利润折回 SOL 计价。
///
/// 取「与 SOL 配对、且 quote 储备最深」的那个池子的中间价：深度决定了这个
/// 汇率有多可信，浅池的中间价会把利润凭空放大或缩小。找不到配对池就返回
/// `None`——调用方要如实说明「这个锚点的利润无法折成 SOL」，而不是猜一个。
pub fn sol_price_of(quotes: &[ReserveQuote], mint: &str) -> Option<f64> {
    if mint == WRAPPED_SOL {
        return Some(1.0);
    }
    let mut best: Option<(f64, f64)> = None; // (深度, SOL per mint)
    for q in quotes {
        if q.base_reserve <= 0.0 || q.quote_reserve <= 0.0 {
            continue;
        }
        let (sol_per_mint, depth) = if q.base_mint == WRAPPED_SOL && q.quote_mint == mint {
            // price = quote per base = mint per SOL
            if q.price <= 0.0 {
                continue;
            }
            (1.0 / q.price, q.base_reserve)
        } else if q.quote_mint == WRAPPED_SOL && q.base_mint == mint {
            // price = SOL per base = SOL per mint
            (q.price, q.quote_reserve)
        } else {
            continue;
        };
        if !sol_per_mint.is_finite() || sol_per_mint <= 0.0 {
            continue;
        }
        if best.is_none_or(|(d, _)| depth > d) {
            best = Some((depth, sol_per_mint));
        }
    }
    best.map(|(_, p)| p)
}

/// 一边的手续费比例，25 bps = 0.25%。
pub fn fee_per_side() -> f64 {
    RAYDIUM_CPMM_FEE_BPS / 10_000.0
}

/// 恒定乘积下单：投入 `amount_in` 的 quote，能换出多少 base。
///
/// 先按手续费打折再进入曲线——这和链上 `swap_base_input` 的处理一致：
/// 手续费留在池子里，不参与定价。
pub fn buy_base_with_quote(quote_reserve: f64, base_reserve: f64, amount_in: f64) -> f64 {
    if amount_in <= 0.0 || quote_reserve <= 0.0 || base_reserve <= 0.0 {
        return 0.0;
    }
    let effective = amount_in * (1.0 - fee_per_side());
    base_reserve * effective / (quote_reserve + effective)
}

/// 反向：把 base 砸进池子，换回多少 quote。
pub fn sell_base_for_quote(base_reserve: f64, quote_reserve: f64, amount_in: f64) -> f64 {
    if amount_in <= 0.0 || base_reserve <= 0.0 || quote_reserve <= 0.0 {
        return 0.0;
    }
    let effective = amount_in * (1.0 - fee_per_side());
    quote_reserve * effective / (base_reserve + effective)
}

/// 一次两池套利的结果：在低价池买入 base，再在高价池卖出。
#[derive(Debug, Clone, PartialEq)]
pub struct TwoPoolArb {
    /// 投入的 quote 数量（已含买入侧的池子容量约束）。
    pub amount_in: f64,
    /// 卖出后拿回的 quote 数量。
    pub amount_out: f64,
    /// `amount_out - amount_in`，未扣 gas。
    pub profit: f64,
    /// 利润占投入的比例（bps）。投入为 0 时记 0。
    pub profit_bps: f64,
    /// 投入吃掉低价池 quote 储备的比例，用来判断这个「最优解」是否现实。
    pub pool_impact: f64,
}

/// 在指定投入规模下的净损益（quote 计价，未扣 gas）。
///
/// `best_two_pool_arb` 回答「最优投多少」，这个函数回答「如果我偏要投 X 呢」。
/// 两者一起看才能说清价差为什么不可执行：最优解是一根针尖，针尖之外迅速转负。
pub fn profit_at_size(cheap: &ReserveQuote, dear: &ReserveQuote, amount_in: f64) -> f64 {
    if amount_in <= 0.0 || cheap.price >= dear.price {
        return f64::NEG_INFINITY;
    }
    let base_out = buy_base_with_quote(cheap.quote_reserve, cheap.base_reserve, amount_in);
    if base_out <= 0.0 {
        return f64::NEG_INFINITY;
    }
    let quote_out = sell_base_for_quote(dear.base_reserve, dear.quote_reserve, base_out);
    quote_out - amount_in
}

/// 求两池之间的最优投入规模。
///
/// 利润关于投入额是单峰的：投太少吃不满价差，投太多把自己的价格推回去。
/// 用三分搜索求极值，区间上限取低价池的 quote 储备（再多也换不出来）。
/// 无利可图时返回 `None` —— 让调用方区分「没有机会」和「机会很小」。
pub fn best_two_pool_arb(cheap: &ReserveQuote, dear: &ReserveQuote) -> Option<TwoPoolArb> {
    if cheap.base_mint != dear.base_mint || cheap.quote_mint != dear.quote_mint {
        return None;
    }
    // 同价（或低价池反而更贵）就没有套利方向可言
    if cheap.price >= dear.price {
        return None;
    }

    let profit_at = |amount_in: f64| -> f64 { profit_at_size(cheap, dear, amount_in) };

    // 上限：低价池的 quote 储备。真的把整个池子买空是不现实的，
    // 但这是模型允许的边界，报告里用 pool_impact 提示。
    let hi = cheap.quote_reserve;
    let lo = hi * 1e-9;
    if hi <= 0.0 || !hi.is_finite() || !profit_at(lo).is_finite() {
        return None;
    }

    let (mut lo, mut hi) = (lo, hi);
    for _ in 0..200 {
        let m1 = lo + (hi - lo) / 3.0;
        let m2 = hi - (hi - lo) / 3.0;
        if profit_at(m1) < profit_at(m2) {
            lo = m1;
        } else {
            hi = m2;
        }
    }

    let amount_in = (lo + hi) / 2.0;
    let profit = profit_at(amount_in);
    // 利润为负或小到低于浮点噪声，就当没有机会
    if !profit.is_finite() || profit <= 0.0 {
        return None;
    }

    Some(TwoPoolArb {
        amount_in,
        amount_out: amount_in + profit,
        profit,
        profit_bps: profit / amount_in * 10_000.0,
        pool_impact: amount_in / cheap.quote_reserve,
    })
}

/// 多跳路径上的一条腿：往 `in_reserve` 的池子里砸 `amount_in`，换出 `out_reserve` 币。
#[derive(Debug, Clone, PartialEq)]
pub struct Leg {
    pub pool_id: String,
    pub in_mint: String,
    pub out_mint: String,
    /// 池子里 `in_mint` 的储备（UI 数量，已按 decimals 缩放）。
    pub in_reserve: f64,
    /// 池子里 `out_mint` 的储备。
    pub out_reserve: f64,
}

/// 一条回到起点的闭环：`legs` 首尾相接，最后一腿的输出币就是起点币。
#[derive(Debug, Clone, PartialEq)]
pub struct Cycle {
    pub start_mint: String,
    pub legs: Vec<Leg>,
}

impl Cycle {
    /// 依次经过的币（不含回到起点的那一次）。
    pub fn mints(&self) -> Vec<&str> {
        let mut out = vec![self.start_mint.as_str()];
        for leg in &self.legs {
            out.push(leg.out_mint.as_str());
        }
        out
    }

    pub fn pools(&self) -> Vec<&str> {
        self.legs.iter().map(|l| l.pool_id.as_str()).collect()
    }
}

/// 多跳闭环的最优解。`profit` 以 `start_mint` 计价，未扣 gas。
#[derive(Debug, Clone, PartialEq)]
pub struct CycleArb {
    pub amount_in: f64,
    pub amount_out: f64,
    pub profit: f64,
    pub profit_bps: f64,
    /// 最吃储备的那条腿的消耗比例。
    pub worst_impact: f64,
    pub hops: usize,
}

/// 单条腿的恒定乘积成交：先扣费再进曲线，和链上 `swap_base_input` 一致。
pub fn leg_out(leg: &Leg, amount_in: f64) -> f64 {
    if amount_in <= 0.0 || leg.in_reserve <= 0.0 || leg.out_reserve <= 0.0 {
        return 0.0;
    }
    let effective = amount_in * (1.0 - fee_per_side());
    leg.out_reserve * effective / (leg.in_reserve + effective)
}

/// 走完整条闭环后拿回多少起始币。
pub fn cycle_out(cycle: &Cycle, amount_in: f64) -> f64 {
    run_cycle(cycle, amount_in).map(|(out, _)| out).unwrap_or(0.0)
}

/// 闭环在指定规模下的净损益（起始币计价，未扣 gas）。
pub fn cycle_profit_at_size(cycle: &Cycle, amount_in: f64) -> f64 {
    match run_cycle(cycle, amount_in) {
        Some((out, _)) => out - amount_in,
        None => f64::NEG_INFINITY,
    }
}

/// 逐腿推进，返回 (最终拿回的数量, 各腿里最吃储备的比例)。
fn run_cycle(cycle: &Cycle, amount_in: f64) -> Option<(f64, f64)> {
    if amount_in <= 0.0 || cycle.legs.len() < 2 {
        return None;
    }
    if cycle.legs.last()?.out_mint != cycle.start_mint {
        return None;
    }
    // 首尾相接是前提：中间断了一节，输入输出就不是同一币种，利润没有意义
    for pair in cycle.legs.windows(2) {
        if pair[0].out_mint != pair[1].in_mint {
            return None;
        }
    }
    let mut amount = amount_in;
    let mut worst: f64 = 0.0;
    for leg in &cycle.legs {
        if leg.in_reserve <= 0.0 {
            return None;
        }
        worst = worst.max(amount / leg.in_reserve);
        amount = leg_out(leg, amount);
        if amount <= 0.0 {
            return None;
        }
    }
    Some((amount, worst))
}

/// 求闭环的最优投入规模。
///
/// 三跳（乃至更多跳）下，`out(x)` 仍是单调递增的凹函数，所以 `out(x) - x` 单峰，
/// 三分搜索同样成立。无利可图（手续费吃掉全部错价）时返回 `None`。
pub fn best_cycle_arb(cycle: &Cycle) -> Option<CycleArb> {
    let first = cycle.legs.first()?;
    let hi = first.in_reserve;
    if hi <= 0.0 || !hi.is_finite() || run_cycle(cycle, hi * 1e-9).is_none() {
        return None;
    }

    let (mut lo, mut hi) = (hi * 1e-9, hi);
    for _ in 0..200 {
        let m1 = lo + (hi - lo) / 3.0;
        let m2 = hi - (hi - lo) / 3.0;
        if cycle_profit_at_size(cycle, m1) < cycle_profit_at_size(cycle, m2) {
            lo = m1;
        } else {
            hi = m2;
        }
    }

    let amount_in = (lo + hi) / 2.0;
    let (amount_out, worst_impact) = run_cycle(cycle, amount_in)?;
    let profit = amount_out - amount_in;
    if !profit.is_finite() || profit <= 0.0 {
        return None;
    }

    Some(CycleArb {
        amount_in,
        amount_out,
        profit,
        profit_bps: profit / amount_in * 10_000.0,
        worst_impact,
        hops: cycle.legs.len(),
    })
}

/// 一笔交易的固定成本下限（SOL）：签名费 5000 lamports + 优先费。
///
/// 只有起点是 SOL 的闭环，利润才和这个数字同量纲可直接比较——这也是把
/// 多跳起点钉在 wrapped SOL 上的原因。
pub fn fee_floor_sol(priority_fee_lamports: u64) -> f64 {
    (5_000 + priority_fee_lamports) as f64 / 1_000_000_000.0
}

/// 枚举从 `start_mint` 出发、长度恰为 `hops` 的简单闭环。
///
/// 「简单」= 同一池子不重复走、中间币种不重复出现。三条腿的三跳闭环
/// 覆盖 SOL → A → B → SOL，这是两池模型完全看不到的一类错价。
pub fn find_cycles(quotes: &[ReserveQuote], start_mint: &str, hops: usize) -> Vec<Cycle> {
    if hops < 2 {
        return Vec::new();
    }

    // mint → 从它出发的所有腿（每个池子两个方向各一条）
    let mut adj: HashMap<String, Vec<Leg>> = HashMap::new();
    for q in quotes {
        if q.base_reserve <= 0.0 || q.quote_reserve <= 0.0 {
            continue;
        }
        adj.entry(q.base_mint.clone()).or_default().push(Leg {
            pool_id: q.pool_id.clone(),
            in_mint: q.base_mint.clone(),
            out_mint: q.quote_mint.clone(),
            in_reserve: q.base_reserve,
            out_reserve: q.quote_reserve,
        });
        adj.entry(q.quote_mint.clone()).or_default().push(Leg {
            pool_id: q.pool_id.clone(),
            in_mint: q.quote_mint.clone(),
            out_mint: q.base_mint.clone(),
            in_reserve: q.quote_reserve,
            out_reserve: q.base_reserve,
        });
    }

    let mut out = Vec::new();
    let mut legs = Vec::new();
    let mut used_pools: HashSet<String> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    walk_cycles(
        start_mint,
        start_mint,
        hops,
        &adj,
        &mut legs,
        &mut used_pools,
        &mut visited,
        &mut out,
    );
    out
}

/// 从多个锚点分别枚举闭环，并按「走过哪几个池子」去重。
///
/// 一个含 SOL 和 USDC 的闭环，从两个锚点都会走到它，只是起点不同。若不去重，
/// 同一笔机会会被算两次、并且以两种计价出现在榜单上。这里以「有向腿集合」
/// （池子 + 进出币，排序后）作指纹：起点旋转后腿集合不变，所以能去重；而正反
/// 两条腿方向相反，指纹不同，不会被误删。
pub fn find_cycles_from_anchors(
    quotes: &[ReserveQuote],
    anchors: &[String],
    hops: usize,
) -> Vec<Cycle> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for anchor in anchors {
        for cycle in find_cycles(quotes, anchor, hops) {
            // 指纹必须带方向：同一组池子的正反两条闭环是两笔不同的交易，
            // 只按池子集合去重会把其中一条（通常是唯一能赚的那条）删掉。
            let mut legs: Vec<String> = cycle
                .legs
                .iter()
                .map(|l| format!("{}:{}>{}", l.pool_id, l.in_mint, l.out_mint))
                .collect();
            legs.sort_unstable();
            let fingerprint = legs.join("|");
            if seen.insert(fingerprint) {
                out.push(cycle);
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn walk_cycles(
    current: &str,
    start: &str,
    remaining: usize,
    adj: &HashMap<String, Vec<Leg>>,
    legs: &mut Vec<Leg>,
    used_pools: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    out: &mut Vec<Cycle>,
) {
    let Some(nexts) = adj.get(current) else {
        return;
    };
    for leg in nexts {
        if used_pools.contains(&leg.pool_id) {
            continue;
        }
        let closes = leg.out_mint == start;
        if closes {
            if remaining == 1 {
                legs.push(leg.clone());
                out.push(Cycle {
                    start_mint: start.to_string(),
                    legs: legs.clone(),
                });
                legs.pop();
            }
            // 提前回到起点 = 长度不足，不是我们要的闭环
            continue;
        }
        if remaining == 1 || visited.contains(&leg.out_mint) {
            continue;
        }

        legs.push(leg.clone());
        used_pools.insert(leg.pool_id.clone());
        visited.insert(leg.out_mint.clone());
        walk_cycles(
            &leg.out_mint,
            start,
            remaining - 1,
            adj,
            legs,
            used_pools,
            visited,
            out,
        );
        visited.remove(&leg.out_mint);
        used_pools.remove(&leg.pool_id);
        legs.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(pool: &str, base_reserve: f64, quote_reserve: f64) -> ReserveQuote {
        ReserveQuote {
            pool_id: pool.to_string(),
            base_mint: "BASE".to_string(),
            quote_mint: "QUOTE".to_string(),
            base_reserve,
            quote_reserve,
            base_decimals: 6,
            quote_decimals: 6,
            price: quote_reserve / base_reserve,
            slot: 100,
        }
    }

    #[test]
    fn buy_then_sell_in_same_pool_loses_the_fee() {
        let p = quote("A", 1_000.0, 1_000_000.0);

        // 小额：几乎只有手续费，0.9975² ≈ 0.995，亏约 0.5%
        let small = buy_base_with_quote(p.quote_reserve, p.base_reserve, 100.0);
        let small_back = sell_base_for_quote(p.base_reserve, p.quote_reserve, small);
        assert!(small_back < 100.0, "同池往返不该赚钱");
        assert!(
            (small_back / 100.0 - 0.995).abs() < 0.002,
            "小额往返应当只亏两边手续费，实际拿回 {small_back}"
        );

        // 大额：手续费之外还要吃两次价格冲击，亏得更多
        let big = buy_base_with_quote(p.quote_reserve, p.base_reserve, 10_000.0);
        let big_back = sell_base_for_quote(p.base_reserve, p.quote_reserve, big);
        assert!(
            big_back < small_back * 100.0,
            "大额往返的亏损应当大于纯手续费"
        );
    }

    #[test]
    fn identical_pools_offer_no_arbitrage() {
        let a = quote("A", 1_000.0, 1_000_000.0);
        let b = quote("B", 1_000.0, 1_000_000.0);
        assert!(best_two_pool_arb(&a, &b).is_none());
    }

    #[test]
    fn mispriced_pools_yield_positive_profit() {
        // 低价 1000，高价 1020，2% 价差扣掉两边 0.5% 手续费还剩 1.5% 空间
        let cheap = quote("CH", 1_000.0, 1_000_000.0);
        let dear = quote("DE", 1_000.0, 1_020_000.0);
        let arb = best_two_pool_arb(&cheap, &dear).expect("应当有套利空间");
        assert!(arb.profit > 0.0);
        assert!(arb.profit_bps > 0.0);
        assert!(arb.amount_in > 0.0);
        assert!(arb.amount_out > arb.amount_in);
    }

    #[test]
    fn bigger_mispricing_yields_more_profit() {
        let cheap = quote("CH", 1_000.0, 1_000_000.0);
        let small = quote("S", 1_000.0, 1_010_000.0);
        let large = quote("L", 1_000.0, 1_050_000.0);
        let a = best_two_pool_arb(&cheap, &small).expect("1% 价差应该能覆盖手续费");
        let b = best_two_pool_arb(&cheap, &large).expect("5% 价差应该有空间");
        assert!(b.profit > a.profit, "价差更大利润应当更高");
    }

    #[test]
    fn fee_alone_kills_small_spread() {
        // 0.2% 价差抵不过两边各 0.25% 的手续费
        let cheap = quote("CH", 1_000.0, 1_000_000.0);
        let dear = quote("DE", 1_000.0, 1_002_000.0);
        assert!(best_two_pool_arb(&cheap, &dear).is_none());
    }

    #[test]
    fn optimal_size_respects_pool_capacity() {
        let cheap = quote("CH", 1.0, 1_000.0); // 极浅的池子
        let dear = quote("DE", 1_000.0, 1_500_000.0);
        let arb = best_two_pool_arb(&cheap, &dear).expect("价差极大，应当有机会");
        assert!(arb.amount_in <= cheap.quote_reserve);
        assert!(arb.pool_impact <= 1.0);
    }

    #[test]
    fn mismatched_pairs_are_rejected() {
        let a = quote("A", 1_000.0, 1_000_000.0);
        let mut b = quote("B", 1_000.0, 1_100_000.0);
        b.quote_mint = "OTHER".to_string();
        assert!(best_two_pool_arb(&a, &b).is_none());
    }

    #[test]
    fn profit_collapses_when_oversized() {
        // 2% 价差、两个中等深度的池子：最优解附近赚钱，放大 100 倍必亏
        let cheap = quote("CH", 1_000.0, 1_000_000.0);
        let dear = quote("DE", 1_000.0, 1_020_000.0);
        let arb = best_two_pool_arb(&cheap, &dear).expect("应当有套利空间");

        let at_optimal = profit_at_size(&cheap, &dear, arb.amount_in);
        assert!(at_optimal > 0.0);
        assert!((at_optimal - arb.profit).abs() < 1e-6);

        let oversized = profit_at_size(&cheap, &dear, arb.amount_in * 100.0);
        assert!(
            oversized < at_optimal,
            "放大 100 倍后的利润必须低于最优解（实际 {oversized} vs {at_optimal}）"
        );
        assert!(oversized < 0.0, "放大 100 倍应当由盈转亏，实际 {oversized}");
    }

    #[test]
    fn profit_at_size_is_negative_without_spread() {
        let cheap = quote("CH", 1_000.0, 1_000_000.0);
        let dear = quote("DE", 1_000.0, 1_002_000.0);
        assert!(profit_at_size(&cheap, &dear, 10.0) < 0.0);
        assert!(profit_at_size(&cheap, &dear, 0.0).is_infinite());
    }

    // ---- 多跳闭环 ----

    fn mk(
        pool: &str,
        base: &str,
        quote_mint: &str,
        base_reserve: f64,
        quote_reserve: f64,
    ) -> ReserveQuote {
        ReserveQuote {
            pool_id: pool.to_string(),
            base_mint: base.to_string(),
            quote_mint: quote_mint.to_string(),
            base_reserve,
            quote_reserve,
            base_decimals: 6,
            quote_decimals: 6,
            price: quote_reserve / base_reserve,
            slot: 100,
        }
    }

    /// SOL → AAA → BBB（三者都按 1:1 定价）
    fn flat_triangle(tail_sol_reserve: f64) -> Vec<ReserveQuote> {
        vec![
            mk("P1", "SOL", "AAA", 1_000.0, 1_000.0),
            mk("P2", "AAA", "BBB", 1_000.0, 1_000.0),
            // BBB → SOL 的兑换率 = quote/base = tail_sol_reserve / 1000
            mk("P3", "BBB", "SOL", 1_000.0, tail_sol_reserve),
        ]
    }

    #[test]
    fn leg_out_agrees_with_two_pool_helpers() {
        let p = quote("A", 1_000.0, 1_000_000.0);
        let buy = Leg {
            pool_id: "A".into(),
            in_mint: "QUOTE".into(),
            out_mint: "BASE".into(),
            in_reserve: p.quote_reserve,
            out_reserve: p.base_reserve,
        };
        let sell = Leg {
            pool_id: "A".into(),
            in_mint: "BASE".into(),
            out_mint: "QUOTE".into(),
            in_reserve: p.base_reserve,
            out_reserve: p.quote_reserve,
        };
        assert!(
            (leg_out(&buy, 1_000.0)
                - buy_base_with_quote(p.quote_reserve, p.base_reserve, 1_000.0))
            .abs()
                < 1e-9
        );
        assert!(
            (leg_out(&sell, 10.0) - sell_base_for_quote(p.base_reserve, p.quote_reserve, 10.0))
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn find_cycles_sees_both_directions() {
        let cycles = find_cycles(&flat_triangle(1_000.0), "SOL", 3);
        assert_eq!(cycles.len(), 2, "三池三角应当只有正反两条闭环");
        for c in &cycles {
            assert_eq!(c.legs.len(), 3);
            assert_eq!(
                c.mints(),
                vec!["SOL", c.legs[0].out_mint.as_str(), c.legs[1].out_mint.as_str(), "SOL"]
            );
        }
    }

    #[test]
    fn two_pools_never_make_a_three_hop_cycle() {
        let pools = vec![
            mk("P1", "SOL", "AAA", 1_000.0, 1_000.0),
            mk("P2", "AAA", "SOL", 1_000.0, 1_000.0),
        ];
        assert!(find_cycles(&pools, "SOL", 3).is_empty());
    }

    #[test]
    fn flat_triangle_loses_only_fees() {
        // 三条腿都 1:1，走一圈只剩手续费：0.9975³ ≈ 0.9925，必亏
        let cycles = find_cycles(&flat_triangle(1_000.0), "SOL", 3);
        for c in &cycles {
            assert!(cycle_profit_at_size(c, 10.0) < 0.0);
            assert!(best_cycle_arb(c).is_none(), "没有错价就不该报出机会");
        }
    }

    #[test]
    fn mispriced_tail_makes_cycle_profitable() {
        // BBB → SOL 这条腿按 2 SOL / BBB 定价，等于白送一倍
        let cycles = find_cycles(&flat_triangle(2_000.0), "SOL", 3);
        let profitable: Vec<_> = cycles.iter().filter_map(best_cycle_arb).collect();
        assert_eq!(
            profitable.len(),
            1,
            "只有正方向能赚，反方向应当被手续费吃掉"
        );

        let arb_result = &profitable[0];
        assert!(arb_result.profit > 0.0);
        assert_eq!(arb_result.hops, 3);
        assert!(arb_result.worst_impact > 0.0 && arb_result.worst_impact <= 1.0);

        // 放大十倍吃掉更多储备，利润必须下降
        let big = cycles
            .iter()
            .map(|c| cycle_profit_at_size(c, arb_result.amount_in * 10.0))
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(big < arb_result.profit, "放大后利润应当被价格冲击吃掉");
    }

    #[test]
    fn broken_cycle_is_rejected() {
        let mut c = find_cycles(&flat_triangle(2_000.0), "SOL", 3)
            .into_iter()
            .next()
            .expect("应当有闭环");
        c.legs[1].in_mint = "OTHER".into(); // 腿与腿之间断开
        assert!(cycle_out(&c, 1.0) == 0.0);
        assert!(best_cycle_arb(&c).is_none());
    }

    #[test]
    fn fee_floor_is_lamports_over_a_billion() {
        assert!((fee_floor_sol(0) - 0.000005).abs() < 1e-12);
        assert!((fee_floor_sol(100_000) - 0.000105).abs() < 1e-12);
    }

    #[test]
    fn anchors_resolve_from_symbols_or_full_mints() {
        assert_eq!(resolve_anchor("SOL").as_deref(), Some(WRAPPED_SOL));
        assert_eq!(resolve_anchor(" wsol ").as_deref(), Some(WRAPPED_SOL));
        assert_eq!(resolve_anchor("usdc").as_deref(), Some(USDC));
        assert_eq!(resolve_anchor(USDC).as_deref(), Some(USDC));
        // 太短的只可能是拼错的简写，不能当 mint 用
        assert!(resolve_anchor("USD").is_none());
        assert!(resolve_anchor("  ").is_none());
    }

    #[test]
    fn sol_price_reads_both_orientations() {
        // SOL 是 base：price = USDC per SOL = 200 ⇒ 1 USDC = 0.005 SOL
        let as_base = vec![mk("P", WRAPPED_SOL, USDC, 1.0, 200.0)];
        // SOL 是 quote：price = SOL per USDC = 0.005
        let as_quote = vec![mk("P", USDC, WRAPPED_SOL, 200.0, 1.0)];
        for quotes in [as_base, as_quote] {
            let p = sol_price_of(&quotes, USDC).expect("应当能读到 USDC 的 SOL 价");
            assert!((p - 0.005).abs() < 1e-12);
        }
        // 自己换自己恒为 1，不需要任何池子
        assert_eq!(sol_price_of(&[], WRAPPED_SOL), Some(1.0));
        // 没有 SOL 配对池就是不猜
        assert!(sol_price_of(&[mk("P", USDC, "AAA", 100.0, 100.0)], USDC).is_none());
    }

    #[test]
    fn sol_price_follows_the_deepest_pool() {
        // 浅池报 1000 USDC/SOL（1 USDC = 0.001 SOL），深池报 200（0.005 SOL）。
        // 汇率必须取深的那一个，否则利润会被浅池的中间价放大五倍。
        let quotes = vec![
            mk("THIN", WRAPPED_SOL, USDC, 0.01, 10.0),
            mk("DEEP", WRAPPED_SOL, USDC, 100.0, 20_000.0),
        ];
        let p = sol_price_of(&quotes, USDC).unwrap();
        assert!((p - 0.005).abs() < 1e-12, "应当取深池的 0.005，实际 {p}");
    }

    #[test]
    fn second_anchor_sees_loops_without_sol() {
        // SOL → AAA → BBB → SOL 与 USDC → AAA → BBB → USDC 是两笔不同的交易
        let quotes = vec![
            mk("P1", WRAPPED_SOL, "AAA", 1_000.0, 1_000.0),
            mk("P2", "AAA", "BBB", 1_000.0, 1_000.0),
            mk("P3", "BBB", WRAPPED_SOL, 1_000.0, 1_000.0),
            mk("P4", USDC, "AAA", 1_000.0, 1_000.0),
            mk("P5", "BBB", USDC, 1_000.0, 1_000.0),
        ];
        let sol_only = find_cycles(&quotes, WRAPPED_SOL, 3);
        let both = find_cycles_from_anchors(&quotes, &[WRAPPED_SOL.into(), USDC.into()], 3);
        assert!(!sol_only.iter().any(|c| c.start_mint == USDC));
        assert!(both.iter().any(|c| c.start_mint == USDC));
        // SOL 那两条（正反两个方向）必须还在
        assert_eq!(
            both.iter().filter(|c| c.start_mint == WRAPPED_SOL).count(),
            sol_only.len()
        );
    }

    #[test]
    fn overlap_between_anchors_is_deduped_by_pool_set() {
        // 一条含 SOL 又有 USDC 的闭环，从两个锚点都会走到；按池子集合去重后
        // 只能剩正反两条，不能变成四条。
        let quotes = vec![
            mk("P1", WRAPPED_SOL, USDC, 1_000.0, 1_000.0),
            mk("P2", USDC, "AAA", 1_000.0, 1_000.0),
            mk("P3", "AAA", WRAPPED_SOL, 1_000.0, 1_000.0),
        ];
        let both = find_cycles_from_anchors(&quotes, &[WRAPPED_SOL.into(), USDC.into()], 3);
        assert_eq!(both.len(), 2, "正反两条，去重后不应翻倍");
        // 两条走的是同一组池子，但方向相反，必须都保留
        let mut directed: Vec<String> = both
            .iter()
            .map(|c| {
                let mut legs: Vec<String> = c
                    .legs
                    .iter()
                    .map(|l| format!("{}:{}>{}", l.pool_id, l.in_mint, l.out_mint))
                    .collect();
                legs.sort_unstable();
                legs.join("|")
            })
            .collect();
        directed.sort();
        directed.dedup();
        assert_eq!(directed.len(), 2, "正反两条的有向指纹必须不同");
    }
}
