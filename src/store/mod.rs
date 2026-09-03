//! store：会话 / 用量的持久化与查询（R5 / R6）。
//!
//! 数据目录布局（决议 D4）：`{sessions, usage, runtime}/`。
//! - 会话 = 消息流 JSONL 逐条追加（崩溃可恢复）+ 元数据快照
//! - 用量账本 = 全局 JSONL，一次 LLM 调用一行（重试也计一行，R6 诚实计量）

pub mod mask;
pub mod profile;
pub mod session;
pub mod usage;

use anyhow::Context;

/// 当前时间的 RFC3339 字符串（本地时区，落盘统一格式）。
pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 生成会话 ID：`2026-0901-1430-a1b2`（时间前缀便于浏览，短随机后缀防碰撞）。
pub fn new_session_id() -> String {
    let now = chrono::Local::now();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 20))
        .unwrap_or(0);
    format!("{}-{:04x}", now.format("%Y-%m%d-%H%M"), nanos % 0xffff)
}

/// 确保目录存在（带上下文的 mkdir -p）。
pub fn ensure_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("创建目录失败：{}", dir.display()))
}
