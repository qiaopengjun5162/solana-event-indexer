use std::collections::HashMap;

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use solana_transaction_status::{
    EncodedTransaction, UiCompiledInstruction, UiMessage, UiTransactionStatusMeta,
};

use crate::models::SwapEvent;

/// Raydium CPMM（CP-Swap）主网程序地址。
///
/// 取自官方文档 reference/program-addresses：`CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`。
/// 此前这里写成了 `...NunggLgHpjANv64BHgf7Ct91`——前 24 位相同、后半段不同，
/// 链上根本不存在这个账户，导致签名查询永远是空数组。
pub const RAYDIUM_CPMM_PROGRAM_ID: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

#[derive(Default)]
struct BalanceInfo {
    pre: Option<(String, String, u8, u64)>,
    post: Option<(String, String, u8, u64)>,
}

struct Delta {
    mint: String,
    decimals: u8,
    raw_delta: i64,
}

struct BalanceChange {
    mint: String,
    owner: String,
    decimals: u8,
    delta: i64,
}

fn account_keys_from_transaction(transaction: &EncodedTransaction) -> Vec<String> {
    match transaction {
        EncodedTransaction::Json(tx) => match &tx.message {
            UiMessage::Raw(msg) => msg.account_keys.clone(),
            UiMessage::Parsed(msg) => msg.account_keys.iter().map(|a| a.pubkey.clone()).collect(),
        },
        _ => Vec::new(),
    }
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
        owner: pre.owner.as_ref().unwrap_or(&String::new()).clone(),
        decimals: pre.ui_token_amount.decimals,
        delta: post_amount as i64 - pre_amount as i64,
    })
}

fn extract_swap_vault_pairs(
    transaction: &EncodedTransaction,
    account_keys: &[String],
) -> Vec<(u8, u8)> {
    let Some(program_index) =
        account_keys.iter().position(|k| k == RAYDIUM_CPMM_PROGRAM_ID).map(|i| i as u8)
    else {
        return Vec::new();
    };

    let instructions = match transaction {
        EncodedTransaction::Json(tx) => match &tx.message {
            UiMessage::Raw(msg) => msg.instructions.clone(),
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };

    let mut pairs = Vec::new();
    for ix in instructions {
        let UiCompiledInstruction {
            program_id_index,
            accounts,
            data,
            stack_height: _,
        } = ix;

        if program_id_index != program_index {
            continue;
        }

        let Ok(decoded) = bs58::decode(&data).into_vec() else {
            continue;
        };
        if decoded.len() < 8 || decoded[..8] != SWAP_DISCRIMINATOR {
            continue;
        }
        if accounts.len() < 7 {
            continue;
        }

        pairs.push((accounts[5], accounts[6]));
    }

    pairs
}

fn extract_swaps_heuristic(
    signature: &str,
    slot: u64,
    block_time: Option<DateTime<Utc>>,
    meta: &UiTransactionStatusMeta,
    transaction: &EncodedTransaction,
) -> Vec<SwapEvent> {
    let account_keys = account_keys_from_transaction(transaction);
    let fee_payer = account_keys.first().cloned().unwrap_or_default();

    let mut balances: HashMap<u8, BalanceInfo> = HashMap::new();
    let empty_balances = Vec::new();
    for b in meta.pre_token_balances.as_ref().unwrap_or(&empty_balances) {
        if let Ok(amount) = b.ui_token_amount.amount.parse::<u64>() {
            balances.entry(b.account_index).or_default().pre = Some((
                b.mint.clone(),
                b.owner.as_ref().unwrap_or(&String::new()).clone(),
                b.ui_token_amount.decimals,
                amount,
            ));
        }
    }
    let empty_balances = Vec::new();
    for b in meta.post_token_balances.as_ref().unwrap_or(&empty_balances) {
        if let Ok(amount) = b.ui_token_amount.amount.parse::<u64>() {
            balances.entry(b.account_index).or_default().post = Some((
                b.mint.clone(),
                b.owner.as_ref().unwrap_or(&String::new()).clone(),
                b.ui_token_amount.decimals,
                amount,
            ));
        }
    }

    let mut deltas_by_owner: HashMap<String, Vec<Delta>> = HashMap::new();
    for (_idx, info) in balances {
        if let (Some(pre), Some(post)) = (&info.pre, &info.post) {
            let raw_delta = post.3 as i64 - pre.3 as i64;
            if raw_delta == 0 {
                continue;
            }
            deltas_by_owner.entry(pre.1.clone()).or_default().push(Delta {
                mint: pre.0.clone(),
                decimals: pre.2,
                raw_delta,
            });
        }
    }

    let mut swaps = Vec::new();
    for (owner, deltas) in deltas_by_owner {
        if owner == fee_payer || deltas.len() != 2 {
            continue;
        }
        let in_delta = deltas.iter().find(|d| d.raw_delta > 0);
        let out_delta = deltas.iter().find(|d| d.raw_delta < 0);
        let (Some(in_d), Some(out_d)) = (in_delta, out_delta) else {
            continue;
        };

        let amount_in = BigDecimal::from(in_d.raw_delta as u64)
            / BigDecimal::from(10u64.pow(in_d.decimals as u32));
        let amount_out = BigDecimal::from(out_d.raw_delta.unsigned_abs())
            / BigDecimal::from(10u64.pow(out_d.decimals as u32));

        swaps.push(SwapEvent {
            signature: signature.to_string(),
            slot,
            block_time,
            program_id: RAYDIUM_CPMM_PROGRAM_ID.to_string(),
            pool_id: owner.clone(),
            token_in: in_d.mint.clone(),
            token_out: out_d.mint.clone(),
            amount_in_raw: in_d.raw_delta as u64,
            amount_out_raw: out_d.raw_delta.unsigned_abs(),
            amount_in,
            amount_out,
            priority_fee_lamports: meta.fee,
        });
    }

    swaps
}

pub fn extract_swaps(
    signature: &str,
    slot: u64,
    block_time: Option<DateTime<Utc>>,
    meta: &UiTransactionStatusMeta,
    transaction: &EncodedTransaction,
) -> Vec<SwapEvent> {
    let account_keys = account_keys_from_transaction(transaction);

    let vault_pairs = extract_swap_vault_pairs(transaction, &account_keys);
    if vault_pairs.is_empty() {
        return extract_swaps_heuristic(signature, slot, block_time, meta, transaction);
    }

    let mut swaps = Vec::new();
    for (input_vault_idx, output_vault_idx) in vault_pairs {
        let Some(input) = balance_change(meta, input_vault_idx) else {
            continue;
        };
        let Some(output) = balance_change(meta, output_vault_idx) else {
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
            pool_id: input.owner,
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
        EncodedTransaction, UiCompiledInstruction, UiMessage, UiRawMessage, UiTransaction,
        UiTransactionStatusMeta, UiTransactionTokenBalance, option_serializer::OptionSerializer,
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

    fn raydium_swap_instruction(
        input_vault: u8,
        output_vault: u8,
        program_index: u8,
    ) -> UiCompiledInstruction {
        let mut data = SWAP_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[0u8; 10]);
        UiCompiledInstruction {
            program_id_index: program_index,
            accounts: vec![0, 1, 2, 3, 4, input_vault, output_vault],
            data: bs58::encode(&data).into_string(),
            stack_height: None,
        }
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
    fn vault_pair_extraction_finds_swap() {
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
        let ix = raydium_swap_instruction(5, 6, 7);
        let tx = raw_transaction(keys, vec![ix]);
        let meta = meta_with_balances(5000, vec![], vec![]);
        let pairs = extract_swap_vault_pairs(&tx, &account_keys_from_transaction(&tx));
        assert_eq!(pairs, vec![(5, 6)]);
        assert!(extract_swaps("sig", 100, None, &meta, &tx).is_empty());
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
        let ix = raydium_swap_instruction(5, 6, 7);
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
    fn heuristic_swap_parsing_fallback() {
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
        assert_eq!(swaps.len(), 1);
        let swap = &swaps[0];
        assert_eq!(swap.token_in, INPUT_MINT);
        assert_eq!(swap.token_out, OUTPUT_MINT);
        assert_eq!(swap.amount_in_raw, 100_000_000);
        assert_eq!(swap.amount_out_raw, 100_000_000);
        assert_eq!(swap.pool_id, owner);
    }
}
