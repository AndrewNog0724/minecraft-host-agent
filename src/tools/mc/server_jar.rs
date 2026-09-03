//! fetch_server_jar：服务端获取与校验（FR-10，设计 §8.10）。
//!
//! 渠道：vanilla（Mojang piston-meta，官方 sha1 + BMCLAPI 镜像）、paper
//!（Fill v3，官方 sha256）、fabric（meta，落地计算 sha256）、spigot
//!（getbukkit 抓页解析，无哈希、轨迹明示第三方）。产物统一改名
//! `server.jar`（D118：屏蔽渠道 jar 名差异，原始名记入轨迹）。
//! Forge 为指导模式（D7 修订）：本工具不做 Forge 下载。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;
use crate::knowledge::compat::SoftwareCatalog;
use crate::knowledge::upstream::fabric::FabricClient;
use crate::knowledge::upstream::mojang::MojangClient;
use crate::knowledge::upstream::paper::PaperClient;
use crate::knowledge::upstream::send_get;
use crate::knowledge::version::McVersion;
use crate::tools::confinement::resolve_in;

use super::download::{ExpectedHash, download_verified};
use super::{Tool, ToolCtx, ToolError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchServerJarArgs {
    /// 服务端软件 id（vanilla | paper | spigot | fabric；forge 为指导模式不支持自动下载）
    pub software: String,
    /// MC 版本号（如 1.21.1）
    pub mc_version: String,
    /// 服务器目录（工作区内，默认 server）
    #[serde(default)]
    pub server_dir: Option<String>,
}

pub struct FetchServerJarTool;

/// getbukkit 白名单 CDN 域（Location 解析后校验，防开放重定向）。
const GETBUKKIT_CDN_HOST: &str = "cdn.getbukkit.org";

/// 渠道解析结果（下载前）。urls 按优先级排列（镜像在前，官方兜底）。
struct Resolved {
    urls: Vec<String>,
    expected: Vec<ExpectedHash>,
    file_name: String,
    trust_note: String,
}

/// getbukkit 抓页解析：下载页版本卡 token → /get/{token} 302 Location → CDN 直链。
async fn resolve_spigot(ctx: &ToolCtx, version: &str) -> Result<(String, String, String), String> {
    let page = send_get(&ctx.http, "https://getbukkit.org/download/spigot")
        .await
        .map_err(|err| format!("getbukkit 下载页抓取失败：{err}"))?;
    if !page.status().is_success() {
        return Err(format!(
            "getbukkit 下载页返回 HTTP {}（第三方渠道不稳定，可改用 Paper）",
            page.status().as_u16()
        ));
    }
    let html = page
        .text()
        .await
        .map_err(|err| format!("读取下载页失败：{err}"))?;
    let marker = format!(">{}</h2>", version);
    let pos = html
        .find(&marker)
        .ok_or_else(|| format!("getbukkit 列表中未找到版本 {version}"))?;
    let before = &html[..pos];
    let token_pos = before
        .rfind("https://getbukkit.org/get/")
        .ok_or_else(|| format!("版本 {version} 的下载入口解析失败"))?;
    let rest = &before[token_pos..];
    let link_end = rest.find('"').unwrap_or(rest.len());
    let get_link = &rest[..link_end];

    // /get/{token} 返回 302 → Location 直链；禁用自动重定向（jar 由本工具带进度下载）
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("构造 HTTP 客户端失败：{err}"))?;
    let resp = no_redirect
        .get(get_link)
        .send()
        .await
        .map_err(|err| format!("解析下载直链失败：{err}"))?;
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            format!(
                "getbukkit 未返回下载直链（HTTP {}）",
                resp.status().as_u16()
            )
        })?
        .to_string();

    // 域校验（§12）：仅允许 getbukkit 官方 CDN
    let host = url_host(&location);
    if host.as_deref() != Some(GETBUKKIT_CDN_HOST) {
        return Err(format!(
            "下载直链域异常（{host:?}），已拒绝；仅允许 {GETBUKKIT_CDN_HOST}"
        ));
    }
    let file_name = location
        .rsplit('/')
        .next()
        .unwrap_or("spigot.jar")
        .to_string();
    let trace = format!("getbukkit（{get_link}）→ {location}");
    Ok((location, file_name, trace))
}

/// 从 URL 提取 host（极简解析，足够域校验用）。
fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    rest.split(['/']).next().map(|s| s.to_ascii_lowercase())
}

#[async_trait::async_trait]
impl Tool for FetchServerJarTool {
    fn name(&self) -> &'static str {
        "fetch_server_jar"
    }
    fn description(&self) -> String {
        "下载服务端 jar 到服务器目录（统一命名 server.jar）：vanilla / paper / fabric / spigot 官方渠道 + 哈希校验（无官方哈希的渠道落地计算 sha256 留痕）。forge 为指导模式，请改用 fabric 或告知用户人工步骤。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(FetchServerJarArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Network
    }
    fn confirm_summary(&self, args: &serde_json::Value) -> Vec<String> {
        let text = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let server_dir = args
            .get("server_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("server");
        vec![
            format!(
                "渠道：{} × MC {}（官方渠道下载，官方哈希校验不过即失败回环）",
                text("software"),
                text("mc_version")
            ),
            format!("落盘：<工作区>/{server_dir}/server.jar（统一命名，原始文件名记入轨迹）"),
        ]
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: FetchServerJarArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let version = match McVersion::parse(&args.mc_version) {
            Ok(v) => v,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let catalog = SoftwareCatalog::builtin();
        let Some(entry) = catalog.find(&args.software) else {
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
        };
        if entry.channel == "guided" {
            return Ok(ToolOutcome::err(format!(
                "{} 为指导模式（D7 修订）：不提供自动下载。请与用户确认改用 fabric，或给出人工安装步骤。",
                entry.id
            )));
        }
        if !entry.supports(&version) {
            return Ok(ToolOutcome::err(format!(
                "{} 不支持 MC {version}（支持范围 {} ~ {}）",
                entry.id,
                entry.min_mc,
                if entry.max_mc.is_empty() {
                    "最新"
                } else {
                    &entry.max_mc
                }
            )));
        }

        // ① 渠道解析
        let resolved = match entry.channel.as_str() {
            "mojang" => {
                let mirror = crate::config::mojang_mirror_base(&ctx.network.mojang_mirror);
                // 解析：镜像失败回退官方直连（实测 BMCLAPI 偶发 5xx）
                let first = {
                    let client = MojangClient::new(&ctx.http, mirror.clone());
                    client.resolve_server(&version.raw).await
                };
                let (r, mirror_used) = match first {
                    Ok(r) => (r, true),
                    Err(reason) if mirror.is_some() => {
                        let client = MojangClient::new(&ctx.http, None);
                        match client.resolve_server(&version.raw).await {
                            Ok(r) => (r, false),
                            Err(reason2) => {
                                return Ok(ToolOutcome::err(format!(
                                    "{reason}；官方直连亦失败：{reason2}"
                                )));
                            }
                        }
                    }
                    Err(reason) => return Ok(ToolOutcome::err(reason)),
                };
                let mirror_text = match (&mirror, mirror_used) {
                    (Some(base), true) => base.clone(),
                    _ => "直连".to_string(),
                };
                // 下载：镜像 URL 优先，官方 URL 兜底
                let mut urls = vec![r.url.clone()];
                if mirror.is_some() && r.official_url != r.url {
                    urls.push(r.official_url.clone());
                }
                Resolved {
                    urls,
                    expected: vec![ExpectedHash::Sha1(r.sha1.clone())],
                    file_name: format!("server-{version}.jar"),
                    trust_note: format!(
                        "Mojang 官方渠道（官方 sha1={}…；镜像策略：{mirror_text}）",
                        &r.sha1[..12.min(r.sha1.len())]
                    ),
                }
            }
            "papermc" => {
                let client = PaperClient::new(&ctx.http);
                match client.latest_build(&version.raw).await {
                    Ok(b) => Resolved {
                        urls: vec![b.url],
                        expected: vec![ExpectedHash::Sha256(b.sha256)],
                        file_name: b.file_name,
                        trust_note: format!(
                            "PaperMC 官方渠道（Fill v3，官方 sha256，build {}）",
                            b.build
                        ),
                    },
                    Err(reason) => return Ok(ToolOutcome::err(reason)),
                }
            }
            "fabricmeta" => {
                let client = FabricClient::new(&ctx.http);
                match client.resolve_server(&version.raw).await {
                    Ok(f) => Resolved {
                        urls: vec![f.url],
                        expected: Vec::new(),
                        file_name: format!("fabric-server-{version}.jar"),
                        trust_note: format!(
                            "Fabric 官方 meta（loader {} / installer {}；整包无官方哈希，落地计算 sha256 留痕）",
                            f.loader, f.installer
                        ),
                    },
                    Err(reason) => return Ok(ToolOutcome::err(reason)),
                }
            }
            "getbukkit" => match resolve_spigot(ctx, &version.raw).await {
                Ok((url, file_name, trace)) => Resolved {
                    urls: vec![url],
                    expected: Vec::new(),
                    file_name,
                    trust_note: format!(
                        "第三方渠道，无官方哈希，落地计算 sha256 留痕；解析链路：{trace}"
                    ),
                },
                Err(reason) => return Ok(ToolOutcome::err(reason)),
            },
            other => {
                return Ok(ToolOutcome::err(format!("渠道「{other}」暂不支持自动下载")));
            }
        };

        // ② 下载（进度 + 强校验，多候选 URL 依序尝试）→ 统一命名 server.jar
        let server_dir = resolve_in(
            &[ctx.workspace.as_path()],
            args.server_dir.as_deref().unwrap_or("server"),
        )?;
        let target = server_dir.join("server.jar");
        let part = server_dir.join(".server.jar.part");
        let label = format!("下载 {} {version} 服务端", entry.id);
        let label = label.as_str();
        let mut last_err = "无可用下载源".to_string();
        let mut result = None;
        let mut used_url = String::new();
        for url in &resolved.urls {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            match download_verified(ctx, url, &part, label, &resolved.expected).await {
                Ok(r) => {
                    result = Some(r);
                    used_url = url.clone();
                    break;
                }
                Err(reason) => {
                    let _ = std::fs::remove_file(&part);
                    last_err = reason;
                }
            }
        }
        let Some(result) = result else {
            return Ok(ToolOutcome::err(format!(
                "{last_err}；可重试、更换渠道（paper/fabric 互为替代）或询问用户"
            )));
        };
        std::fs::rename(&part, &target)
            .map_err(|err| ToolError::Io(format!("落盘失败（{}）：{err}", target.display())))?;

        let mut lines = vec![
            format!(
                "已下载 {}（原文件名 {}）→ {}",
                entry.id,
                resolved.file_name,
                target.display()
            ),
            format!(
                "大小 {:.1} MB；sha256={}",
                result.bytes as f64 / 1024.0 / 1024.0,
                result.sha256
            ),
            format!("来源：{}；实际下载 URL：{used_url}", resolved.trust_note),
        ];
        if let Some(sha1) = &result.sha1 {
            lines.push(format!("官方 sha1 校验通过（{sha1}）"));
        }
        lines.push("下一步：write_server_files 生成配置（eula 需先询问用户确认）。".to_string());
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 repl 生产配置一致的测试 HTTP 客户端（带 UA：部分镜像拒绝空 UA）。
    fn live_http() -> reqwest::Client {
        reqwest::Client::builder()
            .user_agent("mcha/0.2")
            .build()
            .unwrap()
    }

    fn test_ctx(root: &tempfile::TempDir) -> ToolCtx {
        let (tx, _rx) = crate::events::event_channel();
        ToolCtx {
            workspace: root.path().to_path_buf(),
            data_dir: root.path().join("data"),
            http: live_http(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
            curseforge_key: String::new(),
        }
    }

    fn assert_jar_downloaded(outcome: ToolOutcome) {
        let ToolOutcome::Ok { content } = outcome else {
            panic!("应下载成功：{outcome:?}");
        };
        assert!(content.contains("server.jar"), "{content}");
        assert!(content.contains("sha256="), "{content}");
    }

    #[tokio::test]
    #[ignore = "真实下载冒烟（约 1MB）：cargo test --ignored"]
    async fn live_fetch_fabric_downloads_server_jar() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&root);
        let outcome = FetchServerJarTool
            .run(
                serde_json::json!({ "software": "fabric", "mc_version": "1.21.1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_jar_downloaded(outcome);
    }

    #[tokio::test]
    #[ignore = "真实下载冒烟（约 50MB）：cargo test --ignored"]
    async fn live_fetch_paper_verifies_official_sha256() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&root);
        let outcome = FetchServerJarTool
            .run(
                serde_json::json!({ "software": "paper", "mc_version": "1.21.1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_jar_downloaded(outcome);
    }

    #[tokio::test]
    #[ignore = "真实下载冒烟（约 60MB，BMCLAPI 镜像）：cargo test --ignored"]
    async fn live_fetch_vanilla_via_mirror_verifies_sha1() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&root);
        let outcome = FetchServerJarTool
            .run(
                serde_json::json!({ "software": "vanilla", "mc_version": "1.21.1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_jar_downloaded(outcome);
    }

    #[tokio::test]
    #[ignore = "真实下载冒烟（第三方渠道约 50MB）：cargo test --ignored"]
    async fn live_fetch_spigot_via_getbukkit() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&root);
        let outcome = FetchServerJarTool
            .run(
                serde_json::json!({ "software": "spigot", "mc_version": "1.21.1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_jar_downloaded(outcome);
    }

    #[tokio::test]
    async fn forge_channel_is_guided_mode() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(&root);
        let outcome = FetchServerJarTool
            .run(
                serde_json::json!({ "software": "forge", "mc_version": "1.21.1" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "forge 应拒绝自动下载：{outcome:?}");
    }

    #[test]
    fn confirmation_lines_describe_channel_and_target() {
        let lines = FetchServerJarTool.confirm_summary(&serde_json::json!({
            "software": "paper", "mc_version": "1.21.1"
        }));
        assert!(lines.iter().all(|l| !l.trim().is_empty()), "{lines:?}");
        let joined = lines.join("\n");
        assert!(joined.contains("paper × MC 1.21.1"), "{joined}");
        assert!(joined.contains("server/server.jar"), "{joined}");
    }
}
