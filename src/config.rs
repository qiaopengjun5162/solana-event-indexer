// 文件路径: src/config.rs

use config::FileFormat;
use serde::Deserialize;

// 这个结构体用来匹配 [[programs]] 这种列表形式的配置
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ProgramConfig {
    pub name: String,
    pub program_id: String,
    pub idl_path: String,
}

// 这是我们的主配置结构体，它包含了顶层的字段和 ProgramConfig 列表
#[derive(Debug, Deserialize)]
pub struct Config {
    pub start_slot: u64,
    pub rpc_url: String,
    pub database_url: String,
    #[allow(dead_code)]
    pub programs: Vec<ProgramConfig>,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let builder = config::Config::builder()
            .add_source(config::File::with_name("config/default"))
            .add_source(config::File::new("config/local.toml", FileFormat::Toml).required(false))
            .add_source(config::Environment::with_prefix("SOLANA"));

        let config = builder.build()?.try_deserialize()?;
        Ok(config)
    }
}
