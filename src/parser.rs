use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use solana_transaction_status::{
    EncodedTransaction, UiCompiledInstruction, UiInstruction, UiMessage, UiTransactionStatusMeta,
    option_serializer::OptionSerializer,
};

use crate::models::SwapEvent;

/// Raydium CPMM（CP-Swap）主网程序地址。
///
/// 取自官方文档 reference/program-addresses：`CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`。
/// 此前这里写成了 `...NunggLgHpjANv64BHgf7Ct91`——前 24 位相同、后半段不同，
/// 链上根本不存在这个账户，导致签名查询永远是空数组。
pub const RAYDIUM_CPMM_PROGRAM_ID: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";

/// Anchor 的指令判别码 = `sha256("global:<name>")[:8]`。
///
/// 旧值 `f8c69e91e17587c8` 不是 CPMM 的任何一条指令，实测链上从未匹配上——
/// 于是「按指令账户定位金库」这条路一次都没走通，全部交易退到启发式兜底，
/// 把 Orca / Meteora / CLMM 的池子都记成了自己的 pool_id。
const SWAP_BASE_INPUT_DISCRIMINATOR: [u8; 8] = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];
const SWAP_BASE_OUTPUT_DISCRIMINATOR: [u8; 8] = [0x37, 0xd9, 0x62, 0x56, 0xa3, 0x4a, 0xb4, 0xad];

/// `swap_base_input` / `swap_base_output` 的账户顺序（链上实测 13 个账户）：
/// `[payer, authority, amm_config, pool_state, user_in, user_out, in_vault,
///   out_vault, in_tp, out_tp, in_mint, out_mint, observation]`
const ACCOUNT_POOL_STATE: usize = 3;
const ACCOUNT_INPUT_VAULT: usize = 6;
const ACCOUNT_OUTPUT_VAULT: usize = 7;

struct BalanceChange {
    mint: String,
    decimals: u8,
    delta: i64,
}

/// 交易的完整账户表。
///
/// `json` 编码的 v0 交易只返回静态账户，地址查找表（ALT）展开出来的账户放在
/// `meta.loaded_addresses` 里。指令里的 `accounts` / `program_id_index` 用的是
/// **两者拼接后**的下标（静态 → 可写 → 只读），不拼就会出现索引越界，
/// 更糟的是静默取不到 CPMM 程序地址，整笔交易掉进启发式兜底。
fn account_keys_from_transaction(
    transaction: &EncodedTransaction,
    meta: &UiTransactionStatusMeta,
) -> Vec<String> {
    let mut keys = match transaction {
        EncodedTransaction::Json(tx) => match &tx.message {
            UiMessage::Raw(msg) => msg.account_keys.clone(),
            UiMessage::Parsed(msg) => msg.account_keys.iter().map(|a| a.pubkey.clone()).collect(),
        },
        _ => Vec::new(),
    };
    if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
        keys.extend(loaded.writable.iter().cloned());
        keys.extend(loaded.readonly.iter().cloned());
    }
    keys
}

fn balance_change(meta: &UiTransactionStatusMeta, account_index: u8) -> Option<BalanceChange> {
    let empty_balances = Vec::new();
    let pre = meta
        .pre_token_balances
        .as_ref()
        .unwrap_or(&empty_balances)
        .iter()
        .find(|b| b.account_index == account_index)?;
    let post = meta
        .post_token_balances
        .as_ref()
        .unwrap_or(&empty_balances)
        .iter()
        .find(|b| b.account_index == account_index)?;

    let pre_amount = pre.ui_token_amount.amount.parse::<u64>().ok()?;
    let post_amount = post.ui_token_amount.amount.parse::<u64>().ok()?;

    Some(BalanceChange {
        mint: pre.mint.clone(),
        decimals: pre.ui_token_amount.decimals,
        delta: post_amount as i64 - pre_amount as i64,
    })
}

/// 从一条 CPMM swap 指令里取出的三个账户下标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PoolSwapIx {
    pool_state: u8,
    input_vault: u8,
    output_vault: u8,
}

fn as_swap_ix(ix: &UiCompiledInstruction, program_index: u8) -> Option<PoolSwapIx> {
    if ix.program_id_index != program_index || ix.accounts.len() <= ACCOUNT_OUTPUT_VAULT {
        return None;
    }
    let decoded = bs58::decode(&ix.data).into_vec().ok()?;
    if decoded.len() < 8 {
        return None;
    }
    let discriminator = &decoded[..8];
    if discriminator != SWAP_BASE_INPUT_DISCRIMINATOR
        && discriminator != SWAP_BASE_OUTPUT_DISCRIMINATOR
    {
        return None;
    }
    Some(PoolSwapIx {
        pool_state: ix.accounts[ACCOUNT_POOL_STATE],
        input_vault: ix.accounts[ACCOUNT_INPUT_VAULT],
        output_vault: ix.accounts[ACCOUNT_OUTPUT_VAULT],
    })
}

/// 收集一笔交易里所有 CPMM swap 指令（顶层 + CPI 内层）。
///
/// 只扫顶层是不够的：聚合器（GMGN、Jupiter 之类）会把 CPMM 的 swap 当 CPI 调，
/// 顶层指令属于聚合器自己。此前这些交易全部掉进启发式兜底，pool_id 记成了
/// 金库账户的 owner（CPMM authority PDA），那个地址是派生出来的 PDA、
/// 链上既没有数据也不等于 PoolState，于是「想读储备」时根本找不到池子。
fn extract_swap_instructions(
    transaction: &EncodedTransaction,
    meta: &UiTransactionStatusMeta,
    account_keys: &[String],
) -> Vec<PoolSwapIx> {
    let Some(program_index) =
        account_keys.iter().position(|k| k == RAYDIUM_CPMM_PROGRAM_ID).map(|i| i as u8)
    else {
        return Vec::new();
    };

    let mut found = Vec::new();

    if let EncodedTransaction::Json(tx) = transaction
        && let UiMessage::Raw(msg) = &tx.message
    {
        for ix in &msg.instructions {
            if let Some(swap) = as_swap_ix(ix, program_index) {
                found.push(swap);
            }
        }
    }

    if let OptionSerializer::Some(inner) = &meta.inner_instructions {
        for group in inner {
            for ui_ix in &group.instructions {
                // 内层指令可能是编译型（有 accounts/data）或 JSON 解析型；
                // 只有编译型的账户下标能直接对应到 message 的 account_keys
                if let UiInstruction::Compiled(ix) = ui_ix
                    && let Some(swap) = as_swap_ix(ix, program_index)
                {
                    found.push(swap);
                }
            }
        }
    }

    found.dedup();
    found
}

pub fn extract_swaps(
    signature: &str,
    slot: u64,
    block_time: Option<DateTime<Utc>>,
    meta: &UiTransactionStatusMeta,
    transaction: &EncodedTransaction,
) -> Vec<SwapEvent> {
    let account_keys = account_keys_from_transaction(transaction, meta);

    let swap_ixs = extract_swap_instructions(transaction, meta, &account_keys);
    if swap_ixs.is_empty() {
        // 解不出 swap_base_input / swap_base_output 就不猜。
        //
        // 这里以前会退到一个「同一 owner 下两个 token 余额一增一减」的启发式：
        // 它能把漏解析的交易捞回来，代价是 pool_id 只能填金库 owner（一个派生
        // PDA），而那个地址在链上没有账户数据，之后想读储备必然失败。扩样本后
        // 250 个池子里有 8 个是这么来的，全是 Whirlpool 的池子被顺手扫进来。
        // 宁可少一行可解析的数据，也不要多一行不可解析的噪声——调用方用
        // `txs_without_swap` 统计这类交易，需要补解析时看计数即可。
        return Vec::new();
    }

    let mut swaps = Vec::new();
    for ix in swap_ixs {
        // pool_id 必须是 PoolState 地址本身：只有它能 getAccountInfo 读出
        // 两个金库地址和 mint，进而算出储备中间价。记成金库 owner 时，
        // 那个 PDA 在链上没有数据，后续想读储备会静默失败。
        let Some(pool_state) = account_keys.get(ix.pool_state as usize).cloned() else {
            continue;
        };
        let Some(input) = balance_change(meta, ix.input_vault) else {
            continue;
        };
        let Some(output) = balance_change(meta, ix.output_vault) else {
            continue;
        };
        if input.delta <= 0 || output.delta >= 0 {
            continue;
        }

        let amount_in = BigDecimal::from(input.delta as u64)
            / BigDecimal::from(10u64.pow(input.decimals as u32));
        let amount_out = BigDecimal::from(output.delta.unsigned_abs())
            / BigDecimal::from(10u64.pow(output.decimals as u32));

        swaps.push(SwapEvent {
            signature: signature.to_string(),
            slot,
            block_time,
            program_id: RAYDIUM_CPMM_PROGRAM_ID.to_string(),
            pool_id: pool_state,
            token_in: input.mint,
            token_out: output.mint,
            amount_in_raw: input.delta as u64,
            amount_out_raw: output.delta.unsigned_abs(),
            amount_in,
            amount_out,
            priority_fee_lamports: meta.fee,
        });
    }

    swaps
}
#[cfg(test)]
mod tests {
    use solana_account_decoder_client_types::token::UiTokenAmount;
    use solana_message::MessageHeader;
    use solana_transaction_status::{
        EncodedTransaction, UiCompiledInstruction, UiInnerInstructions, UiInstruction,
        UiLoadedAddresses, UiMessage, UiRawMessage, UiTransaction, UiTransactionStatusMeta,
        UiTransactionTokenBalance, option_serializer::OptionSerializer,
    };

    use super::*;

    const FEE_PAYER: &str = "FeePayer111111111111111111111111111111111111";
    const POOL_STATE: &str = "PoolState1111111111111111111111111111111111";
    const INPUT_MINT: &str = "So11111111111111111111111111111111111111112";
    const OUTPUT_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn ui_token_amount(amount: u64, decimals: u8) -> UiTokenAmount {
        UiTokenAmount {
            ui_amount: None,
            decimals,
            amount: amount.to_string(),
            ui_amount_string: String::new(),
        }
    }

    fn token_balance(
        account_index: u8,
        mint: &str,
        owner: &str,
        amount: u64,
        decimals: u8,
    ) -> UiTransactionTokenBalance {
        UiTransactionTokenBalance {
            account_index,
            mint: mint.to_string(),
            ui_token_amount: ui_token_amount(amount, decimals),
            owner: OptionSerializer::Some(owner.to_string()),
            program_id: OptionSerializer::Skip,
        }
    }

    fn raw_transaction(
        account_keys: Vec<String>,
        instructions: Vec<UiCompiledInstruction>,
    ) -> EncodedTransaction {
        EncodedTransaction::Json(UiTransaction {
            signatures: vec!["sig".to_string()],
            message: UiMessage::Raw(UiRawMessage {
                header: MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                },
                account_keys,
                recent_blockhash: "blockhash".to_string(),
                instructions,
                address_table_lookups: None,
            }),
        })
    }

    fn meta_with_balances(
        fee: u64,
        pre: Vec<UiTransactionTokenBalance>,
        post: Vec<UiTransactionTokenBalance>,
    ) -> UiTransactionStatusMeta {
        UiTransactionStatusMeta {
            err: None,
            status: Ok(()),
            fee,
            pre_balances: vec![],
            post_balances: vec![],
            inner_instructions: OptionSerializer::None,
            log_messages: OptionSerializer::None,
            pre_token_balances: OptionSerializer::Some(pre),
            post_token_balances: OptionSerializer::Some(post),
            rewards: OptionSerializer::None,
            loaded_addresses: OptionSerializer::Skip,
            return_data: OptionSerializer::Skip,
            compute_units_consumed: OptionSerializer::Skip,
            cost_units: OptionSerializer::Skip,
        }
    }

    /// 按链上真实的账户顺序构造：payer, authority, amm_config, pool_state,
    /// user_in, user_out, in_vault, out_vault。
    fn raydium_swap_instruction(
        pool_state: u8,
        input_vault: u8,
        output_vault: u8,
        program_index: u8,
    ) -> UiCompiledInstruction {
        let mut data = SWAP_BASE_INPUT_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[0u8; 16]);
        UiCompiledInstruction {
            program_id_index: program_index,
            accounts: vec![0, 1, 2, pool_state, 4, 5, input_vault, output_vault],
            data: bs58::encode(&data).into_string(),
            stack_height: None,
        }
    }

    /// 旧版本用的 `f8c69e91e17587c8` 不是任何一条 CPMM 指令，链上永远不会匹配。
    /// 这条测试把这个错误钉住，防止有人"顺手"改回去。
    #[test]
    fn legacy_wrong_discriminator_never_matches() {
        let mut data = [0xf8u8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8].to_vec();
        data.extend_from_slice(&[0u8; 16]);
        let ix = UiCompiledInstruction {
            program_id_index: 7,
            accounts: vec![0, 1, 2, 4, 4, 5, 5, 6],
            data: bs58::encode(&data).into_string(),
            stack_height: None,
        };
        assert!(as_swap_ix(&ix, 7).is_none(), "错误的判别码不该被当成 swap");
    }

    /// `swap_base_output` 与 `swap_base_input` 账户顺序相同，判别码不同。
    #[test]
    fn swap_base_output_is_recognised() {
        let mut data = SWAP_BASE_OUTPUT_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[0u8; 16]);
        let ix = UiCompiledInstruction {
            program_id_index: 7,
            accounts: vec![0, 1, 2, 4, 4, 5, 5, 6],
            data: bs58::encode(&data).into_string(),
            stack_height: None,
        };
        assert_eq!(
            as_swap_ix(&ix, 7),
            Some(PoolSwapIx {
                pool_state: 4,
                input_vault: 5,
                output_vault: 6
            })
        );
    }

    #[test]
    fn non_raydium_transaction_returns_empty() {
        let keys = vec![FEE_PAYER.to_string(), INPUT_MINT.to_string(), OUTPUT_MINT.to_string()];
        let ix = UiCompiledInstruction {
            program_id_index: 1,
            accounts: vec![0, 1, 2],
            data: bs58::encode([1u8; 8]).into_string(),
            stack_height: None,
        };
        let tx = raw_transaction(keys, vec![ix]);
        let meta = meta_with_balances(5000, vec![], vec![]);
        assert!(extract_swaps("sig", 100, None, &meta, &tx).is_empty());
    }

    #[test]
    fn swap_instruction_yields_pool_state_and_vaults() {
        let keys = vec![
            FEE_PAYER.to_string(),
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            POOL_STATE.to_string(),
            INPUT_MINT.to_string(),
            OUTPUT_MINT.to_string(),
            RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        ];
        let ix = raydium_swap_instruction(4, 5, 6, 7);
        let tx = raw_transaction(keys, vec![ix]);
        let meta = meta_with_balances(5000, vec![], vec![]);
        let ixs = extract_swap_instructions(&tx, &meta, &account_keys_from_transaction(&tx, &meta));
        assert_eq!(
            ixs,
            vec![PoolSwapIx {
                pool_state: 4,
                input_vault: 5,
                output_vault: 6
            }]
        );
        assert!(extract_swaps("sig", 100, None, &meta, &tx).is_empty());
    }

    /// 聚合器把 CPMM swap 当 CPI 调时，顶层指令属于聚合器自己。
    /// 只扫顶层会让这类交易退到启发式兜底，pool_id 记成金库 owner（PDA）。
    #[test]
    fn inner_instruction_swap_uses_pool_state_as_pool_id() {
        let keys = vec![
            FEE_PAYER.to_string(),
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            POOL_STATE.to_string(),
            INPUT_MINT.to_string(),
            OUTPUT_MINT.to_string(),
            RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        ];
        let outer = UiCompiledInstruction {
            program_id_index: 1,
            accounts: vec![0, 4, 5, 6],
            data: bs58::encode([9u8; 8]).into_string(),
            stack_height: None,
        };
        let tx = raw_transaction(keys, vec![outer]);
        let pool_state = "PoolState1111111111111111111111111111111111";
        let inner = UiInnerInstructions {
            index: 0,
            instructions: vec![UiInstruction::Compiled(raydium_swap_instruction(4, 5, 6, 7))],
        };
        let pre = vec![
            token_balance(5, INPUT_MINT, pool_state, 10_000_000_000, 9),
            token_balance(6, OUTPUT_MINT, pool_state, 5_000_000_000, 6),
        ];
        let post = vec![
            token_balance(5, INPUT_MINT, pool_state, 11_000_000_000, 9),
            token_balance(6, OUTPUT_MINT, pool_state, 4_500_000_000, 6),
        ];
        let mut meta = meta_with_balances(5000, pre, post);
        meta.inner_instructions = OptionSerializer::Some(vec![inner]);

        let swaps = extract_swaps("sig", 100, None, &meta, &tx);
        assert_eq!(swaps.len(), 1, "CPI 里的 swap 也要被解析出来");
        assert_eq!(
            swaps[0].pool_id, POOL_STATE,
            "pool_id 必须是 PoolState，不是金库 owner"
        );
        assert_eq!(swaps[0].amount_in_raw, 1_000_000_000);
    }

    /// v0 交易用地址查找表时，`json` 编码的 accountKeys 只给静态部分，
    /// ALT 展开的账户必须按「可写 → 只读」拼在后面，否则下标全错位。
    #[test]
    fn lookup_table_accounts_are_appended() {
        let keys = vec![FEE_PAYER.to_string(), "static1".to_string()];
        let tx = raw_transaction(keys, vec![]);
        let mut meta = meta_with_balances(5000, vec![], vec![]);
        meta.loaded_addresses = OptionSerializer::Some(UiLoadedAddresses {
            writable: vec!["loaded_writable".to_string()],
            readonly: vec!["loaded_readonly".to_string()],
        });

        let merged = account_keys_from_transaction(&tx, &meta);
        assert_eq!(
            merged,
            vec![
                FEE_PAYER.to_string(),
                "static1".to_string(),
                "loaded_writable".to_string(),
                "loaded_readonly".to_string(),
            ]
        );
    }

    #[test]
    fn vault_based_swap_parsing() {
        let keys = vec![
            FEE_PAYER.to_string(),
            "A".to_string(),
            "B".to_string(),
            "C".to_string(),
            POOL_STATE.to_string(),
            INPUT_MINT.to_string(),
            OUTPUT_MINT.to_string(),
            RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        ];
        let ix = raydium_swap_instruction(4, 5, 6, 7);
        let tx = raw_transaction(keys, vec![ix]);
        let pre = vec![
            token_balance(5, INPUT_MINT, POOL_STATE, 10_000_000_000, 9),
            token_balance(6, OUTPUT_MINT, POOL_STATE, 5_000_000_000, 6),
        ];
        let post = vec![
            token_balance(5, INPUT_MINT, POOL_STATE, 11_000_000_000, 9),
            token_balance(6, OUTPUT_MINT, POOL_STATE, 4_500_000_000, 6),
        ];
        let meta = meta_with_balances(5000, pre, post);
        let swaps = extract_swaps("sig", 100, None, &meta, &tx);
        assert_eq!(swaps.len(), 1);
        let swap = &swaps[0];
        assert_eq!(swap.token_in, INPUT_MINT);
        assert_eq!(swap.token_out, OUTPUT_MINT);
        assert_eq!(swap.amount_in_raw, 1_000_000_000);
        assert_eq!(swap.amount_out_raw, 500_000_000);
        assert_eq!(swap.priority_fee_lamports, 5000);
        assert_eq!(swap.pool_id, POOL_STATE);
    }

    #[test]
    fn undecodable_swap_is_dropped_not_guessed() {
        // 同一个交易里某个 owner 的两个 token 余额一增一减，但指令判别码不认识。
        // 以前这里会写出 pool_id = 金库 owner 的行，那个地址链上没有账户，
        // 读储备必然失败。现在必须返回空。
        let keys = vec![
            FEE_PAYER.to_string(),
            INPUT_MINT.to_string(),
            OUTPUT_MINT.to_string(),
            RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        ];
        let ix = UiCompiledInstruction {
            program_id_index: 3,
            accounts: vec![0, 1, 2],
            data: bs58::encode([0u8; 8]).into_string(),
            stack_height: None,
        };
        let tx = raw_transaction(keys, vec![ix]);
        let owner = "Trader11111111111111111111111111111111111111";
        let pre = vec![
            token_balance(1, INPUT_MINT, owner, 0, 9),
            token_balance(2, OUTPUT_MINT, owner, 100_000_000, 6),
        ];
        let post = vec![
            token_balance(1, INPUT_MINT, owner, 100_000_000, 9),
            token_balance(2, OUTPUT_MINT, owner, 0, 6),
        ];
        let meta = meta_with_balances(5000, pre, post);
        let swaps = extract_swaps("sig", 100, None, &meta, &tx);
        assert!(
            swaps.is_empty(),
            "解不出 CPMM 指令的交易不能靠余额差猜出 swap，实际写出 {} 行",
            swaps.len()
        );
    }
}
