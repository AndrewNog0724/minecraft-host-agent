//! ui：CLI 子命令、交互问答、主流程编排（R2/R4/R5/R6 的用户入口）。

pub mod render;

use std::sync::Arc;

use anyhow::{Context as _, bail};
use clap::Subcommand;
use dialoguer::{Confirm, Input, Select};
use rust_decimal::Decimal;
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
    /// 查看当前配置（密钥打码）
    Show,
    /// 修改配置项，如 `agent config set model.model glm-5.2`
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
    }
}

/// 主流程（US1，§7.4 流程 A）。
async fn cmd_new(
    requirement: Option<String>,
    yes: bool,
    cancel: CancellationToken,
    bus: EventBus,
) -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("加载配置失败")?;
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
    bus.publish(TraceEvent::TaskStarted {
        trace: trace.clone(),
    });

    // 2. 事件泵（进度渲染 + 落盘）
    let pump_store = store.clone();
    let pump_bus = bus.clone();
    let pump_task = tokio::spawn(async move {
        let _ = render::pump(pump_bus, pump_store, bars).await;
    });

    // 3. LLM 需求理解环（不可用时引导走 plan 降级）
    let draft = {
        cfg.validate()
            .context("LLM 配置不完整（可改用 `agent plan` 手动填写方案）")?;
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
    let mut spec: Option<ServerSpec> = None;
    for _round in 0..3 {
        match derive_spec(&merged, &Answers::new(), &kb, Some(&releases)) {
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
            }
        }
    }
    let Some(mut spec) = spec else {
        bail!("澄清超过 3 轮仍未齐备，请用更完整的描述重试");
    };

    // 5. 方案摘要 + 风险提示 + 确认（FR-17）
    print_spec_summary(&spec);
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
                "\n档案已保存：profiles/{}（下次可用 `agent plan` 复用）",
                spec.spec_id
            );
            bus.publish(TraceEvent::TaskFinished {
                task_id: task_id.clone(),
                status: TaskStatus::Done,
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
            bus.publish(TraceEvent::TaskFinished {
                task_id: task_id.clone(),
                status,
            });
            return Err(e.into());
        }
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
                    .interact()
                    .context("读取输入失败")
            })?;
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
                "offline" => "offline — 全离线（开启白名单）".to_string(),
                "hybrid" => "hybrid — 混合（需认证方案）".to_string(),
                other => other.to_string(),
            })
            .collect(),
        "software" => options
            .iter()
            .map(|o| match o.as_str() {
                "vanilla" => "vanilla — 原版".to_string(),
                "paper" => "paper — Paper 插件服".to_string(),
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
            crate::spec::AccountPolicy::Online => "全正版".to_string(),
            crate::spec::AccountPolicy::Offline { whitelist } => {
                format!("全离线（白名单 {} 人）", whitelist.len())
            }
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
    let cfg = AppConfig::load()?;
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
            .items(&["vanilla 原版", "paper 插件服", "fabric mod 服"])
            .default(0)
            .interact()
    })?;
    let software = ["vanilla", "paper", "fabric"][sw_idx].to_string();
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
    let mut spec = loop {
        match derive_spec(&partial, &Answers::new(), &kb, Some(&releases)) {
            TreeOutput::Complete(s) => break *s,
            TreeOutput::NeedInput { questions, .. } => {
                let answers = ask_questions(&questions)?;
                if answers.is_empty() {
                    bail!("缺少必要信息，已退出");
                }
                merge_answers(&mut partial, &answers);
            }
        }
    };

    print_spec_summary(&spec);
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
    bus.publish(TraceEvent::TaskStarted { trace });
    let pump_store = store.clone();
    let pump_bus = bus.clone();
    tokio::spawn(async move {
        let _ = render::pump(pump_bus, pump_store, bars).await;
    });
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
            });
            println!("服务器运行中；按 Ctrl-C 停止。");
            tokio::select! { _ = cancel.cancelled() => {} }
            drop(result.server);
        }
        Err(e) => {
            bus.publish(TraceEvent::TaskFinished {
                task_id,
                status: TaskStatus::Failed,
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
