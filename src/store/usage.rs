//! 用量账本（R6）：每次 LLM 调用一行 JSONL，全局累计。
//!
//! 预算守卫所需的"会话累计费用"由 `Session` 自身维护（见 session.rs），
//! 本模块负责跨会话的全局账本落盘与汇总查询。

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use super::ensure_dir;

/// 一次 LLM API 调用的用量记录（重试也各计一条，`kind` 区分）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub ts: String,
    pub session_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 本次调用费用（元）。无价格预设时为 0 且 `priced = false`。
    pub cost_cny: f64,
    /// false = 无价格预设，仅 token 数（课程 Q9 口径的诚实标注）。
    pub priced: bool,
    /// `chat` 正常调用 / `chat-retry` 限流重试 / `chat-test` 连接测试。
    pub kind: String,
    pub duration_ms: u64,
    /// 补充说明（如"上游未返回 usage，仅计调用次数"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 账本文件封装：追加写 + 全量汇总。
pub struct UsageLedger {
    path: PathBuf,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct LedgerSummary {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_cny: f64,
    /// 有多少条记录因为缺价格预设只统计了 token（cost 记 0）。
    pub unpriced_calls: u64,
    pub sessions: u64,
}

impl UsageLedger {
    pub fn new(data_dir: &Path) -> anyhow::Result<Self> {
        let dir = data_dir.join("usage");
        ensure_dir(&dir)?;
        Ok(Self {
            path: dir.join("usage.jsonl"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &UsageRecord) -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("打开用量账本失败：{}", self.path.display()))?;
        let mut line = serde_json::to_string(record).context("序列化用量记录失败")?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .context("写入用量账本失败")?;
        Ok(())
    }

    /// 扫描账本汇总（账本是追加型小文件，全量扫描足够；`session_id` 传入时只汇总该会话）。
    pub fn summarize(&self, session_id: Option<&str>) -> anyhow::Result<LedgerSummary> {
        let mut summary = LedgerSummary::default();
        let mut sessions = std::collections::BTreeSet::new();
        if !self.path.exists() {
            return Ok(summary);
        }
        let file = std::fs::File::open(&self.path)
            .with_context(|| format!("打开用量账本失败：{}", self.path.display()))?;
        for line in BufReader::new(file).lines() {
            let line = line.context("读取用量账本失败")?;
            if line.trim().is_empty() {
                continue;
            }
            let record: UsageRecord = match serde_json::from_str(&line) {
                Ok(r) => r,
                // 单行损坏不阻断汇总（例如历史版本字段变化），跳过即可
                Err(_) => continue,
            };
            if let Some(sid) = session_id
                && record.session_id != sid
            {
                continue;
            }
            summary.calls += 1;
            summary.input_tokens += record.input_tokens;
            summary.output_tokens += record.output_tokens;
            summary.cost_cny += record.cost_cny;
            if !record.priced {
                summary.unpriced_calls += 1;
            }
            sessions.insert(record.session_id);
        }
        summary.sessions = sessions.len() as u64;
        summary.cost_cny = (summary.cost_cny * 10000.0).round() / 10000.0;
        Ok(summary)
    }
}

/// 按价格表计算单次调用费用；无匹配价格时返回 `None`（费用记 0 并标注）。
pub fn compute_cost(
    price: Option<(f64, f64)>,
    input_tokens: u64,
    output_tokens: u64,
) -> (f64, bool) {
    match price {
        Some((input_per_m, output_per_m)) => {
            let cost = input_tokens as f64 / 1_000_000.0 * input_per_m
                + output_tokens as f64 / 1_000_000.0 * output_per_m;
            ((cost * 10000.0).round() / 10000.0, true)
        }
        None => (0.0, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::now_rfc3339;

    #[test]
    fn cost_computation() {
        // 输入 2 元/M、输出 8 元/M：1M 输入 + 0.5M 输出 = 6 元
        let (cost, priced) = compute_cost(Some((2.0, 8.0)), 1_000_000, 500_000);
        assert_eq!(cost, 6.0);
        assert!(priced);

        let (cost, priced) = compute_cost(None, 100, 100);
        assert_eq!(cost, 0.0);
        assert!(!priced);
    }

    #[test]
    fn ledger_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::new(dir.path()).unwrap();
        let rec = UsageRecord {
            ts: now_rfc3339(),
            session_id: "s1".into(),
            model: "m".into(),
            input_tokens: 10,
            output_tokens: 5,
            cost_cny: 0.01,
            priced: true,
            kind: "chat".into(),
            duration_ms: 120,
            note: None,
        };
        ledger.append(&rec).unwrap();
        ledger.append(&rec).unwrap();
        let summary = ledger.summarize(None).unwrap();
        assert_eq!(summary.calls, 2);
        assert_eq!(summary.input_tokens, 20);
        assert_eq!(summary.sessions, 1);
    }
}
