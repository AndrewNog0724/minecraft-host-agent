//! 会话（R5）：完整消息流（含工具调用与结果）JSONL 落盘 + 元数据快照。
//!
//! 每条消息追加一行 `{"ts": …, "msg": …}`，崩溃后可恢复；元数据用于
//! `sessions list` 浏览与 `--continue` 接续最近会话。用量累计同时维护在
//! 内存中（预算守卫用）与账本文件中（全局查询用）。

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::agent::message::Message;

use super::{ensure_dir, new_session_id, now_rfc3339, usage::UsageLedger};

/// 会话元数据快照（`<id>.meta.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    /// 首条用户消息（列表展示用）。
    #[serde(default)]
    pub title: Option<String>,
    pub message_count: u64,
}

/// 一次运行中的会话：内存消息流 + 落盘句柄 + 用量累计。
pub struct Session {
    pub id: String,
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    /// 本会话累计（预算守卫与退出汇总的数据源）。
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_cny: f64,
    jsonl_path: PathBuf,
    meta_path: PathBuf,
    attachments_dir: PathBuf,
    attachment_seq: u64,
}

/// JSONL 中的一行。
#[derive(Serialize, Deserialize)]
struct SessionLine {
    ts: String,
    msg: Message,
}

impl Session {
    /// 新建会话并落盘首条元数据。
    pub fn create(sessions_dir: &Path, data_dir: &Path) -> anyhow::Result<Self> {
        ensure_dir(sessions_dir)?;
        let id = new_session_id();
        let now = now_rfc3339();
        let meta = SessionMeta {
            id: id.clone(),
            created_at: now.clone(),
            updated_at: now,
            title: None,
            message_count: 0,
        };
        let session = Self {
            id: id.clone(),
            meta,
            messages: Vec::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_cny: 0.0,
            jsonl_path: sessions_dir.join(format!("{id}.jsonl")),
            meta_path: sessions_dir.join(format!("{id}.meta.json")),
            attachments_dir: data_dir.join("runtime").join("attachments").join(&id),
            attachment_seq: 0,
        };
        session.persist_meta()?;
        Ok(session)
    }

    /// 从 JSONL 恢复会话（`--continue` / `--resume`）。
    pub fn load(jsonl_path: &Path, data_dir: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(jsonl_path)
            .with_context(|| format!("打开会话文件失败：{}", jsonl_path.display()))?;
        let mut messages = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.context("读取会话文件失败")?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionLine = serde_json::from_str(&line)
                .with_context(|| format!("会话文件行解析失败：{}", jsonl_path.display()))?;
            messages.push(entry.msg);
        }
        let id = jsonl_path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("会话文件名缺少 ID")?
            .to_string();
        let meta_path = jsonl_path.with_extension("meta.json");
        let meta: SessionMeta = match std::fs::read_to_string(&meta_path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| SessionMeta {
                id: id.clone(),
                created_at: String::new(),
                updated_at: String::new(),
                title: None,
                message_count: messages.len() as u64,
            }),
            Err(_) => SessionMeta {
                id: id.clone(),
                created_at: String::new(),
                updated_at: String::new(),
                title: None,
                message_count: messages.len() as u64,
            },
        };
        Ok(Self {
            id: id.clone(),
            meta,
            messages,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_cny: 0.0,
            jsonl_path: jsonl_path.to_path_buf(),
            meta_path,
            attachments_dir: data_dir.join("runtime").join("attachments").join(&id),
            attachment_seq: 0,
        })
    }

    pub fn jsonl_path(&self) -> &Path {
        &self.jsonl_path
    }

    /// 追加一条消息并立即落盘（R5：崩溃可恢复）。
    pub fn push_message(&mut self, msg: Message) -> anyhow::Result<()> {
        if self.meta.title.is_none()
            && let Message::User { content } = &msg
        {
            let title: String = content.chars().take(40).collect();
            self.meta.title = Some(title);
        }
        let line = SessionLine {
            ts: now_rfc3339(),
            msg,
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.jsonl_path)
            .with_context(|| format!("打开会话文件失败：{}", self.jsonl_path.display()))?;
        let mut text = serde_json::to_string(&line).context("序列化会话消息失败")?;
        text.push('\n');
        file.write_all(text.as_bytes())
            .context("写入会话文件失败")?;
        self.messages.push(line.msg);
        self.meta.message_count = self.messages.len() as u64;
        self.meta.updated_at = now_rfc3339();
        self.persist_meta()
    }

    /// 把一次调用的用量并入会话累计（预算守卫数据源；账本落盘由调用方负责）。
    pub fn accumulate_usage(&mut self, input: u64, output: u64, cost_cny: f64) {
        self.total_input_tokens += input;
        self.total_output_tokens += output;
        self.total_cost_cny = (self.total_cost_cny + cost_cny).min(f64::MAX);
    }

    /// 大输出的附件路径（超过阈值的工具结果落盘处，§8.1）。
    pub fn next_attachment_path(&mut self, ext: &str) -> anyhow::Result<PathBuf> {
        ensure_dir(&self.attachments_dir)?;
        self.attachment_seq += 1;
        Ok(self
            .attachments_dir
            .join(format!("output-{:04}.{}", self.attachment_seq, ext)))
    }

    fn persist_meta(&self) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(&self.meta).context("序列化会话元数据失败")?;
        std::fs::write(&self.meta_path, text)
            .with_context(|| format!("写入会话元数据失败：{}", self.meta_path.display()))
    }
}

/// 扫描会话目录，返回按更新时间倒序的 `(meta, jsonl_path)` 列表。
pub fn list_sessions(sessions_dir: &Path) -> anyhow::Result<Vec<(SessionMeta, PathBuf)>> {
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(sessions_dir).context("读取会话目录失败")? {
        let entry = entry.context("读取会话目录失败")?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let meta_path = path.with_extension("meta.json");
        let meta: Option<SessionMeta> = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        let meta = meta.unwrap_or_else(|| SessionMeta {
            id: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            title: None,
            message_count: 0,
        });
        out.push((meta, path));
    }
    sort_by_recency(&mut out);
    Ok(out)
}

/// 按更新时间倒序（新的在前）；同刻时按 ID 倒序兜底，保证顺序稳定。
pub fn sort_by_recency(list: &mut [(SessionMeta, PathBuf)]) {
    list.sort_by(|a, b| {
        b.0.updated_at
            .cmp(&a.0.updated_at)
            .then(b.0.id.cmp(&a.0.id))
    });
}

/// 最近一次会话的 JSONL 路径（`--continue` 用）。
pub fn latest_session(sessions_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    Ok(list_sessions(sessions_dir)?
        .into_iter()
        .next()
        .map(|(_, p)| p))
}

/// 把账本中某会话的用量行并回内存累计（恢复会话时重建预算守卫的累计值）。
pub fn rebuild_totals_from_ledger(
    session: &mut Session,
    ledger: &UsageLedger,
) -> anyhow::Result<()> {
    let summary = ledger.summarize(Some(&session.id))?;
    session.total_input_tokens = summary.input_tokens;
    session.total_output_tokens = summary.output_tokens;
    session.total_cost_cny = summary.cost_cny;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let data = dir.path().join("data");
        (dir, sessions, data)
    }

    #[test]
    fn create_push_reload_roundtrip() {
        let (_dir, sessions, data) = tmp_dirs();
        let mut session = Session::create(&sessions, &data).unwrap();
        session
            .push_message(Message::user("你好，列一下目录"))
            .unwrap();
        session
            .push_message(Message::tool_result(
                "c1",
                "list_dir",
                crate::agent::message::ToolOutcome::ok("README.md"),
            ))
            .unwrap();

        let loaded = Session::load(&sessions.join(format!("{}.jsonl", session.id)), &data).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.meta.title.as_deref(), Some("你好，列一下目录"));
    }

    #[test]
    fn list_orders_by_recency() {
        let (_dir, sessions, data) = tmp_dirs();
        let mut s1 = Session::create(&sessions, &data).unwrap();
        let mut s2 = Session::create(&sessions, &data).unwrap();
        s1.push_message(Message::user("更新时间更早")).unwrap();
        s2.push_message(Message::user("更新时间更晚")).unwrap();
        // 同秒创建时 updated_at 相同无法断言时序，直接覆写磁盘上的元数据
        for (session, updated_at) in [
            (&s1, "2026-09-01T10:00:00+08:00"),
            (&s2, "2026-09-01T10:01:00+08:00"),
        ] {
            let mut meta = session.meta.clone();
            meta.updated_at = updated_at.to_string();
            let path = sessions.join(format!("{}.meta.json", session.id));
            std::fs::write(&path, serde_json::to_string(&meta).unwrap()).unwrap();
        }
        let list = list_sessions(&sessions).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0.id, s2.id);
        assert_eq!(list[1].0.id, s1.id);
    }
}
