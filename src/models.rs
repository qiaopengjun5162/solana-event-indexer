use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};

pub struct SwapEvent {
    pub signature: String,
    pub slot: u64,
    pub block_time: Option<DateTime<Utc>>,
    pub program_id: String,
    pub pool_id: String,
    pub token_in: String,
    pub token_out: String,
    pub amount_in_raw: u64,
    pub amount_out_raw: u64,
    pub amount_in: BigDecimal,
    pub amount_out: BigDecimal,
    pub priority_fee_lamports: u64,
}
