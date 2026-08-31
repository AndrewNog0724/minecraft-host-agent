//! ui：CLI 子命令、交互问答、主流程编排（R2/R4/R5/R6 的用户入口）。

pub mod render;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use clap::Subcommand;
use dialoguer::{Confirm, Input, Password, Select};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentDeps, RequirementAgent};
use crate::config::AppConfig;
use crate::events::{EventBus, Phase, TaskStatus, TaskTrace, TraceEvent};
use crate::knowledge::KnowledgeBase;
use crate::llm::LlmService;
use crate::provision::{Answers, DeployContext, TreeOutput, deploy, derive_spec};
use crate::spec::{PartialSpec, Question, ServerSpec};
use crate::store::Store;

/// clap 子命令定义（§8.7：new / diag / profiles / sessions / config / usage）。
#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// 一句话开服（US1 主流程）
    New {
        /// 需求描述（省略则交互输入）
        requirement: Option<String>,
        /// 跳过确认（仅限演示/CI，会在轨迹中留痕）
        #[arg(long)]
        yes: bool,
    },
    /// 手动填写方案（无 LLM 的降级路径 / 离线演示）
    Plan,
    /// 查看开服配置档案
    Profiles {
        /// 档案 id（省略则列出全部）
        id: Option<String>,
    },
    /// 任务历史（R5）
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// 用量与费用统计（R6）
    Usage,
    /// 配置管理（R3）
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// 故障诊断（P1，随互试版交付）
    Diag,
    /// 上手引导（FR-18）：配置向导 + 工作区设定 + 二进制注册
    Setup,
}

#[derive(Subcommand, Debug)]
pub enum SessionsAction {
    /// 列出全部任务
    List,
    /// 查看一次任务的完整轨迹
    Show { task_id: String },
    /// 导出一次任务的完整上下文（JSON，自动打码）
    Export { task_id: String },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// 初始化配置模板（config.toml + .env）
    Init,
    /// 交互式向导修改配置（问答式，保留注释；必填 3 项 + 可选高级项）
    Wizard,
    /// 查看当前配置（密钥打码）
    Show,
    /// 修改配置项，如 `mcha config set model.model glm-5.2`
    Set { key: String, value: String },
}

/// 命令分发入口。
pub async fn run(cmd: Cmd, cancel: CancellationToken, bus: EventBus) -> anyhow::Result<()> {
    match cmd {
        Cmd::New { requirement, yes } => cmd_new(requirement, yes, cancel, bus).await,
        Cmd::Plan => cmd_plan(cancel, bus).await,
        Cmd::Profiles { id } => cmd_profiles(id),
        Cmd::Sessions { action } => cmd_sessions(action),
        Cmd::Usage => cmd_usage(),
        Cmd::Config { action } => cmd_config(action),
        Cmd::Diag => {
            bail!("诊断功能（FR-09）随 P1 互试版交付；当前版本请把服务端日志贴给开发者分析")
        }
        Cmd::Setup => cmd_setup(true).await,
    }
}

/// 主流程（US1，§7.4 流程 A）。
async fn cmd_new(
    requirement: Option<String>,
    yes: bool,
    cancel: CancellationToken,
    bus: EventBus,
) -> anyhow::Result<()> {
    let mut cfg = AppConfig::load().context("加载配置失败")?;
    let kb = KnowledgeBase::embedded().context("加载知识库失败")?;
    let store = Arc::new(Store::open().context("打开数据目录失败")?);
    let bars = indicatif::MultiProgress::new();

    // 1. 需求输入
    let requirement = match requirement {
        Some(r) => r,
        None => tokio::task::block_in_place(|| {
            Input::<String>::new()
                .with_prompt("描述你的开服需求（如：我们 5 个人，2 正版 3 离线，想玩暮色森林）")
                .interact()
                .context("读取输入失败")
        })?,
    };

    let task_id = uuid::Uuid::new_v4().to_string();
    let trace = TaskTrace::new(task_id.clone(), requirement.clone());

    // 2. 事件泵（进度渲染 + 落盘 + 仓库备份）。
    // 必须先订阅再发布 TaskStarted：广播不可回放，顺序颠倒泵会错过首个事件（v0.9.3）。
    let rx = bus.subscribe();
    let backup = crate::store::SessionBackup::open();
    println!(
        "会话备份：{}（随任务写入，便于提交分析）",
        backup.root().display()
    );
    let pump_store = store.clone();
    let pump_task = tokio::spawn(async move {
        let _ = render::pump(rx, pump_store, bars, backup).await;
    });
    bus.publish(TraceEvent::TaskStarted {
        trace: trace.clone(),
    });

    // 3. LLM 需求理解环（不可用时引导走 plan 降级）
    let draft = {
        cfg.validate()
            .context("LLM 配置不完整（可改用 `mcha plan` 手动填写方案）")?;
        let svc = LlmService::new(&cfg, bus.clone(), Arc::new(crate::llm::SpendLedger::new()))
            .context("初始化 LLM 客户端失败")?;
        let deps = AgentDeps::new(kb.clone(), cfg.clone()).context("初始化工具依赖失败")?;
        let agent =
            RequirementAgent::new(&svc, &deps, bus.clone(), task_id.clone(), cancel.clone());
        bus.publish(TraceEvent::SpecDrafted {
            task_id: task_id.clone(),
            draft: Box::default(),
        });
        let (draft, _) = agent.run(&requirement).await.map_err(|e| {
            bus.publish(TraceEvent::TaskFinished {
                task_id: task_id.clone(),
                status: TaskStatus::Failed,
                error: Some(format!("需求理解失败：{e}")),
            });
            anyhow::anyhow!("需求理解失败：{e}")
        })?;
        draft
    };

    // 4. 决策树 + 澄清问答循环（最多 3 轮；决策树是纯函数，合并回答后重推导）
    let releases = {
        let deps = AgentDeps::new(kb.clone(), cfg.clone())?;
        deps.known_releases().await.unwrap_or_default()
    };
    let mut merged = draft.partial.clone();
    // 累积回答表：每轮传给决策树（v0.9.5 修复——此前恒传空表，
    // 白名单等未落入 PartialSpec 的回答被静默丢弃，导致重复追问直至超轮）
    let mut answered = Answers::new();
    let mut spec: Option<ServerSpec> = None;
    for _round in 0..3 {
        match derive_spec(&merged, &answered, &kb, Some(&releases)) {
            TreeOutput::Complete(s) => {
                spec = Some(*s);
                break;
            }
            TreeOutput::NeedInput { questions, .. } => {
                let answers = ask_questions(&questions)?;
                if answers.is_empty() {
                    bail!("缺少必要信息，已退出");
                }
                merge_answers(&mut merged, &answers);
                answered.extend(answers);
            }
        }
    }
    let Some(mut spec) = spec else {
        bail!("澄清超过 3 轮仍未齐备，请用更完整的描述重试");
    };

    // 5. 方案摘要 + 安装目录确认（FR-17/FR-19，决议 D18）+ 风险提示 + 确认
    print_spec_summary(&spec);
    // 决议 D18：开服前交互确认安装目录，默认当前目录；--yes（演示/CI）跳过
    if !yes {
        cfg.workspace.path = tokio::task::block_in_place(|| ask_workspace_dir(&cfg))?;
    }
    let install_dir = cfg.workspace_dir().context("解析安装目录失败")?;
    let install_dir = if install_dir.is_absolute() {
        install_dir
    } else {
        std::env::current_dir()?.join(install_dir)
    };
    confirm_existing_server(&install_dir, yes)?;
    println!("安装位置：{}", install_dir.display());
    let confirmed = yes
        || tokio::task::block_in_place(|| {
            Confirm::new()
                .with_prompt("确认按此方案开服？（同时表示同意 Minecraft EULA）")
                .interact()
                .unwrap_or(false)
        });
    if !confirmed {
        bus.publish(TraceEvent::TaskFinished {
            task_id,
            status: TaskStatus::Cancelled,
            error: None,
        });
        let _ = pump_task.await;
        bail!("已取消");
    }
    bus.publish(TraceEvent::SpecConfirmed {
        task_id: task_id.clone(),
        spec: Box::new(spec.clone()),
    });

    // 6. 确定性执行流水线
    let ctx = DeployContext::new(cfg, kb, bus.clone(), cancel.clone())?;
    let deploy_result = deploy(&mut spec, &ctx, &task_id).await;

    match deploy_result {
        Ok(result) => {
            store.save_profile(&spec).context("保存档案失败")?;
            println!("\n=== 部署完成，朋友们这样连 ===");
            for line in &result.connection.lines {
                println!("{line}");
            }
            println!(
                "\n档案已保存：profiles/{}（下次可用 `mcha plan` 复用）",
                spec.spec_id
            );
            bus.publish(TraceEvent::TaskFinished {
                task_id: task_id.clone(),
                status: TaskStatus::Done,
                error: None,
            });
            // 服务器保持运行，等用户 Ctrl-C
            println!("服务器运行中；按 Ctrl-C 停止服务端并退出。");
            tokio::select! {
                _ = cancel.cancelled() => {}
            }
            drop(result.server); // Drop 守卫停进程
            println!("服务端已停止。");
        }
        Err(e) => {
            let status = if e.to_string().contains("取消") || cancel.is_cancelled() {
                TaskStatus::Cancelled
            } else {
                TaskStatus::Failed
            };
            // 决议 D19：失败原因随事件落盘（events.jsonl + TaskTrace.error），
            // 不再只打一次 stderr 了事
            bus.publish(TraceEvent::TaskFinished {
                task_id: task_id.clone(),
                status,
                error: Some(e.to_string()),
            });
            return Err(e.into());
        }
    }
    Ok(())
}

/// 开服前询问安装目录（决议 D18）：默认值按 `MCHA_WORKSPACE` > config > 当前目录，
/// 显示来源注记；本次输入仅对本次运行生效（不写回 config.toml，固定目录走向导）。
fn ask_workspace_dir(cfg: &AppConfig) -> anyhow::Result<String> {
    let env_val = std::env::var(crate::config::ENV_WORKSPACE)
        .ok()
        .filter(|s| !s.trim().is_empty());
    let (default, source) = if let Some(v) = env_val {
        (v, "环境变量 MCHA_WORKSPACE")
    } else if !cfg.workspace.path.trim().is_empty() {
        (cfg.workspace.path.clone(), "config.toml [workspace]")
    } else {
        (
            std::env::current_dir()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default(),
            "当前目录",
        )
    };
    let input: String = Input::new()
        .with_prompt(format!("服务端安装目录（回车 = {source}）"))
        .default(default)
        .show_default(false)
        .interact_text()
        .context("读取输入失败")?;
    Ok(input.trim().to_string())
}

/// v0.10.2 目录彻底拍平后的防误混拦截：目标目录里已有服务器文件痕迹时，
/// 交互模式要求确认，`--yes`（演示/CI）直接拒绝，绝不静默覆盖用户目录。
fn confirm_existing_server(install_dir: &Path, yes: bool) -> anyhow::Result<()> {
    const MARKERS: [&str; 5] = [
        "eula.txt",
        "server.properties",
        "server.jar",
        "world",
        "mods",
    ];
    let hits: Vec<&str> = MARKERS
        .iter()
        .filter(|m| install_dir.join(m).exists())
        .copied()
        .collect();
    if hits.is_empty() {
        return Ok(());
    }
    let note = format!(
        "{} 已包含服务器文件（{}）",
        install_dir.display(),
        hits.join("、")
    );
    if yes {
        bail!("{note}；--yes 模式不覆盖，请换一个安装目录");
    }
    let ok = tokio::task::block_in_place(|| {
        Confirm::new()
            .with_prompt(format!(
                "{note}，继续会混用/覆盖这些文件。确认在此目录开服？"
            ))
            .default(false)
            .interact()
            .context("读取确认失败")
    })?;
    if !ok {
        bail!("已取消：请换一个安装目录");
    }
    Ok(())
}

/// 澄清问答：把 Question 渲染为交互控件，产出 Answers。
fn ask_questions(questions: &[Question]) -> anyhow::Result<Answers> {
    let mut answers = Answers::new();
    for q in questions {
        if q.options.is_empty() {
            let input: String = tokio::task::block_in_place(|| {
                Input::new()
                    .with_prompt(&q.text)
                    .allow_empty(q.allow_empty)
                    .interact()
                    .context("读取输入失败")
            })?;
            // 空回答同样记入：决策树以"键存在"识别用户已明确表态跳过
            answers.insert(q.topic.clone(), input);
        } else {
            let labels = friendly_options(&q.topic, &q.options);
            let idx = tokio::task::block_in_place(|| {
                Select::new()
                    .with_prompt(&q.text)
                    .items(&labels)
                    .default(0)
                    .interact()
                    .context("读取选择失败")
            })?;
            answers.insert(q.topic.clone(), q.options[idx].clone());
        }
    }
    Ok(answers)
}

/// 常见 topic 的选项中文标签。
fn friendly_options(topic: &str, options: &[String]) -> Vec<String> {
    match topic {
        "account_kind" => options
            .iter()
            .map(|o| match o.as_str() {
                "online" => "online — 全正版".to_string(),
                "offline" => "offline — 全离线（建议白名单，可跳过）".to_string(),
                "hybrid" => "hybrid — 混合（需认证方案）".to_string(),
                other => other.to_string(),
            })
            .collect(),
        "software" => options
            .iter()
            .map(|o| match o.as_str() {
                "vanilla" => "vanilla — 原版".to_string(),
                "spigot" => "spigot — Spigot 插件服（Bukkit 原生）".to_string(),
                "paper" => "paper — Paper 插件服（Spigot 优化分支）".to_string(),
                "fabric" => "fabric — Fabric mod 服".to_string(),
                other => other.to_string(),
            })
            .collect(),
        "cross_network" => options
            .iter()
            .map(|o| match o.as_str() {
                "yes" => "yes — 跨网络（需要公网 IP 或穿透）".to_string(),
                "no" => "no — 同一局域网".to_string(),
                other => other.to_string(),
            })
            .collect(),
        _ => options.to_vec(),
    }
}

/// 把回答写回 PartialSpec（决策树纯函数的输入合并）。
fn merge_answers(partial: &mut PartialSpec, answers: &Answers) {
    if let Some(v) = answers.get("mc_version") {
        partial.mc_version = Some(v.clone());
    }
    if let Some(v) = answers.get("software") {
        partial.software = Some(v.clone());
    }
    if let Some(v) = answers.get("account_kind") {
        partial.account_kind = Some(v.clone());
    }
    if let Some(v) = answers.get("cross_network") {
        partial.cross_network = Some(matches!(v.trim(), "yes" | "y" | "true" | "1" | "是"));
    }
    if let Some(v) = answers.get("max_players")
        && let Ok(n) = v.trim().parse()
    {
        partial.max_players = Some(n);
    }
    if let Some(v) = answers.get("machine_memory_mb")
        && let Ok(n) = v.trim().parse()
    {
        partial.machine_memory_mb = Some(n);
    }
}

/// 方案摘要 + 风险提示（FR-17）。
fn print_spec_summary(spec: &ServerSpec) {
    println!("\n=== 方案摘要（{spec_id}) ===", spec_id = spec.spec_id);
    println!(
        "MC 版本：{}（需要 Java {}）",
        spec.mc_version, spec.java.required_major
    );
    println!(
        "服务端：{}",
        match &spec.software {
            crate::spec::ServerSoftware::Vanilla => "原版".to_string(),
            crate::spec::ServerSoftware::Paper { build } => format!(
                "Paper{b}",
                b = build.map(|x| format!(" 构建{x}")).unwrap_or_default()
            ),
            crate::spec::ServerSoftware::Spigot => "Spigot（Bukkit 原生）".to_string(),
            crate::spec::ServerSoftware::Fabric { loader_version, .. } =>
                format!("Fabric（loader {loader_version}）"),
        }
    );
    println!(
        "内存：-Xmx{}MB；端口：{}；最大玩家：{}",
        spec.jvm_memory_mb, spec.port, spec.max_players
    );
    println!(
        "账号：{}",
        match &spec.account {
            crate::spec::AccountPolicy::Online =>
                "全正版（开启正版验证；离线/第三方启动器进服会提示「无效会话」）".to_string(),
            crate::spec::AccountPolicy::Offline { whitelist } =>
                format!("全离线（关闭正版验证，白名单 {} 人）", whitelist.len()),
            crate::spec::AccountPolicy::Hybrid { auth, whitelist } => format!(
                "混合（{}，白名单 {} 人）",
                match auth {
                    crate::spec::HybridAuth::Plugin => "登录插件",
                    crate::spec::HybridAuth::EasyAuth => "EasyAuth",
                },
                whitelist.len()
            ),
        }
    );
    if !spec.mod_names.is_empty() {
        println!("mod：{}", spec.mod_names.join("、"));
    }
    println!(
        "网络：{}",
        match &spec.network {
            crate::spec::NetworkPlan::LanOnly => "仅局域网".to_string(),
            crate::spec::NetworkPlan::Direct { .. } => "跨网络（直连/端口映射）".to_string(),
            crate::spec::NetworkPlan::Tunnel { .. } => "内网穿透".to_string(),
        }
    );
    if !spec.notes.is_empty() {
        println!("注意事项：");
        for note in &spec.notes {
            println!("  - {note}");
        }
    }
}

/// 手动方案（plan）：跳过 LLM，逐项输入后复用决策树与流水线。
async fn cmd_plan(cancel: CancellationToken, bus: EventBus) -> anyhow::Result<()> {
    let mut cfg = AppConfig::load()?;
    let kb = KnowledgeBase::embedded()?;
    let store = Arc::new(Store::open()?);
    let bars = indicatif::MultiProgress::new();

    let mc_version = tokio::task::block_in_place(|| {
        Input::<String>::new()
            .with_prompt("MC 版本")
            .default("1.21.1".into())
            .interact_text()
    })?;
    let online_players = tokio::task::block_in_place(|| {
        Input::<u32>::new()
            .with_prompt("正版玩家数")
            .default(0)
            .interact_text()
    })?;
    let offline_players = tokio::task::block_in_place(|| {
        Input::<u32>::new()
            .with_prompt("离线玩家数")
            .default(0)
            .interact_text()
    })?;
    let sw_idx = tokio::task::block_in_place(|| {
        Select::new()
            .with_prompt("服务端类型")
            .items(&[
                "vanilla 原版",
                "spigot 插件服",
                "paper 插件服",
                "fabric mod 服",
            ])
            .default(0)
            .interact()
    })?;
    let software = ["vanilla", "spigot", "paper", "fabric"][sw_idx].to_string();
    let mods_in: String = tokio::task::block_in_place(|| {
        Input::<String>::new()
            .with_prompt("mod（逗号分隔，可中文，留空跳过）")
            .allow_empty(true)
            .interact()
    })?;
    let mut partial = PartialSpec {
        mc_version: Some(mc_version),
        online_players: Some(online_players),
        offline_players: Some(offline_players),
        software: Some(software),
        ..PartialSpec::default()
    };
    partial.mods = mods_in
        .split([',', '，'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    partial.cross_network = Some(tokio::task::block_in_place(|| {
        Confirm::new().with_prompt("朋友跨网络联机？").interact()
    })?);

    let releases = {
        let deps = AgentDeps::new(kb.clone(), cfg.clone())?;
        deps.known_releases().await.unwrap_or_default()
    };
    // 同 cmd_new（v0.9.5）：累积回答表逐轮传入，并加 3 轮上限（原为无限循环）
    let mut answered = Answers::new();
    let mut resolved: Option<ServerSpec> = None;
    for _round in 0..3 {
        match derive_spec(&partial, &answered, &kb, Some(&releases)) {
            TreeOutput::Complete(s) => {
                resolved = Some(*s);
                break;
            }
            TreeOutput::NeedInput { questions, .. } => {
                let answers = ask_questions(&questions)?;
                if answers.is_empty() {
                    bail!("缺少必要信息，已退出");
                }
                merge_answers(&mut partial, &answers);
                answered.extend(answers);
            }
        }
    }
    let Some(mut spec) = resolved else {
        bail!("澄清超过 3 轮仍未齐备，请重试");
    };

    print_spec_summary(&spec);
    // 决议 D18：同 cmd_new，开服前确认安装目录（默认当前目录）
    cfg.workspace.path = tokio::task::block_in_place(|| ask_workspace_dir(&cfg))?;
    let install_dir = cfg.workspace_dir().context("解析安装目录失败")?;
    let install_dir = if install_dir.is_absolute() {
        install_dir
    } else {
        std::env::current_dir()?.join(install_dir)
    };
    confirm_existing_server(&install_dir, false)?;
    println!("安装位置：{}", install_dir.display());
    if !tokio::task::block_in_place(|| {
        Confirm::new()
            .with_prompt("确认开服？")
            .interact()
            .unwrap_or(false)
    }) {
        bail!("已取消");
    }

    let task_id = uuid::Uuid::new_v4().to_string();
    let trace = TaskTrace::new(task_id.clone(), format!("手动方案 {}", spec.spec_id));
    // 事件泵（进度渲染 + 落盘 + 仓库备份）；先订阅再发布（v0.9.3，同 cmd_new）
    let rx = bus.subscribe();
    let backup = crate::store::SessionBackup::open();
    let pump_store = store.clone();
    tokio::spawn(async move {
        let _ = render::pump(rx, pump_store, bars, backup).await;
    });
    bus.publish(TraceEvent::TaskStarted { trace });
    bus.publish(TraceEvent::SpecConfirmed {
        task_id: task_id.clone(),
        spec: Box::new(spec.clone()),
    });

    let ctx = DeployContext::new(cfg, kb, bus.clone(), cancel.clone())?;
    match deploy(&mut spec, &ctx, &task_id).await {
        Ok(result) => {
            store.save_profile(&spec)?;
            println!("\n=== 部署完成 ===");
            for line in &result.connection.lines {
                println!("{line}");
            }
            bus.publish(TraceEvent::TaskFinished {
                task_id,
                status: TaskStatus::Done,
                error: None,
            });
            println!("服务器运行中；按 Ctrl-C 停止。");
            tokio::select! { _ = cancel.cancelled() => {} }
            drop(result.server);
        }
        Err(e) => {
            bus.publish(TraceEvent::TaskFinished {
                task_id,
                status: TaskStatus::Failed,
                error: Some(e.to_string()),
            });
            return Err(e.into());
        }
    }
    Ok(())
}

fn cmd_profiles(id: Option<String>) -> anyhow::Result<()> {
    let store = Store::open()?;
    match id {
        Some(id) => {
            let spec = store.load_profile(&id)?;
            print_spec_summary(&spec);
        }
        None => {
            let list = store.list_profiles();
            if list.is_empty() {
                println!("暂无档案。开服后会自动保存。");
                return Ok(());
            }
            println!("{:<24} {:<28} 创建时间", "ID", "摘要");
            for (id, summary, at) in list {
                println!("{id:<24} {summary:<28} {at}");
            }
        }
    }
    Ok(())
}

fn cmd_sessions(action: SessionsAction) -> anyhow::Result<()> {
    let store = Store::open()?;
    match action {
        SessionsAction::List => {
            let list = store.list_sessions();
            if list.is_empty() {
                println!("暂无任务记录。");
                return Ok(());
            }
            println!("{:<38} {:<20} {:<17} 状态", "任务ID", "标题", "开始时间");
            for (id, title, at, status) in list {
                println!(
                    "{id:<38} {:<20} {at:<17} {}",
                    truncate(&title, 20),
                    render::status_text(status)
                );
            }
        }
        SessionsAction::Show { task_id } => {
            let trace = store.load_trace(&task_id)?;
            println!(
                "任务 {}：{}（{}）",
                trace.task_id,
                trace.title,
                render::status_text(trace.status)
            );
            println!(
                "开始：{}  结束：{:?}",
                trace.started_at.format("%F %T"),
                trace.finished_at.map(|t| t.format("%F %T").to_string())
            );
            println!("步骤（{} 条）：", trace.steps.len());
            for (i, step) in trace.steps.iter().enumerate() {
                println!("  {:>2}. [{:?}] {}", i + 1, step.kind, step.summary);
            }
            if let Some(err) = &trace.error {
                println!("失败原因：{err}");
            }
        }
        SessionsAction::Export { task_id } => {
            let json = store.export_session(&task_id)?;
            let out = format!("{task_id}.session.json");
            std::fs::write(&out, &json).with_context(|| format!("写入 {out} 失败"))?;
            println!("已导出（已打码）：{out}");
        }
    }
    Ok(())
}

fn cmd_usage() -> anyhow::Result<()> {
    let store = Store::open()?;
    let records = store.read_usage();
    if records.is_empty() {
        println!("暂无 API 调用记录。");
        return Ok(());
    }
    let total_in: u64 = records.iter().map(|r| r.input_tokens).sum();
    let total_out: u64 = records.iter().map(|r| r.output_tokens).sum();
    let total_cost: Decimal = records.iter().map(|r| r.cost).sum();
    let unreported = records.iter().filter(|r| !r.usage_reported).count();
    let by_phase = |phase: Phase| -> (usize, u64, u64, Decimal) {
        records
            .iter()
            .filter(|r| r.phase == phase)
            .fold((0, 0, 0, Decimal::ZERO), |(n, i, o, c), r| {
                (n + 1, i + r.input_tokens, o + r.output_tokens, c + r.cost)
            })
    };
    println!("=== API 用量统计（全部任务）===");
    println!("调用次数：{}", records.len());
    for (name, phase) in [
        ("需求理解", Phase::Requirement),
        ("诊断", Phase::Diagnosis),
        ("对话", Phase::Chat),
    ] {
        let (n, i, o, c) = by_phase(phase);
        if n > 0 {
            println!("  {name}：{n} 次，in {i} / out {o} tokens，¥{:.4}", c);
        }
    }
    println!("输入 token：{total_in}；输出 token：{total_out}");
    println!("总费用：¥{total_cost:.4}");
    if unreported > 0 {
        println!("（其中 {unreported} 次上游未返回 usage，仅计次数，对应课程 Q9 口径）");
    }
    let cfg = AppConfig::load()?;
    if cfg.budget.limit > Decimal::ZERO {
        println!(
            "预算上限：¥{}（{}）；已用 {:.1}%",
            cfg.budget.limit,
            cfg.budget.currency,
            (total_cost / cfg.budget.limit * Decimal::from(100)).round_dp(1)
        );
    }
    Ok(())
}

fn cmd_config(action: ConfigAction) -> anyhow::Result<()> {
    match action {
        ConfigAction::Wizard => {
            // 配置向导的 config 子集：不含二进制注册（决议 D12）
            tokio::task::block_in_place(|| cmd_setup_inner(false))?;
        }
        ConfigAction::Init => {
            let cfg = AppConfig::load().context("生成模板失败")?;
            let env_path = crate::config::data_dir().join(crate::config::ENV_FILE);
            if !env_path.exists() {
                std::fs::write(
                    &env_path,
                    format!(
                        "# 把你的 API Key 写在下面\n{}=在这里粘贴你的API密钥\n",
                        cfg.model.api_key_env
                    ),
                )
                .context("写 .env 失败")?;
            }
            println!("配置目录：{}", crate::config::data_dir().display());
            println!("已生成 config.toml（填 model.endpoint/model）与 .env（填 API Key）");
        }
        ConfigAction::Show => {
            let cfg = AppConfig::load()?;
            print!("{cfg}");
        }
        ConfigAction::Set { key, value } => {
            let path = crate::config::config_path();
            let raw = std::fs::read_to_string(&path).context("读取 config.toml 失败")?;
            let mut root: toml::Value = toml::from_str(&raw).context("解析 config.toml 失败")?;
            let segments: Vec<&str> = key.split('.').collect();
            if segments.len() < 2 {
                bail!("键名需形如 section.field，例如 model.model");
            }
            let mut node = &mut root;
            for seg in &segments[..segments.len() - 1] {
                node = node
                    .as_table_mut()
                    .and_then(|t| t.get_mut(*seg))
                    .with_context(|| format!("配置段 {seg} 不存在"))?;
            }
            let last = segments[segments.len() - 1];
            let table = node.as_table_mut().context("目标不是配置表")?;
            // 数字/布尔自动识别，其余按字符串
            let parsed = if value == "true" || value == "false" {
                toml::Value::Boolean(value == "true")
            } else if let Ok(n) = value.parse::<i64>() {
                toml::Value::Integer(n)
            } else if let Ok(f) = value.parse::<f64>() {
                toml::Value::Float(f)
            } else {
                toml::Value::String(value.clone())
            };
            table.insert(last.to_string(), parsed);
            std::fs::write(&path, toml::to_string_pretty(&root).context("序列化失败")?)
                .context("写回 config.toml 失败")?;
            println!("已更新 {key} = {value}");
        }
    }
    Ok(())
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let cut: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

// ── 上手引导与配置向导（FR-18/19，决议 D12/D13/D14）────────────────────

/// `mcha setup`（FR-18）：配置向导 + 二进制注册。
async fn cmd_setup(with_binary: bool) -> anyhow::Result<()> {
    tokio::task::block_in_place(|| cmd_setup_inner(with_binary))
}

/// 向导主体（同步函数：dialoguer 阻塞交互；调用方负责 block_in_place）。
fn cmd_setup_inner(with_binary: bool) -> anyhow::Result<()> {
    let mut cfg = AppConfig::load().context("加载配置失败")?;
    println!("=== Minecraft Host Agent (MCHA) 上手向导 ===");
    println!("数据目录：{}", crate::config::data_dir().display());
    println!("必填仅 3 项；其余保持默认，随时可重跑 `mcha config wizard` 修改。");

    wizard_required(&mut cfg)?;

    let advanced = Confirm::new()
        .with_prompt("是否配置高级选项？（上下文长度/思考模式/超时/预算/代理/镜像/工作区）")
        .default(false)
        .interact()
        .context("读取确认失败")?;
    if advanced {
        wizard_optional(&mut cfg)?;
    }

    save_config_toml(&cfg)?;
    println!("[ok] 已写回 {}", crate::config::config_path().display());

    match cfg.validate() {
        Ok(()) => println!("[ok] 配置校验通过"),
        Err(e) => println!(
            "[!] 尚未完全可用：{e}\n    可重跑本向导补齐；`mcha plan` / `mcha profiles` 等无需 LLM 的功能不受影响。"
        ),
    }

    if with_binary {
        println!();
        register_binary()?;
    }

    println!("\n=== 下一步 ===");
    println!("  mcha new \"我们 5 个人，想玩暮色森林\"   一句话开服");
    println!("  mcha usage                          查看用量与费用");
    Ok(())
}

/// 必填段（决议 D14）：endpoint / model / API Key，缺项循环重问。
fn wizard_required(cfg: &mut AppConfig) -> anyhow::Result<()> {
    println!("\n── 必填项（LLM 服务信息，见服务商控制台）──");

    // 1. API 服务：预设快捷项 + 自定义
    let presets: [(&str, &str, &str); 3] = [
        (
            "智谱 GLM（open.bigmodel.cn）",
            "https://open.bigmodel.cn/api/paas/v4",
            "glm-5.2",
        ),
        (
            "DeepSeek（api.deepseek.com）",
            "https://api.deepseek.com/v1",
            "deepseek-chat",
        ),
        ("自定义（粘贴 OpenAI 兼容地址）", "", ""),
    ];
    let labels: Vec<&str> = presets.iter().map(|(l, _, _)| *l).collect();
    let idx = Select::new()
        .with_prompt("API 服务")
        .items(&labels)
        .default(0)
        .interact()
        .context("读取选择失败")?;
    let (endpoint, model_default) = if idx == presets.len() - 1 {
        let e: String = Input::<String>::new()
            .with_prompt("OpenAI 兼容 API 地址（不含 /chat/completions 后缀）")
            .interact_text()
            .context("读取输入失败")?;
        (e, String::new())
    } else {
        (presets[idx].1.to_string(), presets[idx].2.to_string())
    };
    cfg.model.endpoint = endpoint;

    // 2. 模型名（选预设时带推荐值，回车即采用）
    cfg.model.model = if model_default.is_empty() {
        nonempty_input("模型名称（如 glm-5.2）", &cfg.model.model)?
    } else {
        Input::<String>::new()
            .with_prompt("模型名称")
            .default(model_default)
            .interact_text()
            .context("读取输入失败")?
    };

    // 3. API Key：隐藏输入，直写 .env（不进仓库）
    let env_name = cfg.model.api_key_env.clone();
    let env_path = crate::config::data_dir().join(crate::config::ENV_FILE);
    let placeholder = "在这里粘贴你的API密钥";
    if let Some(existing) = read_env_value(&env_name) {
        let filled = !existing.trim().is_empty() && existing.trim() != placeholder;
        if filled {
            let overwrite = Confirm::new()
                .with_prompt(format!(".env 中已有 {env_name}，覆盖？"))
                .default(false)
                .interact()
                .context("读取确认失败")?;
            if !overwrite {
                bail!("已取消：保留原密钥");
            }
        }
    }
    let key: String = Password::new()
        .with_prompt(format!(
            "API Key（写入 {}，输入不回显）",
            env_path.display()
        ))
        .interact()
        .context("读取输入失败")?;
    write_env_key(&env_name, key.trim())?;
    // 会话内立即生效，使随后的 validate() 可过（与 load_dotenv 同语义：已存在则不动）。
    // SAFETY：向导阶段无并发读取该键的线程，与 config::load_dotenv 的调用时机同理。
    if std::env::var_os(&env_name).is_none() {
        unsafe { std::env::set_var(&env_name, key.trim()) };
    }
    Ok(())
}

/// 选填段（决议 D14）：逐项显示当前值，回车 = 保持默认。
fn wizard_optional(cfg: &mut AppConfig) -> anyhow::Result<()> {
    println!("\n── 高级选项（回车 = 保持当前值）──");

    cfg.model.context_len = Input::<u32>::new()
        .with_prompt("上下文长度（token，发送前按此裁剪历史）")
        .default(cfg.model.context_len)
        .interact_text()
        .context("读取输入失败")?;

    cfg.model.thinking = Confirm::new()
        .with_prompt("思考模式（部分模型如 GLM 支持；更深入但更慢更贵）")
        .default(cfg.model.thinking)
        .interact()
        .context("读取确认失败")?;

    cfg.model.timeout_secs = Input::<u64>::new()
        .with_prompt("单次请求超时（秒）")
        .default(cfg.model.timeout_secs)
        .interact_text()
        .context("读取输入失败")?;

    // 预算上限：数字循环校验，不静默兜底
    loop {
        let raw: String = Input::new()
            .with_prompt("预算上限（累计费用达到即中断任务；0 = 不限制）")
            .default(cfg.budget.limit.to_string())
            .interact_text()
            .context("读取输入失败")?;
        match raw.trim().parse::<Decimal>() {
            Ok(d) => {
                cfg.budget.limit = d;
                break;
            }
            Err(_) => println!("请输入数字，例如 5 或 0"),
        }
    }

    cfg.network.proxy = optional_text_input(
        "HTTP(S) 代理（如 http://127.0.0.1:7890；留空 = 直连）",
        &cfg.network.proxy,
    )?;
    cfg.network.adoptium_mirror = optional_text_input(
        "Adoptium JRE 下载镜像（国内推荐 https://mirrors.tuna.tsinghua.edu.cn/Adoptium；留空 = 官方渠道）",
        &cfg.network.adoptium_mirror,
    )?;
    cfg.workspace.path = optional_text_input(
        "工作区：服务端安装根目录（留空 = 数据目录内 profiles/）",
        &cfg.workspace.path,
    )?;
    Ok(())
}

/// 非空文本输入循环：空输入时重问（必填项语义）。
fn nonempty_input(prompt: &str, initial: &str) -> anyhow::Result<String> {
    loop {
        let mut input = Input::<String>::new().with_prompt(prompt);
        if !initial.is_empty() {
            input = input.with_initial_text(initial);
        }
        let v: String = input.interact_text().context("读取输入失败")?;
        if !v.trim().is_empty() {
            return Ok(v.trim().to_string());
        }
        println!("该项不能为空。");
    }
}

/// 允许留空的文本输入（回车 = 保持默认/现有值）。
fn optional_text_input(prompt: &str, default: &str) -> anyhow::Result<String> {
    let v: String = Input::<String>::new()
        .with_prompt(prompt)
        .default(default.to_string())
        .show_default(false)
        .allow_empty(true)
        .interact_text()
        .context("读取输入失败")?;
    Ok(v.trim().to_string())
}

/// 用 toml_edit 写回 config.toml：只改目标键，保留用户注释与排版（决议 D12）。
fn save_config_toml(cfg: &AppConfig) -> anyhow::Result<()> {
    write_config_toml(&crate::config::config_path(), cfg)
}

/// 把 cfg 写入指定 config.toml（路径参数化，便于测试）。
fn write_config_toml(path: &Path, cfg: &AppConfig) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("解析 {} 失败（可手工修复或删除后重跑向导）", path.display()))?;

    doc["model"]["endpoint"] = toml_edit::value(cfg.model.endpoint.clone());
    doc["model"]["model"] = toml_edit::value(cfg.model.model.clone());
    doc["model"]["api_key_env"] = toml_edit::value(cfg.model.api_key_env.clone());
    doc["model"]["context_len"] = toml_edit::value(i64::from(cfg.model.context_len));
    doc["model"]["thinking"] = toml_edit::value(cfg.model.thinking);
    doc["model"]["timeout_secs"] = toml_edit::value(cfg.model.timeout_secs as i64);
    doc["budget"]["limit"] = toml_edit::value(cfg.budget.limit.to_f64().unwrap_or(0.0));
    doc["budget"]["currency"] = toml_edit::value(cfg.budget.currency.clone());
    doc["network"]["proxy"] = toml_edit::value(cfg.network.proxy.clone());
    doc["network"]["adoptium_mirror"] = toml_edit::value(cfg.network.adoptium_mirror.clone());
    doc["workspace"]["path"] = toml_edit::value(cfg.workspace.path.clone());

    std::fs::write(path, doc.to_string())
        .with_context(|| format!("写回 {} 失败", path.display()))?;
    Ok(())
}

/// 读取 .env 中某键的当前值（向导预检用）。
fn read_env_value(env_name: &str) -> Option<String> {
    read_env_file_value(
        &crate::config::data_dir().join(crate::config::ENV_FILE),
        env_name,
    )
}

/// 读取指定 .env 中某键的当前值（路径参数化，便于测试）。
fn read_env_file_value(path: &Path, env_name: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let prefix = format!("{env_name}=");
    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix(&prefix) {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// 把 API Key 写入 .env：已存在同键则原位替换，否则追加；其它行原样保留。
fn write_env_key(env_name: &str, value: &str) -> anyhow::Result<()> {
    write_env_file(
        &crate::config::data_dir().join(crate::config::ENV_FILE),
        env_name,
        value,
    )
}

/// 把键值写入指定 .env（路径参数化，便于测试）。
fn write_env_file(path: &Path, env_name: &str, value: &str) -> anyhow::Result<()> {
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .map(|c| c.lines().map(String::from).collect())
        .unwrap_or_default();
    let prefix = format!("{env_name}=");
    let new_line = format!("{env_name}={value}");
    if let Some(pos) = lines
        .iter()
        .position(|l| l.trim_start().starts_with(&prefix) && !l.trim_start().starts_with('#'))
    {
        lines[pos] = new_line;
    } else {
        lines.push(new_line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(path, out).with_context(|| format!("写 {} 失败", path.display()))?;
    Ok(())
}

/// 二进制注册（决议 D12）：`mcha` 不在 PATH 时把当前可执行文件复制到 cargo bin。
fn register_binary() -> anyhow::Result<()> {
    let current = std::env::current_exe().context("定位当前可执行文件失败")?;
    match command_in_path() {
        Some(found) if found == current => {
            println!("[ok] mcha 已注册在 PATH：{}", found.display());
        }
        Some(found) => {
            println!("[i] PATH 中已有 mcha：{}", found.display());
            println!("    当前运行的程序：{}", current.display());
            let replace = Confirm::new()
                .with_prompt(
                    "用当前程序覆盖 PATH 中的 mcha？（正式安装请用 cargo install --path .）",
                )
                .default(false)
                .interact()
                .context("读取确认失败")?;
            if replace {
                copy_self_to_cargo_bin(&current)?;
            }
        }
        None => {
            let dest = cargo_bin_dir().join(exe_name());
            let do_copy = Confirm::new()
                .with_prompt(format!(
                    "把当前程序复制到 {}（加入 PATH 后可全局调用）？",
                    dest.display()
                ))
                .default(true)
                .interact()
                .context("读取确认失败")?;
            if do_copy {
                copy_self_to_cargo_bin(&current)?;
            } else {
                println!("[i] 跳过注册；以后可用 `cargo install --path .` 安装。");
            }
        }
    }
    Ok(())
}

/// 可执行文件名（跨平台）。
fn exe_name() -> &'static str {
    if cfg!(windows) { "mcha.exe" } else { "mcha" }
}

/// 在 PATH 各目录中查找 mcha 可执行文件。
fn command_in_path() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(exe_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// cargo bin 目录（CARGO_HOME 优先，否则 ~/.cargo/bin）。
fn cargo_bin_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("CARGO_HOME") {
        return PathBuf::from(h).join("bin");
    }
    crate::config::expand_tilde("~/.cargo/bin")
}

fn copy_self_to_cargo_bin(current: &Path) -> anyhow::Result<()> {
    let dest = cargo_bin_dir().join(exe_name());
    std::fs::create_dir_all(cargo_bin_dir()).context("创建 cargo bin 目录失败")?;
    std::fs::copy(current, &dest)
        .with_context(|| format!("复制 {} → {} 失败", current.display(), dest.display()))?;
    println!(
        "[ok] 已复制到 {}；新开终端后运行 `mcha --version` 验证",
        dest.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 配置写回保留注释且新值生效() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "# 头部注释\n[model]\n# 模型名注释\nmodel = \"glm-5.2\"\nendpoint = \"https://old\"\n",
        )
        .unwrap();

        let mut cfg = AppConfig::default();
        cfg.model.model = "glm-4.7".into();
        cfg.model.endpoint = "https://new".into();
        cfg.model.context_len = 64_000;
        cfg.workspace.path = "/data/mc".into();
        write_config_toml(&path, &cfg).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 头部注释"), "头部注释应保留");
        assert!(text.contains("# 模型名注释"), "节内注释应保留");
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.model.model, "glm-4.7");
        assert_eq!(parsed.model.endpoint, "https://new");
        assert_eq!(parsed.model.context_len, 64_000);
        assert_eq!(parsed.workspace.path, "/data/mc");
    }

    #[test]
    fn 空文件写回可再次解析() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        write_config_toml(&path, &AppConfig::default()).unwrap();
        let parsed: AppConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.model.context_len, 128_000);
        assert!(parsed.workspace.path.is_empty());
    }

    #[test]
    fn env键替换与追加() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        std::fs::write(&path, "# 注释保留\nOTHER=1\nMCHA_API_KEY=旧的\n").unwrap();

        write_env_file(&path, "MCHA_API_KEY", "新的").unwrap();
        assert_eq!(
            read_env_file_value(&path, "MCHA_API_KEY").as_deref(),
            Some("新的")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# 注释保留") && text.contains("OTHER=1"),
            "无关行应保留"
        );
        assert_eq!(text.matches("MCHA_API_KEY=").count(), 1, "原位替换不重复");

        write_env_file(&path, "EXTRA_KEY", "v2").unwrap();
        assert_eq!(
            read_env_file_value(&path, "EXTRA_KEY").as_deref(),
            Some("v2")
        );
    }
}
