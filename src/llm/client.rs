//! OpenAI 兼容 Chat API 客户端：SSE 流式、tool_calls 增量拼装、限流重试（D112）。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;

use super::{
    AssistantReply, AttemptOutcome, ChatRequest, ChatResponse, ChatUsage, LlmClient, LlmError,
    LlmFailure, ToolCallOut, messages_to_wire,
};
use crate::agent::message::truncate_chars;
use crate::events::{Event, EventTx};

/// 连接超时（设计 §8.3：连接 15s）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 请求发出到响应头到达的窗口。
const HEADER_TIMEOUT: Duration = Duration::from_secs(20);
/// 单次调用整体超时（设计 §8.3：300s，可配留作后续）。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(300);
/// 429 / 5xx 指数退避重试上限（设计 §8.3：≤2 次）。
const MAX_RETRIES: u32 = 2;

pub struct OpenAiCompatClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl OpenAiCompatClient {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent("mcha/0.2")
            .build()?;
        Ok(Self {
            http,
            endpoint: endpoint.into(),
            api_key: api_key.into(),
        })
    }

    /// 组装请求体。`thinking` 字段仅对智谱系端点显式发送（其他兼容端遇到
    /// 未知参数可能报 400，因此关闭思考时不发送该字段）。
    fn build_body(&self, req: &ChatRequest) -> Value {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), Value::from(req.model.clone()));
        body.insert(
            "messages".into(),
            Value::Array(messages_to_wire(&req.messages)),
        );
        body.insert("stream".into(), Value::from(true));
        body.insert(
            "stream_options".into(),
            serde_json::json!({ "include_usage": true }),
        );
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|spec| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": spec.name,
                            "description": spec.description,
                            "parameters": spec.parameters,
                        },
                    })
                })
                .collect();
            body.insert("tools".into(), Value::from(tools));
        }
        let is_glm = self.endpoint.contains("bigmodel");
        if req.thinking || is_glm {
            body.insert(
                "thinking".into(),
                serde_json::json!({ "type": if req.thinking { "enabled" } else { "disabled" } }),
            );
        }
        Value::Object(body)
    }

    /// 单次尝试：发请求、分流 SSE / 整段 JSON。
    async fn attempt_once(
        &self,
        req: &ChatRequest,
        sink: Option<&EventTx>,
    ) -> Result<(AssistantReply, Option<ChatUsage>), AttemptFailure> {
        let url = format!("{}/chat/completions", self.endpoint.trim_end_matches('/'));
        let body = self.build_body(req);
        let response = match tokio::time::timeout(
            HEADER_TIMEOUT,
            self.http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send(),
        )
        .await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(err)) => {
                return Err(AttemptFailure::Retryable(
                    LlmError::Http(err),
                    "连接失败".to_string(),
                ));
            }
            Err(_) => {
                return Err(AttemptFailure::Fatal(
                    LlmError::Timeout {
                        limit_secs: HEADER_TIMEOUT.as_secs(),
                    },
                    "响应头超时".to_string(),
                ));
            }
        };

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let code = status.as_u16();
            let err = LlmError::Status {
                status: code,
                body: truncate_chars(&text, 500),
            };
            if code == 429 || code >= 500 {
                return Err(AttemptFailure::Retryable(err, format!("HTTP {code}")));
            }
            return Err(AttemptFailure::Fatal(err, format!("HTTP {code}")));
        }

        let is_sse = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));
        if is_sse {
            self.read_stream(response, sink).await
        } else {
            // 个别兼容端忽略 stream 参数，退化为整段 JSON
            self.read_full_json(response).await
        }
    }

    /// SSE 流式读取与增量拼装。
    async fn read_stream(
        &self,
        response: reqwest::Response,
        sink: Option<&EventTx>,
    ) -> Result<(AssistantReply, Option<ChatUsage>), AttemptFailure> {
        let stream = response.bytes_stream().eventsource();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut reasoning_started: Option<Instant> = None;
        let mut reasoning_secs: Option<u64> = None;
        let mut calls: BTreeMap<u32, ToolCallOut> = BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<ChatUsage> = None;

        let read_all = async {
            let mut stream = std::pin::pin!(stream);
            while let Some(event) = stream.next().await {
                let event = event.map_err(|err| {
                    AttemptFailure::Fatal(
                        LlmError::Protocol(format!("SSE 流错误：{err}")),
                        "SSE 流中断".to_string(),
                    )
                })?;
                let data = event.data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    break;
                }
                let chunk: Value = serde_json::from_str(data).map_err(|err| {
                    AttemptFailure::Fatal(
                        LlmError::Protocol(format!("SSE 数据非合法 JSON：{err}")),
                        "SSE 数据异常".to_string(),
                    )
                })?;
                if chunk.get("usage").is_some_and(|u| !u.is_null()) {
                    usage = Some(parse_usage(&chunk["usage"]));
                }
                let Some(choice) = chunk["choices"].get(0) else {
                    continue;
                };
                if let Some(reason) = choice["finish_reason"].as_str() {
                    finish_reason = Some(reason.to_string());
                }
                let delta = &choice["delta"];
                let reasoning_piece = delta["reasoning_content"]
                    .as_str()
                    .or_else(|| delta["reasoning"].as_str());
                if let Some(piece) = reasoning_piece {
                    if reasoning_started.is_none() {
                        reasoning_started = Some(Instant::now());
                    }
                    reasoning.push_str(piece);
                    if let Some(tx) = sink {
                        let _ = tx.send(Event::ThinkingDelta(piece.to_string()));
                    }
                } else if !reasoning.is_empty() && reasoning_secs.is_none() {
                    // 思考结束（切换到正文 / 工具调用）：收起计时
                    reasoning_secs = reasoning_started.map(|t| t.elapsed().as_secs());
                }
                if let Some(piece) = delta["content"].as_str() {
                    content.push_str(piece);
                    if let Some(tx) = sink {
                        let _ = tx.send(Event::TextDelta(piece.to_string()));
                    }
                }
                if let Some(fragments) = delta["tool_calls"].as_array() {
                    for fragment in fragments {
                        let index = fragment["index"].as_u64().unwrap_or(0) as u32;
                        let entry = calls.entry(index).or_default();
                        if let Some(id) = fragment["id"].as_str() {
                            entry.id = id.to_string();
                        }
                        if let Some(name) = fragment["function"]["name"].as_str() {
                            entry.name = name.to_string();
                        }
                        if let Some(args) = fragment["function"]["arguments"].as_str() {
                            entry.arguments.push_str(args);
                        }
                    }
                }
            }
            Ok::<(), AttemptFailure>(())
        };

        match tokio::time::timeout(TOTAL_TIMEOUT, read_all).await {
            Err(_) => {
                return Err(AttemptFailure::Fatal(
                    LlmError::Timeout {
                        limit_secs: TOTAL_TIMEOUT.as_secs(),
                    },
                    "流式读取整体超时".to_string(),
                ));
            }
            Ok(Err(failure)) => return Err(failure),
            Ok(Ok(())) => {}
        }

        if reasoning_secs.is_none() {
            reasoning_secs = reasoning_started.map(|t| t.elapsed().as_secs());
        }
        if !reasoning.is_empty()
            && let Some(tx) = sink
        {
            let _ = tx.send(Event::ThinkingFinished {
                seconds: reasoning_secs.unwrap_or(0),
            });
        }

        let reply = AssistantReply {
            content: (!content.is_empty()).then_some(content),
            reasoning_secs,
            tool_calls: calls.into_values().collect(),
            finish_reason,
        };
        Ok((reply, usage))
    }

    /// 整段 JSON 读取（非流式退化路径）。
    async fn read_full_json(
        &self,
        response: reqwest::Response,
    ) -> Result<(AssistantReply, Option<ChatUsage>), AttemptFailure> {
        let value: Value = tokio::time::timeout(TOTAL_TIMEOUT, response.json())
            .await
            .map_err(|_| {
                AttemptFailure::Fatal(
                    LlmError::Timeout {
                        limit_secs: TOTAL_TIMEOUT.as_secs(),
                    },
                    "整段读取超时".to_string(),
                )
            })?
            .map_err(|err| {
                AttemptFailure::Fatal(
                    LlmError::Protocol(format!("响应非合法 JSON：{err}")),
                    "响应解析失败".to_string(),
                )
            })?;

        let usage = value.get("usage").filter(|u| !u.is_null()).map(parse_usage);
        let message = &value["choices"][0]["message"];
        let tool_calls: Vec<ToolCallOut> = message["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| ToolCallOut {
                        id: call["id"].as_str().unwrap_or_default().to_string(),
                        name: call["function"]["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        arguments: call["function"]["arguments"]
                            .as_str()
                            .unwrap_or("{}")
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let reply = AssistantReply {
            content: message["content"].as_str().map(str::to_string),
            reasoning_secs: None,
            tool_calls,
            finish_reason: value["choices"][0]["finish_reason"]
                .as_str()
                .map(str::to_string),
        };
        Ok((reply, usage))
    }
}

/// 单次尝试的失败分类：可重试（429 / 5xx / 连接问题）或致命。均带原因说明。
enum AttemptFailure {
    Retryable(LlmError, String),
    Fatal(LlmError, String),
}

fn parse_usage(value: &Value) -> ChatUsage {
    ChatUsage {
        input_tokens: value["prompt_tokens"].as_u64(),
        output_tokens: value["completion_tokens"].as_u64(),
    }
}

#[async_trait::async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn chat(
        &self,
        req: ChatRequest,
        sink: Option<&EventTx>,
    ) -> Result<ChatResponse, LlmFailure> {
        let mut attempts: Vec<AttemptOutcome> = Vec::new();
        let mut retry_count = 0u32;
        loop {
            let started = Instant::now();
            match self.attempt_once(&req, sink).await {
                Ok((reply, usage)) => {
                    attempts.push(AttemptOutcome {
                        ok: true,
                        usage,
                        duration_ms: started.elapsed().as_millis() as u64,
                        note: (retry_count > 0).then(|| format!("第 {retry_count} 次重试成功")),
                    });
                    return Ok(ChatResponse { reply, attempts });
                }
                Err(AttemptFailure::Fatal(error, reason)) => {
                    attempts.push(AttemptOutcome {
                        ok: false,
                        usage: None,
                        duration_ms: started.elapsed().as_millis() as u64,
                        note: Some(format!("失败（{reason}）")),
                    });
                    return Err(LlmFailure { error, attempts });
                }
                Err(AttemptFailure::Retryable(error, reason)) => {
                    attempts.push(AttemptOutcome {
                        ok: false,
                        usage: None,
                        duration_ms: started.elapsed().as_millis() as u64,
                        note: Some(format!("失败（{reason}）")),
                    });
                    if retry_count >= MAX_RETRIES {
                        return Err(LlmFailure { error, attempts });
                    }
                    retry_count += 1;
                    // 指数退避：1s、2s
                    tokio::time::sleep(Duration::from_millis(1000 << (retry_count - 1))).await;
                }
            }
        }
    }
}
