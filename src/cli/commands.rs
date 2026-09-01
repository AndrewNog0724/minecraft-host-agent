//! 管理子命令：config / usage / sessions（R3 / R5 / R6 的查询入口）。

use anyhow::Context;
use crossterm::style::{Color, Stylize};
use std::path::Path;

use crate::config::{self, AppConfig};
use crate::store::session::{self, Session};
use crate::store::usage::UsageLedger;

pub fn config_list(data_dir: &Path) -> anyhow::Result<()> {
    let loaded = AppConfig::load(data_dir)?;
    if !loaded.existed {
        println!(
            "尚未创建配置（{} 不存在）。运行 mcha setup 开始配置。",
            AppConfig::config_path(data_dir).display()
        );
        return Ok(());
    }
    let config = &loaded.config;
    println!("{}", "── 当前生效配置 ──".with(Color::Cyan));
    println!(
        "model.endpoint        = {}",
        if config.model.endpoint.is_empty() {
            "（未设置）"
        } else {
            &config.model.endpoint
        }
    );
    println!(
        "model.model           = {}",
        if config.model.model.is_empty() {
            "（未设置）"
        } else {
            &config.model.model
        }
    );
    println!("model.context_len     = {}", config.model.context_len);
    println!("model.thinking        = {}", config.model.thinking);
    println!("model.api_key_env     = {}", config.model.api_key_env);
    let key_present = std::env::var(&config.model.api_key_env)
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    println!(
        "api_key               = {}",
        if key_present {
            "已设置（不显示）"
        } else {
            "未设置！"
        }
    );
    println!("budget.limit_cny      = {:.2}", config.budget.limit_cny);
    println!("safety.confirm_level  = {}", config.safety.confirm_level);
    println!(
        "search.backend        = {}",
        if config.search.backend.is_empty() {
            "（无，web_search 将返回说明）"
        } else {
            &config.search.backend
        }
    );
    println!(
        "agent.max_tool_calls_per_turn = {}",
        config.agent.max_tool_calls_per_turn
    );
    println!(
        "agent.command_timeout_secs    = {}",
        config.agent.command_timeout_secs
    );
    println!(
        "价格表条目            = {} 条（含内置预设）",
        config.prices.len()
    );
    println!();
    println!("配置文件：{}", AppConfig::config_path(data_dir).display());
    println!("修改示例：mcha config set budget.limit_cny 5");
    Ok(())
}

pub fn config_set(data_dir: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    let path = AppConfig::config_path(data_dir);
    config::edit::set_key(&path, key, value)?;
    println!("已写入 {} = {value}", key.with(Color::Green));
    println!("文件：{}", path.display());
    Ok(())
}

pub async fn config_test(data_dir: &Path) -> anyhow::Result<()> {
    let loaded = AppConfig::load(data_dir)?;
    let config = &loaded.config;
    config
        .validate()
        .map_err(|missing| anyhow::anyhow!("配置缺失，无法测试：\n{missing}"))?;
    let api_key = config.api_key()?;
    println!(
        "测试 {} @ {} …",
        config.model.model.clone().with(Color::Cyan),
        config.model.endpoint
    );
    let (latency, reply) =
        crate::cli::setup::connection_test(&config.model.endpoint, &config.model.model, &api_key)
            .await?;
    println!(
        "{}",
        format!("✓ 连接正常（{latency} ms）：{reply}").with(Color::Green)
    );
    Ok(())
}

pub fn usage(data_dir: &Path, session_id: Option<&str>) -> anyhow::Result<()> {
    let ledger = UsageLedger::new(data_dir)?;
    let summary = ledger.summarize(session_id)?;
    println!("{}", "── 用量与费用（R6）──".with(Color::Cyan));
    match session_id {
        Some(id) => println!("会话：{id}"),
        None => println!("范围：全部会话（{} 个）", summary.sessions),
    }
    println!("调用次数      = {}", summary.calls);
    println!("输入 token    = {}", summary.input_tokens);
    println!("输出 token    = {}", summary.output_tokens);
    println!("费用          = ¥{:.4}", summary.cost_cny);
    if summary.unpriced_calls > 0 {
        println!(
            "{}",
            format!("其中 {} 次调用无价格预设，费用记 0、仅计 token（可在 config.toml [[prices]] 补充）", summary.unpriced_calls)
                .with(Color::Yellow)
        );
    }
    println!("账本文件：{}", ledger.path().display());
    Ok(())
}

pub fn sessions_list(data_dir: &Path) -> anyhow::Result<()> {
    let list = session::list_sessions(&data_dir.join("sessions"))?;
    if list.is_empty() {
        println!("尚无历史会话。");
        return Ok(());
    }
    println!("{}", "── 历史会话 ──".with(Color::Cyan));
    for (meta, path) in &list {
        println!(
            "{}  {} 条消息  {}",
            meta.id.clone().with(Color::Green),
            meta.message_count,
            meta.updated_at
        );
        if let Some(title) = &meta.title {
            println!("    {title}");
        }
        let _ = path;
    }
    println!("查看明细：mcha sessions show <id>；接续会话：mcha --continue");
    Ok(())
}

pub fn sessions_show(data_dir: &Path, id: &str) -> anyhow::Result<()> {
    let jsonl = data_dir.join("sessions").join(format!("{id}.jsonl"));
    let session = Session::load(&jsonl, data_dir)
        .with_context(|| format!("找不到会话 {id}（路径：{}）", jsonl.display()))?;
    println!("{}", format!("── 会话 {id} ──").with(Color::Cyan));
    for message in &session.messages {
        print_message_brief(message);
    }
    // 该会话的调用明细（R6 三层展示的第三层）
    let ledger = UsageLedger::new(data_dir)?;
    let summary = ledger.summarize(Some(id))?;
    println!();
    println!("{}", "── 调用明细汇总 ──".with(Color::DarkGrey));
    println!(
        "调用 {} 次：输入 {} tokens · 输出 {} tokens · 费用 ¥{:.4}（未计价 {} 次）",
        summary.calls,
        summary.input_tokens,
        summary.output_tokens,
        summary.cost_cny,
        summary.unpriced_calls
    );
    Ok(())
}

pub fn sessions_export(data_dir: &Path, id: &str, out_path: Option<&Path>) -> anyhow::Result<()> {
    let jsonl = data_dir.join("sessions").join(format!("{id}.jsonl"));
    let session = Session::load(&jsonl, data_dir).with_context(|| format!("找不到会话 {id}"))?;
    // 导出打码（NFR-2）：遮蔽密钥与公网 IP
    let api_key = AppConfig::load(data_dir)
        .ok()
        .and_then(|loaded| loaded.config.api_key().ok())
        .unwrap_or_default();
    let secrets: Vec<String> = if api_key.is_empty() {
        Vec::new()
    } else {
        vec![api_key]
    };
    let payload = serde_json::json!({
        "meta": session.meta,
        "totals": {
            "input_tokens": session.total_input_tokens,
            "output_tokens": session.total_output_tokens,
            "cost_cny": session.total_cost_cny,
        },
        "messages": session.messages,
    });
    let raw = serde_json::to_string_pretty(&payload).context("序列化会话失败")?;
    let masked = crate::store::mask::mask_sensitive(&raw, &secrets);
    let target = out_path.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_default()
            .join(format!("{id}.export.json"))
    });
    std::fs::write(&target, masked).with_context(|| format!("写出失败：{}", target.display()))?;
    println!("已导出（自动打码）：{}", target.display());
    Ok(())
}

fn print_message_brief(message: &crate::agent::message::Message) {
    use crate::agent::message::Message as M;
    let (role, text) = match message {
        M::User { content } => ("user", content.clone()),
        M::Assistant { content, .. } => {
            if let Some(text) = content {
                ("assistant", text.clone())
            } else if let Some(call) = message.tool_calls().first() {
                ("assistant", format!("→ {}({})", call.name, call.arguments))
            } else {
                ("assistant", String::new())
            }
        }
        M::Tool { outcome, .. } => ("tool", outcome.summary(600)),
        M::System { content } => ("system", content.clone()),
    };
    let text = crate::agent::message::truncate_chars(text.trim(), 800);
    println!("[{role}] {text}");
    println!();
}
