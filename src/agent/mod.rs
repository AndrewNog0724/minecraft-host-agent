//! agent：Agent Loop（唯一执行引擎，决议 D100；设计 §8.1）。
//!
//! 循环：发送消息（system + 裁剪后的历史 + 工具声明）→ 解析回复 →
//! 有 tool_calls 则逐个执行并回传 → 纯文本且无工具调用则本回合结束。
//! 停止条件：模型自然结束 / Ctrl-C 打断 / 预算耗尽 / 轮数保险丝。

pub mod context;
pub mod message;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::cancel::CancelToken;
use crate::config::{AppConfig, ConfirmLevel};
use crate::events::{Event, EventTx, summarize_args};
use crate::llm::{ChatRequest, ChatResponse, LlmClient};
use crate::store::now_rfc3339;
use crate::store::session::Session;
use crate::store::usage::{UsageLedger, UsageRecord, compute_cost};
use crate::tools::general::load_skill::available_skills;
use crate::tools::{
    ConfirmDecision, ConfirmRequest, Interaction, Permission, ToolCtx, ToolError, ToolRegistry,
    validate_args,
};

pub use message::{Message, ToolCall, ToolOutcome};

/// 回合结束原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    /// 模型给出最终回复，交还用户输入。
    Natural,
    /// 用户 Ctrl-C 打断（会话保留，决议 D109）。
    Interrupted,
    /// 预算耗尽（R6，强制收尾并报告）。
    BudgetExhausted,
    /// 单回合工具调用次数保险丝熔断（防失控循环）。
    Fuse,
    /// 模型调用失败（网络 / 上游错误），控制权交还用户。
    LlmFailed,
}

impl TurnEnd {
    pub fn notice(&self) -> Option<String> {
        match self {
            TurnEnd::Natural => None,
            TurnEnd::Interrupted => Some("⏹ 已打断，会话保留".to_string()),
            TurnEnd::BudgetExhausted => Some(
                "预算已耗尽，回合中断。可调大 config.toml 的 budget.limit_cny 后继续".to_string(),
            ),
            TurnEnd::Fuse => Some("已达单回合工具调用上限（保险丝），回合中断".to_string()),
            TurnEnd::LlmFailed => None, // 失败详情已单独发 Notice
        }
    }
}

/// Agent 运行环境：cli 装配一次，多回合共用。
pub struct AgentEnv {
    pub llm: Arc<dyn LlmClient>,
    pub registry: Arc<ToolRegistry>,
    pub config: AppConfig,
    pub system_prompt: String,
    pub workspace: PathBuf,
    pub data_dir: PathBuf,
    pub http: reqwest::Client,
    pub interaction: Arc<dyn Interaction>,
    pub ledger: UsageLedger,
}

/// Agent Loop（无状态方法集；会话状态在 `Session`，框架配置在 `AgentEnv`）。
pub struct Agent;

impl Agent {
    /// 执行一个用户回合：从追加 user 消息开始，到停止条件满足为止。
    ///
    /// `allowed_tools` 是"本会话允许此工具"的授权集合（确认门 y/a/n 的 a，
    /// 决议 D110），跨回合保留、由会话层持有。
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        env: &AgentEnv,
        session: &mut Session,
        user_text: &str,
        events: &EventTx,
        cancel: CancelToken,
        allowed_tools: &mut HashSet<String>,
    ) -> anyhow::Result<TurnEnd> {
        session.push_message(Message::user(user_text.to_string()))?;
        let mut tool_calls_used: u32 = 0;
        let max_calls = env.config.agent.max_tool_calls_per_turn;

        let end = 'turn: loop {
            // 停止条件检查（NFR-4：预算硬上限在每轮 LLM 调用前强制）
            if session.total_cost_cny >= env.config.budget.limit_cny {
                if let Some(text) = TurnEnd::BudgetExhausted.notice() {
                    let _ = events.send(Event::Notice(text));
                }
                break 'turn TurnEnd::BudgetExhausted;
            }
            if tool_calls_used >= max_calls {
                if let Some(text) = TurnEnd::Fuse.notice() {
                    let _ = events.send(Event::Notice(text));
                }
                break 'turn TurnEnd::Fuse;
            }

            // 组装请求：system prompt + 裁剪后的完整回合（R3 context_len）
            let system_tokens = context::estimate_tokens(&env.system_prompt);
            let history = context::trim_context(
                system_tokens,
                &session.messages,
                env.config.model.context_len as usize,
            );
            let mut messages = vec![Message::system(env.system_prompt.clone())];
            messages.extend(history);
            let request = ChatRequest {
                model: env.config.model.model.clone(),
                messages,
                tools: env.registry.specs(),
                thinking: env.config.model.thinking,
            };

            // LLM 调用（可被 Ctrl-C 打断；打断即回合结束，无未配对消息）
            let response: ChatResponse = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    if let Some(text) = TurnEnd::Interrupted.notice() {
                        let _ = events.send(Event::Notice(text));
                    }
                    break 'turn TurnEnd::Interrupted;
                }
                result = env.llm.chat(request, Some(events)) => match result {
                    Ok(resp) => resp,
                    Err(failure) => {
                        Self::record_attempts(env, session, events, &failure.attempts, None)?;
                        let _ = events.send(Event::Notice(format!("模型调用失败：{}。请检查网络与配置后重试", failure.error)));
                        break 'turn TurnEnd::LlmFailed;
                    }
                }
            };

            // 思考模式占位（D112）：思考全文不入史，耗时并入用量记录备注
            // 回复被截断时提示用户（finish_reason 的真实消费）
            if response.reply.finish_reason.as_deref() == Some("length") {
                let _ = events.send(Event::Notice(
                    "模型回复因长度上限被截断；可分步继续或调大上下文".to_string(),
                ));
            }

            // 用量入账（重试各计一条，R6 诚实计量）
            Self::record_attempts(
                env,
                session,
                events,
                &response.attempts,
                response.reply.reasoning_secs,
            )?;

            // 追加 assistant 消息；arguments 非法 JSON 时保留原文供模型自纠
            let tool_calls: Vec<ToolCall> = response
                .reply
                .tool_calls
                .iter()
                .enumerate()
                .map(|(index, call)| ToolCall {
                    id: if call.id.is_empty() {
                        format!("call_{index}")
                    } else {
                        call.id.clone()
                    },
                    name: call.name.clone(),
                    arguments: serde_json::from_str(&call.arguments)
                        .unwrap_or(serde_json::Value::String(call.arguments.clone())),
                })
                .collect();
            session.push_message(Message::assistant(
                response.reply.content.clone(),
                tool_calls.clone(),
            ))?;

            if tool_calls.is_empty() {
                break 'turn TurnEnd::Natural;
            }

            // 逐个执行工具（回合原子性：任何打断路径都把未完成调用回填错误）
            for (index, call) in tool_calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    Self::fill_rest(session, &tool_calls[index..], "用户中断")?;
                    if let Some(text) = TurnEnd::Interrupted.notice() {
                        let _ = events.send(Event::Notice(text));
                    }
                    break 'turn TurnEnd::Interrupted;
                }
                tool_calls_used += 1;
                if tool_calls_used > max_calls {
                    Self::fill_rest(
                        session,
                        &tool_calls[index..],
                        "已达到单回合工具调用上限，未执行",
                    )?;
                    if let Some(text) = TurnEnd::Fuse.notice() {
                        let _ = events.send(Event::Notice(text));
                    }
                    break 'turn TurnEnd::Fuse;
                }

                let outcome = match Self::dispatch(
                    env,
                    session,
                    call,
                    events,
                    &cancel,
                    allowed_tools,
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(ToolError::Cancelled) => ToolOutcome::err("用户中断"),
                    Err(other) => ToolOutcome::err(format!("工具框架错误：{other}")),
                };
                session.push_message(Message::tool_result(
                    call.id.clone(),
                    call.name.clone(),
                    outcome,
                ))?;

                // 工具执行期间被打断：剩余调用回填（D109）
                if cancel.is_cancelled() {
                    Self::fill_rest(session, &tool_calls[index + 1..], "用户中断")?;
                    if let Some(text) = TurnEnd::Interrupted.notice() {
                        let _ = events.send(Event::Notice(text));
                    }
                    break 'turn TurnEnd::Interrupted;
                }
            }
        };
        Ok(end)
    }

    /// 未执行的调用回填占位错误，保证消息流配对合法（D109）。
    fn fill_rest(session: &mut Session, pending: &[ToolCall], reason: &str) -> anyhow::Result<()> {
        for call in pending {
            session.push_message(Message::tool_result(
                call.id.clone(),
                call.name.clone(),
                ToolOutcome::err(reason),
            ))?;
        }
        Ok(())
    }

    /// 单个工具调用：校验 → 确认门 → 执行 → 大输出落盘。
    async fn dispatch(
        env: &AgentEnv,
        session: &mut Session,
        call: &ToolCall,
        events: &EventTx,
        cancel: &CancelToken,
        allowed_tools: &mut HashSet<String>,
    ) -> Result<ToolOutcome, ToolError> {
        let Some(tool) = env.registry.get(&call.name) else {
            return Ok(ToolOutcome::err(format!(
                "未知工具「{}」；可用工具：{}",
                call.name,
                env.registry.names().join("、")
            )));
        };

        // 参数 Schema 校验（D112：携错误回传，模型自行修正）
        if let Err(message) = validate_args(tool, &call.arguments) {
            return Ok(ToolOutcome::err(message));
        }

        // 确认门（D106/D110）
        match Self::gate(env, tool, &call.arguments, allowed_tools).await? {
            ConfirmDecision::Allow | ConfirmDecision::AllowAlways => {}
            ConfirmDecision::Deny => {
                return Ok(ToolOutcome::err(
                    "用户拒绝了该操作。请调整方案，或用 ask_user 询问用户意图",
                ));
            }
        }

        let ctx = ToolCtx {
            workspace: env.workspace.clone(),
            data_dir: env.data_dir.clone(),
            http: env.http.clone(),
            cancel: cancel.clone(),
            interaction: env.interaction.clone(),
            events: events.clone(),
            command_timeout_secs: env.config.agent.command_timeout_secs,
            search_backend: env.config.search.backend.clone(),
        };

        let _ = events.send(Event::ToolStarted {
            name: call.name.clone(),
            args_summary: summarize_args(&call.arguments, 120),
        });
        let started = Instant::now();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolError::Cancelled),
            outcome = tool.run(call.arguments.clone(), &ctx) => outcome,
        };
        let duration_ms = started.elapsed().as_millis() as u64;

        // 大输出落盘（§8.1）：附件保存 + 摘要回传，避免撑爆上下文
        let outcome = match result {
            Ok(outcome) => Self::maybe_dump_attachment(env, session, outcome)
                .map_err(|err| ToolError::Io(err.to_string()))?,
            Err(err) => return Err(err),
        };
        let ok = outcome.is_ok();
        let _ = events.send(Event::ToolFinished {
            ok,
            summary: outcome.summary(200),
            duration_ms,
        });
        Ok(outcome)
    }

    /// 确认门：按 Permission 与 confirm_level 决定是否请求用户授权。
    async fn gate(
        env: &AgentEnv,
        tool: &dyn crate::tools::Tool,
        args: &serde_json::Value,
        allowed_tools: &mut HashSet<String>,
    ) -> Result<ConfirmDecision, ToolError> {
        let permission = tool.permission();
        let level = env
            .config
            .confirm_level()
            .map_err(|err| ToolError::Io(err.to_string()))?;
        let needs_confirm = match level {
            ConfirmLevel::Auto => false,
            ConfirmLevel::Paranoid => true,
            ConfirmLevel::Standard => !matches!(permission, Permission::ReadOnly),
        };
        if !needs_confirm {
            return Ok(ConfirmDecision::Allow);
        }
        if allowed_tools.contains(tool.name()) {
            return Ok(ConfirmDecision::AllowAlways);
        }
        let request = ConfirmRequest {
            title: match permission {
                Permission::Write => format!("工具 {} 要写入文件", tool.name()),
                Permission::Execute => format!("工具 {} 要执行命令", tool.name()),
                Permission::Network => format!("工具 {} 要下载文件", tool.name()),
                Permission::ReadOnly => format!("工具 {} 请求执行", tool.name()),
            },
            lines: confirm_lines(tool.name(), permission, args),
        };
        match env.interaction.confirm(request).await {
            Ok(decision) => {
                if decision == ConfirmDecision::AllowAlways {
                    allowed_tools.insert(tool.name().to_string());
                }
                Ok(decision)
            }
            Err(crate::tools::InteractionError::Cancelled) => Err(ToolError::Cancelled),
            Err(err) => Err(ToolError::Io(err.to_string())),
        }
    }

    /// 大输出：超过阈值转存附件，回传"路径 + 首尾摘要"。
    fn maybe_dump_attachment(
        env: &AgentEnv,
        session: &mut Session,
        outcome: ToolOutcome,
    ) -> anyhow::Result<ToolOutcome> {
        let ToolOutcome::Ok { content } = outcome else {
            return Ok(outcome);
        };
        if content.len() <= env.config.agent.large_output_bytes {
            return Ok(ToolOutcome::Ok { content });
        }
        let path = session.next_attachment_path("txt")?;
        std::fs::write(&path, &content)
            .map_err(|err| anyhow::anyhow!("附件写入失败（{}）：{err}", path.display()))?;
        let head: String = content.chars().take(600).collect();
        let tail_chars: Vec<char> = content.chars().rev().take(300).collect();
        let mut tail: String = tail_chars.into_iter().rev().collect();
        tail = tail.trim_start().to_string();
        Ok(ToolOutcome::Ok {
            content: format!(
                "输出过大（{} 字节），全文已保存至 {}。\n开头：\n{}\n……\n结尾：\n{}",
                content.len(),
                path.display(),
                head,
                tail
            ),
        })
    }

    /// 尝试记录 → 账本 + 会话累计 + 事件（R6）。
    fn record_attempts(
        env: &AgentEnv,
        session: &mut Session,
        events: &EventTx,
        attempts: &[crate::llm::AttemptOutcome],
        reasoning_secs: Option<u64>,
    ) -> anyhow::Result<()> {
        for attempt in attempts {
            let (input, output) = attempt
                .usage
                .map(|u| (u.input_tokens.unwrap_or(0), u.output_tokens.unwrap_or(0)))
                .unwrap_or((0, 0));
            let price = env.config.price_for(&env.config.model.model);
            let (cost, priced) = compute_cost(price, input, output);
            let mut note = match (&attempt.usage, &attempt.note) {
                (None, _) if attempt.ok => Some("上游未返回 usage，仅计调用次数".to_string()),
                (_, note) => note.clone(),
            };
            // 思考耗时占位（D112：仅留"已思考 Ns"，全文不入史）
            if let (true, Some(secs)) = (attempt.ok, reasoning_secs) {
                let appended = format!("已思考 {secs}s");
                note = Some(match note {
                    Some(existing) => format!("{existing}；{appended}"),
                    None => appended,
                });
            }
            let record = UsageRecord {
                ts: now_rfc3339(),
                session_id: session.id.clone(),
                model: env.config.model.model.clone(),
                input_tokens: input,
                output_tokens: output,
                cost_cny: cost,
                priced,
                kind: if attempt.ok { "chat" } else { "chat-failed" }.to_string(),
                duration_ms: attempt.duration_ms,
                note,
            };
            env.ledger.append(&record)?;
            session.accumulate_usage(input, output, cost);
            let _ = events.send(Event::UsageRecorded(record));
        }
        Ok(())
    }
}

/// 确认弹窗的具体内容（显示关键信息：完整命令 / 写入摘要 / 下载目标）。
fn confirm_lines(tool: &str, permission: Permission, args: &serde_json::Value) -> Vec<String> {
    let get = |key: &str| {
        args.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    match permission {
        Permission::Execute => {
            let mut lines = vec![format!("命令：{}", get("command"))];
            if let Some(cwd) = args.get("cwd").and_then(|v| v.as_str()) {
                lines.push(format!("目录：{cwd}"));
            }
            lines
        }
        Permission::Write => {
            let mut lines = vec![format!("文件：{}", get("path"))];
            if let Some(content) = args.get("content").and_then(|v| v.as_str()) {
                let preview: Vec<String> = content
                    .lines()
                    .take(12)
                    .map(|line| format!("| {line}"))
                    .collect();
                lines.push("内容预览：".to_string());
                lines.extend(preview);
                let total = content.lines().count();
                if total > 12 {
                    lines.push(format!("| …（共 {total} 行）"));
                }
            } else if let Some(new) = args.get("new_string").and_then(|v| v.as_str()) {
                lines.push(format!(
                    "替换为：{}",
                    crate::agent::message::truncate_chars(new, 200)
                ));
            }
            lines
        }
        Permission::Network => {
            vec![
                format!("地址：{}", get("url")),
                format!("保存到：{}", get("path")),
            ]
        }
        Permission::ReadOnly => vec![format!("工具：{tool}")],
    }
}

/// 框架级 system prompt（L4 基础段，M1 交付；M2 会叠加场景段）。
pub fn build_system_prompt(available_skills: &[String]) -> String {
    let base = include_str!("../assets/prompts/system-base.md");
    if available_skills.is_empty() {
        base.to_string()
    } else {
        format!(
            "{base}\n\n## 可用技能清单\n\n以下技能可用 `load_skill` 按需加载全文：\n\n{}",
            available_skills
                .iter()
                .map(|name| format!("- {name}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

/// 便捷装配：从环境构建默认 system prompt（扫描已安装技能）。
pub fn default_system_prompt(env: &AgentEnv) -> String {
    build_system_prompt(&available_skills(&env.data_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PriceEntry;
    use crate::events::event_channel;
    use crate::llm::fake::{FakeLlm, FakeStep};
    use crate::tools::general::register_general_tools;
    use crate::tools::{AskRequest, ConfirmRequest, Interaction, InteractionError};
    use std::collections::VecDeque;

    /// 脚本化交互：确认按预置决定依次消费，提问一律报错；记录确认次数。
    struct ScriptedInteraction {
        decisions: std::sync::Mutex<VecDeque<ConfirmDecision>>,
        confirm_count: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedInteraction {
        fn new(decisions: Vec<ConfirmDecision>) -> Self {
            Self {
                decisions: std::sync::Mutex::new(decisions.into_iter().collect()),
                confirm_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn confirms(&self) -> usize {
            self.confirm_count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Interaction for ScriptedInteraction {
        async fn confirm(&self, _req: ConfirmRequest) -> Result<ConfirmDecision, InteractionError> {
            self.confirm_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut queue = self.decisions.lock().expect("测试互斥锁");
            Ok(queue.pop_front().unwrap_or(ConfirmDecision::Deny))
        }
        async fn ask(&self, _req: AskRequest) -> Result<String, InteractionError> {
            Err(InteractionError::Failed("测试未实现 ask".into()))
        }
    }

    /// 测试工具：返回指定大小的文本。
    struct BigTool;

    #[async_trait::async_trait]
    impl crate::tools::Tool for BigTool {
        fn name(&self) -> &'static str {
            "big_tool"
        }
        fn description(&self) -> String {
            "返回一大段文本".into()
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        fn permission(&self) -> crate::tools::Permission {
            crate::tools::Permission::ReadOnly
        }
        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome::ok("x".repeat(10_000)))
        }
    }

    /// 测试工具：执行时发起取消。
    struct CancellingTool;

    #[async_trait::async_trait]
    impl crate::tools::Tool for CancellingTool {
        fn name(&self) -> &'static str {
            "cancel_tool"
        }
        fn description(&self) -> String {
            "执行时取消当前回合".into()
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        fn permission(&self) -> crate::tools::Permission {
            crate::tools::Permission::ReadOnly
        }
        async fn run(
            &self,
            _args: serde_json::Value,
            ctx: &ToolCtx,
        ) -> Result<ToolOutcome, ToolError> {
            ctx.cancel.cancel();
            Ok(ToolOutcome::ok("已触发取消"))
        }
    }

    fn test_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.model.model = "m1".to_string();
        config.model.context_len = 128_000;
        config.prices = vec![PriceEntry {
            model: "m1".to_string(),
            input_per_m: 2.0,
            output_per_m: 8.0,
        }];
        config
    }

    fn test_env(
        steps: Vec<FakeStep>,
        interaction: Arc<dyn Interaction>,
        config: AppConfig,
    ) -> (AgentEnv, Arc<FakeLlm>, tempfile::TempDir) {
        let guard = tempfile::tempdir().unwrap();
        let workspace = guard.path().join("workspace");
        let data_dir = guard.path().join("data");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let mut registry = ToolRegistry::new();
        register_general_tools(&mut registry);
        registry.register(Box::new(BigTool));
        registry.register(Box::new(CancellingTool));
        let fake = Arc::new(FakeLlm::new(steps));
        let env = AgentEnv {
            llm: fake.clone(),
            registry: Arc::new(registry),
            system_prompt: build_system_prompt(&[]),
            workspace,
            data_dir,
            http: reqwest::Client::new(),
            interaction,
            ledger: UsageLedger::new(guard.path()).unwrap(),
            config,
        };
        (env, fake, guard)
    }

    fn drain_events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    async fn run_simple_turn(
        env: &AgentEnv,
        session: &mut Session,
        cancel: CancelToken,
    ) -> (TurnEnd, Vec<Event>) {
        let (tx, mut rx) = event_channel();
        let mut allowed = HashSet::new();
        let end = Agent::run_turn(env, session, "测试输入", &tx, cancel, &mut allowed)
            .await
            .unwrap();
        let events = drain_events(&mut rx);
        (end, events)
    }

    #[tokio::test]
    async fn natural_text_turn_persists_messages_and_usage() {
        let (env, _fake, _guard) = test_env(
            vec![FakeStep::Text("你好，我是模型".into())],
            Arc::new(crate::tools::general::tests::QuietInteraction),
            test_config(),
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();
        let cancel = CancelToken::new();

        let (end, events) = run_simple_turn(&env, &mut session, cancel).await;
        assert_eq!(end, TurnEnd::Natural);
        assert_eq!(session.messages.len(), 2);
        assert!(matches!(&session.messages[0], Message::User { .. }));
        assert!(matches!(&session.messages[1], Message::Assistant { .. }));
        assert_eq!(session.total_input_tokens, 100);
        assert_eq!(session.total_output_tokens, 20);
        // 用量事件已发出（D108：渲染层只累计，不即时打印）
        assert!(events.iter().any(|e| matches!(e, Event::UsageRecorded(_))));
        // 账本落盘，且有价格预设（费用已换算）
        let summary = env.ledger.summarize(None).unwrap();
        assert_eq!(summary.calls, 1);
        assert_eq!(summary.unpriced_calls, 0);
        assert!(summary.cost_cny > 0.0);
    }

    #[tokio::test]
    async fn tool_call_round_shape_and_flow() {
        let (env, fake, _guard) = test_env(
            vec![
                FakeStep::ToolCalls(vec![("list_dir".into(), serde_json::json!({}))]),
                FakeStep::Text("目录列好了".into()),
            ],
            Arc::new(crate::tools::general::tests::QuietInteraction),
            test_config(),
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();

        let (end, _) = run_simple_turn(&env, &mut session, CancelToken::new()).await;
        assert_eq!(end, TurnEnd::Natural);
        // 消息流形状：user → assistant(tool_calls) → tool → assistant(text)
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[1].tool_calls().len(), 1);
        assert!(matches!(
            &session.messages[2],
            Message::Tool {
                outcome: ToolOutcome::Ok { .. },
                ..
            }
        ));
        assert!(matches!(
            &session.messages[3],
            Message::Assistant { content: Some(_), tool_calls } if tool_calls.is_empty()
        ));
        // 第二次请求应包含工具结果（Loop 回传）与 tool 声明
        let requests = fake.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|m| matches!(m, Message::Tool { .. }))
        );
        assert_eq!(requests[0].tools.len(), env.registry.specs().len());
    }

    #[tokio::test]
    async fn unknown_tool_and_bad_args_return_structured_errors() {
        let (env, _fake, _guard) = test_env(
            vec![
                FakeStep::ToolCalls(vec![("no_such_tool".into(), serde_json::json!({}))]),
                FakeStep::ToolCalls(vec![(
                    "list_dir".into(),
                    serde_json::json!({ "path": 123 }),
                )]),
                FakeStep::Text("明白了".into()),
            ],
            Arc::new(crate::tools::general::tests::QuietInteraction),
            test_config(),
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();

        let (end, _) = run_simple_turn(&env, &mut session, CancelToken::new()).await;
        assert_eq!(end, TurnEnd::Natural);
        let errors: Vec<&Message> = session
            .messages
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    Message::Tool {
                        outcome: ToolOutcome::Err { .. },
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(errors.len(), 2, "未知工具与非法参数都应结构化报错");
    }

    #[tokio::test]
    async fn budget_exhausted_stops_before_next_call() {
        let mut config = test_config();
        config.budget.limit_cny = 0.0001; // 一次调用（100/20 token @2/8 元每百万）即超限
        let (env, _fake, _guard) = test_env(
            vec![
                // 先经一次工具调用，循环回到顶部时预算检查生效
                FakeStep::ToolCalls(vec![("list_dir".into(), serde_json::json!({}))]),
                FakeStep::Text("不应该发生".into()),
            ],
            Arc::new(crate::tools::general::tests::QuietInteraction),
            config,
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();

        let (end, events) = run_simple_turn(&env, &mut session, CancelToken::new()).await;
        assert_eq!(end, TurnEnd::BudgetExhausted);
        assert!(session.total_cost_cny >= 0.0001);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Notice(t) if t.contains("预算")))
        );
    }

    #[tokio::test]
    async fn fuse_caps_tool_calls_mid_batch() {
        let mut config = test_config();
        config.agent.max_tool_calls_per_turn = 2;
        let (env, _fake, _guard) = test_env(
            vec![
                // 一批 3 个调用，上限 2：第 3 个应被占位拒绝
                FakeStep::ToolCalls(vec![
                    ("list_dir".into(), serde_json::json!({})),
                    ("list_dir".into(), serde_json::json!({})),
                    ("list_dir".into(), serde_json::json!({})),
                ]),
            ],
            Arc::new(crate::tools::general::tests::QuietInteraction),
            config,
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();

        let (end, _) = run_simple_turn(&env, &mut session, CancelToken::new()).await;
        assert_eq!(end, TurnEnd::Fuse);
        let tool_msgs: Vec<&Message> = session
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .collect();
        assert_eq!(tool_msgs.len(), 3, "未执行的调用也要回填占位");
        assert!(matches!(
            tool_msgs[2],
            Message::Tool {
                outcome: ToolOutcome::Err { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cancel_during_tool_batch_fills_rest() {
        let (env, _fake, _guard) = test_env(
            vec![FakeStep::TextWithToolCalls(
                "我先取消一下".into(),
                vec![
                    ("cancel_tool".into(), serde_json::json!({})),
                    ("cancel_tool".into(), serde_json::json!({})),
                ],
            )],
            Arc::new(crate::tools::general::tests::QuietInteraction),
            test_config(),
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();

        let (end, _) = run_simple_turn(&env, &mut session, CancelToken::new()).await;
        assert_eq!(end, TurnEnd::Interrupted);
        // assistant 消息同时携带文本与工具调用（组合形态入史）
        assert!(matches!(
            &session.messages[1],
            Message::Assistant { content: Some(_), tool_calls } if tool_calls.len() == 2
        ));
        let tool_msgs: Vec<&Message> = session
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .collect();
        assert_eq!(tool_msgs.len(), 2);
        // 第一个已执行（返回 ok），第二个回填"用户中断"
        assert!(matches!(
            tool_msgs[0],
            Message::Tool {
                outcome: ToolOutcome::Ok { .. },
                ..
            }
        ));
        assert!(matches!(
            &tool_msgs[1],
            Message::Tool { outcome: ToolOutcome::Err { error, .. }, .. } if error.contains("用户中断")
        ));
    }

    #[tokio::test]
    async fn large_tool_output_dumped_to_attachment() {
        let mut config = test_config();
        config.agent.large_output_bytes = 1024;
        let (env, _fake, _guard) = test_env(
            vec![
                FakeStep::ToolCalls(vec![("big_tool".into(), serde_json::json!({}))]),
                FakeStep::Text("完成".into()),
            ],
            Arc::new(crate::tools::general::tests::QuietInteraction),
            config,
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();

        let (end, _) = run_simple_turn(&env, &mut session, CancelToken::new()).await;
        assert_eq!(end, TurnEnd::Natural);
        let tool_msg = session
            .messages
            .iter()
            .find(|m| matches!(m, Message::Tool { .. }))
            .unwrap();
        let Message::Tool {
            outcome: ToolOutcome::Ok { content },
            ..
        } = tool_msg
        else {
            panic!("应为成功结果")
        };
        assert!(content.contains("输出过大"));
        assert!(content.contains("已保存至"));
        assert!(content.len() < 2048, "回传的应是摘要而非全文");
    }

    #[tokio::test]
    async fn confirm_gate_standard_and_allow_always() {
        // standard 级：write_file 需要确认；第一次 Allow，第二次 AllowAlways
        let interaction = Arc::new(ScriptedInteraction::new(vec![
            ConfirmDecision::Allow,
            ConfirmDecision::AllowAlways,
        ]));
        let (env, _fake, _guard) = test_env(
            vec![
                FakeStep::ToolCalls(vec![(
                    "write_file".into(),
                    serde_json::json!({ "path": "a.txt", "content": "hello" }),
                )]),
                FakeStep::Text("第一次写完".into()),
                FakeStep::ToolCalls(vec![(
                    "write_file".into(),
                    serde_json::json!({ "path": "b.txt", "content": "world" }),
                )]),
                FakeStep::Text("第二次写完".into()),
            ],
            interaction.clone(),
            test_config(),
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();

        // 回合 1：确认一次（Allow）
        let (end, _) = run_simple_turn(&env, &mut session, CancelToken::new()).await;
        assert_eq!(end, TurnEnd::Natural);
        assert_eq!(interaction.confirms(), 1);

        // 回合 2：同一工具已在授权集合 → 不再确认
        let (tx, mut rx) = event_channel();
        let mut allowed = HashSet::new();
        allowed.insert("write_file".to_string());
        let end = Agent::run_turn(
            &env,
            &mut session,
            "再来一次",
            &tx,
            CancelToken::new(),
            &mut allowed,
        )
        .await
        .unwrap();
        assert_eq!(end, TurnEnd::Natural);
        drop(drain_events(&mut rx));
        assert_eq!(interaction.confirms(), 1, "AllowAlways 后不应再次确认");
    }

    #[tokio::test]
    async fn llm_failure_ends_turn_with_notice() {
        let (env, _fake, _guard) = test_env(
            vec![FakeStep::Fail("模拟上游 500".into())],
            Arc::new(crate::tools::general::tests::QuietInteraction),
            test_config(),
        );
        let sessions = env.data_dir.join("sessions");
        let mut session = Session::create(&sessions, &env.data_dir).unwrap();

        let (end, events) = run_simple_turn(&env, &mut session, CancelToken::new()).await;
        assert_eq!(end, TurnEnd::LlmFailed);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Notice(t) if t.contains("模型调用失败")))
        );
        // 失败调用也计入调用次数（R6）
        let summary = env.ledger.summarize(None).unwrap();
        assert_eq!(summary.calls, 1);
        // 会话里只有 user 消息（无半截 assistant）
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn compute_cost_without_price_is_honest() {
        // 无价格预设：费用记 0 且标注未计价（设计 §8.3 / 课程 Q9）
        let (cost, priced) = crate::store::usage::compute_cost(None, 10, 5);
        assert_eq!(cost, 0.0);
        assert!(!priced);
    }
}
