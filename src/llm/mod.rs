//! llm：自研 OpenAI 兼容客户端（设计 §8.3，决议 D8/D112）。
//!
//! 不引入 LLM SDK——核心调用编排即课程考察点（R1）。本模块只做"一次对话调用"：
//! SSE 流式解析、tool_calls 增量拼装、限流重试、用量上报；预算守卫与价格换算
//! 在 agent 层完成（需要配置与会话上下文）。

pub mod client;
#[cfg(test)]
pub mod fake;

pub use client::OpenAiCompatClient;

use crate::agent::message::Message;
use crate::events::EventTx;

/// 发给模型的工具声明（来自工具注册表）。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema（schemars 从类型派生）。
    pub parameters: serde_json::Value,
}

/// 一次对话请求。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// 思考模式开关（GLM 系语义：thinking.type；其他兼容端多会忽略）。
    pub thinking: bool,
}

/// 模型请求的一次工具调用（arguments 为原始 JSON 文本，解析交给 agent 层）。
#[derive(Debug, Clone, Default)]
pub struct ToolCallOut {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 模型的助理回复（一轮对话的产物）。
#[derive(Debug, Clone, Default)]
pub struct AssistantReply {
    pub content: Option<String>,
    /// 思考耗时（秒）：上游返回 reasoning 增量时测得；全文不入史（决议 D112）。
    pub reasoning_secs: Option<u64>,
    pub tool_calls: Vec<ToolCallOut>,
    pub finish_reason: Option<String>,
}

/// 上游返回的 token 用量；字段缺失时为 None（诚实计量：记调用次数并标注）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// 一次尝试的结果（重试各计一条，R6 诚实计量）。
#[derive(Debug, Clone)]
pub struct AttemptOutcome {
    pub ok: bool,
    pub usage: Option<ChatUsage>,
    pub duration_ms: u64,
    /// 说明（如"429 限流，第 1 次重试"）。
    pub note: Option<String>,
}

/// 一次成功的对话调用：回复 + 全部尝试记录。
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub reply: AssistantReply,
    pub attempts: Vec<AttemptOutcome>,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP 请求失败：{0}")]
    Http(#[from] reqwest::Error),
    #[error("上游返回错误（HTTP {status}）：{body}")]
    Status { status: u16, body: String },
    #[error("响应协议解析失败：{0}")]
    Protocol(String),
    #[error("调用超时（超过 {limit_secs} 秒），可检查网络或调大超时")]
    Timeout { limit_secs: u64 },
}

/// 调用失败：错误 + 全部尝试记录（重试各计一条，R6 诚实计量要求失败也留痕）。
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct LlmFailure {
    pub error: LlmError,
    pub attempts: Vec<AttemptOutcome>,
}

/// LLM 客户端统一抽象。测试用 `fake::FakeLlm` 实现同一 trait（设计 §13 Loop 级集成）。
#[async_trait::async_trait]
pub trait LlmClient: Send + Sync {
    /// 发起一次对话。`sink` 提供时，文本 / 思考增量以事件流出（R4 流式渲染）。
    ///
    /// 失败时返回 `LlmFailure`，携带全部尝试记录供用量入账（含失败调用）。
    async fn chat(
        &self,
        req: ChatRequest,
        sink: Option<&EventTx>,
    ) -> Result<ChatResponse, LlmFailure>;
}

/// 把内部消息模型转换为 OpenAI Chat 的 wire 格式。
pub fn messages_to_wire(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|msg| match msg {
            Message::System { content } => serde_json::json!({
                "role": "system",
                "content": content,
            }),
            Message::User { content } => serde_json::json!({
                "role": "user",
                "content": content,
            }),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut obj = serde_json::Map::new();
                obj.insert("role".into(), serde_json::Value::from("assistant"));
                match content {
                    Some(text) => {
                        obj.insert("content".into(), serde_json::Value::from(text.as_str()));
                    }
                    None => {
                        obj.insert("content".into(), serde_json::Value::Null);
                    }
                }
                if !tool_calls.is_empty() {
                    let calls: Vec<serde_json::Value> = tool_calls
                        .iter()
                        .map(|call| {
                            serde_json::json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.arguments.to_string(),
                                },
                            })
                        })
                        .collect();
                    obj.insert("tool_calls".into(), serde_json::Value::from(calls));
                }
                serde_json::Value::Object(obj)
            }
            Message::Tool {
                call_id, outcome, ..
            } => {
                let text = match outcome {
                    crate::agent::message::ToolOutcome::Ok { content } => content.clone(),
                    crate::agent::message::ToolOutcome::Err { error } => error.clone(),
                };
                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": text,
                })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::ToolOutcome;

    #[test]
    fn wire_conversion_matches_openai_shape() {
        let msgs = vec![
            Message::system("s"),
            Message::user("u"),
            Message::assistant(
                None,
                vec![crate::agent::message::ToolCall {
                    id: "c1".into(),
                    name: "list_dir".into(),
                    arguments: serde_json::json!({ "path": "." }),
                }],
            ),
            Message::tool_result("c1", "list_dir", ToolOutcome::ok("README.md")),
        ];
        let wire = messages_to_wire(&msgs);
        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[1]["content"], "u");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "list_dir");
        assert_eq!(
            wire[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"."}"#
        );
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "c1");
        assert_eq!(wire[3]["content"], "README.md");
    }
}
