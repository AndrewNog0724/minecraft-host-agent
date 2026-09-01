//! 上下文窗口管理（R3 的 context_len 落点，设计 §8.1）。
//!
//! M1 用保守的字符近似估算 token（CJK ≈ 1 token/字，ASCII ≈ 4 字符/token），
//! 不引入 tokenizer。裁剪以完整回合（user + assistant + 全部 tool 消息）为
//! 单位，从最老的回合开始丢弃——决议 D109 回合原子性。

use crate::agent::message::Message;

/// 发送请求时为模型输出预留的 token。
const OUTPUT_RESERVE_TOKENS: usize = 4096;
/// 预算下限：即使 context_len 配得很小，也至少保留这么多 token 的输入。
const MIN_BUDGET_TOKENS: usize = 512;

/// 保守字符近似估算 token 数。
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

/// CJK 统一表意文字 / 日文假名 / 谚文 / 全角符号等按 1 token/字计。
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3000..=0x303F   // CJK 符号与标点
            | 0x3040..=0x30FF // 假名
            | 0x3400..=0x4DBF // 扩展 A
            | 0x4E00..=0x9FFF // 基本区
            | 0xAC00..=0xD7AF // 谚文
            | 0xF900..=0xFAFF // 兼容表意
            | 0xFF00..=0xFFEF // 全角形式
    )
}

/// 一条消息的估算 token。
pub fn message_tokens(message: &Message) -> usize {
    const OVERHEAD: usize = 8; // role / 结构开销
    let body = match message {
        Message::System { content } | Message::User { content } => content.as_str(),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut total = content.as_deref().map(estimate_tokens).unwrap_or(0);
            for call in tool_calls {
                total += estimate_tokens(&call.arguments.to_string()) + 16;
            }
            return total + OVERHEAD;
        }
        Message::Tool { outcome, .. } => match outcome {
            crate::agent::message::ToolOutcome::Ok { content } => content.as_str(),
            crate::agent::message::ToolOutcome::Err { error } => error.as_str(),
        },
    };
    estimate_tokens(body) + OVERHEAD
}

/// 裁剪历史：保留能放进预算的最近完整回合（至少保留最后一个回合）。
///
/// system prompt 不在 `messages` 内，由调用方单独拼接（永远保留）。
pub fn trim_context(
    system_tokens: usize,
    messages: &[Message],
    context_len: usize,
) -> Vec<Message> {
    let budget = context_len
        .saturating_sub(system_tokens)
        .saturating_sub(OUTPUT_RESERVE_TOKENS)
        .max(MIN_BUDGET_TOKENS);

    // 回合切分：每个 User 消息开启一个回合，延续到下一个 User 之前
    let mut groups: Vec<(usize, usize)> = Vec::new(); // [start, end)
    let mut current_start: Option<usize> = None;
    for (index, message) in messages.iter().enumerate() {
        if matches!(message, Message::User { .. }) {
            if let Some(start) = current_start.take() {
                groups.push((start, index));
            }
            current_start = Some(index);
        }
    }
    if let Some(start) = current_start {
        groups.push((start, messages.len()));
    }
    // 开头散落的非 User 消息
    let head_end = groups
        .first()
        .map(|(start, _)| *start)
        .unwrap_or(messages.len());
    if head_end > 0 {
        groups.insert(0, (0, head_end));
    }
    if groups.is_empty() {
        return messages.to_vec();
    }

    // 从最近回合向前累计，装得下多少保留多少（最后一个回合无条件保留）
    let group_tokens = |(start, end): (usize, usize)| -> usize {
        messages[start..end].iter().map(message_tokens).sum()
    };
    let mut keep_from = groups.len() - 1;
    let mut acc = group_tokens(groups[keep_from]);
    for index in (0..groups.len() - 1).rev() {
        let tokens = group_tokens(groups[index]);
        if acc + tokens > budget {
            break;
        }
        acc += tokens;
        keep_from = index;
    }
    messages[groups[keep_from].0..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::ToolCall;

    /// 造一个回合：user "qN" + assistant 长文本（`chars` 个 ASCII 字符）。
    /// 估算：user ≈ 9 token，assistant ≈ chars/4 + 8 token。
    fn big_turn(index: usize, chars: usize) -> Vec<Message> {
        vec![
            Message::user(format!("q{index}")),
            Message::assistant(Some("x".repeat(chars)), vec![]),
        ]
    }

    #[test]
    fn estimate_tokens_cjk_vs_ascii() {
        assert_eq!(estimate_tokens("三个字"), 3);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("中文 english"), 2 + 2); // " english" = 8 字符 → 2
    }

    #[test]
    fn trim_keeps_recent_complete_turns() {
        // 3 个回合，每回合 ≈ 517 token（2000 字符 assistant）
        let mut messages = Vec::new();
        for i in 1..=3 {
            messages.extend(big_turn(i, 2000));
        }
        // 预算 600：只装得下最后一个回合
        let trimmed = trim_context(0, &messages, OUTPUT_RESERVE_TOKENS + 600);
        assert_eq!(trimmed.len(), 2);
        assert!(matches!(&trimmed[0], Message::User { content } if content == "q3"));

        // 预算 1100：装得下两个回合
        let trimmed = trim_context(0, &messages, OUTPUT_RESERVE_TOKENS + 1100);
        assert_eq!(trimmed.len(), 4);
        assert!(matches!(&trimmed[0], Message::User { content } if content == "q2"));
    }

    #[test]
    fn trim_never_splits_turns_or_drops_last() {
        let mut messages = Vec::new();
        messages.extend(big_turn(1, 100));
        // 带工具调用的回合：user + assistant(tool_calls) + tool(3000 字符) + assistant
        messages.push(Message::user("q2"));
        messages.push(Message::assistant(
            None,
            vec![ToolCall {
                id: "c1".into(),
                name: "list_dir".into(),
                arguments: serde_json::json!({}),
            }],
        ));
        messages.push(Message::tool_result(
            "c1",
            "list_dir",
            crate::agent::message::ToolOutcome::ok("y".repeat(3000)),
        ));
        messages.push(Message::assistant(Some("done".into()), vec![]));

        // 预算远小于最后回合（>800 token）：仍必须完整保留，不许拆分
        let trimmed = trim_context(0, &messages, OUTPUT_RESERVE_TOKENS + 1);
        assert_eq!(trimmed.len(), 4);
        assert!(matches!(&trimmed[0], Message::User { content } if content == "q2"));
        assert!(matches!(
            &trimmed[2],
            Message::Tool {
                outcome: crate::agent::message::ToolOutcome::Ok { .. },
                ..
            }
        ));
    }

    #[test]
    fn trim_drops_multiple_turns_until_fit() {
        let mut messages = Vec::new();
        // 每回合 ≈ 9 + (4000/4 + 8) = 1017 token
        for i in 1..=10 {
            messages.extend(big_turn(i, 4000));
        }
        // 预算 2500 → 从尾部保留 2 个回合（2034 ≤ 2500 < 3051）
        // （预算必须高于 512 的下限地板才会生效）
        let trimmed = trim_context(0, &messages, OUTPUT_RESERVE_TOKENS + 2500);
        assert_eq!(trimmed.len(), 4);
        assert!(matches!(&trimmed[0], Message::User { content } if content == "q9"));
        assert!(matches!(&trimmed[2], Message::User { content } if content == "q10"));
    }
}
