//! store：档案 / 会话 / 用量的持久化与查询（R5/R6，§8.7）。
//!
//! 数据目录布局：
//! ```text
//! <数据目录>/
//! ├── profiles/<spec_id>/spec.json   开服配置档案
//! ├── sessions/<task_id>.json        任务完整轨迹
//! ├── sessions/<task_id>.events.jsonl 事件流（进度/用量原文）
//! └── usage/usage.jsonl              全局用量追加日志
//! ```
//! 导出会话 = 打包上述三类文件（自动打码，NFR-2）。

use std::io::Write as _;
use std::path::PathBuf;

use thiserror::Error;

use crate::events::{TaskStatus, TaskTrace, UsageRecord};
use crate::spec::ServerSpec;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("创建目录 {path} 失败：{source}")]
    Mkdir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("读取 {path} 失败：{source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("写入 {path} 失败：{source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("解析 {path} 失败：{source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("会话 {0} 不存在")]
    SessionNotFound(String),
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    /// 以默认数据目录构造。
    pub fn open() -> Result<Self, StoreError> {
        Self::open_at(crate::config::data_dir())
    }

    pub fn open_at(root: PathBuf) -> Result<Self, StoreError> {
        for sub in ["profiles", "sessions", "usage"] {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir)
                .map_err(|source| StoreError::Mkdir { path: dir, source })?;
        }
        Ok(Self { root })
    }

    fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    fn usage_path(&self) -> PathBuf {
        self.root.join("usage").join("usage.jsonl")
    }

    // ── 档案（R5：配置可保存/加载）────────────────────────────────

    /// 保存开服档案：profiles/<spec_id>/spec.json。
    pub fn save_profile(&self, spec: &ServerSpec) -> Result<PathBuf, StoreError> {
        let dir = self.profiles_dir().join(&spec.spec_id);
        std::fs::create_dir_all(&dir).map_err(|source| StoreError::Mkdir {
            path: dir.clone(),
            source,
        })?;
        let path = dir.join("spec.json");
        let json = serde_json::to_string_pretty(spec).map_err(|source| StoreError::Parse {
            path: path.clone(),
            source,
        })?;
        std::fs::write(&path, json).map_err(|source| StoreError::Write {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// 加载档案。
    pub fn load_profile(&self, spec_id: &str) -> Result<ServerSpec, StoreError> {
        let path = self.profiles_dir().join(spec_id).join("spec.json");
        let raw = std::fs::read_to_string(&path).map_err(|source| StoreError::Read {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str(&raw).map_err(|source| StoreError::Parse { path, source })
    }

    /// 列出全部档案（spec_id + 摘要）。
    pub fn list_profiles(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.profiles_dir()) else {
            return out;
        };
        for entry in entries.flatten() {
            let spec_path = entry.path().join("spec.json");
            let Ok(raw) = std::fs::read_to_string(&spec_path) else {
                continue;
            };
            if let Ok(spec) = serde_json::from_str::<ServerSpec>(&raw) {
                out.push((
                    spec.spec_id.clone(),
                    format!("MC {} / {} 玩家", spec.mc_version, spec.max_players),
                    spec.created_at.format("%Y-%m-%d %H:%M").to_string(),
                ));
            }
        }
        out.sort();
        out
    }

    // ── 会话轨迹（R5：任务非黑盒）────────────────────────────────

    /// 保存任务轨迹（覆盖写快照）。
    pub fn save_trace(&self, trace: &TaskTrace) -> Result<PathBuf, StoreError> {
        let path = self.sessions_dir().join(format!("{}.json", trace.task_id));
        let json = serde_json::to_string_pretty(trace).map_err(|source| StoreError::Parse {
            path: path.clone(),
            source,
        })?;
        std::fs::write(&path, json).map_err(|source| StoreError::Write {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// 保存需求理解环的完整对话消息（决议 D16：messages 为已序列化的 JSON 数组，
    /// 失败留痕与成功归档共用；R5"非黑盒"的对话原文层）。
    pub fn save_messages(
        &self,
        task_id: &str,
        messages: &serde_json::Value,
    ) -> Result<PathBuf, StoreError> {
        let path = self.sessions_dir().join(format!("{task_id}.messages.json"));
        let json = serde_json::to_string_pretty(messages).map_err(|source| StoreError::Parse {
            path: path.clone(),
            source,
        })?;
        std::fs::write(&path, json).map_err(|source| StoreError::Write {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// 追加一行事件到该任务的 events.jsonl（进度/用量原文留痕）。
    pub fn append_event(&self, task_id: &str, event: &serde_json::Value) -> Result<(), StoreError> {
        let path = self.sessions_dir().join(format!("{task_id}.events.jsonl"));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| StoreError::Write {
                path: path.clone(),
                source,
            })?;
        let mut line = serde_json::to_string(event).map_err(|source| StoreError::Parse {
            path: path.clone(),
            source,
        })?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|source| StoreError::Write { path, source })?;
        Ok(())
    }

    /// 加载任务轨迹。
    pub fn load_trace(&self, task_id: &str) -> Result<TaskTrace, StoreError> {
        let path = self.sessions_dir().join(format!("{task_id}.json"));
        let raw = std::fs::read_to_string(&path).map_err(|source| StoreError::Read {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str(&raw).map_err(|source| StoreError::Parse { path, source })
    }

    /// 列出全部会话。
    pub fn list_sessions(&self) -> Vec<(String, String, String, TaskStatus)> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.sessions_dir()) else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") || name.ends_with(".events.jsonl") {
                continue;
            }
            if let Ok(trace) = self.load_trace(name.trim_end_matches(".json")) {
                out.push((
                    trace.task_id,
                    trace.title,
                    trace.started_at.format("%Y-%m-%d %H:%M").to_string(),
                    trace.status,
                ));
            }
        }
        out.sort_by(|a, b| b.2.cmp(&a.2));
        out
    }

    /// 导出会话完整上下文（轨迹 + 事件流）为单文件 JSON；敏感字段打码（NFR-2）。
    pub fn export_session(&self, task_id: &str) -> Result<String, StoreError> {
        let trace = self
            .load_trace(task_id)
            .map_err(|_| StoreError::SessionNotFound(task_id.into()))?;
        let mut events = Vec::new();
        let events_path = self.sessions_dir().join(format!("{task_id}.events.jsonl"));
        if let Ok(raw) = std::fs::read_to_string(&events_path) {
            for line in raw.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    events.push(mask_value(v));
                }
            }
        }
        let doc = serde_json::json!({
            "task_id": trace.task_id,
            "exported_at": chrono::Local::now().to_rfc3339(),
            "trace": serde_json::to_value(&trace).unwrap_or_default(),
            "events": events,
        });
        serde_json::to_string_pretty(&doc).map_err(|source| StoreError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })
    }

    // ── 用量（R6）───────────────────────────────────────────────

    /// 追加一条用量记录。
    pub fn append_usage(&self, record: &UsageRecord) -> Result<(), StoreError> {
        let path = self.usage_path();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| StoreError::Write {
                path: path.clone(),
                source,
            })?;
        let mut line = serde_json::to_string(record).map_err(|source| StoreError::Parse {
            path: path.clone(),
            source,
        })?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|source| StoreError::Write { path, source })
    }

    /// 读取全部用量记录（文件按行回放）。
    pub fn read_usage(&self) -> Vec<UsageRecord> {
        let Ok(raw) = std::fs::read_to_string(self.usage_path()) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

/// 仓库内会话备份（v0.9.2 调试设施）：把任务轨迹 / 对话原文 / 事件流镜像到
/// `<仓库>/session-backups/<task_id>/`，便于把故障现场整目录提交进 git 分析。
/// 与数据目录 `~/.mcha/sessions/`、仓库既有的 `sessions/`（基线实验材料）互不影响。
/// 备份是非关键路径：任何写入失败只 warn，不影响主流程。
pub struct SessionBackup {
    root: PathBuf,
}

impl SessionBackup {
    /// 定位备份根目录：环境变量 `MCHA_BACKUP_DIR` 优先，
    /// 否则用编译期仓库路径（pull → build → run 工作流下即仓库目录）。
    pub fn open() -> Self {
        let root = std::env::var_os("MCHA_BACKUP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("session-backups"));
        Self { root }
    }

    /// 根目录参数化（测试用）。
    #[cfg(test)]
    pub fn open_at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 备份根目录（CLI 展示用）。
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    fn task_dir(&self, task_id: &str) -> PathBuf {
        self.root.join(task_id)
    }

    /// 覆盖写一个文本文件（序列化失败也只 warn——备份不阻塞主流程）。
    fn write(&self, task_id: &str, name: &str, content: String) {
        let dir = self.task_dir(task_id);
        let result =
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join(name), content));
        if let Err(e) = result {
            tracing::warn!("会话备份 {task_id}/{name} 写入失败（不影响主流程）：{e}");
        }
    }

    /// 镜像任务轨迹（与 [`Store::save_trace`] 同构）。
    pub fn save_trace(&self, trace: &TaskTrace) {
        match serde_json::to_string_pretty(trace) {
            Ok(json) => self.write(&trace.task_id, "trace.json", json),
            Err(e) => tracing::warn!("会话备份轨迹序列化失败：{e}"),
        }
    }

    /// 镜像对话原文（与 [`Store::save_messages`] 同构，messages 为已序列化 JSON）。
    pub fn save_messages(&self, task_id: &str, messages: &serde_json::Value) {
        match serde_json::to_string_pretty(messages) {
            Ok(json) => self.write(task_id, "messages.json", json),
            Err(e) => tracing::warn!("会话备份对话序列化失败：{e}"),
        }
    }

    /// 镜像事件流追加（与 [`Store::append_event`] 同构，JSONL）。
    pub fn append_event(&self, task_id: &str, event: &serde_json::Value) {
        let dir = self.task_dir(task_id);
        let result = std::fs::create_dir_all(&dir).and_then(|()| {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("events.jsonl"))?;
            writeln!(file, "{event}")
        });
        if let Err(e) = result {
            tracing::warn!("会话备份 {task_id}/events.jsonl 追加失败（不影响主流程）：{e}");
        }
    }
}

/// 打码：把取值中疑似密钥/公网地址的字段替换为占位（NFR-2）。
fn mask_value(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        for (key, val) in obj.iter_mut() {
            let k = key.to_lowercase();
            if (k.contains("token") || k.contains("api_key") || k.contains("apikey"))
                && val.is_string()
            {
                *val = serde_json::Value::String("<已打码>".into());
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn sample_trace() -> TaskTrace {
        TaskTrace::new("task-1".to_string(), "测试任务")
    }

    #[test]
    fn 会话备份镜像写入仓库目录() {
        // v0.9.2：备份与数据目录 sessions/、仓库既有 sessions/ 互不影响
        let tmp = tempfile::tempdir().unwrap();
        let backup = SessionBackup::open_at(tmp.path());
        let trace = TaskTrace::new("t-bk".into(), "备份测试");
        backup.save_trace(&trace);
        backup.save_messages("t-bk", &serde_json::json!([{"role": "user"}]));
        backup.append_event("t-bk", &serde_json::json!({"event": "x"}));
        backup.append_event("t-bk", &serde_json::json!({"event": "y"}));

        let dir = tmp.path().join("t-bk");
        assert!(dir.join("trace.json").is_file());
        let trace_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("trace.json")).unwrap())
                .unwrap();
        assert_eq!(trace_json["task_id"], "t-bk");
        let msgs: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("messages.json")).unwrap())
                .unwrap();
        assert_eq!(msgs[0]["role"], "user");
        let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert_eq!(events.lines().count(), 2, "events.jsonl 应为追加式两行");
    }

    fn sample_usage() -> UsageRecord {
        UsageRecord {
            call_id: "c1".into(),
            task_id: "task-1".into(),
            at: chrono::Local::now(),
            model: "glm-5.2".into(),
            input_tokens: 100,
            output_tokens: 50,
            cost: Decimal::from_str_exact("0.0012").unwrap(),
            phase: crate::events::Phase::Requirement,
            usage_reported: true,
        }
    }

    #[test]
    fn 档案保存与读取() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path().to_path_buf()).unwrap();
        let spec = ServerSpec::new("demo");
        store.save_profile(&spec).unwrap();
        let loaded = store.load_profile("demo").unwrap();
        assert_eq!(loaded.spec_id, "demo");
        assert_eq!(store.list_profiles().len(), 1);
    }

    #[test]
    fn 会话保存导出与打码() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path().to_path_buf()).unwrap();
        let mut trace = sample_trace();
        trace.status = TaskStatus::Done;
        trace.finished_at = Some(chrono::Local::now());
        store.save_trace(&trace).unwrap();

        let secret = serde_json::json!({"natfrp_token": "SECRET"});
        store.append_event("task-1", &secret).unwrap();

        let exported = store.export_session("task-1").unwrap();
        assert!(exported.contains("task-1"));
        assert!(!exported.contains("SECRET"), "导出必须打码");
        assert!(exported.contains("已打码"));
    }

    #[test]
    fn 用量追加与汇总() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(dir.path().to_path_buf()).unwrap();
        store.append_usage(&sample_usage()).unwrap();
        store.append_usage(&sample_usage()).unwrap();
        let records = store.read_usage();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].input_tokens, 100);
    }
}
