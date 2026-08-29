//! agent-core：需求理解窄 Agent Loop（§8.2）。
//!
//! 循环：发消息（含工具声明）→ 解析回复 → tool_calls 则执行并回传 → 继续；
//! 终止：submit_spec 交卷 / 最大轮数 / 用户取消 / 预算耗尽。
//! 工具全部只读无副作用（原则 1）；版本事实只经工具获得（原则 2，§8.9 红线）。

use serde_json::json;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::events::{EventBus, Phase, TaskId, TraceKind, TraceStep};
use crate::knowledge::upstream::{ModrinthClient, MojangClient};
use crate::knowledge::{CompatReport, KnowledgeBase};
use crate::llm::{ChatMessage, LlmError, LlmService, ToolCall, ToolDecl};
use crate::provision::java;
use crate::spec::ServerSpecDraft;

/// L3 指南（Skills 式按需注入，§8.9）。
const GUIDES: &[(&str, &str)] = &[
    (
        "offline-auth",
        include_str!("assets/guides/offline-auth.md"),
    ),
    (
        "fabric-basics",
        include_str!("assets/guides/fabric-basics.md"),
    ),
    (
        "tunnel-basics",
        include_str!("assets/guides/tunnel-basics.md"),
    ),
];

/// L4 系统提示词（requirement 环）。
pub const REQUIREMENT_SYSTEM_PROMPT: &str = include_str!("assets/prompts/requirement_system.md");

/// 最大轮数（§8.2 默认 8）。
const MAX_ROUNDS: usize = 8;
/// schema 校验失败重试上限（§8.3），超过则降级逐项问答。
const MAX_SCHEMA_RETRIES: usize = 2;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("{0}")]
    Llm(#[from] LlmError),
    #[error("达到最大轮数（{0}）仍未交卷；请换一种描述再试或直接使用 `plan` 命令手填")]
    MaxRounds(usize),
    #[error("任务已取消")]
    Cancelled,
}

/// 工具执行依赖（只读）。
pub struct AgentDeps {
    pub kb: KnowledgeBase,
    pub cfg: AppConfig,
    pub http: crate::knowledge::upstream::HttpBase,
    releases_cache: Mutex<Option<Vec<String>>>,
}

impl AgentDeps {
    pub fn new(
        kb: KnowledgeBase,
        cfg: AppConfig,
    ) -> Result<Self, crate::knowledge::upstream::UpstreamError> {
        let http = crate::knowledge::upstream::HttpBase::new(&cfg)?;
        Ok(Self {
            kb,
            cfg,
            http,
            releases_cache: Mutex::new(None),
        })
    }

    /// Mojang 官方正式版清单（缓存一次）。
    pub async fn known_releases(
        &self,
    ) -> Result<Vec<String>, crate::knowledge::upstream::UpstreamError> {
        let mut cache = self.releases_cache.lock().await;
        if cache.is_none() {
            *cache = Some(
                MojangClient::new(self.http.clone())
                    .release_versions()
                    .await?,
            );
        }
        Ok(cache.clone().unwrap_or_default())
    }
}

/// 需求理解环。
pub struct RequirementAgent<'a> {
    svc: &'a LlmService,
    deps: &'a AgentDeps,
    bus: EventBus,
    task_id: TaskId,
    cancel: CancellationToken,
}

impl<'a> RequirementAgent<'a> {
    pub fn new(
        svc: &'a LlmService,
        deps: &'a AgentDeps,
        bus: EventBus,
        task_id: TaskId,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            svc,
            deps,
            bus,
            task_id,
            cancel,
        }
    }

    /// 工具声明集（§8.2）：参数 Schema 一部分手写（简单）、
    /// submit_spec 用 schemars 从类型派生（单一事实来源）。
    fn tool_decls(&self) -> Vec<ToolDecl> {
        let submit_schema = serde_json::to_value(schemars::schema_for!(ServerSpecDraft))
            .unwrap_or_else(|_| json!({"type": "object"}));
        vec![
            ToolDecl::new(
                "probe_environment",
                "探测玩家机器环境：系统、架构、系统 Java 版本。在需要判断机器条件时调用。",
                json!({"type": "object", "properties": {}}),
            ),
            ToolDecl::new(
                "check_version_compat",
                "核对 MC 版本是否存在（官方清单），并返回该版本所需的 Java 大版本。任何版本号都必须经此工具核实。",
                json!({
                    "type": "object",
                    "required": ["mc_version"],
                    "properties": {
                        "mc_version": {"type": "string", "description": "MC 版本号，如 1.21.1"},
                        "software": {"type": "string", "description": "可选：vanilla/paper/fabric"}
                    }
                }),
            ),
            ToolDecl::new(
                "search_mods",
                "按名称检索 mod（支持中文名）。返回候选列表 (slug, title, project_id)。",
                json!({
                    "type": "object",
                    "required": ["query"],
                    "properties": {"query": {"type": "string"}}
                }),
            ),
            ToolDecl::new(
                "resolve_mod",
                "解析一个 mod 在指定 MC 版本/fabric 下的可用版本与依赖，返回带哈希的下载信息。",
                json!({
                    "type": "object",
                    "required": ["project", "mc_version"],
                    "properties": {
                        "project": {"type": "string", "description": "mod 的 slug 或 project id"},
                        "mc_version": {"type": "string"}
                    }
                }),
            ),
            ToolDecl::new(
                "load_guide",
                "按需加载领域指南（offline-auth / fabric-basics / tunnel-basics）。在相关分支需要背景知识时调用。",
                json!({
                    "type": "object",
                    "required": ["topic"],
                    "properties": {
                        "topic": {"type": "string", "enum": ["offline-auth", "fabric-basics", "tunnel-basics"]}
                    }
                }),
            ),
            ToolDecl::new(
                "submit_spec",
                "提交整理好的开服方案草案（最终交卷）。partial 填已确认信息，questions 列需要玩家回答的问题。",
                submit_schema,
            ),
        ]
    }

    /// 执行一次工具调用。返回要回传给模型的内容（JSON 字符串）。
    /// 提交类工具（submit_spec）在此只做校验，交卷由循环层识别处理。
    async fn execute_tool(&self, call: &ToolCall) -> Result<serde_json::Value, String> {
        let args: serde_json::Value =
            serde_json::from_str(call.function.arguments.trim()).unwrap_or(json!({}));
        match call.function.name.as_str() {
            "probe_environment" => {
                let report = java::probe_environment_report().await;
                Ok(report)
            }
            "check_version_compat" => {
                let mc = args["mc_version"].as_str().unwrap_or_default().to_string();
                let software = args["software"].as_str().unwrap_or("vanilla").to_string();
                let releases = self
                    .deps
                    .known_releases()
                    .await
                    .map_err(|e| e.to_string())?;
                let exists = crate::knowledge::normalize_version(&mc)
                    .map(|v| {
                        releases
                            .iter()
                            .filter_map(|r| crate::knowledge::normalize_version(r).ok())
                            .any(|r| r == v)
                    })
                    .unwrap_or(false);
                let java_major = self.deps.kb.java_major_for(&mc);
                let suggestions = crate::knowledge::suggest_versions(&releases, &mc, 5);
                let report = CompatReport {
                    mc_version: mc,
                    exists,
                    java_major,
                    software,
                    issues: if exists {
                        vec![]
                    } else {
                        vec!["版本不在官方正式版清单中".into()]
                    },
                    suggestions,
                };
                serde_json::to_value(&report).map_err(|e| e.to_string())
            }
            "search_mods" => {
                let query = args["query"].as_str().unwrap_or_default();
                // 别名表优先（L1），未命中再走 Modrinth 检索（L2）
                if let Some(slug) = self.deps.kb.alias_lookup(query) {
                    return Ok(json!({"resolved": true, "slug": slug, "source": "alias_table"}));
                }
                let modrinth = ModrinthClient::new(self.deps.http.clone());
                let hits = modrinth.search(query).await.map_err(|e| e.to_string())?;
                serde_json::to_value(hits).map_err(|e| e.to_string())
            }
            "resolve_mod" => {
                let project = args["project"].as_str().unwrap_or_default();
                let mc = args["mc_version"].as_str().unwrap_or_default();
                let modrinth = ModrinthClient::new(self.deps.http.clone());
                let resolved = modrinth
                    .resolve_mod(project, mc, "fabric")
                    .await
                    .map_err(|e| e.to_string())?;
                serde_json::to_value(&resolved).map_err(|e| e.to_string())
            }
            "load_guide" => {
                let topic = args["topic"].as_str().unwrap_or_default();
                match GUIDES.iter().find(|(t, _)| *t == topic) {
                    Some((_, content)) => Ok(json!({"topic": topic, "content": content})),
                    None => Err(format!("未知指南 topic：{topic}")),
                }
            }
            other => Err(format!("未知工具：{other}")),
        }
    }

    /// 需求理解主循环：返回解析后的草案与对话消息（供 R5 会话落盘）。
    pub async fn run(
        &self,
        user_input: &str,
    ) -> Result<(ServerSpecDraft, Vec<ChatMessage>), AgentError> {
        let mut messages = vec![
            ChatMessage::system(REQUIREMENT_SYSTEM_PROMPT),
            ChatMessage::user(user_input),
        ];
        let tools = self.tool_decls();
        let mut schema_failures = 0;

        for round in 1..=MAX_ROUNDS {
            if self.cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let rate = self.deps.cfg.rate_for(&self.deps.cfg.model.model);
            let resp = self
                .svc
                .chat_traced(
                    &self.task_id,
                    Phase::Requirement,
                    &messages,
                    &tools,
                    self.cancel.clone(),
                    rate,
                )
                .await?;

            // R5 留痕：一轮一次 TraceStep
            self.bus.publish(crate::events::TraceEvent::StepAdded {
                task_id: self.task_id.clone(),
                step: TraceStep {
                    kind: TraceKind::Llm,
                    summary: format!(
                        "第 {round} 轮：{} 个工具调用，文本 {} 字",
                        resp.tool_calls.len(),
                        resp.content.chars().count()
                    ),
                    usage_refs: vec![],
                    at: chrono::Local::now(),
                    detail: Some(json!({ "finish_reason": resp.finish_reason })),
                },
            });

            if resp.tool_calls.is_empty() {
                // 模型只回了文本：把它当澄清话术展示，同时提醒其交卷
                messages.push(ChatMessage::assistant(resp.content.clone()));
                messages.push(ChatMessage::user(
                    "请通过调用 submit_spec 工具提交方案草案（partial + questions）。",
                ));
                continue;
            }

            let mut new_messages = vec![ChatMessage {
                role: "assistant".into(),
                content: if resp.content.is_empty() {
                    None
                } else {
                    Some(resp.content.clone())
                },
                tool_calls: Some(resp.tool_calls.clone()),
                tool_call_id: None,
                name: None,
            }];

            let mut submitted: Option<ServerSpecDraft> = None;
            for call in &resp.tool_calls {
                if call.function.name == "submit_spec" {
                    let parsed: Result<ServerSpecDraft, String> = (|| {
                        let value: serde_json::Value =
                            serde_json::from_str(call.function.arguments.trim())
                                .map_err(|e| format!("参数不是合法 JSON：{e}"))?;
                        // Schema 校验（schemars 派生 → jsonschema 校验，§8.3）
                        let schema = serde_json::to_value(schemars::schema_for!(ServerSpecDraft))
                            .map_err(|e| format!("内部 schema 错误：{e}"))?;
                        jsonschema::validate(&schema, &value)
                            .map_err(|e| format!("schema 校验失败：{e}"))?;
                        serde_json::from_value(value).map_err(|e| format!("反序列化失败：{e}"))
                    })();
                    match parsed {
                        Ok(draft) => {
                            schema_failures = 0;
                            submitted = Some(draft);
                            new_messages.push(ChatMessage::tool(&call.id, "已收到方案草案"));
                        }
                        Err(err) => {
                            schema_failures += 1;
                            new_messages.push(ChatMessage::tool(
                                &call.id,
                                format!("提交被拒绝：{err}。请修正后重新提交。"),
                            ));
                        }
                    }
                } else {
                    let result = self.execute_tool(call).await;
                    let payload = match result {
                        Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()),
                        Err(e) => json!({ "error": e }).to_string(),
                    };
                    new_messages.push(ChatMessage::tool(&call.id, payload));
                }
            }
            messages.extend(new_messages);

            if let Some(draft) = submitted {
                return Ok((draft, messages));
            }
            if schema_failures > MAX_SCHEMA_RETRIES {
                // 降级：放弃结构化提交，转为逐项问答（§8.3）
                return Err(AgentError::Llm(LlmError::Stream(
                    "结构化输出连续校验失败，请改用手动方案输入（plan 命令）".into(),
                )));
            }
        }
        Err(AgentError::MaxRounds(MAX_ROUNDS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AppEvent;
    use crate::llm::{LlmClient, LlmResponse, Usage};
    use rust_decimal::Decimal;
    use std::sync::{Arc, Mutex as StdMutex};

    /// 脚本化 Fake：第一轮回文本，第二轮交卷（§13 集成测试基础）。
    struct ScriptedClient {
        calls: StdMutex<Vec<LlmResponse>>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<LlmResponse>) -> Arc<Self> {
            Arc::new(Self {
                calls: StdMutex::new(responses.into_iter().rev().collect()),
            })
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for ScriptedClient {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolDecl],
            _cancel: CancellationToken,
        ) -> Result<LlmResponse, LlmError> {
            let mut guard = self.calls.lock().unwrap();
            guard
                .pop()
                .ok_or_else(|| LlmError::Stream("脚本耗尽".into()))
        }
    }

    fn resp_text(content: &str) -> LlmResponse {
        LlmResponse {
            content: content.into(),
            tool_calls: vec![],
            usage: Usage::default(),
            finish_reason: Some("stop".into()),
        }
    }

    fn resp_submit() -> LlmResponse {
        let args = serde_json::json!({
            "partial": {"mc_version": "1.21.1", "online_players": 2, "offline_players": 3},
            "questions": [{"topic": "cross_network", "text": "跨网络吗？", "options": ["yes", "no"]}]
        });
        LlmResponse {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                r#type: Some("function".into()),
                function: crate::llm::ToolCallFn {
                    name: "submit_spec".into(),
                    arguments: args.to_string(),
                },
            }],
            usage: Usage::default(),
            finish_reason: Some("tool_calls".into()),
        }
    }

    #[tokio::test]
    async fn 需求环两轮交卷() {
        let bus = EventBus::new();
        let ledger = Arc::new(crate::llm::SpendLedger::new());
        let client = ScriptedClient::new(vec![resp_text("请问几个朋友？"), resp_submit()]);
        let svc = LlmService::with_client(client, "fake", Decimal::ZERO, ledger, bus.clone());
        let deps =
            AgentDeps::new(KnowledgeBase::embedded().unwrap(), AppConfig::default()).unwrap();
        let agent = RequirementAgent::new(&svc, &deps, bus, "t1".into(), CancellationToken::new());

        let (draft, messages) = agent.run("我们想玩暮色森林").await.unwrap();
        assert_eq!(draft.partial.mc_version.as_deref(), Some("1.21.1"));
        assert_eq!(draft.questions.len(), 1);
        assert_eq!(
            messages.len(),
            6,
            "system+user+assistant(文本)+user(催交卷)+assistant(交卷)+tool(确认)"
        );
    }

    #[tokio::test]
    async fn 环形工具调用回传() {
        // resolve_mod 失败（无网络）时错误以 tool 消息回传，不 panic
        let bus = EventBus::new();
        let ledger = Arc::new(crate::llm::SpendLedger::new());
        let client = ScriptedClient::new(vec![resp_text("x")]);
        let svc = LlmService::with_client(client, "fake", Decimal::ZERO, ledger, bus.clone());
        let deps =
            AgentDeps::new(KnowledgeBase::embedded().unwrap(), AppConfig::default()).unwrap();
        let agent = RequirementAgent::new(&svc, &deps, bus, "t2".into(), CancellationToken::new());

        let call = ToolCall {
            id: "c".into(),
            r#type: None,
            function: crate::llm::ToolCallFn {
                name: "load_guide".into(),
                arguments: r#"{"topic": "offline-auth"}"#.into(),
            },
        };
        let out = agent.execute_tool(&call).await.unwrap();
        assert!(out["content"].as_str().unwrap().contains("离线"));
    }

    #[tokio::test]
    async fn trace事件被发布() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let ledger = Arc::new(crate::llm::SpendLedger::new());
        let client = ScriptedClient::new(vec![resp_submit()]);
        let svc = LlmService::with_client(client, "fake", Decimal::ZERO, ledger, bus.clone());
        let deps =
            AgentDeps::new(KnowledgeBase::embedded().unwrap(), AppConfig::default()).unwrap();
        let agent = RequirementAgent::new(&svc, &deps, bus, "t3".into(), CancellationToken::new());
        let _ = agent.run("hi").await.unwrap();

        let mut saw_trace = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Trace(_) = ev {
                saw_trace = true;
            }
        }
        assert!(saw_trace);
    }
}
