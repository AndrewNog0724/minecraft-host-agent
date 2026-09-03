//! sys_info：环境探测（决议 D116，设计 §8.10）。
//!
//! 只读、免确认；返回 OS / 架构 / 内存 / CPU 信息，作为 JVM -Xmx 推荐依据，
//! 后续诊断步骤亦复用。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;

use super::{Tool, ToolCtx, ToolError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SysInfoArgs {}

pub struct SysInfoTool;

/// 人类可读的内存规格（GB，保留 1 位小数）。
fn gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
}

#[async_trait::async_trait]
impl Tool for SysInfoTool {
    fn name(&self) -> &'static str {
        "sys_info"
    }
    fn description(&self) -> String {
        "探测本机环境：操作系统 / 架构 / 总内存与可用内存 / CPU 核数。用于 JVM 内存（-Xmx）推荐与故障诊断。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(SysInfoArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutcome, ToolError> {
        // 阻塞探测很轻（毫秒级内存读数），放 spawn_blocking 避免占用执行器
        let report = tokio::task::spawn_blocking(|| {
            use sysinfo::System;
            let mut sys = System::new();
            sys.refresh_memory();
            sys.refresh_cpu_usage();
            let os_name = System::name().unwrap_or_else(|| "未知系统".to_string());
            let os_version = System::os_version().unwrap_or_default();
            let total = sys.total_memory();
            let available = sys.available_memory();
            let cpus = sys.cpus().len();
            format!(
                "系统环境：{os_name} {os_version} / {}；内存 {}（可用 {}）；CPU {} 核\n\
                 -Xmx 推荐依据：总内存 − 1024 MB 预留；512 MB ≤ Xmx ≤ {}。",
                std::env::consts::ARCH,
                gib(total),
                gib(available),
                cpus,
                gib(total.saturating_sub(1024 * 1024 * 1024))
            )
        })
        .await
        .map_err(|err| ToolError::Io(format!("环境探测失败：{err}")))?;
        Ok(ToolOutcome::ok(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_environment_on_host() {
        let (tx, _rx) = crate::events::event_channel();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
            curseforge_key: String::new(),
        };
        let outcome = SysInfoTool.run(serde_json::json!({}), &ctx).await.unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("应为成功结果：{outcome:?}");
        };
        assert!(content.contains(std::env::consts::ARCH));
        assert!(content.contains("CPU"));
    }

    #[test]
    fn tool_identity_is_readonly() {
        assert_eq!(SysInfoTool.name(), "sys_info");
        assert_eq!(SysInfoTool.permission(), crate::tools::Permission::ReadOnly);
    }

    #[test]
    fn gib_formats_readably() {
        assert_eq!(gib(2 * 1024 * 1024 * 1024 + 536_870_912), "2.5 GB");
    }
}
