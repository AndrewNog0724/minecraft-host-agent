//! 会话消息模型（设计 §8.1）。
//!
//! 与 OpenAI Chat 消息同构，是 R5 落盘（JSONL）与上下文裁剪的主体。
//! 任何时刻会话中的消息流都保持结构合法（决议 D109 回合原子性）。

use serde::{Deserialize, Serialize};

/// 工具执行结果：失败也结构化回传，由 Agent（模型）决定下一步（重试 / 换路 / 问用户）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok { content: String },
    Err { error: String },
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutcome::Ok {
            content: content.into(),
        }
    }

    pub fn err(error: impl Into<String>) -> Self {
        ToolOutcome::Err {
            error: error.into(),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, ToolOutcome::Ok { .. })
    }

    /// 渲染 / 摘要用的一行文本。
    pub fn summary(&self, max_chars: usize) -> String {
        let text = match self {
            ToolOutcome::Ok { content } => content.as_str(),
            ToolOutcome::Err { error } => error.as_str(),
        };
        truncate_chars(text, max_chars)
    }
}

/// 一次工具调用（模型发起的"请求"，id 用于与 tool 结果消息配对）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 原始参数（JSON 值；解析失败的情况在进入本类型前已被拦下）。
    pub arguments: serde_json::Value,
}

/// 会话中的一条消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        /// 纯文本部分（模型可能只发工具调用，此时为 None）。
        content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        #[serde(flatten)]
        outcome: ToolOutcome,
    },
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Message::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Message::User {
            content: content.into(),
        }
    }

    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Message::Assistant {
            content,
            tool_calls,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        outcome: ToolOutcome,
    ) -> Self {
        Message::Tool {
            call_id: call_id.into(),
            name: name.into(),
            outcome,
        }
    }

    /// 本条消息携带的工具调用（Assistant 消息才有）。
    pub fn tool_calls(&self) -> &[ToolCall] {
        match self {
            Message::Assistant { tool_calls, .. } => tool_calls,
            _ => &[],
        }
    }
}

/// 按字符数截断（中文按字符计），超长追加省略提示。
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…（截断）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_json_roundtrip() {
        let msgs = vec![
            Message::system("你是助手"),
            Message::user("列一下目录"),
            Message::assistant(
                None,
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "list_dir".into(),
                    arguments: serde_json::json!({ "path": "." }),
                }],
            ),
            Message::tool_result("call_1", "list_dir", ToolOutcome::ok("README.md")),
        ];
        let json = serde_json::to_string(&msgs).unwrap();
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msgs);
    }

    #[test]
    fn truncate_keeps_char_boundary() {
        let s = "一二三四五";
        assert_eq!(truncate_chars(s, 3), "一二三…（截断）");
        assert_eq!(truncate_chars(s, 5), s);
    }
}
