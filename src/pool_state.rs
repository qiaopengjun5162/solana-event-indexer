//! Raydium CPMM 池子状态与储备中间价。
//!
//! 为什么需要读链上账户：成交数据只能给出「已经发生过的价格」，
//! 用它做 VWAP 得到的是一个滞后估计。真正可执行的报价来自池子当下的储备，
//! 而 CPMM 的储备不在 PoolState 结构体里——PoolState 只存了两个金库账户地址，
//! 余额得再去查那两个 SPL / Token-2022 账户。
//!
//! 所以一笔完整的「读价」是三次账户查询：
//! `PoolState` → `token_0_vault` / `token_1_vault` → 两个 `amount`。

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_pubkey::Pubkey;

use crate::parser::RAYDIUM_CPMM_PROGRAM_ID;

/// Raydium CPMM `PoolState` 账户长度（主网实测 637 字节）。
pub const CPMM_POOL_STATE_LEN: usize = 637;

/// Anchor `account:PoolState` 的判别码（主网实测）。
///
/// 只靠长度判类型会出事：Orca Whirlpool 的池子账户是 653 字节，
/// 也大于 637，按长度放行会把它的字段按 CPMM 偏移读成一堆乱码地址。
pub const POOL_STATE_DISCRIMINATOR: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];

// 字段偏移：8 字节 Anchor discriminator 之后依次是 10 个 Pubkey（各 32 字节）。
const OFF_AMM_CONFIG: usize = 8;
const OFF_POOL_CREATOR: usize = 40;
const OFF_TOKEN_0_VAULT: usize = 72;
const OFF_TOKEN_1_VAULT: usize = 104;
const OFF_LP_MINT: usize = 136;
const OFF_TOKEN_0_MINT: usize = 168;
const OFF_TOKEN_1_MINT: usize = 200;
const OFF_OBSERVATION_KEY: usize = 296;
const OFF_STATUS: usize = 329;
const OFF_LP_MINT_DECIMALS: usize = 330;
const OFF_MINT_0_DECIMALS: usize = 331;
const OFF_MINT_1_DECIMALS: usize = 332;
const OFF_LP_SUPPLY: usize = 333;

/// SPL Token / Token-2022 账户里 `amount` 的偏移（mint 32 + owner 32）。
const OFF_TOKEN_AMOUNT: usize = 64;

/// `getMultipleAccounts` 单次请求的账户上限。
const MAX_ACCOUNTS_PER_REQUEST: usize = 100;

#[derive(Debug, PartialEq, Eq)]
pub enum PoolStateError {
    TooShort {
        expected: usize,
        actual: usize,
    },
    /// 长度够了但判别码不是 CPMM PoolState（比如 Orca Whirlpool 的池子）
    NotCpmmPoolState {
        got: [u8; 8],
    },
    TokenAccountTooShort {
        actual: usize,
    },
}

impl fmt::Display for PoolStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { expected, actual } => {
                write!(
                    f,
                    "账户数据长度 {actual} 不足 {expected}，不是 CPMM PoolState"
                )
            }
            Self::NotCpmmPoolState { got } => {
                write!(f, "判别码 {got:02x?} 不是 CPMM PoolState")
            }
            Self::TokenAccountTooShort { actual } => {
                write!(
                    f,
                    "代币账户长度 {actual} 不足 {OFF_TOKEN_AMOUNT}，读不到 amount"
                )
            }
        }
    }
}

impl std::error::Error for PoolStateError {}

/// `PoolState` 里跟定价相关的部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpmmPoolState {
    pub pool_id: String,
    pub amm_config: String,
    pub pool_creator: String,
    /// token_0 的金库账户（SPL Token 或 Token-2022）。
    pub token_0_vault: String,
    pub token_1_vault: String,
    pub lp_mint: String,
    pub token_0_mint: String,
    pub token_1_mint: String,
    pub observation_key: String,
    pub status: u8,
    pub lp_mint_decimals: u8,
    pub mint_0_decimals: u8,
    pub mint_1_decimals: u8,
    pub lp_supply: u64,
}

fn read_pubkey(data: &[u8], offset: usize) -> String {
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&data[offset..offset + 32]);
    Pubkey::new_from_array(raw).to_string()
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&data[offset..offset + 8]);
    u64::from_le_bytes(raw)
}

/// 解析 CPMM `PoolState`。
///
/// 只看长度不够严谨，但长度是这里唯一可靠的廉价判据：同长度、同 owner 的账户
/// 才会走到这里（调用方按 owner 过滤），再叠加 mint 可解析性检查。
pub fn parse_pool_state(pool_id: &str, data: &[u8]) -> Result<CpmmPoolState, PoolStateError> {
    if data.len() < CPMM_POOL_STATE_LEN {
        return Err(PoolStateError::TooShort {
            expected: CPMM_POOL_STATE_LEN,
            actual: data.len(),
        });
    }
    let mut discriminator = [0u8; 8];
    discriminator.copy_from_slice(&data[..8]);
    if discriminator != POOL_STATE_DISCRIMINATOR {
        return Err(PoolStateError::NotCpmmPoolState { got: discriminator });
    }

    Ok(CpmmPoolState {
        pool_id: pool_id.to_string(),
        amm_config: read_pubkey(data, OFF_AMM_CONFIG),
        pool_creator: read_pubkey(data, OFF_POOL_CREATOR),
        token_0_vault: read_pubkey(data, OFF_TOKEN_0_VAULT),
        token_1_vault: read_pubkey(data, OFF_TOKEN_1_VAULT),
        lp_mint: read_pubkey(data, OFF_LP_MINT),
        token_0_mint: read_pubkey(data, OFF_TOKEN_0_MINT),
        token_1_mint: read_pubkey(data, OFF_TOKEN_1_MINT),
        observation_key: read_pubkey(data, OFF_OBSERVATION_KEY),
        status: data[OFF_STATUS],
        lp_mint_decimals: data[OFF_LP_MINT_DECIMALS],
        mint_0_decimals: data[OFF_MINT_0_DECIMALS],
        mint_1_decimals: data[OFF_MINT_1_DECIMALS],
        lp_supply: read_u64(data, OFF_LP_SUPPLY),
    })
}

/// 读 SPL Token / Token-2022 账户余额（raw，未按 decimals 缩放）。
///
/// Token-2022 账户带扩展、长度不定，但 `amount` 的位置固定在 64..72。
pub fn parse_token_amount(data: &[u8]) -> Result<u64, PoolStateError> {
    if data.len() < OFF_TOKEN_AMOUNT + 8 {
        return Err(PoolStateError::TokenAccountTooShort { actual: data.len() });
    }
    Ok(read_u64(data, OFF_TOKEN_AMOUNT))
}

/// 一个池子在某一 slot 的储备中间价。
#[derive(Debug, Clone)]
pub struct ReserveQuote {
    pub pool_id: String,
    /// 按 mint 字典序归一后的 base（与 `spread::PairKey` 同一规则）。
    pub base_mint: String,
    pub quote_mint: String,
    /// 已按 decimals 缩放的 UI 数量。
    pub base_reserve: f64,
    pub quote_reserve: f64,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    /// quote per base。
    pub price: f64,
    /// 读取这些账户时的 slot，供判断报价新鲜度。
    pub slot: u64,
}

impl ReserveQuote {
    /// 池子深度（按 quote 计）——报价能承载多大单子的粗略上限。
    pub fn depth_quote(&self) -> f64 {
        self.quote_reserve
    }
}

/// 由储备算中间价（CPMM 无手续费瞬时报价 = 储备比）。
///
/// base/quote 的选择必须与成交价一致：都取 mint 字典序小的当 base，
/// 否则反向池子的价格会变成倒数，和 VWAP 放一起比就是两个数。
pub fn reserve_quote(
    state: &CpmmPoolState,
    amount_0: u64,
    amount_1: u64,
    slot: u64,
) -> Option<ReserveQuote> {
    if state.mint_0_decimals > 19 || state.mint_1_decimals > 19 {
        return None;
    }
    let ui_0 = amount_0 as f64 / 10f64.powi(state.mint_0_decimals as i32);
    let ui_1 = amount_1 as f64 / 10f64.powi(state.mint_1_decimals as i32);
    // 空池子或单边为零：算不出价格，也不要给出 inf 去污染排序
    if !ui_0.is_finite() || !ui_1.is_finite() || ui_0 <= 0.0 || ui_1 <= 0.0 {
        return None;
    }

    let (base_mint, quote_mint, base_reserve, quote_reserve, base_decimals, quote_decimals) =
        if state.token_0_mint <= state.token_1_mint {
            (
                state.token_0_mint.clone(),
                state.token_1_mint.clone(),
                ui_0,
                ui_1,
                state.mint_0_decimals,
                state.mint_1_decimals,
            )
        } else {
            (
                state.token_1_mint.clone(),
                state.token_0_mint.clone(),
                ui_1,
                ui_0,
                state.mint_1_decimals,
                state.mint_0_decimals,
            )
        };

    let price = quote_reserve / base_reserve;
    if !price.is_finite() || price <= 0.0 {
        return None;
    }

    Some(ReserveQuote {
        pool_id: state.pool_id.clone(),
        base_mint,
        quote_mint,
        base_reserve,
        quote_reserve,
        base_decimals,
        quote_decimals,
        price,
        slot,
    })
}

/// 批量读储备的结果。失败的三类分开记：地址不存在、长度不对、金库读不到。
#[derive(Debug, Default)]
pub struct ReserveSnapshot {
    pub quotes: Vec<ReserveQuote>,
    /// 链上查不到账户的 pool_id（旧版本把金库 owner PDA 当 pool_id 写过库）。
    pub missing: Vec<String>,
    /// 账户存在但解析失败的 pool_id，附带原因。
    pub unparsable: Vec<String>,
    /// 金库账户读不到余额的池子数量。
    pub missing_vaults: usize,
    pub slot: u64,
}

async fn fetch_amounts(client: &RpcClient, keys: &[Pubkey]) -> Result<HashMap<Pubkey, u64>> {
    let mut amounts = HashMap::new();
    for chunk in keys.chunks(MAX_ACCOUNTS_PER_REQUEST) {
        let accounts = client
            .get_multiple_accounts(chunk)
            .await
            .context("getMultipleAccounts(vault) failed")?;
        for (key, account) in chunk.iter().zip(accounts) {
            let Some(account) = account else { continue };
            if let Ok(amount) = parse_token_amount(&account.data) {
                amounts.insert(*key, amount);
            }
        }
    }
    Ok(amounts)
}

/// 从 PoolState 地址批量读储备中间价。
pub async fn fetch_reserves(client: &RpcClient, pool_ids: &[String]) -> Result<ReserveSnapshot> {
    let mut snapshot = ReserveSnapshot::default();
    let mut states: Vec<CpmmPoolState> = Vec::new();

    let pool_keys: Vec<Pubkey> = pool_ids
        .iter()
        .filter_map(|id| match Pubkey::from_str(id) {
            Ok(key) => Some(key),
            Err(_) => {
                snapshot.unparsable.push(format!("{id}: 不是合法 base58 地址"));
                None
            }
        })
        .collect();

    for chunk in pool_keys.chunks(MAX_ACCOUNTS_PER_REQUEST) {
        let accounts = client
            .get_multiple_accounts(chunk)
            .await
            .context("getMultipleAccounts(pool) failed")?;
        for (key, account) in chunk.iter().zip(accounts) {
            match account {
                None => snapshot.missing.push(key.to_string()),
                // owner 先过一遍：不是 CPMM 程序的账户连长度校验都不用做
                Some(account) if account.owner.to_string() != RAYDIUM_CPMM_PROGRAM_ID => {
                    snapshot
                        .unparsable
                        .push(format!("{key}: owner {} 不是 CPMM 程序", account.owner));
                }
                Some(account) => match parse_pool_state(&key.to_string(), &account.data) {
                    Ok(state) => states.push(state),
                    Err(e) => snapshot.unparsable.push(format!("{key}: {e}")),
                },
            }
        }
    }

    let mut vault_keys: Vec<Pubkey> = Vec::with_capacity(states.len() * 2);
    for state in &states {
        for vault in [&state.token_0_vault, &state.token_1_vault] {
            if let Ok(key) = Pubkey::from_str(vault) {
                vault_keys.push(key);
            }
        }
    }
    let amounts = fetch_amounts(client, &vault_keys).await?;
    snapshot.slot = client.get_slot().await.unwrap_or_default();

    for state in &states {
        let (Ok(v0), Ok(v1)) = (
            Pubkey::from_str(&state.token_0_vault),
            Pubkey::from_str(&state.token_1_vault),
        ) else {
            snapshot.missing_vaults += 1;
            continue;
        };
        let (Some(a0), Some(a1)) = (amounts.get(&v0), amounts.get(&v1)) else {
            snapshot.missing_vaults += 1;
            continue;
        };
        match reserve_quote(state, *a0, *a1, snapshot.slot) {
            Some(quote) => snapshot.quotes.push(quote),
            None => snapshot.missing_vaults += 1,
        }
    }

    snapshot.quotes.sort_by(|a, b| a.pool_id.cmp(&b.pool_id));

    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_0_MINT: &str = "So11111111111111111111111111111111111111112";
    const TOKEN_1_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const VAULT_0: &str = "3veB2six6Qpv4SQDZeH7wVcDHyT4CJ9Tv72i22rXyQUi";
    const VAULT_1: &str = "D7EfrAnySUBzadAwZZeouUbKFewYDrMKXM7vLWhgxLmz";
    const POOL: &str = "G9GrmZr34cGg2PdkmnVY9EHvzFQkb9QNWBxTEnh1tupU";

    fn write_pubkey(buf: &mut [u8], offset: usize, key: &str) {
        let bytes = Pubkey::from_str(key).unwrap().to_bytes();
        buf[offset..offset + 32].copy_from_slice(&bytes);
    }

    fn pool_state_bytes(decimals_0: u8, decimals_1: u8) -> Vec<u8> {
        let mut buf = vec![0u8; CPMM_POOL_STATE_LEN];
        buf[..8].copy_from_slice(&POOL_STATE_DISCRIMINATOR);
        write_pubkey(&mut buf, OFF_AMM_CONFIG, TOKEN_0_MINT);
        write_pubkey(&mut buf, OFF_POOL_CREATOR, TOKEN_0_MINT);
        write_pubkey(&mut buf, OFF_TOKEN_0_VAULT, VAULT_0);
        write_pubkey(&mut buf, OFF_TOKEN_1_VAULT, VAULT_1);
        write_pubkey(&mut buf, OFF_LP_MINT, VAULT_0);
        write_pubkey(&mut buf, OFF_TOKEN_0_MINT, TOKEN_0_MINT);
        write_pubkey(&mut buf, OFF_TOKEN_1_MINT, TOKEN_1_MINT);
        write_pubkey(&mut buf, OFF_OBSERVATION_KEY, VAULT_1);
        buf[OFF_STATUS] = 0;
        buf[OFF_LP_MINT_DECIMALS] = 9;
        buf[OFF_MINT_0_DECIMALS] = decimals_0;
        buf[OFF_MINT_1_DECIMALS] = decimals_1;
        buf[OFF_LP_SUPPLY..OFF_LP_SUPPLY + 8].copy_from_slice(&123_456_789u64.to_le_bytes());
        buf
    }

    fn token_account_bytes(amount: u64) -> Vec<u8> {
        let mut buf = vec![0u8; 165];
        buf[OFF_TOKEN_AMOUNT..OFF_TOKEN_AMOUNT + 8].copy_from_slice(&amount.to_le_bytes());
        buf
    }

    #[test]
    fn parses_pool_state_fields() {
        let data = pool_state_bytes(9, 6);
        let state = parse_pool_state(POOL, &data).expect("应当解析成功");
        assert_eq!(state.pool_id, POOL);
        assert_eq!(state.token_0_vault, VAULT_0);
        assert_eq!(state.token_1_vault, VAULT_1);
        assert_eq!(state.token_0_mint, TOKEN_0_MINT);
        assert_eq!(state.token_1_mint, TOKEN_1_MINT);
        assert_eq!(state.mint_0_decimals, 9);
        assert_eq!(state.mint_1_decimals, 6);
        assert_eq!(state.lp_supply, 123_456_789);
    }

    /// Orca Whirlpool 的池子账户 653 字节，长度上过得去，必须靠判别码挡住。
    #[test]
    fn rejects_foreign_pool_account_with_matching_length() {
        let mut data = pool_state_bytes(9, 6);
        data[..8].copy_from_slice(&[63, 149, 209, 12, 225, 128, 99, 9]);
        assert_eq!(
            parse_pool_state(POOL, &data).unwrap_err(),
            PoolStateError::NotCpmmPoolState {
                got: [63, 149, 209, 12, 225, 128, 99, 9]
            }
        );
    }

    #[test]
    fn rejects_short_pool_state() {
        let data = vec![0u8; 100];
        let err = parse_pool_state(POOL, &data).unwrap_err();
        assert_eq!(
            err,
            PoolStateError::TooShort {
                expected: CPMM_POOL_STATE_LEN,
                actual: 100
            }
        );
    }

    #[test]
    fn parses_token_amount_from_token2022_layout() {
        // Token-2022 账户带扩展，长度不是 165，但 amount 位置不变
        let mut data = token_account_bytes(42);
        data.extend_from_slice(&[7u8; 10]);
        assert_eq!(parse_token_amount(&data).unwrap(), 42);
    }

    #[test]
    fn rejects_short_token_account() {
        assert!(parse_token_amount(&[0u8; 64]).is_err());
    }

    /// SOL 的 base58 以 'S' 开头、USDC 以 'E' 开头，所以 USDC 是 base。
    #[test]
    fn base_is_the_lexicographically_smaller_mint() {
        let state = parse_pool_state(POOL, &pool_state_bytes(9, 6)).unwrap();
        // token_0 = SOL 1e9 raw = 1 SOL；token_1 = USDC 200e6 raw = 200 USDC
        let quote = reserve_quote(&state, 1_000_000_000, 200_000_000, 470_000_000).unwrap();
        assert_eq!(
            quote.base_mint, TOKEN_1_MINT,
            "USDC 字典序在前，应当是 base"
        );
        assert_eq!(quote.quote_mint, TOKEN_0_MINT);
        assert!((quote.base_reserve - 200.0).abs() < 1e-9);
        assert!((quote.quote_reserve - 1.0).abs() < 1e-9);
        // 1 SOL = 200 USDC ⇒ 每 1 USDC = 0.005 SOL
        assert!((quote.price - 0.005).abs() < 1e-12, "price={}", quote.price);
    }

    /// 同一个交易对，两个池子把 mint 存成相反顺序时，价格必须可比。
    #[test]
    fn mint_order_does_not_change_price() {
        let a = parse_pool_state(POOL, &pool_state_bytes(9, 6)).unwrap();
        // a：token_0 = SOL(9 位), token_1 = USDC(6 位)，储备 1 SOL / 200 USDC
        let a_quote = reserve_quote(&a, 1_000_000_000, 200_000_000, 1).unwrap();

        let mut swapped = pool_state_bytes(9, 6);
        write_pubkey(&mut swapped, OFF_TOKEN_0_MINT, TOKEN_1_MINT);
        write_pubkey(&mut swapped, OFF_TOKEN_1_MINT, TOKEN_0_MINT);
        swapped[OFF_MINT_0_DECIMALS] = 6;
        swapped[OFF_MINT_1_DECIMALS] = 9;
        let b = parse_pool_state(POOL, &swapped).unwrap();
        // b：token_0 = USDC(6 位), token_1 = SOL(9 位)，储备 200 USDC / 1 SOL
        let b_quote = reserve_quote(&b, 200_000_000, 1_000_000_000, 1).unwrap();

        assert_eq!(a_quote.base_mint, b_quote.base_mint);
        assert!(
            (a_quote.price - b_quote.price).abs() < 1e-12,
            "同一个池子的两种字段顺序应当给出同一个价：{} vs {}",
            a_quote.price,
            b_quote.price
        );
    }

    #[test]
    fn empty_side_yields_no_quote() {
        let state = parse_pool_state(POOL, &pool_state_bytes(9, 6)).unwrap();
        assert!(reserve_quote(&state, 0, 200_000_000, 1).is_none());
        assert!(reserve_quote(&state, 1_000_000_000, 0, 1).is_none());
    }

    /// 真实主网池子：偏移写错一个字节，下面这些断言就会集体失败。
    /// `cargo test -- --ignored live_pool_state_from_mainnet`
    #[test]
    #[ignore = "需要主网 RPC 与代理"]
    fn live_pool_state_from_mainnet() {
        // 这组值取自 2026-11-18 主网实测（`getAccountInfo` 637 字节）
        const LIVE_POOL: &str = "G9GrmZr34cGg2PdkmnVY9EHvzFQkb9QNWBxTEnh1tupU";
        const LIVE_MINT_0: &str = "TTWofwAge91oFhZs7kpQdyrVRkmevgM88xijGvQFbKo";
        const LIVE_MINT_1: &str = "AWytauWViaF8yBAPqBE6AxxVDq14smvhjP3K9is1yhzE";
        const LIVE_VAULT_0: &str = "3veB2six6Qpv4SQDZeH7wVcDHyT4CJ9Tv72i22rXyQUi";
        const LIVE_VAULT_1: &str = "D7EfrAnySUBzadAwZZeouUbKFewYDrMKXM7vLWhgxLmz";

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let client = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());
            let key = Pubkey::from_str(LIVE_POOL).unwrap();
            let account = client.get_account(&key).await.expect("账户应当存在");
            assert_eq!(account.data.len(), CPMM_POOL_STATE_LEN);

            let state = parse_pool_state(LIVE_POOL, &account.data).unwrap();
            assert_eq!(state.token_0_mint, LIVE_MINT_0);
            assert_eq!(state.token_1_mint, LIVE_MINT_1);
            assert_eq!(state.token_0_vault, LIVE_VAULT_0);
            assert_eq!(state.token_1_vault, LIVE_VAULT_1);

            let snapshot = fetch_reserves(&client, &[LIVE_POOL.to_string()]).await.unwrap();
            assert!(snapshot.missing.is_empty());
            assert_eq!(snapshot.quotes.len(), 1, "应当读到 1 个池子的储备");
            assert!(snapshot.quotes[0].price > 0.0);
        });
    }
}
