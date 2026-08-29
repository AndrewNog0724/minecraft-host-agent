//! MC 联机设施建设 Agent —— 入口。
//! 装配：clap 子命令解析 → 取消总线（Ctrl-C）→ 事件总线 → cli 分发。
//! 模块职责见 docs/project-design.md §7.3 / §14。

mod agent;
mod cli;
mod config;
mod events;
mod knowledge;
mod llm;
mod provision;
mod spec;
mod store;

use clap::Parser;
use tokio_util::sync::CancellationToken;

/// 命令行参数。
#[derive(Parser, Debug)]
#[command(
    name = "agent",
    version,
    about = "MC 联机设施建设 Agent：一句话开服管家",
    long_about = "面向 Minecraft Java 版好友联机场景的开服管家。\n用一句自然语言描述需求，本工具完成方案推导、服务端部署、Java 供给、内网穿透与故障诊断的全流程。"
)]
struct Cli {
    #[command(subcommand)]
    cmd: cli::Cmd,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // 日志：默认只输出告警以上（交互输出以进度条为主），RUST_LOG 可调
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    // 统一取消总线：Ctrl-C 触发，贯穿流水线检查点与进程 Drop 守卫（R4）
    let cancel = CancellationToken::new();
    let bus = events::EventBus::new();
    let ctrl_cancel = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("收到 Ctrl-C，开始取消当前任务");
            ctrl_cancel.cancel();
        }
    });

    if let Err(e) = cli::run(cli.cmd, cancel, bus).await {
        eprintln!("错误：{e:#}");
        let mut source = e.source();
        while let Some(cause) = source {
            eprintln!("  ↑ 因：{cause}");
            source = cause.source();
        }
        std::process::exit(1);
    }
}
