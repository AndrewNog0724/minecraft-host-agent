//! Mojang piston-meta 客户端：版本清单、官方服务端解析（url + sha1 +
//! javaVersion）；镜像经域名重写实现（决议 D115，设计 §8.10）。

use serde::Deserialize;

use super::{read_json, send_get};

/// 官方版本清单基址。
pub const OFFICIAL_META: &str = "https://piston-meta.mojang.com";
/// BMCLAPI 镜像基址（`bmclapi` 预设；S4 fetch 起对外使用）。
#[allow(dead_code)]
pub const BMCLAPI_BASE: &str = "https://bmclapi2.bangbang93.com";

/// 官方资源域前缀（重写匹配；launchermeta / launcher 为老域，piston-data 为二进制域）。
const OFFICIAL_PREFIXES: [&str; 4] = [
    "https://piston-meta.mojang.com/",
    "https://launchermeta.mojang.com/",
    "https://piston-data.mojang.com/",
    "https://launcher.mojang.com/",
];

/// 把官方资源 URL 重写到镜像基址；无镜像或非官方域时原样返回。
pub fn mirror_url(url: &str, mirror_base: Option<&str>) -> String {
    let Some(base) = mirror_base else {
        return url.to_string();
    };
    for prefix in OFFICIAL_PREFIXES {
        if let Some(rest) = url.strip_prefix(prefix) {
            return format!("{base}/{rest}");
        }
    }
    url.to_string()
}

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub latest: ManifestLatest,
    pub versions: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestLatest {
    pub release: String,
    #[allow(dead_code)]
    pub snapshot: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ManifestEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(rename = "releaseTime")]
    #[allow(dead_code)]
    pub release_time: String,
}

#[derive(Debug, Deserialize)]
struct VersionJson {
    #[serde(rename = "javaVersion")]
    java_version: Option<JavaVersionMeta>,
    downloads: Option<DownloadsJson>,
}

#[derive(Debug, Deserialize)]
struct JavaVersionMeta {
    #[serde(rename = "majorVersion")]
    major: u32,
}

#[derive(Debug, Deserialize)]
struct DownloadsJson {
    server: Option<ServerDownloadJson>,
}

#[derive(Debug, Deserialize)]
struct ServerDownloadJson {
    url: String,
    sha1: String,
    size: u64,
}

/// 官方服务端解析结果。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedServer {
    /// 下载 URL（已按镜像重写；S4 fetch 使用）。
    pub url: String,
    /// 重写前的官方 URL（轨迹留档；S4 fetch 使用）。
    pub official_url: String,
    pub sha1: String,
    pub size: u64,
    /// Mojang 官方标注的 Java 大版本（版本 JSON javaVersion 字段，权威来源）。
    pub java_major: Option<u32>,
}

/// Mojang piston-meta 客户端。
pub struct MojangClient<'a> {
    http: &'a reqwest::Client,
    /// 清单/资源基址（生产为官方域，可注入 mock）。
    meta_base: String,
    /// 镜像基址（Some 时重写官方资源 URL）。
    mirror: Option<String>,
}

impl<'a> MojangClient<'a> {
    pub fn new(http: &'a reqwest::Client, mirror: Option<String>) -> Self {
        Self {
            http,
            meta_base: OFFICIAL_META.to_string(),
            mirror,
        }
    }

    /// 测试注入：自定义清单基址（本地 mock）。
    #[allow(dead_code)]
    pub fn with_base(http: &'a reqwest::Client, meta_base: String, mirror: Option<String>) -> Self {
        Self {
            http,
            meta_base: meta_base.trim_end_matches('/').to_string(),
            mirror,
        }
    }

    /// 版本清单（镜像下走镜像域）。
    pub async fn manifest(&self) -> Result<Manifest, String> {
        let url = mirror_url(
            &format!("{}/mc/game/version_manifest_v2.json", self.meta_base),
            self.mirror.as_deref(),
        );
        let response = send_get(self.http, &url).await?;
        read_json(response, "Mojang 版本清单").await
    }

    /// 在清单中查找版本；不存在时给出就近建议（同主版本段，最多 5 条）。
    pub async fn find_entry(&self, version: &str) -> Result<ManifestEntry, String> {
        let manifest = self.manifest().await?;
        if let Some(entry) = manifest.versions.iter().find(|e| e.id == version) {
            return Ok(entry.clone());
        }
        let prefix = version.split('.').next().unwrap_or(version);
        let near: Vec<String> = manifest
            .versions
            .iter()
            .filter(|e| e.id.starts_with(prefix) && e.kind == "release")
            .take(5)
            .map(|e| e.id.clone())
            .collect();
        let hint = if near.is_empty() {
            format!("最新稳定版为 {}", manifest.latest.release)
        } else {
            format!(
                "相近的稳定版：{}（最新稳定版：{}）",
                near.join("、"),
                manifest.latest.release
            )
        };
        Err(format!("版本「{version}」不存在于 Mojang 官方清单；{hint}"))
    }

    /// 解析某版本的官方服务端下载（url + sha1 + javaVersion）。
    pub async fn resolve_server(&self, version: &str) -> Result<ResolvedServer, String> {
        let entry = self.find_entry(version).await?;
        let version_url = mirror_url(&entry.url, self.mirror.as_deref());
        let response = send_get(self.http, &version_url).await?;
        let json: VersionJson = read_json(response, &format!("版本 {version} 详情")).await?;
        let server = json
            .downloads
            .and_then(|d| d.server)
            .ok_or_else(|| format!("版本 {version} 无官方服务端下载（可能是快照或旧实验版）"))?;
        Ok(ResolvedServer {
            url: mirror_url(&server.url, self.mirror.as_deref()),
            official_url: server.url,
            sha1: server.sha1,
            size: server.size,
            java_major: json.java_version.map(|j| j.major),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_rewrites_official_domains() {
        let m = Some(BMCLAPI_BASE);
        assert_eq!(
            mirror_url(
                "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json",
                m
            ),
            "https://bmclapi2.bangbang93.com/mc/game/version_manifest_v2.json"
        );
        assert_eq!(
            mirror_url(
                "https://piston-data.mojang.com/v1/objects/abc/server.jar",
                m
            ),
            "https://bmclapi2.bangbang93.com/v1/objects/abc/server.jar"
        );
        assert_eq!(
            mirror_url("https://launchermeta.mojang.com/a/b", m),
            "https://bmclapi2.bangbang93.com/a/b"
        );
    }

    #[test]
    fn mirror_passthrough_non_official_and_off_mode() {
        let url = "https://api.papermc.io/v2/x";
        assert_eq!(mirror_url(url, Some(BMCLAPI_BASE)), url);
        assert_eq!(mirror_url(url, None), url);
        // 自定义镜像
        assert_eq!(
            mirror_url(
                "https://piston-data.mojang.com/f.jar",
                Some("https://mirror.example.com")
            ),
            "https://mirror.example.com/f.jar"
        );
    }
}
