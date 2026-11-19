//! 规模感知的两池套利测算。
//!
//! 中间价只能回答「有没有价差」，回答不了「值不值得做」。价差 2% 听起来很美，
//! 但如果只有在某个池子里砸进去 3 个 token 才成立，那这笔机会在手续费和 gas
//! 面前等于零。这里用恒定乘积公式把「投多少、换多少、剩多少」算出来。
//!
//! 模型假设（都会让结果**偏乐观**，因此报告里必须一起交代）：
//!
//! - 只走两个池子，不考虑多跳路径与聚合器拆单
//! - 每边扣 25 bps（Raydium CPMM 的 `trade_fee_rate`），从投入额里先扣
//! - 不含 gas、优先费、报价与成交之间的 slot 漂移
//! - 储备在交易过程中不变（真实情况是自打自跌，所以实际利润只会更低）

use crate::pool_state::ReserveQuote;
use crate::spread::RAYDIUM_CPMM_FEE_BPS;

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

    let profit_at = |amount_in: f64| -> f64 {
        let base_out = buy_base_with_quote(cheap.quote_reserve, cheap.base_reserve, amount_in);
        if base_out <= 0.0 {
            return f64::NEG_INFINITY;
        }
        let quote_out = sell_base_for_quote(dear.base_reserve, dear.quote_reserve, base_out);
        quote_out - amount_in
    };

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
}
