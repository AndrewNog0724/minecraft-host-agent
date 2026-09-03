//! 上游 API 客户端（定制 2 的 L2 实时事实来源；设计 §8.4/§8.10）。
//!
//! 统一约定：基址可注入（生产用官方域，测试指向本地 mock）；请求带超时；
//! 失败一律结构化字符串回传给调用方（工具再回传 Agent）。

pub mod adoptium;
pub mod curseforge;
pub mod fabric;
pub mod modrinth;
pub mod mojang;
pub mod paper;

use serde::de::DeserializeOwned;
use std::time::Duration;

/// 单个上游请求的默认超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// 发起 GET 并返回响应（不校验状态码，由调用方按语义处理 404 等）。
pub(crate) async fn send_get(
    http: &reqwest::Client,
    url: &str,
) -> Result<reqwest::Response, String> {
    let response = tokio::time::timeout(REQUEST_TIMEOUT, http.get(url).send())
        .await
        .map_err(|_| format!("请求超时（{url}）"))?
        .map_err(|err| format!("请求失败（{url}）：{err}"))?;
    Ok(response)
}

/// 断言响应成功后解析 JSON。
pub(crate) async fn read_json<T: DeserializeOwned>(
    response: reqwest::Response,
    what: &str,
) -> Result<T, String> {
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{what} 返回 HTTP {}", status.as_u16()));
    }
    tokio::time::timeout(REQUEST_TIMEOUT, response.json::<T>())
        .await
        .map_err(|_| format!("读取 {what} 超时"))?
        .map_err(|err| format!("解析 {what} 失败：{err}"))
}

/// 渠道无关的下载产物解析结果（check_version_compat 与 fetch_server_jar 共用）。
/// fetch_server_jar 于 S4 落地时启用。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DownloadArtifact {
    pub software: String,
    pub mc_version: String,
    /// 下载 URL（已按镜像策略重写）。
    pub url: String,
    /// 期望哈希（hex）；None 表示无官方哈希，下载后计算 sha256 留痕。
    pub hash: Option<String>,
    pub hash_kind: HashKind,
    pub size: Option<u64>,
    /// 哈希可信度 / 来源说明（如实入轨迹）。
    pub trust_note: String,
    /// Java 大版本（vanilla 来自官方 javaVersion，其余来自 L1 知识）。
    pub java_major: Option<u32>,
}

/// 哈希语义：官方预置（强校验）vs 下载后计算（留痕）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HashKind {
    Sha1,
    Sha256,
    /// 无官方哈希，下载后计算 sha256 留痕。
    Computed,
}

/// 路径段的极简百分号编码（MC 版本号等安全字符集直通，其余保守转义）。
pub(crate) fn urlencode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::knowledge::upstream::fabric::FabricClient;
    use crate::knowledge::upstream::mojang::MojangClient;
    use crate::knowledge::upstream::paper::PaperClient;

    /// 与 repl 生产配置一致的测试 HTTP 客户端（带 UA：部分镜像拒绝空 UA）。
    fn live_http() -> reqwest::Client {
        reqwest::Client::builder()
            .user_agent("mcha/0.2")
            .build()
            .unwrap()
    }

    /// 真实 API 冒烟（`cargo test --ignored` 跑，日常测试不联网）。
    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_mojang_resolve_has_hash_and_java() {
        let http = live_http();
        let client = MojangClient::new(&http, None);
        let resolved = client.resolve_server("1.21.1").await.expect("vanilla 解析");
        assert_eq!(resolved.sha1.len(), 40);
        assert!(resolved.size > 1_000_000);
        assert!(resolved.java_major.is_some(), "1.21.1 应带 javaVersion");
    }

    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_paper_latest_build_has_sha256() {
        let http = live_http();
        let build = PaperClient::new(&http)
            .latest_build("1.21.1")
            .await
            .expect("paper 解析");
        assert_eq!(build.sha256.len(), 64);
        assert!(build.url.starts_with("https://"));
    }

    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_fabric_resolve_has_loader() {
        let http = live_http();
        let resolved = FabricClient::new(&http)
            .resolve_server("1.21.1")
            .await
            .expect("fabric 解析");
        assert!(!resolved.loader.is_empty());
        assert!(resolved.url.ends_with("/server/jar"));
    }

    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_nonexistent_version_reports_nearest() {
        let http = live_http();
        let client = MojangClient::new(&http, None);
        let err = client.resolve_server("99.99").await.unwrap_err();
        assert!(err.contains("不存在"), "{err}");
    }
}
