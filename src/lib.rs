//! Solana Raydium CPMM swap indexer.
//!
//! 拆成 lib 的目的：`spread` 里的价差模型要能被主程序和 examples 同时复用，
//! 否则同一套算法会存在两份实现（此前 examples/find_opportunities.rs 就是复制品）。

pub mod arb;
pub mod backfill;
pub mod config;
pub mod db;
pub mod logger;
pub mod models;
pub mod parser;
pub mod pool_state;
pub mod spread;
