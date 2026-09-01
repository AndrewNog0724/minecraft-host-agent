//! mcha：Minecraft Host Agent（MCHA）入口。
//!
//! 职责（设计 §14）：clap 解析、日志初始化、数据目录与 .env 装配、
//! tokio 运行时启动，其余交给 cli 模块分发。

mod agent;
mod cancel;
mod cli;
mod config;
mod events;
mod llm;
mod paths;
mod store;
mod tools;

use anyhow::Context as _;
use clap::Parser;

fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // 日志：默认 warn（界面渲染走事件总线，不走 tracing）；RUST_LOG 可调
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // 数据目录与 .env（API Key）在进入任何命令前装配
    let data_dir = paths::data_dir()?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("初始化数据目录失败：{}", data_dir.display()))?;
    config::load_dotenv(&data_dir)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("启动 tokio 运行时失败")?;
    runtime.block_on(cli::run(cli))
}
