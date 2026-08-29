//! OpenAI 兼容 LLM 薄客户端（§8.3，决议 D8：不引入 LLM SDK）。
//!
//! 职责：Chat Completions 调用（SSE 流式）、工具调用协议、
//! 用量计量钩子（R6）与预算守卫（超限中断）。
//! 结构化输出校验与重试在 agent 模块完成。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::events::{EventBus, Phase, UsageRecord};

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("网络请求失败：{0}")]
    Http(#[from] reqwest::Error),
    #[error("API 返回错误（HTTP {status}）：{message}")]
    Api { status: u16, message: String },
    #[error("响应流解析失败：{0}")]
    Stream(String),
    #[error(
        "预算已耗尽（已花 {spent}，上限 {limit}），任务中断。可在配置中调整 [budget] limit 后重试"
    )]
    BudgetExceeded { spent: Decimal, limit: Decimal },
    #[error("任务已被用户取消")]
    Cancelled,
}

/// 一条对话消息（工具调用协议四角色）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// assistant 消息携带的工具调用请求
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// tool 角色消息关联的调用 id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
            name: None,
        }
    }
}

/// 模型发起的一次工具调用请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    /// 兼容不同上游：id 可能放在顶层或 function 内
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub function: ToolCallFn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFn {
    pub name: String,
    /// JSON 字符串形式的参数
    pub arguments: String,
}

/// 工具声明：name + 描述 + JSON Schema 参数（由 schemars 从类型派生）。
#[derive(Debug, Clone, Serialize)]
pub struct ToolDecl {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDecl {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    fn to_request_json(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// 一次调用的用量（R6 原始数据）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 上游是否报告了真实 token 数
    pub reported: bool,
}

/// LLM 响应：文本 + 工具调用 + 用量。
#[derive(Debug, Clone, Default)]
pub struct LlmResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// 模型自报的结束原因（stop / tool_calls / length）
    pub finish_reason: Option<String>,
}

/// 客户端抽象：生产用 OpenAI 兼容实现，测试用 Fake（§13）。
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDecl],
        cancel: CancellationToken,
    ) -> Result<LlmResponse, LlmError>;
}

// ---------------------------------------------------------------------------
// OpenAI 兼容实现（SSE 流式）
// ---------------------------------------------------------------------------

pub struct OpenAiCompatClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
    thinking: bool,
}

/// SSE 流式响应的分片结构（OpenAI 兼容格式）。
#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<ChunkChoice>,
    usage: Option<ApiUsage>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    delta: Option<DeltaPayload>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeltaPayload {
    content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    index: usize,
    id: Option<String>,
    function: Option<DeltaToolFn>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolFn {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

impl OpenAiCompatClient {
    pub fn from_config(cfg: &AppConfig, api_key: String) -> Result<Self, LlmError> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.model.timeout_secs));
        if !cfg.network.proxy.is_empty() {
            let proxy = reqwest::Proxy::all(&cfg.network.proxy)
                .map_err(|e| LlmError::Stream(format!("代理配置非法：{e}")))?;
            builder = builder.proxy(proxy);
        }
        Ok(Self {
            http: builder.build()?,
            endpoint: cfg.model.endpoint.trim_end_matches('/').to_string(),
            model: cfg.model.model.clone(),
            api_key,
            thinking: cfg.model.thinking,
        })
    }

    fn build_body(&self, messages: &[ChatMessage], tools: &[ToolDecl]) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            // OpenAI 兼容：最后一个 chunk 携带 usage
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            let decls: Vec<_> = tools.iter().map(|t| t.to_request_json()).collect();
            body["tools"] = serde_json::Value::Array(decls);
        }
        if self.thinking {
            // GLM 系思考模式；其它端点默认关闭不受影响
            body["thinking"] = serde_json::json!({ "type": "enabled" });
        }
        body
    }

    /// 解析 SSE 流，边收边累积 content / tool_calls / usage。
    async fn consume_stream(
        &self,
        body: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<LlmResponse, LlmError> {
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.endpoint))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                message: text,
            });
        }

        let byte_stream = resp.bytes_stream();
        let mut es = eventsource_stream::EventStream::new(byte_stream);

        let mut content = String::new();
        let mut builders: Vec<Option<ToolCall>> = Vec::new();
        let mut usage = Usage::default();
        let mut finish_reason: Option<String> = None;

        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                ev = es.next() => ev,
            };
            let Some(event) = event else { break };
            let event = event.map_err(|e| LlmError::Stream(format!("SSE 事件读取失败：{e}")))?;
            if event.data.trim() == "[DONE]" {
                break;
            }
            let chunk: StreamChunk = match serde_json::from_str(&event.data) {
                Ok(c) => c,
                // 个别上游会发注释或空行，跳过无法解析的块
                Err(_) => continue,
            };
            if let Some(u) = chunk.usage {
                usage = Usage {
                    input_tokens: u.prompt_tokens.unwrap_or(0),
                    output_tokens: u.completion_tokens.unwrap_or(0),
                    reported: u.prompt_tokens.is_some() || u.completion_tokens.is_some(),
                };
            }
            for choice in chunk.choices {
                if let Some(reason) = choice.finish_reason {
                    finish_reason = Some(reason);
                }
                let Some(delta) = choice.delta else { continue };
                if let Some(text) = delta.content {
                    content.push_str(&text);
                }
                if let Some(calls) = delta.tool_calls {
                    for call in calls {
                        let slot = match builders.get_mut(call.index) {
                            Some(slot) => slot,
                            None => {
                                // 按 index 稀疏到达时补齐中间空位
                                while builders.len() <= call.index {
                                    builders.push(None);
                                }
                                builders.get_mut(call.index).expect("上方已补齐长度")
                            }
                        };
                        let slot = slot.get_or_insert_with(|| ToolCall {
                            id: String::new(),
                            r#type: Some("function".into()),
                            function: ToolCallFn {
                                name: String::new(),
                                arguments: String::new(),
                            },
                        });
                        if let Some(id) = call.id {
                            slot.id = id;
                        }
                        if let Some(f) = call.function {
                            if let Some(name) = f.name
                                && !name.is_empty()
                            {
                                slot.function.name = name;
                            }
                            if let Some(args) = f.arguments {
                                slot.function.arguments.push_str(&args);
                            }
                        }
                    }
                }
            }
        }

        let tool_calls = builders.into_iter().flatten().collect();
        Ok(LlmResponse {
            content,
            tool_calls,
            usage,
            finish_reason,
        })
    }
}

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDecl],
        cancel: CancellationToken,
    ) -> Result<LlmResponse, LlmError> {
        let body = self.build_body(messages, tools);
        // 网络级瞬时失败重试一次（NFR-3）；取消随时生效
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.consume_stream(body.clone(), cancel.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(LlmError::Cancelled) => return Err(LlmError::Cancelled),
                Err(e) if attempt < 2 && is_transient(&e) => {
                    tracing::warn!("LLM 请求瞬时失败（第 {attempt} 次），重试：{e}");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn is_transient(e: &LlmError) -> bool {
    match e {
        LlmError::Http(_) => true,
        LlmError::Api { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// 带预算守卫与用量上报的服务层（R6 核心落点）
// ---------------------------------------------------------------------------

/// 累计花费：跨调用共享（同一任务内）。
#[derive(Default)]
pub struct SpendLedger {
    total: Mutex<Decimal>,
}

impl SpendLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn total(&self) -> Decimal {
        *self
            .total
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn add(&self, cost: Decimal) {
        let mut total = self
            .total
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *total += cost;
    }
}

/// 包装 LlmClient：调用前查预算，调用后强制生成 UsageRecord 并广播（§8.3）。
pub struct LlmService {
    client: Arc<dyn LlmClient>,
    model_name: String,
    budget_limit: Decimal,
    ledger: Arc<SpendLedger>,
    bus: EventBus,
}

impl LlmService {
    pub fn new(cfg: &AppConfig, bus: EventBus, ledger: Arc<SpendLedger>) -> Result<Self, LlmError> {
        let api_key = cfg.api_key().unwrap_or_default();
        let client = OpenAiCompatClient::from_config(cfg, api_key)?;
        Ok(Self {
            client: Arc::new(client),
            model_name: cfg.model.model.clone(),
            budget_limit: cfg.budget.limit,
            ledger,
            bus,
        })
    }

    /// 测试（§13 Fake 注入）与离线演示用：替换底层客户端实现。
    #[allow(dead_code)]
    pub fn with_client(
        client: Arc<dyn LlmClient>,
        model_name: impl Into<String>,
        budget_limit: Decimal,
        ledger: Arc<SpendLedger>,
        bus: EventBus,
    ) -> Self {
        Self {
            client,
            model_name: model_name.into(),
            budget_limit,
            ledger,
            bus,
        }
    }

    /// 预算守卫：累计花费已达上限即拒绝调用（NFR-4 由 Rust 侧强制）。
    fn check_budget(&self) -> Result<(), LlmError> {
        if self.budget_limit > Decimal::ZERO {
            let spent = self.ledger.total();
            if spent >= self.budget_limit {
                return Err(LlmError::BudgetExceeded {
                    spent,
                    limit: self.budget_limit,
                });
            }
        }
        Ok(())
    }

    /// 带计量的一次调用：agent 模块的所有 LLM 调用都走这里。
    pub async fn chat_traced(
        &self,
        task_id: &str,
        phase: Phase,
        messages: &[ChatMessage],
        tools: &[ToolDecl],
        cancel: CancellationToken,
        rate: Option<crate::config::PriceEntry>,
    ) -> Result<LlmResponse, LlmError> {
        self.check_budget()?;
        let resp = self.client.chat(messages, tools, cancel).await?;

        let (cost, _priced) = match &rate {
            Some(r) if resp.usage.reported => {
                let in_cost = Decimal::from(resp.usage.input_tokens) * r.input_per_m
                    / Decimal::from(1_000_000u32);
                let out_cost = Decimal::from(resp.usage.output_tokens) * r.output_per_m
                    / Decimal::from(1_000_000u32);
                (in_cost + out_cost, true)
            }
            _ => (Decimal::ZERO, false),
        };
        self.ledger.add(cost);

        let record = UsageRecord {
            call_id: uuid::Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            at: chrono::Local::now(),
            model: self.model_name.clone(),
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            cost,
            phase,
            usage_reported: resp.usage.reported,
        };
        self.bus.publish(record);
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 消息序列化形状正确() {
        let msgs = vec![
            ChatMessage::system("s"),
            ChatMessage::user("u"),
            ChatMessage::tool("call-1", "result"),
        ];
        let json = serde_json::to_string(&msgs).unwrap();
        assert!(json.contains("\"role\":\"tool\""));
        assert!(json.contains("\"tool_call_id\":\"call-1\""));
    }

    #[test]
    fn 预算守卫超限拒绝() {
        // 由 LlmService 行为覆盖，这里验证错误信息可读
        let e = LlmError::BudgetExceeded {
            spent: Decimal::from(10),
            limit: Decimal::from(5),
        };
        assert!(e.to_string().contains("预算"));
    }

    struct DummyClient;

    #[async_trait]
    impl LlmClient for DummyClient {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolDecl],
            _cancel: CancellationToken,
        ) -> Result<LlmResponse, LlmError> {
            Ok(LlmResponse {
                content: "ok".into(),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 500_000,
                    reported: true,
                },
                finish_reason: Some("stop".into()),
            })
        }
    }

    #[tokio::test]
    async fn 计量与预算累计() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let ledger = Arc::new(SpendLedger::new());
        let rate = crate::config::PriceEntry {
            model: "m".into(),
            input_per_m: Decimal::from(2),
            output_per_m: Decimal::from(8),
            currency: "CNY".into(),
        };
        let svc = LlmService::with_client(
            Arc::new(DummyClient),
            "m",
            Decimal::ZERO,
            ledger.clone(),
            bus.clone(),
        );

        let resp = svc
            .chat_traced(
                "t1",
                Phase::Chat,
                &[ChatMessage::user("hi")],
                &[],
                CancellationToken::new(),
                Some(rate),
            )
            .await
            .unwrap();
        assert_eq!(resp.content, "ok");
        // 1M * 2 + 0.5M * 8 = 6
        assert_eq!(ledger.total(), Decimal::from(6));

        let record = match rx.recv().await.unwrap() {
            crate::events::AppEvent::Usage(u) => u,
            other => panic!("应收到 Usage 事件，实际 {other:?}"),
        };
        assert_eq!(record.input_tokens, 1_000_000);
        assert_eq!(record.cost, Decimal::from(6));
    }

    #[tokio::test]
    async fn 超预算中断() {
        let bus = EventBus::new();
        let ledger = Arc::new(SpendLedger::new());
        ledger.add(Decimal::from(5));
        let svc =
            LlmService::with_client(Arc::new(DummyClient), "m", Decimal::from(5), ledger, bus);
        let err = svc
            .chat_traced("t", Phase::Chat, &[], &[], CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::BudgetExceeded { .. }));
    }
}
