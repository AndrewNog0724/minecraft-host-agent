//! 事件总线（R4）：Agent / 工具侧产生事件，渲染器订阅消费。
//!
//! 设计（D107）：渲染器是事件总线的一个订阅者；R4 的实时进度与 R5 的落盘
//! 是同一数据流的两个视图——落盘在消息产生处直接完成（agent / store），
//! 事件只驱动界面，因此用单消费者 mpsc 即可，无消息丢失风险。

use tokio::sync::mpsc::UnboundedSender;

use crate::store::usage::UsageRecord;

/// 会话回合中产生的各类事件（每种事件对应一种语义化渲染块，见设计 §8.6）。
#[derive(Debug, Clone)]
pub enum Event {
    /// 思考（reasoning）流式增量（暗灰斜体）。
    ThinkingDelta(String),
    /// 思考结束，收起为"已思考 Ns"占位。
    ThinkingFinished { seconds: u64 },
    /// 助理文本流式增量（原样直显）。
    TextDelta(String),
    /// 工具开始：`⏺ 工具名(参数摘要)` + spinner。
    ToolStarted { name: String, args_summary: String },
    /// 工具结束：`⎿ ✓/✗ 结果摘要`，并清理对应 spinner / 进度条。
    ToolFinished {
        ok: bool,
        summary: String,
        duration_ms: u64,
    },
    /// 通用进度（下载字节等）：label 标识同一条进度条，total 为 None 时显示 spinner。
    Progress {
        label: String,
        done: u64,
        total: Option<u64>,
    },
    /// 命令输出行等原样滚动内容。
    OutputLine(String),
    /// 空行：板块间呼吸感由需要保证顺序的模块主动插入（如确认门前）。
    Blank,
    /// 一般提示（打断、预算告警、auto 模式留痕等，暗色块）。
    Notice(String),
    /// 一次 LLM 调用的用量入账。D108：会话中不渲染、退出时由 REPL 汇总；
    /// 保留 payload 供未来订阅者使用（如 M2 的会话内预算条）。
    #[allow(dead_code)]
    UsageRecorded(UsageRecord),
}

/// 事件发送端（工具上下文与 Agent 共享）。
pub type EventTx = UnboundedSender<Event>;

/// 创建事件通道：(发送端, 接收端)。
pub fn event_channel() -> (EventTx, tokio::sync::mpsc::UnboundedReceiver<Event>) {
    tokio::sync::mpsc::unbounded_channel()
}

/// 工具调用参数摘要：`key=value` 用逗号连接，整体截断。
pub fn summarize_args(args: &serde_json::Value, max_chars: usize) -> String {
    let text = match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let rendered = match v {
                        serde_json::Value::String(s) => s.replace('\n', " "),
                        other => other.to_string(),
                    };
                    let rendered = crate::agent::message::truncate_chars(&rendered, 60);
                    format!("{k}={rendered}")
                })
                .collect();
            parts.join(", ")
        }
        other => other.to_string(),
    };
    crate::agent::message::truncate_chars(&text, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_summary_formats_scalars() {
        let args = serde_json::json!({ "path": "a/b.txt", "timeout_secs": 30, "flag": true });
        let summary = summarize_args(&args, 200);
        assert!(summary.contains("path=a/b.txt"));
        assert!(summary.contains("timeout_secs=30"));
        assert!(summary.contains("flag=true"));
    }

    #[test]
    fn args_summary_truncates() {
        let args = serde_json::json!({ "command": "x".repeat(500) });
        let summary = summarize_args(&args, 40);
        assert!(summary.chars().count() <= 45);
    }
}
