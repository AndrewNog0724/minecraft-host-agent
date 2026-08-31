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
use crate::knowledge::{CompatReport, JavaMajorSource, KnowledgeBase};
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
    #[error("结构化输出连续校验失败（最后一次：{last_error}）；可改用 `mcha plan` 手动填写方案")]
    SpecSubmitFailed { last_error: String },
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

    /// MC 版本 → 官方最低 Java 大版本（v0.9，"能查就不猜"）：
    /// piston-meta 版本 JSON 的 `javaVersion.majorVersion` 动态优先，
    /// 上游不可达或字段缺失时回落 L1 静态表，并标明口径来源。
    pub async fn java_major_for_version(&self, mc_version: &str) -> (Option<u8>, JavaMajorSource) {
        let mojang = MojangClient::new(self.http.clone());
        let manifest_major = match mojang.version_java_major(mc_version).await {
            Ok(Some(major)) => Some(major),
            Ok(None) => {
                tracing::warn!("版本 {mc_version} 的版本 JSON 缺 javaVersion 字段，回落 L1 静态表");
                None
            }
            Err(e) => {
                tracing::warn!("获取 {mc_version} 的官方 Java 需求失败，回落 L1 静态表：{e}");
                None
            }
        };
        crate::knowledge::resolve_java_major(manifest_major, &self.kb, mc_version)
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
                "核对 MC 版本是否存在（官方清单），并返回该版本所需的 Java 大版本。任何版本号都必须经此工具核实；报告 canonical_version 是官方清单原文 id，写草案时必须原样引用它。",
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
    /// `args` 由调用方解析（决议 D16：解析失败在循环层留痕，不静默兜底）。
    async fn execute_tool(
        &self,
        call: &ToolCall,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
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
                // 规范 id 原则（§8.4 v0.9.6）：存在性判定与"该抄哪个版本号"
                // 统一走 canonicalize_version——报告里直接给清单原文 id，
                // 模型写草案时应原样引用，不得自行改写（如 26.2 → 26.2.0）
                let canonical = crate::knowledge::canonicalize_version(&releases, &mc);
                let exists = canonical.is_some();
                // v0.9：Java 需求以官方动态值为准（L1 表仅离线兜底），口径随报告返回
                let (java_major, java_major_source) = self.deps.java_major_for_version(&mc).await;
                let suggestions = crate::knowledge::suggest_versions(&releases, &mc, 5);
                let report = CompatReport {
                    mc_version: mc,
                    canonical_version: canonical,
                    exists,
                    java_major,
                    java_major_source,
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
    /// 外层负责进度条生命周期（R4/D17），内层 `run_inner` 承载循环本体。
    pub async fn run(
        &self,
        user_input: &str,
    ) -> Result<(ServerSpecDraft, Vec<ChatMessage>), AgentError> {
        self.bus.publish(crate::events::ProgressEvent::StepStarted {
            task_id: self.task_id.clone(),
            step: "requirement".into(),
            title: format!("需求理解中（{}）…", self.deps.cfg.model.model),
        });
        let result = self.run_inner(user_input).await;
        let (ok, detail) = match &result {
            Ok((draft, _)) => (
                true,
                Some(format!(
                    "已收到方案草案（{} 个问题）",
                    draft.questions.len()
                )),
            ),
            Err(e) => (false, Some(format!("需求理解失败：{e}"))),
        };
        self.bus
            .publish(crate::events::ProgressEvent::StepFinished {
                task_id: self.task_id.clone(),
                step: "requirement".into(),
                ok,
                detail,
            });
        result
    }

    /// 流式活动钩子（R4/D17）：接收增量满 200 字上报一次，避免刷爆事件总线。
    fn stream_tick(&self) -> crate::llm::StreamTick {
        use std::sync::atomic::{AtomicU64, Ordering};
        let bus = self.bus.clone();
        let task_id = self.task_id.clone();
        let last = AtomicU64::new(0);
        std::sync::Arc::new(move |received| {
            let prev = last.swap(received, Ordering::Relaxed);
            if received - prev >= 200 {
                bus.publish(crate::events::ProgressEvent::StepProgress {
                    task_id: task_id.clone(),
                    step: "requirement".into(),
                    current: received,
                    total: None,
                    detail: Some(format!("思考中…已接收 {received} 字")),
                });
            }
        })
    }

    async fn run_inner(
        &self,
        user_input: &str,
    ) -> Result<(ServerSpecDraft, Vec<ChatMessage>), AgentError> {
        let mut messages = vec![
            ChatMessage::system(REQUIREMENT_SYSTEM_PROMPT),
            ChatMessage::user(user_input),
        ];
        let tools = self.tool_decls();
        let mut schema_failures = 0;
        let tick = self.stream_tick();

        for round in 1..=MAX_ROUNDS {
            if self.cancel.is_cancelled() {
                // 取消同样落盘对话（v0.9.3：messages.json 对所有退出路径可用）
                self.bus
                    .publish(crate::events::TraceEvent::SessionMessages {
                        task_id: self.task_id.clone(),
                        messages: messages.clone(),
                    });
                return Err(AgentError::Cancelled);
            }
            let rate = self.deps.cfg.rate_for(&self.deps.cfg.model.model);
            let resp = match self
                .svc
                .chat_traced(
                    &self.task_id,
                    Phase::Requirement,
                    &messages,
                    &tools,
                    self.cancel.clone(),
                    rate,
                    Some(tick.clone()),
                )
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    // LLM 级失败（网络/预算/截断）同样留痕对话，排障不再依赖退出方式
                    self.bus
                        .publish(crate::events::TraceEvent::SessionMessages {
                            task_id: self.task_id.clone(),
                            messages: messages.clone(),
                        });
                    return Err(e.into());
                }
            };

            // R5 留痕：一轮一次 TraceStep（含 D16 解析留痕）
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
                    detail: Some(json!({
                        "finish_reason": resp.finish_reason,
                        "notes": resp.notes,
                    })),
                },
            });

            // 进度与直显（R4/D17/D19）：本轮在做什么、模型说了什么。
            // 所有轮次的非空文本都直显——模型惯用"我来核实一下版本…"这类
            // 过程叙述伴随工具调用，只在此类轮次可见，用户才能理解 Agent 在干什么
            let tool_names: Vec<&str> = resp
                .tool_calls
                .iter()
                .map(|c| c.function.name.as_str())
                .collect();
            if !resp.content.trim().is_empty() {
                self.bus.publish(crate::events::ProgressEvent::Notice {
                    task_id: self.task_id.clone(),
                    text: format!("开服管家：{}", resp.content.trim()),
                });
            }
            let round_detail = if tool_names.is_empty() {
                format!("第 {round} 轮：模型输出澄清文本")
            } else {
                format!("第 {round} 轮：调用 {}", tool_names.join("、"))
            };
            self.bus
                .publish(crate::events::ProgressEvent::StepProgress {
                    task_id: self.task_id.clone(),
                    step: "requirement".into(),
                    current: round as u64,
                    total: None,
                    detail: Some(round_detail),
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
                        // v0.9.1：参数整体是 JSON 字符串 = 双重编码。服务商只校验外层
                        // 字符串、内层无任何校验，模型常在字符串里手写坏 JSON——
                        // 此时 schema 只会报"not of type object"，模型无法自查，
                        // 必须给出可执行的重交指令（内层合法的情形已由 llm 层解包）。
                        let value = match value {
                            serde_json::Value::String(inner) => {
                                match serde_json::from_str::<serde_json::Value>(inner.trim()) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        let head: String = inner.chars().take(120).collect();
                                        return Err(format!(
                                            "参数被整体写成了一个 JSON 字符串（双重编码），且字符串内容不是合法 JSON：{e}。\
                                             请重新调用 submit_spec：arguments 直接写 JSON 对象本体，最外层不要加引号；\
                                             顶层只含 partial 与 questions 两个键；布尔用小写 true/false，不要漏冒号漏值；\
                                             不要包含 machine_os 等工具返回字段。你上一次的参数开头：{head}"
                                        ));
                                    }
                                }
                            }
                            other => other,
                        };
                        // 交卷形状规整（v0.8.1/v0.9.3 实测勘误，决议 D16）
                        let value = normalize_draft(value)?;
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
                            // 校验失败三处留痕（决议 D16）：
                            // 模型可修正、终端可见（进度条详情）、轨迹可查（含原始参数头）
                            let args_head: String =
                                call.function.arguments.chars().take(300).collect();
                            self.bus.publish(crate::events::TraceEvent::StepAdded {
                                task_id: self.task_id.clone(),
                                step: TraceStep {
                                    kind: TraceKind::Tool,
                                    summary: format!(
                                        "submit_spec 第 {schema_failures} 次校验失败：{err}"
                                    ),
                                    usage_refs: vec![],
                                    at: chrono::Local::now(),
                                    detail: Some(json!({ "args_head": args_head })),
                                },
                            });
                            self.bus
                                .publish(crate::events::ProgressEvent::StepProgress {
                                    task_id: self.task_id.clone(),
                                    step: "requirement".into(),
                                    current: schema_failures as u64,
                                    total: None,
                                    detail: Some(format!(
                                        "参数校验失败（第 {schema_failures} 次）：{err}"
                                    )),
                                });
                            new_messages.push(ChatMessage::tool(
                                &call.id,
                                format!("提交被拒绝：{err}。请修正后重新提交。"),
                            ));
                        }
                    }
                } else {
                    // 参数解析失败不允许静默按空参数执行（决议 D16）：warn 留痕
                    let args = match serde_json::from_str::<serde_json::Value>(
                        call.function.arguments.trim(),
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                "工具 {} 参数解析失败（按空参数执行）：{e}；原文头 200 字：{}",
                                call.function.name,
                                call.function
                                    .arguments
                                    .chars()
                                    .take(200)
                                    .collect::<String>()
                            );
                            json!({})
                        }
                    };
                    let result = self.execute_tool(call, &args).await;
                    let payload = match result {
                        Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()),
                        Err(e) => json!({ "error": e }).to_string(),
                    };
                    new_messages.push(ChatMessage::tool(&call.id, payload));
                }
            }
            messages.extend(new_messages);

            if let Some(draft) = submitted {
                // 模型标注的待确认问题摘要直显（决议 D17；后续问答由决策树驱动）
                if !draft.questions.is_empty() {
                    let heads: Vec<String> = draft
                        .questions
                        .iter()
                        .take(3)
                        .map(|q| q.text.clone())
                        .collect();
                    self.bus.publish(crate::events::ProgressEvent::Notice {
                        task_id: self.task_id.clone(),
                        text: format!("模型提示待确认：{}", heads.join("；")),
                    });
                }
                self.bus
                    .publish(crate::events::TraceEvent::SessionMessages {
                        task_id: self.task_id.clone(),
                        messages: messages.clone(),
                    });
                return Ok((draft, messages));
            }
            if schema_failures > MAX_SCHEMA_RETRIES {
                // 降级：放弃结构化提交，转为逐项问答（§8.3）；留痕后再失败退出（决议 D16）
                let last_error = format!("共 {schema_failures} 次未通过校验");
                self.bus
                    .publish(crate::events::TraceEvent::SessionMessages {
                        task_id: self.task_id.clone(),
                        messages: messages.clone(),
                    });
                return Err(AgentError::SpecSubmitFailed { last_error });
            }
        }
        self.bus
            .publish(crate::events::TraceEvent::SessionMessages {
                task_id: self.task_id.clone(),
                messages: messages.clone(),
            });
        Err(AgentError::MaxRounds(MAX_ROUNDS))
    }
}

/// 模型交卷形状规整（决议 D16，v0.8.1/v0.9.3 实测勘误）——在 schema 校验前执行：
/// 1. `partial` 字段被写成字符串（内嵌 JSON）→ 解出对象本体；
/// 2. 顶层直接平铺 PartialSpec 字段（缺 `partial` 包装）→ 包一层；
/// 3. questions 元素键名 `question` → 规范名 `text`（实测高频别名）；
/// 4. questions 元素缺 `options`（schema 必填）→ 默认空数组（自由文本问答）。
///
/// 全部是确定性的形状归一；无法修复的形状返回带重交指令的错误。
fn normalize_draft(value: serde_json::Value) -> Result<serde_json::Value, String> {
    const PARTIAL_FIELDS: &[&str] = &[
        "spec_id",
        "online_players",
        "offline_players",
        "account_kind",
        "software",
        "mc_version",
        "mods",
        "cross_network",
        "machine_memory_mb",
        "max_players",
        "extra",
    ];

    // ① partial 字符串化（v0.9.3 实测：{"partial": "{\"spec_id\": ...}"}
    let value = match &value {
        serde_json::Value::Object(map)
            if matches!(map.get("partial"), Some(serde_json::Value::String(_))) =>
        {
            let mut map = map.clone();
            let serde_json::Value::String(inner) = map.remove("partial").unwrap_or_default() else {
                unreachable!("上方 match 已保证 partial 是字符串")
            };
            match serde_json::from_str::<serde_json::Value>(inner.trim()) {
                Ok(v) if v.is_object() => {
                    map.insert("partial".into(), v);
                    serde_json::Value::Object(map)
                }
                Ok(_) => {
                    return Err(
                        "partial 字段被写成了字符串，且内容不是 JSON 对象。请把 partial 写成对象本体（键值对），不要整体加引号"
                            .into(),
                    )
                }
                Err(e) => {
                    return Err(format!(
                        "partial 字段被写成了字符串，且内容不是合法 JSON：{e}。请把 partial 写成对象本体，最外层不要加引号；布尔用小写 true/false，不要漏冒号漏值"
                    ))
                }
            }
        }
        _ => value,
    };

    // ② 顶层平铺字段 → 包一层 partial
    let value = match &value {
        serde_json::Value::Object(map)
            if !map.contains_key("partial")
                && PARTIAL_FIELDS.iter().any(|f| map.contains_key(*f)) =>
        {
            let mut map = map.clone();
            let questions = map.remove("questions").unwrap_or(json!([]));
            json!({ "partial": map, "questions": questions })
        }
        _ => value,
    };

    // ③④ questions 键名别名与 options 缺省
    let mut value = value;
    if let Some(questions) = value.get_mut("questions")
        && let Some(items) = questions.as_array_mut()
    {
        for q in items.iter_mut() {
            if let Some(obj) = q.as_object_mut() {
                if !obj.contains_key("text")
                    && let Some(alias) = obj.remove("question")
                {
                    obj.insert("text".into(), alias);
                }
                if !obj.contains_key("options") {
                    obj.insert("options".into(), json!([]));
                }
            }
        }
    }
    Ok(value)
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
            _on_tick: Option<crate::llm::StreamTick>,
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
            notes: vec![],
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
            notes: vec![],
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
        let args = serde_json::json!({ "topic": "offline-auth" });
        let out = agent.execute_tool(&call, &args).await.unwrap();
        assert!(out["content"].as_str().unwrap().contains("离线"));
    }

    #[test]
    fn 扁平交卷规整为partial包装() {
        let flat = json!({"software":"vanilla","mc_version":"1.21.1","cross_network":null});
        let norm = normalize_draft(flat).unwrap();
        assert!(norm["partial"].is_object());
        assert_eq!(norm["questions"], json!([]));
        assert_eq!(norm["partial"]["mc_version"], json!("1.21.1"));
    }

    #[test]
    fn 正常包装交卷保持不变() {
        let wrapped = json!({
            "partial": {"mc_version": "1.21.1"},
            "questions": [{"topic": "t", "text": "?", "options": ["a"]}]
        });
        assert_eq!(normalize_draft(wrapped.clone()).unwrap(), wrapped);
    }

    #[test]
    fn questions缺options补空数组() {
        let v = json!({"partial": {}, "questions": [{"topic": "t", "text": "?"}]});
        assert_eq!(
            normalize_draft(v).unwrap()["questions"][0]["options"],
            json!([])
        );
    }

    #[test]
    fn partial字符串化解包() {
        // v0.9.3 实测：顶层正常，但 partial 的值是内嵌 JSON 的字符串
        let v = json!({
            "partial": "{\"mc_version\": \"26.2\", \"software\": \"vanilla\"}",
            "questions": []
        });
        let norm = normalize_draft(v).unwrap();
        assert!(norm["partial"].is_object());
        assert_eq!(norm["partial"]["mc_version"], json!("26.2"));
    }

    #[test]
    fn partial字符串化且内容坏_报可执行错误() {
        // v0.9.3 实测延伸：内层手写坏 JSON 时给出带原因的重交指令
        let v = json!({"partial": "{\"mc_version\" \"26.2\"}"});
        let err = normalize_draft(v).unwrap_err();
        assert!(
            err.contains("partial 字段被写成了字符串") && err.contains("最外层不要加引号"),
            "拒绝信息应可执行：{err}"
        );
    }

    #[test]
    fn questions键名question别名规整() {
        // v0.9.3 实测：模型把正文字段写成 question（schema 要求 text）
        let v = json!({
            "partial": {},
            "questions": [{"topic": "cross_network", "question": "跨网络吗？"}]
        });
        let norm = normalize_draft(v).unwrap();
        assert_eq!(norm["questions"][0]["text"], json!("跨网络吗？"));
        assert_eq!(norm["questions"][0]["options"], json!([]));
    }

    #[tokio::test]
    async fn 扁平交卷实测载荷回归() {
        // v0.8.1 实测：模型以顶层平铺字段交卷（双重编码解包在 llm 层，见 llm 单测），
        // agent 层须规整接收——用实测载荷（含 null 字段与幻觉版本号）回归。
        let flat = json!({
            "software": "vanilla",
            "mc_version": "26.2",
            "max_players": 6,
            "cross_network": null,
            "online_players": null,
            "offline_players": null,
            "account_kind": null,
            "machine_memory_mb": null,
            "extra": "Player said 5 friends"
        });
        let mut resp = resp_submit();
        resp.tool_calls[0].function.arguments = flat.to_string();
        let bus = EventBus::new();
        let client = ScriptedClient::new(vec![resp]);
        let svc = LlmService::with_client(
            client,
            "fake",
            Decimal::ZERO,
            Arc::new(crate::llm::SpendLedger::new()),
            bus.clone(),
        );
        let deps =
            AgentDeps::new(KnowledgeBase::embedded().unwrap(), AppConfig::default()).unwrap();
        let agent = RequirementAgent::new(&svc, &deps, bus, "t4".into(), CancellationToken::new());
        let (draft, _) = agent.run("hi").await.unwrap();
        assert_eq!(draft.partial.mc_version.as_deref(), Some("26.2"));
        assert_eq!(draft.partial.max_players, Some(6));
    }

    #[tokio::test]
    async fn 双重编码坏json_拒绝信息可指导修正() {
        // v0.9.1 实测：模型把参数整体写成字符串，且内层手写坏 JSON
        //（缺冒号、大写 False）——拒绝信息必须点名双重编码并给出重交指令
        let bad_inner = r#"{"account_kind":"online", "java_installed"False, "mc_version":"26.2"}"#;
        let mut bad_resp = resp_submit();
        bad_resp.tool_calls[0].function.arguments =
            serde_json::Value::String(bad_inner.into()).to_string();
        let bus = EventBus::new();
        let client = ScriptedClient::new(vec![bad_resp, resp_submit()]);
        let svc = LlmService::with_client(
            client,
            "fake",
            Decimal::ZERO,
            Arc::new(crate::llm::SpendLedger::new()),
            bus.clone(),
        );
        let deps =
            AgentDeps::new(KnowledgeBase::embedded().unwrap(), AppConfig::default()).unwrap();
        let agent = RequirementAgent::new(&svc, &deps, bus, "t5".into(), CancellationToken::new());

        let (draft, messages) = agent.run("hi").await.unwrap();
        assert_eq!(
            draft.partial.mc_version.as_deref(),
            Some("1.21.1"),
            "第二轮修正后的交卷应被接受"
        );
        let rejected = messages
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("call-1"))
            .expect("第一轮坏参数应产生 tool 拒绝消息");
        let text = rejected.content.as_deref().unwrap();
        assert!(text.contains("双重编码"), "拒绝信息应点名双重编码：{text}");
        assert!(
            text.contains("true/false") && text.contains("重"),
            "拒绝信息应含重交指令：{text}"
        );
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
