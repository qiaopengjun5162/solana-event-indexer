//! 跨池价差检测。
//!
//! 目标不是「找出两个池子报价不一样」，而是「找出一笔真能赚到钱的交易」。
//! 因此在裸价差之外必须扣掉三样东西：
//!
//! 1. 两边各收一次的 CPMM 手续费（25 bps × 2）
//! 2. 链上 gas（基础 5000 lamports + 实际观测到的优先费）
//! 3. 时间差——两个池子的价格如果不是同一时刻的，价差可能早就消失
//!
//! 价格口径统一成「每 1 个 base 换多少 quote」，base/quote 按 mint 字典序确定，
//! 这样 A→B 和 B→A 的成交可以放在一起比较。

use std::collections::HashMap;

/// Raydium CPMM 单边手续费，25 bps。
pub const RAYDIUM_CPMM_FEE_BPS: f64 = 25.0;

/// 一笔 swap 的原始观测，单位已经按 decimals 归一化过（不是 raw lamports）。
#[derive(Debug, Clone)]
pub struct RawSwap {
    pub pool_id: String,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: f64,
    pub amount_out: f64,
    pub slot: i64,
    pub priority_fee_lamports: i64,
}

/// 交易对，按 mint 字典序排列，保证 A/B 与 B/A 落到同一个 key。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PairKey {
    pub base: String,
    pub quote: String,
}

impl PairKey {
    pub fn new(a: &str, b: &str) -> Self {
        if a <= b {
            Self {
                base: a.to_string(),
                quote: b.to_string(),
            }
        } else {
            Self {
                base: b.to_string(),
                quote: a.to_string(),
            }
        }
    }
}

/// 一个池子在某段窗口内的加权报价。
#[derive(Debug, Clone)]
pub struct PoolQuote {
    pub pool_id: String,
    /// 窗口内的成交量加权价格，单位：quote per base。
    pub vwap: f64,
    /// 窗口内以 base 计的成交额，用来判断这个报价背后有多少成交支撑。
    pub base_volume: f64,
    pub swap_count: usize,
    pub last_slot: i64,
    pub avg_priority_fee_lamports: f64,
}

#[derive(Debug, Clone)]
pub struct Params {
    pub fee_bps_per_side: f64,
    pub base_fee_lamports: f64,
    pub priority_multiplier: f64,
    pub sol_price_usd: f64,
    /// 两个池子最新成交允许的最大 slot 差，超过就认为价格不同期。
    pub max_slot_gap: i64,
    /// 净价差门槛（百分比）。
    pub min_net_pct: f64,
    pub trade_size_usd: f64,
    /// 一个池子至少要几笔成交才参与比较。
    pub min_swaps_per_pool: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            fee_bps_per_side: RAYDIUM_CPMM_FEE_BPS,
            base_fee_lamports: 5_000.0,
            priority_multiplier: 1.0,
            sol_price_usd: 150.0,
            max_slot_gap: 20,
            min_net_pct: 0.3,
            trade_size_usd: 1_000.0,
            min_swaps_per_pool: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Opportunity {
    pub pair: PairKey,
    pub buy_pool: String,
    pub sell_pool: String,
    pub buy_price: f64,
    pub sell_price: f64,
    pub gross_pct: f64,
    pub fee_pct: f64,
    pub gas_usd: f64,
    pub net_pct: f64,
    pub est_profit_usd: f64,
    pub buy_slot: i64,
    pub sell_slot: i64,
    /// 算利润用的交易额，渲染时直接引用，避免从利润率反推。
    pub trade_size_usd: f64,
    /// 两池窗口成交额中较小的那个：报价的可执行上限参考，不等于真实深度。
    pub observed_volume_usd: f64,
}

impl Opportunity {
    /// 交易额明显大于窗口内实际成交额时，价差不能线性放大——标注出来，
    /// 否则一个只成交了 $118 的池子会被报成「$1000 可赚 $8.77」。
    pub fn is_size_unrealistic(&self) -> bool {
        self.trade_size_usd > self.observed_volume_usd
    }

    pub fn render(&self) -> String {
        let caution = if self.is_size_unrealistic() {
            format!(
                " ⚠ 窗口成交额仅 ${:.0}，${:.0} 的单子吃不下",
                self.observed_volume_usd, self.trade_size_usd
            )
        } else {
            String::new()
        };
        format!(
            "{} -> {} | 净 {:.3}% @ ${:.0} | 毛利 {:.3}% − 手续费 {:.3}% − gas ${:.2} = {:.3}% | 利润 ${:.2} | 买 {:.6}({} @slot {}) 卖 {:.6}({} @slot {}) | 窗口成交额 ${:.0}{}",
            short(&self.pair.base),
            short(&self.pair.quote),
            self.net_pct,
            self.trade_size_usd,
            self.gross_pct,
            self.fee_pct,
            self.gas_usd,
            self.net_pct,
            self.est_profit_usd,
            self.buy_price,
            short(&self.buy_pool),
            self.buy_slot,
            self.sell_price,
            short(&self.sell_pool),
            self.sell_slot,
            self.observed_volume_usd,
            caution,
        )
    }
}

fn short(s: &str) -> String {
    s.chars().take(8).collect()
}

/// 把原始成交聚合成「每个池子一个报价」。
///
/// 反向成交（quote → base）会先取倒数再计入，否则同一池子的两个方向
/// 会被当成两个价格，凭空造出价差。
pub fn build_quotes(swaps: &[RawSwap]) -> HashMap<PairKey, Vec<PoolQuote>> {
    struct Acc {
        weighted_price_sum: f64,
        base_volume: f64,
        count: usize,
        last_slot: i64,
        priority_sum: f64,
    }

    let mut acc: HashMap<(PairKey, String), Acc> = HashMap::new();

    for s in swaps {
        if !s.amount_in.is_finite() || !s.amount_out.is_finite() {
            continue;
        }
        if s.amount_in <= 0.0 || s.amount_out <= 0.0 {
            continue;
        }
        let pair = PairKey::new(&s.token_in, &s.token_out);
        let forward = s.token_in == pair.base;

        // 换算成「quote per base」
        let (price, base_amount) = if forward {
            (s.amount_out / s.amount_in, s.amount_in)
        } else {
            (s.amount_in / s.amount_out, s.amount_out)
        };
        if !price.is_finite() || price <= 0.0 {
            continue;
        }

        let e = acc.entry((pair, s.pool_id.clone())).or_insert(Acc {
            weighted_price_sum: 0.0,
            base_volume: 0.0,
            count: 0,
            last_slot: s.slot,
            priority_sum: 0.0,
        });
        e.weighted_price_sum += price * base_amount;
        e.base_volume += base_amount;
        e.count += 1;
        e.last_slot = e.last_slot.max(s.slot);
        e.priority_sum += s.priority_fee_lamports as f64;
    }

    let mut out: HashMap<PairKey, Vec<PoolQuote>> = HashMap::new();
    for ((pair, pool_id), a) in acc {
        if a.base_volume <= 0.0 {
            continue;
        }
        out.entry(pair).or_default().push(PoolQuote {
            pool_id,
            vwap: a.weighted_price_sum / a.base_volume,
            base_volume: a.base_volume,
            swap_count: a.count,
            last_slot: a.last_slot,
            avg_priority_fee_lamports: a.priority_sum / a.count as f64,
        });
    }
    out
}

/// 在同一个交易对的两个池子之间找可执行价差。
pub fn find_opportunities(
    quotes: &HashMap<PairKey, Vec<PoolQuote>>,
    params: &Params,
) -> Vec<Opportunity> {
    let fee_pct = params.fee_bps_per_side * 2.0 / 100.0;
    let mut found = Vec::new();

    for (pair, pools) in quotes {
        let eligible: Vec<&PoolQuote> = pools
            .iter()
            .filter(|q| q.swap_count >= params.min_swaps_per_pool && q.vwap > 0.0)
            .collect();

        for i in 0..eligible.len() {
            for j in 0..eligible.len() {
                if i == j {
                    continue;
                }
                let buy = eligible[i];
                let sell = eligible[j];
                if buy.vwap >= sell.vwap {
                    continue;
                }
                if (buy.last_slot - sell.last_slot).abs() > params.max_slot_gap {
                    continue;
                }

                let gross_pct = (sell.vwap - buy.vwap) / buy.vwap * 100.0;
                if gross_pct <= fee_pct {
                    continue;
                }

                let avg_priority =
                    (buy.avg_priority_fee_lamports + sell.avg_priority_fee_lamports) / 2.0;
                let gas_lamports =
                    params.base_fee_lamports + params.priority_multiplier * avg_priority;
                let gas_usd = gas_lamports / 1_000_000_000.0 * params.sol_price_usd;

                let net_pct = gross_pct - fee_pct - (gas_usd / params.trade_size_usd * 100.0);
                if net_pct < params.min_net_pct {
                    continue;
                }

                found.push(Opportunity {
                    pair: pair.clone(),
                    buy_pool: buy.pool_id.clone(),
                    sell_pool: sell.pool_id.clone(),
                    buy_price: buy.vwap,
                    sell_price: sell.vwap,
                    gross_pct,
                    fee_pct,
                    gas_usd,
                    net_pct,
                    est_profit_usd: params.trade_size_usd * net_pct / 100.0,
                    buy_slot: buy.last_slot,
                    sell_slot: sell.last_slot,
                    trade_size_usd: params.trade_size_usd,
                    observed_volume_usd: buy.base_volume.min(sell.base_volume),
                });
            }
        }
    }

    found.sort_by(|a, b| b.net_pct.partial_cmp(&a.net_pct).unwrap_or(std::cmp::Ordering::Equal));
    found
}

/// 一次买一卖组成一笔交易，只算一次签名费。
pub fn gas_usd_for_tx(params: &Params, avg_priority_fee_lamports: f64) -> f64 {
    let lamports =
        params.base_fee_lamports + params.priority_multiplier * avg_priority_fee_lamports;
    lamports / 1_000_000_000.0 * params.sol_price_usd
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: &str = "So11111111111111111111111111111111111111112";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn swap(
        pool: &str,
        tin: &str,
        tout: &str,
        amount_in: f64,
        amount_out: f64,
        slot: i64,
    ) -> RawSwap {
        RawSwap {
            pool_id: pool.to_string(),
            token_in: tin.to_string(),
            token_out: tout.to_string(),
            amount_in,
            amount_out,
            slot,
            priority_fee_lamports: 5_000,
        }
    }

    /// SOL 在字典序上大于 USDC（'S' > 'E'），所以 base 是 USDC。
    #[test]
    fn pair_key_is_order_independent() {
        assert_eq!(PairKey::new(SOL, USDC), PairKey::new(USDC, SOL));
        assert_eq!(PairKey::new(SOL, USDC).base, USDC);
    }

    #[test]
    fn reverse_direction_yields_same_price() {
        // 正向：100 USDC 换 1 SOL ⇒ 0.01 SOL per USDC
        // 反向：1 SOL 换 100 USDC ⇒ 同样 100 USDC per SOL = 0.01
        let swaps =
            vec![swap("pool", USDC, SOL, 100.0, 1.0, 10), swap("pool", SOL, USDC, 1.0, 100.0, 11)];
        let quotes = build_quotes(&swaps);
        let key = PairKey::new(USDC, SOL);
        let q = &quotes[&key];
        assert_eq!(q.len(), 1, "同一池子两个方向不应产生两个报价");
        assert!((q[0].vwap - 0.01).abs() < 1e-12, "vwap={}", q[0].vwap);
        assert_eq!(q[0].swap_count, 2);
        assert!((q[0].base_volume - 200.0).abs() < 1e-9);
    }

    #[test]
    fn vwap_weights_by_base_volume() {
        let swaps = vec![
            swap("pool", USDC, SOL, 100.0, 1.0, 1),  // 0.01，量 100
            swap("pool", USDC, SOL, 900.0, 18.0, 2), // 0.02，量 900
        ];
        let quotes = build_quotes(&swaps);
        let q = &quotes[&PairKey::new(USDC, SOL)][0];
        // (0.01*100 + 0.02*900) / 1000 = 0.019
        assert!((q.vwap - 0.019).abs() < 1e-12, "vwap={}", q.vwap);
    }

    #[test]
    fn detects_cross_pool_spread_after_fees() {
        let mut swaps = Vec::new();
        // cheap 池：0.0100 USDC/SOL；dear 池：0.0101 USDC/SOL ⇒ 毛价差 1%
        for i in 0..5 {
            swaps.push(swap("cheap", USDC, SOL, 1_000.0, 10.0, 100 + i));
            swaps.push(swap("dear", USDC, SOL, 1_000.0, 10.1, 100 + i));
        }
        let quotes = build_quotes(&swaps);
        let params = Params {
            min_swaps_per_pool: 3,
            ..Default::default()
        };
        let found = find_opportunities(&quotes, &params);

        assert_eq!(found.len(), 1, "应该只报一次（买 cheap 卖 dear）");
        let o = &found[0];
        assert_eq!(o.buy_pool, "cheap");
        assert_eq!(o.sell_pool, "dear");
        assert!((o.gross_pct - 1.0).abs() < 1e-9, "gross={}", o.gross_pct);
        assert!((o.fee_pct - 0.5).abs() < 1e-12, "fee={}", o.fee_pct);
        // 净价差 = 1% - 0.5% - gas
        assert!(o.net_pct < 0.5 && o.net_pct > 0.45, "net={}", o.net_pct);
    }

    #[test]
    fn spread_below_fee_is_dropped() {
        // 毛价差 0.3% < 双边手续费 0.5%
        let mut swaps = Vec::new();
        for i in 0..5 {
            swaps.push(swap("a", USDC, SOL, 1_000.0, 10.0, 100 + i));
            swaps.push(swap("b", USDC, SOL, 1_000.0, 10.03, 100 + i));
        }
        let quotes = build_quotes(&swaps);
        let found = find_opportunities(
            &quotes,
            &Params {
                min_swaps_per_pool: 3,
                ..Default::default()
            },
        );
        assert!(found.is_empty());
    }

    #[test]
    fn thin_pool_is_ignored() {
        let mut swaps = Vec::new();
        for i in 0..5 {
            swaps.push(swap("deep", USDC, SOL, 1_000.0, 10.0, 100 + i));
        }
        swaps.push(swap("thin", USDC, SOL, 1_000.0, 11.0, 100));
        let quotes = build_quotes(&swaps);
        let found = find_opportunities(
            &quotes,
            &Params {
                min_swaps_per_pool: 3,
                ..Default::default()
            },
        );
        assert!(found.is_empty(), "只有 1 笔成交的池子不该被比较");
    }

    #[test]
    fn stale_quote_is_ignored() {
        let mut swaps = Vec::new();
        for i in 0..5 {
            swaps.push(swap("old", USDC, SOL, 1_000.0, 10.0, 100 + i));
            swaps.push(swap("new", USDC, SOL, 1_000.0, 10.5, 1_000 + i));
        }
        let quotes = build_quotes(&swaps);
        let params = Params {
            min_swaps_per_pool: 3,
            max_slot_gap: 20,
            ..Default::default()
        };
        let found = find_opportunities(&quotes, &params);
        assert!(found.is_empty(), "相隔 900 个 slot 的报价不能算套利机会");
    }

    #[test]
    fn gas_is_charged_once_per_transaction() {
        let params = Params {
            sol_price_usd: 200.0,
            ..Default::default()
        };
        // 5000 lamports = 0.000005 SOL × 200 = $0.001
        let gas = gas_usd_for_tx(&params, 0.0);
        assert!((gas - 0.001).abs() < 1e-9, "gas={}", gas);
    }

    #[test]
    fn flags_trade_size_larger_than_observed_volume() {
        // 每笔只有 10 USDC，5 笔共 50 USDC；用 $1000 去算利润是虚的
        let mut swaps = Vec::new();
        for i in 0..5 {
            swaps.push(swap("thin", USDC, SOL, 10.0, 0.10, 100 + i));
            swaps.push(swap("thin2", USDC, SOL, 10.0, 0.101, 100 + i));
        }
        let quotes = build_quotes(&swaps);
        let params = Params {
            min_swaps_per_pool: 3,
            trade_size_usd: 1_000.0,
            min_net_pct: 0.1,
            ..Default::default()
        };
        let found = find_opportunities(&quotes, &params);
        assert_eq!(found.len(), 1);
        assert!(found[0].is_size_unrealistic(), "50 < 1000 应当被标记");
        assert!(found[0].render().contains("吃不下"));
    }
}
