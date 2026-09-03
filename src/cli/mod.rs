//! cli：入口分发、子命令（R2 / R3 / R5 / R6 的用户入口）。

pub mod commands;
pub mod interaction;
pub mod links;
pub mod render;
pub mod repl;
pub mod setup;

use anyhow::Context as _;
use clap::{Parser, Subcommand};

use crate::paths;

/// `mcha`（无子命令）进入交互会话。
#[derive(Parser)]
#[command(
    name = "mcha",
    version,
    about = "Minecraft Host Agent（MCHA）：场景定制化 AI Agent",
    after_help = "提示：首次运行会自动进入配置向导；mcha new \"…\" 预填首条消息。"
)]
pub struct Cli {
    /// 接续最近一次会话（--continue）
    #[arg(long = "continue", conflicts_with = "resume")]
    continue_last: bool,
    /// 交互式选择恢复历史会话（--resume）
    #[arg(long)]
    resume: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 以预填首条消息进入交互会话
    New {
        /// 首条消息内容
        msg: Option<String>,
    },
    /// 运行配置向导（无配置时 `mcha` 自动进入）
    Setup,
    /// 查看或修改配置（R3）
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// 查看用量与费用（R6）
    Usage {
        /// 只统计指定会话
        #[arg(long)]
        session: Option<String>,
    },
    /// 查看历史会话（R5）
    Sessions {
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
    /// 查看部署档案（R5 / US3）
    Profiles {
        #[command(subcommand)]
        action: ProfilesAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// 显示当前生效配置
    List,
    /// 修改配置项（保留注释），如：mcha config set budget.limit_cny 5
    Set {
        /// 点分键名（如 model.endpoint）
        key: String,
        /// 新值（按类型自动识别：true/1.5/字符串）
        value: String,
    },
    /// 连接测试（最小对话请求）
    Test,
}

#[derive(Subcommand)]
enum SessionsAction {
    /// 列出历史会话
    List,
    /// 查看会话内容（含工具调用、结果与调用明细）
    Show { id: String },
    /// 导出会话为 JSON（自动打码密钥与公网 IP）
    Export {
        id: String,
        /// 输出路径（默认当前目录 <id>.export.json）
        path: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum ProfilesAction {
    /// 列出已保存的部署档案（新到旧）
    List,
    /// 查看档案完整内容
    Show { id: String },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let data_dir = paths::shared_data_dir()?.clone();
    crate::store::ensure_dir(&data_dir)?;

    match cli.command {
        Some(Command::Setup) => {
            setup::run_setup(&data_dir).await?;
            Ok(())
        }
        Some(Command::Config { action }) => match action {
            ConfigAction::List => commands::config_list(&data_dir),
            ConfigAction::Set { key, value } => commands::config_set(&data_dir, &key, &value),
            ConfigAction::Test => commands::config_test(&data_dir).await,
        },
        Some(Command::Usage { session }) => commands::usage(&data_dir, session.as_deref()),
        Some(Command::Sessions { action }) => match action {
            None | Some(SessionsAction::List) => commands::sessions_list(&data_dir),
            Some(SessionsAction::Show { id }) => commands::sessions_show(&data_dir, &id),
            Some(SessionsAction::Export { id, path }) => {
                commands::sessions_export(&data_dir, &id, path.as_deref())
            }
        },
        Some(Command::Profiles { action }) => match action {
            ProfilesAction::List => commands::profiles_list(&data_dir),
            ProfilesAction::Show { id } => commands::profiles_show(&data_dir, &id),
        },
        Some(Command::New { msg }) => {
            repl::run(repl::ReplMode::WithMessage(msg.unwrap_or_default())).await
        }
        None if cli.continue_last => repl::run(repl::ReplMode::Continue).await,
        None if cli.resume => repl::run(repl::ReplMode::Resume).await,
        None => repl::run(repl::ReplMode::Fresh).await,
    }
    .with_context(|| "执行失败")
}
