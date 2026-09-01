//! REPL 多轮会话（FR-02 / D101 / D107 / D108）。

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use crossterm::style::{Attribute, Color, Stylize};

use crate::agent::{Agent, AgentEnv, TurnEnd};
use crate::cancel::CancelToken;
use crate::config::AppConfig;
use crate::events::event_channel;
use crate::llm::OpenAiCompatClient;
use crate::paths;
use crate::store::session::{self, Session};
use crate::store::usage::UsageLedger;
use crate::tools::ToolRegistry;
use crate::tools::general::register_general_tools;

use super::interaction::TerminalInteraction;
use super::render::render_task;
use super::setup;

/// 会话启动模式。
pub enum ReplMode {
    /// 全新会话。
    Fresh,
    /// 全新会话 + 预填首条消息（mcha new "…"）。
    WithMessage(String),
    /// 接续最近会话（--continue）。
    Continue,
    /// 交互式选择恢复（--resume）。
    Resume,
}

pub async fn run(mode: ReplMode) -> anyhow::Result<()> {
    let data_dir = paths::shared_data_dir()?.clone();

    // 1. 配置就绪：无配置进向导；缺必填项打印模板（D113）
    let loaded = AppConfig::load(&data_dir)?;
    let config = if !loaded.existed {
        let saved = setup::run_setup(&data_dir)?;
        if !saved {
            anyhow::bail!("未完成配置，无法进入会话");
        }
        AppConfig::load(&data_dir)?.config
    } else if let Err(missing) = loaded.config.validate() {
        println!("{}", "配置缺失，无法进入会话：".with(Color::Red));
        println!("{missing}");
        println!();
        println!("{}", AppConfig::template("", "").with(Color::DarkGrey));
        println!("提示：mcha setup 重新运行向导，或 mcha config set <键> <值> 逐项补齐。");
        anyhow::bail!("配置不完整");
    } else {
        loaded.config
    };
    let api_key = config.api_key()?;

    // 2. 装配环境
    let workspace = paths::workspace_dir()?;
    let llm = OpenAiCompatClient::new(config.model.endpoint.clone(), api_key)?;
    let mut registry = ToolRegistry::new();
    register_general_tools(&mut registry);
    let ledger = UsageLedger::new(&data_dir)?;
    let interaction: Arc<dyn crate::tools::Interaction> = Arc::new(TerminalInteraction);
    let mut env = AgentEnv {
        llm: Arc::new(llm),
        registry: Arc::new(registry),
        system_prompt: String::new(),
        workspace,
        data_dir: data_dir.clone(),
        http: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .user_agent("mcha/0.2")
            .build()?,
        interaction: interaction.clone(),
        ledger,
        config,
    };
    env.system_prompt = crate::agent::default_system_prompt(&env);

    // 3. 会话：新建 / 接续
    let sessions_dir = data_dir.join("sessions");
    let mut session = match &mode {
        ReplMode::Continue => {
            let latest = session::latest_session(&sessions_dir)?
                .context("没有可接续的历史会话（先正常开一次会话）")?;
            let s = Session::load(&latest, &data_dir)?;
            println!(
                "已接续会话 {}（{} 条消息）",
                s.id.clone().with(Color::Green),
                s.messages.len()
            );
            s
        }
        ReplMode::Resume => {
            let list = session::list_sessions(&sessions_dir)?;
            if list.is_empty() {
                anyhow::bail!("没有历史会话可恢复");
            }
            let items: Vec<String> = list
                .iter()
                .map(|(meta, _)| {
                    format!(
                        "{}  {}  {} 条消息",
                        meta.id,
                        meta.title.as_deref().unwrap_or("（无标题）"),
                        meta.message_count
                    )
                })
                .collect();
            let picked = tokio::task::spawn_blocking(move || {
                dialoguer::Select::new()
                    .with_prompt("选择要恢复的会话")
                    .items(&items)
                    .default(0)
                    .interact()
            })
            .await??;
            let (_, path) = &list[picked];
            Session::load(path, &data_dir)?
        }
        ReplMode::Fresh | ReplMode::WithMessage(_) => Session::create(&sessions_dir, &data_dir)?,
    };
    // 恢复会话时从账本重建用量累计（预算守卫跨会话连续）
    session::rebuild_totals_from_ledger(&mut session, &env.ledger)?;

    // 4. REPL 主循环
    let started = Instant::now();
    print_banner(&env, &session);
    // "本会话允许此工具"授权集合（确认门 y/a/n 的 a，D110），跨回合保留
    let mut allowed: HashSet<String> = HashSet::new();

    // 预填首条消息
    if let ReplMode::WithMessage(first) = &mode {
        run_turn(&env, &mut session, first, &mut allowed).await?;
    }

    loop {
        print!("{}", "> ".with(Color::DarkGrey).attribute(Attribute::Bold));
        let _ = std::io::stdout().flush();
        let line = tokio::select! {
            // 提示符处 Ctrl-C：打印汇总后退出（D108）
            _ = tokio::signal::ctrl_c() => {
                println!();
                print_exit_summary(&env, &session, started);
                std::process::exit(0);
            }
            line = read_line() => line?,
        };
        let Some(line) = line else {
            // EOF（Ctrl-D）
            println!();
            break;
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if let Some(command) = input.strip_prefix('/') {
            match command {
                "exit" | "quit" => break,
                "usage" => {
                    println!(
                        "本会话累计：输入 {} tokens · 输出 {} tokens · 费用 ¥{:.4} · {} 次调用",
                        session.total_input_tokens,
                        session.total_output_tokens,
                        session.total_cost_cny,
                        env.ledger.summarize(Some(&session.id))?.calls
                    );
                }
                "help" => print_help(),
                "sessions" => super::commands::sessions_list(&data_dir)?,
                other => {
                    println!(
                        "{}",
                        format!("未知命令 /{other}；可用：/exit /usage /help /sessions")
                            .with(Color::Yellow)
                    );
                }
            }
            continue;
        }
        run_turn(&env, &mut session, input, &mut allowed).await?;
    }

    print_exit_summary(&env, &session, started);
    Ok(())
}

/// 执行一个回合：事件通道 + 渲染器 + Ctrl-C 打断（R4）。
async fn run_turn(
    env: &AgentEnv,
    session: &mut Session,
    input: &str,
    allowed: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let (tx, rx) = event_channel();
    let renderer = tokio::spawn(render_task(rx));
    let cancel = CancelToken::new();

    println!();
    let end = {
        let mut turn = std::pin::pin!(Agent::run_turn(
            env,
            session,
            input,
            &tx,
            cancel.clone(),
            allowed
        ));
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                // Ctrl-C 打断当前回合：取消令牌 → Loop 按回合原子性收尾（D109）
                cancel.cancel();
                turn.await?
            }
            result = &mut turn => result?,
        }
    };
    // 回合结束（未来已丢弃），关闭事件通道让渲染器自然收尾
    drop(tx);
    let _ = renderer.await;
    println!();
    if end == TurnEnd::LlmFailed {
        println!(
            "{}",
            "回合已中断（模型调用失败），输入可重试。".with(Color::Yellow)
        );
    }
    Ok(())
}

async fn read_line() -> anyhow::Result<Option<String>> {
    let line = tokio::task::spawn_blocking(|| {
        let mut buf = String::new();
        match std::io::stdin().read_line(&mut buf) {
            Ok(0) => Ok::<Option<String>, std::io::Error>(None),
            Ok(_) => Ok(Some(buf)),
            Err(err) => Err(err),
        }
    })
    .await
    .context("输入线程失败")??;
    Ok(line)
}

fn print_banner(env: &AgentEnv, session: &Session) {
    let resume_note = if session.messages.is_empty() {
        "新会话".to_string()
    } else {
        format!("已恢复 {} 条历史消息", session.messages.len())
    };
    println!(
        "{}",
        "MCHA — Minecraft Host Agent（M1 Agent 框架）"
            .with(Color::Cyan)
            .attribute(Attribute::Bold)
    );
    println!(
        "模型：{} @ {} · 上下文 {} · 预算 ¥{:.2}/会话 · 确认级别 {}",
        env.config.model.model,
        env.config.model.endpoint,
        env.config.model.context_len,
        env.config.budget.limit_cny,
        env.config.safety.confirm_level
    );
    println!("工作区：{} · {resume_note}", env.workspace.display());
    println!(
        "工具：{} · {}",
        env.registry.names().join(" / "),
        "输入 /help 查看命令；Ctrl-C 打断回合；/exit 或 Ctrl-D 退出".with(Color::DarkGrey)
    );
    println!();
}

fn print_help() {
    println!("{}", "── 命令 ──".with(Color::Cyan));
    println!("/exit    退出会话（Ctrl-D 同效；提示符处 Ctrl-C 亦可）");
    println!("/usage   显示本会话累计用量与费用");
    println!("/sessions 列出历史会话");
    println!("/help    显示本帮助");
    println!("提示：回合执行中按 Ctrl-C 可打断当前操作（会话保留）。");
}

fn print_exit_summary(env: &AgentEnv, session: &Session, started: Instant) {
    // D108：退出时一次性汇总（会话过程中不刷用量行）
    let minutes = started.elapsed().as_secs_f64() / 60.0;
    println!("{}", "── 会话结束 ──".with(Color::DarkGrey));
    println!(
        "{}",
        format!(
            "输入 {} tokens · 输出 {} tokens · 费用 ¥{:.4} · 用时 {:.0} 分钟",
            session.total_input_tokens,
            session.total_output_tokens,
            session.total_cost_cny,
            minutes
        )
        .with(Color::DarkGrey)
    );
    println!(
        "{}",
        format!("轨迹已保存：{}", session.jsonl_path().display()).with(Color::DarkGrey)
    );
    let _ = env;
}
