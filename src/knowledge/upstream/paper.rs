//! PaperMC Fill API v3 客户端：版本存在性、最新构建与官方 sha256（设计 §8.10）。
//!
//! 旧 v2 API（api.papermc.io）已返回 410 Gone，实测迁移到 fill.papermc.io v3：
//! `GET /v3/projects/paper/versions/{v}/builds/latest`，产物位于
//! `downloads["server:default"]`（name / size / url / checksums.sha256）。

use serde::Deserialize;

use super::{read_json, send_get, urlencode};

/// 官方 Fill API 基址。
pub const OFFICIAL_API: &str = "https://fill.papermc.io/v3";

#[derive(Debug, Deserialize)]
struct BuildJson {
    id: u32,
    downloads: DownloadsJson,
}

#[derive(Debug, Deserialize)]
struct DownloadsJson {
    #[serde(rename = "server:default")]
    server_default: ArtifactJson,
}

#[derive(Debug, Deserialize)]
struct ArtifactJson {
    name: String,
    url: String,
    size: u64,
    checksums: ChecksumsJson,
}

#[derive(Debug, Deserialize)]
struct ChecksumsJson {
    sha256: String,
}

/// Paper 服务端解析结果（官方 sha256 强校验）。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedPaper {
    /// 构建号（Fill API 的 id 字段）。
    pub build: u32,
    /// 下载 URL（官方 fill-data 分发域）。
    pub url: String,
    /// 官方 sha256（S4 fetch 强校验）。
    pub sha256: String,
    pub file_name: String,
    pub size: u64,
}

pub struct PaperClient<'a> {
    http: &'a reqwest::Client,
    api_base: String,
}

impl<'a> PaperClient<'a> {
    pub fn new(http: &'a reqwest::Client) -> Self {
        Self {
            http,
            api_base: OFFICIAL_API.to_string(),
        }
    }

    /// 测试注入：自定义 API 基址（本地 mock）。
    #[allow(dead_code)]
    pub fn with_base(http: &'a reqwest::Client, api_base: String) -> Self {
        Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
        }
    }

    /// 解析某 MC 版本的最新 Paper 构建；版本不存在（404）时给结构化错误。
    pub async fn latest_build(&self, mc_version: &str) -> Result<ResolvedPaper, String> {
        let url = format!(
            "{}/projects/paper/versions/{}/builds/latest",
            self.api_base,
            urlencode(mc_version)
        );
        let response = send_get(self.http, &url).await?;
        if response.status().as_u16() == 404 {
            return Err(format!(
                "Paper 不支持 MC {mc_version}（该版本在 Paper 项目中不存在）"
            ));
        }
        let build: BuildJson =
            read_json(response, &format!("Paper 最新构建（{mc_version}）")).await?;
        Ok(ResolvedPaper {
            build: build.id,
            url: build.downloads.server_default.url,
            sha256: build
                .downloads
                .server_default
                .checksums
                .sha256
                .to_ascii_lowercase(),
            file_name: build.downloads.server_default.name,
            size: build.downloads.server_default.size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_sample() -> BuildJson {
        // 实测响应样本（2026-09-02，1.21.1#133）：字段形状的回归锚点
        serde_json::from_value(serde_json::json!({
            "id": 133,
            "time": "2025-03-28T16:16:41.212Z",
            "channel": "STABLE",
            "downloads": {
                "server:default": {
                    "name": "paper-1.21.1-133.jar",
                    "checksums": { "sha256": "39bd8c00b9e18de91dcabd3cc3dcfa5328685a53b7187a2f63280c22e2d287b9" },
                    "size": 49394394,
                    "url": "https://fill-data.papermc.io/v1/objects/39bd/paper-1.21.1-133.jar"
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn fill_v3_sample_shape_parses() {
        let build = parse_sample();
        assert_eq!(build.id, 133);
        assert_eq!(build.downloads.server_default.name, "paper-1.21.1-133.jar");
        assert_eq!(build.downloads.server_default.size, 49_394_394);
        assert_eq!(build.downloads.server_default.checksums.sha256.len(), 64);
    }
}
