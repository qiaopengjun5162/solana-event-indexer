// src/main.rs

mod config;
mod logger; // 引入 logger 模块

use crate::config::Config;
use dotenvy::dotenv;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcBlockConfig}; // 引入 RpcClient
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();

    // 1. 初始化日志
    logger::init_logger();

    // 2. 加载配置
    info!("Loading configuration...");
    let config = Config::load()?;
    info!("✅ Configuration loaded successfully!");
    // 我们可以保留这行来调试，也可以暂时注释掉
    // info!("{:#?}", config);

    // 3. 🚀 使用配置信息来连接 Solana RPC
    let client = RpcClient::new(config.rpc_url);
    info!("Connecting to Solana RPC at: {}", client.url());

    // 4. 获取链上的最新 slot
    let latest_slot = client.get_slot()?;
    info!("Chain tip is at slot: {}", latest_slot);

    // 5. 确定我们的同步范围
    let mut current_slot = config.start_slot;
    info!(
        "Starting historic sync from slot {} up to {}",
        current_slot, latest_slot
    );

    // 3. 循环获取并处理每一个区块
    while current_slot <= latest_slot {
        info!("Fetching block for slot: {}", current_slot);

        let rpc_block_config = RpcBlockConfig {
            max_supported_transaction_version: Some(0),
            ..Default::default()
        };

        // 使用 match 来优雅地处理可能跳过的空 slot
        match client.get_block_with_config(current_slot, rpc_block_config) {
            Ok(block) => {
                info!(
                    "✅ Processed block {}: blockhash={}",
                    current_slot, block.blockhash
                );
                // 这里就是未来调用 parser 和 db 的地方
            }
            Err(e) => {
                // 如果一个 slot 不存在（被跳过），RPC 会返回错误，我们可以简单地记录并继续
                // 注意：这里需要更精细的错误处理，判断是真的网络错误还是只是 slot 被跳过
                tracing::warn!(
                    "Could not fetch block {}: {}. It might be a skipped slot.",
                    current_slot,
                    e
                );
            }
        }

        current_slot += 1;
    }

    info!("🎉 Historic sync finished!");

    // 5. 🚀 遍历我们想要索引的程序
    //    这段代码读取了 config.programs，又消除了一个警告！
    info!("Will be indexing the following programs:");
    for program in &config.programs {
        info!("-> Name: {}, ID: {}", program.name, program.program_id);
        info!("-> IDL path: {}", program.idl_path);
    }

    Ok(())
}
