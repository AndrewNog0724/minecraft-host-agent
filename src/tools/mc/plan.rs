//! check_plan：部署前确定性校验（决议 D111/D119，设计 §8.10）。
//!
//! "不漏分支"的第二重保险：Agent 在执行部署前必须调用本工具，逐项核对
//! 方案完整性；缺项以结构化清单返回，由 Agent 回环补齐或征询用户。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;
use crate::knowledge::compat::{JavaCompat, SoftwareCatalog};
use crate::knowledge::version::McVersion;
use crate::tools::confinement::resolve_in;

use super::{Tool, ToolCtx, ToolError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckPlanArgs {
    /// 服务端软件 id（vanilla | paper | spigot | fabric）
    pub software: String,
    /// MC 版本号
    pub mc_version: String,
    /// 方案选定的 Java 大版本（来自 check_version_compat 查证）
    pub java_major: u32,
    /// 账号类型：all_online（全正版）| all_offline（全离线）| mixed（混合）
    pub account_type: String,
    /// 方案中的 online-mode 取值
    pub online_mode: bool,
    /// 用户已确认接受 EULA
    pub eula_accepted: bool,
    /// JVM -Xmx（MB）
    pub jvm_memory_mb: u32,
    /// 本机总内存 MB（来自 sys_info）
    pub total_memory_mb: u32,
    /// 服务端口
    pub port: u16,
    /// 白名单开关
    pub whitelist_enabled: bool,
    /// 用户已明确拒绝白名单并知悉风险（离线模式下默认要求白名单，D119）
    #[serde(default)]
    pub whitelist_disabled_ack: bool,
    /// 服务器目录（工作区内，默认 server）
    #[serde(default)]
    pub server_dir: Option<String>,
}

/// 单项检查结果。
struct Item {
    id: &'static str,
    ok: bool,
    /// warn = 提示但不算失败。
    severity: Severity,
    detail: String,
}

#[derive(PartialEq)]
enum Severity {
    Error,
    Warning,
}

impl Item {
    fn pass(id: &'static str, detail: String) -> Self {
        Self {
            id,
            ok: true,
            severity: Severity::Error,
            detail,
        }
    }
    fn fail(id: &'static str, detail: String) -> Self {
        Self {
            id,
            ok: false,
            severity: Severity::Error,
            detail,
        }
    }
    fn warn(id: &'static str, detail: String) -> Self {
        Self {
            id,
            ok: true,
            severity: Severity::Warning,
            detail,
        }
    }
}

pub struct CheckPlanTool;

/// offline 模式的合法性映射：all_online → 必须开；all_offline / mixed → 必须关。
fn online_mode_consistent(account_type: &str, online_mode: bool) -> Result<bool, String> {
    match account_type {
        "all_online" => Ok(online_mode),
        "all_offline" | "mixed" => Ok(!online_mode),
        other => Err(format!(
            "未知账号类型「{other}」；可选 all_online | all_offline | mixed"
        )),
    }
}

#[async_trait::async_trait]
impl Tool for CheckPlanTool {
    fn name(&self) -> &'static str {
        "check_plan"
    }
    fn description(&self) -> String {
        "部署前确定性校验：逐项核对方案（软件×版本、Java 匹配、online-mode 与账号一致、EULA、内存范围、端口、白名单配套、目录冲突）。返回 pass/fail 与结构化缺项清单；未通过不得开始部署。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(CheckPlanArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: CheckPlanArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let mut items: Vec<Item> = Vec::new();

        // ① software × mc_version
        let version = match McVersion::parse(&args.mc_version) {
            Ok(v) => v,
            Err(reason) => {
                return Ok(ToolOutcome::err(reason));
            }
        };
        let catalog = SoftwareCatalog::builtin();
        match catalog.find(&args.software) {
            Some(entry) if entry.supports(&version) => {
                items.push(Item::pass(
                    "software_version",
                    format!("{} 支持 MC {version}", entry.id),
                ));
            }
            Some(entry) => items.push(Item::fail(
                "software_version",
                format!(
                    "{} 不支持 MC {version}（支持范围 {} ~ {}）",
                    entry.id,
                    entry.min_mc,
                    if entry.max_mc.is_empty() {
                        "最新"
                    } else {
                        &entry.max_mc
                    }
                ),
            )),
            None => {
                return Ok(ToolOutcome::err(format!(
                    "未知软件「{}」；可用：{}",
                    args.software,
                    catalog
                        .software
                        .iter()
                        .map(|s| s.id.as_str())
                        .collect::<Vec<_>>()
                        .join("、")
                )));
            }
        }

        // ② Java 大版本匹配（L1 知识；无覆盖时提示以上游为准，不算失败）
        let compat = JavaCompat::builtin();
        match compat.lookup(&version) {
            Some(req) if req.java_major == args.java_major => items.push(Item::pass(
                "java_version",
                format!(
                    "Java {args} 与 MC {version} 的要求（Java {jvm}+）一致",
                    args = args.java_major,
                    jvm = req.java_major
                ),
            )),
            Some(req) => items.push(Item::fail(
                "java_version",
                format!(
                    "Java 版本不匹配：MC {version} 需 Java {}+，方案为 {}（{}）",
                    req.java_major, args.java_major, req.note
                ),
            )),
            None => items.push(Item::warn(
                "java_version",
                format!(
                    "MC {version} 超出知识库覆盖（更新于 {}）；以 Mojang 官方 javaVersion 为准",
                    compat.updated
                ),
            )),
        }

        // ③ online-mode 与账号类型一致
        match online_mode_consistent(&args.account_type, args.online_mode) {
            Ok(true) => items.push(Item::pass(
                "online_mode",
                format!(
                    "online-mode={} 与账号类型（{}）一致",
                    args.online_mode, args.account_type
                ),
            )),
            Ok(false) => items.push(Item::fail(
                "online_mode",
                format!(
                    "online-mode={} 与账号类型（{}）矛盾；决策指南：全正版 → true，全离线/混合 → false",
                    args.online_mode, args.account_type
                ),
            )),
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        }

        // ④ EULA
        if args.eula_accepted {
            items.push(Item::pass("eula", "用户已确认接受 EULA".into()));
        } else {
            items.push(Item::fail(
                "eula",
                "尚未确认 EULA；必须先经 ask_user 征得用户同意".into(),
            ));
        }

        // ⑤ 内存范围
        let max_xmx = args.total_memory_mb.saturating_sub(1024);
        if (512..=max_xmx.max(512)).contains(&args.jvm_memory_mb) {
            items.push(Item::pass(
                "jvm_memory",
                format!(
                    "-Xmx{}MB 在合理范围（512 ~ 总内存−1024 = {max_xmx}）",
                    args.jvm_memory_mb
                ),
            ));
        } else if args.jvm_memory_mb < 512 {
            items.push(Item::fail(
                "jvm_memory",
                format!("-Xmx{}MB 过小（下限 512）", args.jvm_memory_mb),
            ));
        } else {
            items.push(Item::fail(
                "jvm_memory",
                format!(
                    "-Xmx{}MB 超过总内存−1024（{max_xmx}MB），会挤压系统",
                    args.jvm_memory_mb
                ),
            ));
        }

        // ⑥ 端口合法 + 未占用
        if args.port <= 1024 {
            items.push(Item::fail(
                "port",
                format!(
                    "端口 {} 为特权端口（≤1024），需要管理员权限且易冲突",
                    args.port
                ),
            ));
        } else {
            let addr = format!("127.0.0.1:{}", args.port);
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(listener) => {
                    drop(listener);
                    items.push(Item::pass("port", format!("端口 {addr} 空闲可用")));
                }
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                    items.push(Item::fail("port", format!("端口 {addr} 已被占用")));
                }
                Err(err) => {
                    items.push(Item::warn(
                        "port",
                        format!("端口 {addr} 占用检测失败：{err}"),
                    ));
                }
            }
        }

        // ⑦ 离线模式 ↔ 白名单配套（D119 ack 机制）
        if !args.online_mode && !args.whitelist_enabled {
            if args.whitelist_disabled_ack {
                items.push(Item::warn(
                    "whitelist",
                    "用户已明确拒绝白名单并知悉离线模式风险（ack 留痕）".into(),
                ));
            } else {
                items.push(Item::fail(
                    "whitelist",
                    "离线模式必须配套白名单；如用户明确拒绝，请以 whitelist_disabled_ack=true 留痕后重检"
                        .into(),
                ));
            }
        } else {
            items.push(Item::pass("whitelist", "白名单配置与模式配套正确".into()));
        }

        // ⑧ server_dir 冲突（非空目录提示，不算失败）
        let server_dir = resolve_in(
            &[ctx.workspace.as_path()],
            args.server_dir.as_deref().unwrap_or("server"),
        )?;
        if server_dir.exists() {
            let has_content = std::fs::read_dir(&server_dir)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false);
            if has_content {
                items.push(Item::warn(
                    "server_dir",
                    format!(
                        "{} 已存在且非空（复用或覆盖请向用户确认）",
                        server_dir.display()
                    ),
                ));
            } else {
                items.push(Item::pass(
                    "server_dir",
                    "服务器目录为空，可安全初始化".into(),
                ));
            }
        } else {
            items.push(Item::pass("server_dir", "服务器目录不存在，将新建".into()));
        }

        // 汇总
        let failures: Vec<&Item> = items
            .iter()
            .filter(|i| !i.ok && i.severity == Severity::Error)
            .collect();
        let warnings: Vec<&Item> = items
            .iter()
            .filter(|i| i.severity == Severity::Warning)
            .collect();
        let mut lines = vec![format!("部署方案校验（{} 项）：", items.len())];
        for item in &items {
            let mark = if !item.ok {
                "✗"
            } else if item.severity == Severity::Warning {
                "⚠"
            } else {
                "✓"
            };
            lines.push(format!("{mark} [{}] {}", item.id, item.detail));
        }
        if failures.is_empty() {
            lines.push(format!(
                "结论：通过{}；可开始部署（ensure_java → fetch_server_jar → write_server_files → start_server）。",
                if warnings.is_empty() {
                    String::new()
                } else {
                    format!("（{} 项提示）", warnings.len())
                }
            ));
            Ok(ToolOutcome::ok(lines.join("\n")))
        } else {
            lines.push(format!(
                "结论：未通过（{} 项缺省）；请补齐后重检，缺项清单：{}",
                failures.len(),
                failures.iter().map(|i| i.id).collect::<Vec<_>>().join("、")
            ));
            Ok(ToolOutcome::err(lines.join("\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_ctx() -> (ToolCtx, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = crate::events::event_channel();
        let ctx = ToolCtx {
            workspace: root.path().join("workspace"),
            data_dir: root.path().join("data"),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
        };
        (ctx, root)
    }

    fn good_plan(port: u16) -> serde_json::Value {
        serde_json::json!({
            "software": "paper", "mc_version": "1.21.1",
            "java_major": 21, "account_type": "all_offline",
            "online_mode": false, "eula_accepted": true,
            "jvm_memory_mb": 4096, "total_memory_mb": 16384,
            "port": port, "whitelist_enabled": true
        })
    }

    /// 探一个当前空闲端口（测试不能依赖固定端口）。
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        tokio::time::sleep(Duration::from_millis(100)).await;
        port
    }

    #[tokio::test]
    async fn good_plan_passes() {
        let (ctx, _root) = test_ctx();
        let outcome = CheckPlanTool
            .run(good_plan(free_port().await), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("应通过：{outcome:?}");
        };
        assert!(content.contains("结论：通过"), "{content}");
        assert!(content.contains("✓ [online_mode]"), "{content}");
    }

    #[tokio::test]
    async fn each_failure_is_detected() {
        let (ctx, _root) = test_ctx();
        let port = free_port().await;
        // online-mode 矛盾
        let mut plan = good_plan(port);
        plan["online_mode"] = serde_json::json!(true);
        let outcome = CheckPlanTool.run(plan, &ctx).await.unwrap();
        assert!(!outcome.is_ok(), "online-mode 矛盾应失败");
        if let ToolOutcome::Err { error } = outcome {
            assert!(error.contains("online_mode"));
        }

        // Java 不匹配
        let mut plan = good_plan(port);
        plan["java_major"] = serde_json::json!(17);
        if let ToolOutcome::Err { error } = CheckPlanTool.run(plan, &ctx).await.unwrap() {
            assert!(error.contains("Java 版本不匹配"));
        } else {
            panic!("Java 不匹配应失败");
        }

        // EULA 未确认
        let mut plan = good_plan(port);
        plan["eula_accepted"] = serde_json::json!(false);
        if let ToolOutcome::Err { error } = CheckPlanTool.run(plan, &ctx).await.unwrap() {
            assert!(error.contains("eula"));
        } else {
            panic!("EULA 缺失应失败");
        }

        // 内存超限
        let mut plan = good_plan(port);
        plan["jvm_memory_mb"] = serde_json::json!(32768);
        if let ToolOutcome::Err { error } = CheckPlanTool.run(plan, &ctx).await.unwrap() {
            assert!(error.contains("jvm_memory"));
        } else {
            panic!("内存超限应失败");
        }

        // 软件不支持
        let mut plan = good_plan(port);
        plan["software"] = serde_json::json!("fabric");
        plan["mc_version"] = serde_json::json!("1.13.2");
        plan["java_major"] = serde_json::json!(8);
        if let ToolOutcome::Err { error } = CheckPlanTool.run(plan, &ctx).await.unwrap() {
            assert!(error.contains("software_version"));
        } else {
            panic!("软件不支持应失败");
        }

        // 未知账号类型 → 结构化错误
        let mut plan = good_plan(port);
        plan["account_type"] = serde_json::json!("half");
        assert!(!CheckPlanTool.run(plan, &ctx).await.unwrap().is_ok());

        // 端口被占用
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let occupied = listener.local_addr().unwrap().port();
        let mut plan = good_plan(port);
        plan["port"] = serde_json::json!(occupied);
        if let ToolOutcome::Err { error } = CheckPlanTool.run(plan, &ctx).await.unwrap() {
            assert!(error.contains("[port]"), "{error}");
        } else {
            panic!("端口占用应失败");
        }
        drop(listener);
    }

    #[tokio::test]
    async fn whitelist_requires_ack_when_disabled() {
        let (ctx, _root) = test_ctx();
        let port = free_port().await;
        let mut plan = good_plan(port);
        plan["whitelist_enabled"] = serde_json::json!(false);
        let outcome = CheckPlanTool.run(plan.clone(), &ctx).await.unwrap();
        assert!(!outcome.is_ok(), "离线无白名单且无 ack 应失败");

        plan["whitelist_disabled_ack"] = serde_json::json!(true);
        let outcome = CheckPlanTool.run(plan, &ctx).await.unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("ack 后应放行：{outcome:?}");
        };
        assert!(content.contains("⚠ [whitelist]"), "{content}");
    }
}
