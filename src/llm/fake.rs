//! 测试支持：脚本化的假 LLM 客户端（设计 §13 Loop 级集成——不花真钱）。
//!
//! 用法：预置一串 `FakeStep`，Agent Loop 每调用一次 `chat` 就消费一步；
//! 同时记录收到的请求，供测试断言消息流形状与工具声明。

use std::collections::VecDeque;
use std::sync::Mutex;

use super::{
    AssistantReply, AttemptOutcome, ChatRequest, ChatResponse, ChatUsage, LlmClient, LlmError,
    LlmFailure, ToolCallOut,
};
use crate::events::EventTx;

/// 一步脚本：模型的一次回复形态。
#[derive(Debug, Clone)]
pub enum FakeStep {
    /// 纯文本回复（回合自然结束）。
    Text(String),
    /// 只发工具调用（不携带文本）。
    ToolCalls(Vec<(String, serde_json::Value)>),
    /// 文本 + 工具调用。
    TextWithToolCalls(String, Vec<(String, serde_json::Value)>),
    /// 模拟失败（错误文本进入 `LlmError::Protocol`）。
    Fail(String),
}

/// 假客户端：固定用量（可调），按序消费脚本步骤；脚本耗尽视为测试逻辑错误。
pub struct FakeLlm {
    steps: Mutex<VecDeque<FakeStep>>,
    requests: Mutex<Vec<ChatRequest>>,
    usage: ChatUsage,
}

impl FakeLlm {
    pub fn new(steps: impl IntoIterator<Item = FakeStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            usage: ChatUsage {
                input_tokens: Some(100),
                output_tokens: Some(20),
            },
        }
    }

    /// 测试断言用：历次请求快照。
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("测试内互斥锁").clone()
    }

    fn next_step(&self) -> Result<FakeStep, LlmFailure> {
        let mut steps = self.steps.lock().expect("测试内互斥锁");
        match steps.pop_front() {
            Some(step) => Ok(step),
            None => Err(self.fail("脚本耗尽：测试未预期到这次调用")),
        }
    }

    fn fail(&self, message: &str) -> LlmFailure {
        LlmFailure {
            error: LlmError::Protocol(message.to_string()),
            attempts: vec![AttemptOutcome {
                ok: false,
                usage: None,
                duration_ms: 0,
                note: Some(message.to_string()),
            }],
        }
    }
}

#[async_trait::async_trait]
impl LlmClient for FakeLlm {
    async fn chat(
        &self,
        req: ChatRequest,
        _sink: Option<&EventTx>,
    ) -> Result<ChatResponse, LlmFailure> {
        self.requests.lock().expect("测试内互斥锁").push(req);
        let step = self.next_step()?;
        let (reply, ok) = match step {
            FakeStep::Text(text) => (
                AssistantReply {
                    content: Some(text),
                    reasoning_secs: None,
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".to_string()),
                },
                true,
            ),
            FakeStep::ToolCalls(calls) => (
                AssistantReply {
                    content: None,
                    reasoning_secs: None,
                    tool_calls: calls
                        .into_iter()
                        .enumerate()
                        .map(|(index, (name, args))| ToolCallOut {
                            id: format!("fake_call_{index}"),
                            name,
                            arguments: serde_json::to_string(&args)
                                .unwrap_or_else(|_| "{}".to_string()),
                        })
                        .collect(),
                    finish_reason: Some("tool_calls".to_string()),
                },
                true,
            ),
            FakeStep::TextWithToolCalls(text, calls) => (
                AssistantReply {
                    content: Some(text),
                    reasoning_secs: None,
                    tool_calls: calls
                        .into_iter()
                        .enumerate()
                        .map(|(index, (name, args))| ToolCallOut {
                            id: format!("fake_call_{index}"),
                            name,
                            arguments: serde_json::to_string(&args)
                                .unwrap_or_else(|_| "{}".to_string()),
                        })
                        .collect(),
                    finish_reason: Some("tool_calls".to_string()),
                },
                true,
            ),
            FakeStep::Fail(message) => return Err(self.fail(&message)),
        };
        Ok(ChatResponse {
            reply,
            attempts: vec![AttemptOutcome {
                ok,
                usage: Some(self.usage),
                duration_ms: 5,
                note: None,
            }],
        })
    }
}
