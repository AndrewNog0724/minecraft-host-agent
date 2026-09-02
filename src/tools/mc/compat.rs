//! check_version_compat：MC × 服务端软件 × Java 兼容性查证（定制 2 查询工具）。
//!
//! 事实来源：L1 知识表（java_compat / server_software）+ 上游 API 实时核对
//! ——vanilla 以 Mojang 官方 javaVersion 为权威，paper / fabric 核对存在性
//! 与最新构建。mod 维度随 M2.2 扩展；Forge 在 M2.1 为指导模式（D7 修订）。
//! 只读工具，不做任何下载。

use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;

use crate::agent::message::ToolOutcome;
use crate::knowledge::compat::{JavaCompat, SoftwareCatalog, SoftwareEntry};
use crate::knowledge::upstream::{fabric::FabricClient, mojang::MojangClient, paper::PaperClient};
use crate::knowledge::version::McVersion;

use super::{Tool, ToolCtx, ToolError};

/// 上游查询的整体超时（每个软件渠道）。
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckVersionArgs {
    /// MC 版本号（如 1.21.1；快照不支持）
    pub mc_version: String,
    /// 服务端软件 id（vanilla | paper | spigot | fabric | forge）；缺省查全部
    #[serde(default)]
    pub software: Option<String>,
}

pub struct CheckVersionCompatTool;

#[async_trait::async_trait]
impl Tool for CheckVersionCompatTool {
    fn name(&self) -> &'static str {
        "check_version_compat"
    }
    fn description(&self) -> String {
        "查证 MC 版本 × 服务端软件 × Java 兼容性：返回版本存在性、Java 大版本要求、各渠道下载可用性与哈希可信度。版本类事实必须先经本工具查证，不得凭记忆回答。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(CheckVersionArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: CheckVersionArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let version = match McVersion::parse(&args.mc_version) {
            Ok(v) => v,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let catalog = SoftwareCatalog::builtin();
        let compat = JavaCompat::builtin();

        // 选定要查的软件
        let targets: Vec<&SoftwareEntry> = match &args.software {
            Some(id) => match catalog.find(id) {
                Some(entry) => vec![entry],
                None => {
                    return Ok(ToolOutcome::err(format!(
                        "未知软件「{id}」；可用：{}",
                        catalog
                            .software
                            .iter()
                            .map(|s| s.id.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    )));
                }
            },
            None => catalog.software.iter().collect(),
        };

        let mut lines = vec![format!(
            "MC {version} × 服务端选型查证（服务端知识表更新于 {}）：",
            catalog.updated
        )];

        // Java 要求（L1 知识表）
        match compat.lookup(&version) {
            Some(req) => lines.push(format!(
                "[Java] 知识库（更新 {}）：需 Java {}+（区间 {}；{}）",
                compat.updated, req.java_major, req.source_range, req.note
            )),
            None => lines.push(format!(
                "[Java] 版本超出知识库覆盖范围（表更新于 {}）；以 Mojang 官方 javaVersion 为准",
                compat.updated
            )),
        }

        // 逐软件查证
        let mut vanilla_java: Option<u32> = None;
        let mut results = Vec::new();
        for entry in targets {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if !entry.supports(&version) {
                results.push(format!(
                    "[{}] ✗ 知识库标注不支持 MC {version}（支持范围 {} ~ {}）",
                    entry.id,
                    entry.min_mc,
                    if entry.max_mc.is_empty() {
                        "最新"
                    } else {
                        &entry.max_mc
                    }
                ));
                continue;
            }
            let line = match entry.channel.as_str() {
                "mojang" => {
                    let mirror = crate::config::mojang_mirror_base(&ctx.network.mojang_mirror);
                    let client = MojangClient::new(&ctx.http, mirror);
                    match tokio::time::timeout(CHECK_TIMEOUT, client.resolve_server(&version.raw))
                        .await
                    {
                        Err(_) => format!("[vanilla] ✗ 查证超时（{} 秒）", CHECK_TIMEOUT.as_secs()),
                        Ok(Err(reason)) => format!("[vanilla] ✗ {reason}"),
                        Ok(Ok(resolved)) => {
                            if let Some(major) = resolved.java_major {
                                vanilla_java = Some(major);
                            }
                            format!(
                                "[vanilla] ✓ 官方服务端可用：sha1={}…，{:.1} MB（Mojang 官方渠道，官方 sha1 校验；Java 要求以官方为准{}）",
                                &resolved.sha1[..12.min(resolved.sha1.len())],
                                resolved.size as f64 / 1024.0 / 1024.0,
                                if resolved.java_major.is_some() {
                                    "，javaVersion 见下"
                                } else {
                                    ""
                                }
                            )
                        }
                    }
                }
                "papermc" => {
                    let client = PaperClient::new(&ctx.http);
                    match tokio::time::timeout(CHECK_TIMEOUT, client.latest_build(&version.raw))
                        .await
                    {
                        Err(_) => format!("[paper] ✗ 查证超时（{} 秒）", CHECK_TIMEOUT.as_secs()),
                        Ok(Err(reason)) => format!("[paper] ✗ {reason}"),
                        Ok(Ok(build)) => format!(
                            "[paper] ✓ 可用：最新 build {}（{}，PaperMC 官方 API，官方 sha256 校验）",
                            build.build, build.file_name
                        ),
                    }
                }
                "fabricmeta" => {
                    let client = FabricClient::new(&ctx.http);
                    match tokio::time::timeout(CHECK_TIMEOUT, client.resolve_server(&version.raw))
                        .await
                    {
                        Err(_) => format!("[fabric] ✗ 查证超时（{} 秒）", CHECK_TIMEOUT.as_secs()),
                        Ok(Err(reason)) => format!("[fabric] ✗ {reason}"),
                        Ok(Ok(resolved)) => format!(
                            "[fabric] ✓ 可用：loader {} / installer {}（Fabric 官方 meta；整包无官方哈希，下载时计算 sha256 留痕）",
                            resolved.loader, resolved.installer
                        ),
                    }
                }
                "getbukkit" => format!(
                    "[spigot] ✓ 版本在支持范围内；{}",
                    entry.note.trim_end_matches('。')
                ),
                "guided" => format!("[forge] {}", entry.note),
                other => format!("[{}] ⚠ 未知渠道「{other}」（知识表配置问题）", entry.id),
            };
            results.push(line);
        }
        lines.extend(results);

        // Java 要求结论：官方权威优先
        if let Some(major) = vanilla_java {
            lines.push(format!(
                "[Java] Mojang 官方权威：此版本需 Java {major}（版本 JSON javaVersion 字段）"
            ));
        }

        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}
