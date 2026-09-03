//! Modrinth API v2 客户端（mod 场景的 L2 实时事实来源；设计 §8.12）。
//!
//! 端点：
//! - `GET /v2/search`——检索（facets：`project_type:mod`，可选 `versions` / `categories`）
//! - `GET /v2/project/{slug}/version`——项目版本列表（`game_versions` / `loaders` 过滤）
//! - `GET /v2/versions?ids=[..]`——批量重取（安装期权威数据源）
//!
//! 免 key；失败一律结构化字符串回传（工具再回传 Agent）。
//! 下载 URL 的域必须为 `cdn.modrinth.com`（安装前强校验，设计 §12）。

use serde::Deserialize;

use super::{read_json, send_get, urlencode};

/// 官方 API 基址。
pub const OFFICIAL_API: &str = "https://api.modrinth.com";
/// 下载 CDN 域（install_mods 对 API 下发 url 的域强校验）。
pub const CDN_HOST: &str = "cdn.modrinth.com";

// ---------------------------------------------------------------------------
// 响应类型
// ---------------------------------------------------------------------------

/// 检索命中（/v2/search 的 hits 元素）。
#[derive(Debug, Clone, Deserialize)]
pub struct SearchHit {
    pub slug: String,
    pub title: String,
    pub description: String,
    /// 项目分类标签（含加载器与玩法标签，如 `fabric` / `adventure`）。
    #[serde(default)]
    pub categories: Vec<String>,
    pub downloads: u64,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
}

/// 项目摘要（/v2/project/{slug}，别名命中时直接定位项目用）。
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectSummary {
    pub slug: String,
    pub title: String,
    pub description: String,
    pub downloads: u64,
}

/// 依赖声明（version.dependencies 元素）。
#[derive(Debug, Clone, Deserialize)]
pub struct Dependency {
    #[serde(default)]
    pub project_id: Option<String>,
    /// required | optional | incompatible | embedded（仅 required 入闭包）。
    #[serde(default)]
    pub dependency_type: Option<String>,
}

/// 版本下载文件（version.files 元素）。
#[derive(Debug, Clone, Deserialize)]
pub struct VersionFile {
    pub url: String,
    pub filename: String,
    pub primary: bool,
    pub hashes: FileHashes,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileHashes {
    pub sha1: String,
    pub sha512: String,
}

/// 项目版本（安装与闭包解析的核心事实）。
#[derive(Debug, Clone, Deserialize)]
pub struct ModVersion {
    pub id: String,
    pub project_id: String,
    pub version_number: String,
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub files: Vec<VersionFile>,
}

impl ModVersion {
    /// 主文件（primary 优先，否则首个）；Modrinth 每版本必有至少一个文件，
    /// 异常缺失返回 None 由调用方结构化报错。
    pub fn primary_file(&self) -> Option<&VersionFile> {
        self.files
            .iter()
            .find(|f| f.primary)
            .or_else(|| self.files.first())
    }

    /// 该版本是否覆盖指定 MC 版本与加载器（确定性复核与闭包共用）。
    pub fn covers(&self, mc_version: &str, loader: &str) -> bool {
        self.game_versions.iter().any(|v| v == mc_version)
            && self.loaders.iter().any(|l| l.eq_ignore_ascii_case(loader))
    }
}

// ---------------------------------------------------------------------------
// 请求构造（纯函数，便于单测）
// ---------------------------------------------------------------------------

/// 构造 search 的 facets 参数：`[["project_type:mod"],["versions:1.21.1"],…]`。
pub(crate) fn facets_json(mc_version: Option<&str>, loader: Option<&str>) -> String {
    let mut facets: Vec<Vec<String>> = vec![vec!["project_type:mod".to_string()]];
    if let Some(v) = mc_version.filter(|v| !v.trim().is_empty()) {
        facets.push(vec![format!("versions:{}", v.trim())]);
    }
    if let Some(l) = loader.filter(|l| !l.trim().is_empty()) {
        facets.push(vec![format!("categories:{}", l.trim())]);
    }
    serde_json::to_string(&facets).unwrap_or_else(|_| r#"[["project_type:mod"]]"#.to_string())
}

/// 从 URL 提取主机名（小写）；非法 URL 返回 None。
pub(crate) fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', ':', '?', '#']).next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

pub struct ModrinthClient<'a> {
    http: &'a reqwest::Client,
    api_base: String,
}

impl<'a> ModrinthClient<'a> {
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

    /// 检索 mod 项目（query 可空；mc_version / loader 作为 facets 过滤）。
    pub async fn search(
        &self,
        query: Option<&str>,
        mc_version: Option<&str>,
        loader: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SearchHit>, String> {
        let mut url = format!(
            "{}/v2/search?limit={}&facets={}",
            self.api_base,
            limit.clamp(1, 100),
            urlencode(&facets_json(mc_version, loader))
        );
        if let Some(q) = query.map(str::trim).filter(|q| !q.is_empty()) {
            url.push_str(&format!("&query={}", urlencode(q)));
        }
        let response = send_get(self.http, &url).await?;
        let parsed: SearchResponse = read_json(response, "Modrinth 检索").await?;
        Ok(parsed.hits)
    }

    /// 项目详情（别名命中 → 直接定位项目，免检索歧义）。
    pub async fn project(&self, slug_or_id: &str) -> Result<ProjectSummary, String> {
        let url = format!("{}/v2/project/{}", self.api_base, urlencode(slug_or_id));
        let response = send_get(self.http, &url).await?;
        if response.status().as_u16() == 404 {
            return Err(format!("Modrinth 上不存在项目「{slug_or_id}」"));
        }
        read_json(response, &format!("Modrinth 项目（{slug_or_id}）")).await
    }

    /// 项目的版本列表（新到旧），可按 MC 版本与加载器过滤。
    /// 项目不存在返回结构化错误；无匹配版本返回空列表。
    pub async fn project_versions(
        &self,
        slug_or_id: &str,
        mc_version: Option<&str>,
        loader: Option<&str>,
    ) -> Result<Vec<ModVersion>, String> {
        let mut url = format!(
            "{}/v2/project/{}/version",
            self.api_base,
            urlencode(slug_or_id)
        );
        let mut query: Vec<String> = Vec::new();
        if let Some(v) = mc_version.map(str::trim).filter(|v| !v.is_empty()) {
            query.push(format!(
                "game_versions={}",
                urlencode(&serde_json::to_string(&[v]).unwrap_or_default())
            ));
        }
        if let Some(l) = loader.map(str::trim).filter(|l| !l.is_empty()) {
            query.push(format!(
                "loaders={}",
                urlencode(&serde_json::to_string(&[l]).unwrap_or_default())
            ));
        }
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query.join("&"));
        }
        let response = send_get(self.http, &url).await?;
        if response.status().as_u16() == 404 {
            return Err(format!("Modrinth 上不存在项目「{slug_or_id}」"));
        }
        read_json(response, &format!("Modrinth 版本列表（{slug_or_id}）")).await
    }

    /// 按 version id 批量取版本（安装期权威重取；顺序不保证与入参一致）。
    pub async fn versions_by_ids(&self, ids: &[String]) -> Result<Vec<ModVersion>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids_json = serde_json::to_string(ids).unwrap_or_default();
        let url = format!("{}/v2/versions?ids={}", self.api_base, urlencode(&ids_json));
        let response = send_get(self.http, &url).await?;
        read_json(response, "Modrinth 批量版本").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facets_include_type_and_filters() {
        let facets = facets_json(Some("1.21.1"), Some("fabric"));
        assert_eq!(
            facets,
            r#"[["project_type:mod"],["versions:1.21.1"],["categories:fabric"]]"#
        );
        // 空过滤值不进 facets
        assert_eq!(facets_json(Some(""), None), r#"[["project_type:mod"]]"#);
    }

    #[test]
    fn url_host_extracts_lowercase_host() {
        assert_eq!(
            url_host("https://cdn.modrinth.com/data/xxx.jar"),
            Some("cdn.modrinth.com".into())
        );
        assert_eq!(
            url_host("https://EVIL.example.com/a"),
            Some("evil.example.com".into())
        );
        assert_eq!(url_host("ftp://x"), None);
        assert_eq!(url_host("notaurl"), None);
    }

    /// 最小 HTTP mock：按路径返回预置响应体（与 llm/client.rs 测试同套路）。
    fn spawn_mock(routes: Vec<(String, u16, String)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for (path_prefix, status, body) in routes {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf);
                let target = request.split_whitespace().nth(1).unwrap_or("");
                let (head, reason) = if status == 200 {
                    ("HTTP/1.1 200 OK", "OK")
                } else {
                    ("HTTP/1.1 404 Not Found", "Not Found")
                };
                if target.starts_with(&path_prefix) {
                    let response = format!(
                        "{head}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                } else {
                    let _ = stream.write_all(
                        format!("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n{reason}").as_bytes(),
                    );
                }
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn search_parses_hits_and_sends_facets() {
        let base = spawn_mock(vec![(
            "/v2/search".into(),
            200,
            r#"{"hits":[{"project_id":"AAbbCCdd","slug":"twilightforest","title":"The Twilight Forest","description":"一座魔法森林","categories":["fabric","adventure"],"downloads":12345}],"offset":0,"limit":5,"total_hits":1}"#.into(),
        )]);
        let http = reqwest::Client::new();
        let client = ModrinthClient::with_base(&http, base);
        let hits = client
            .search(Some("暮色森林"), Some("1.21.1"), Some("fabric"), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "twilightforest");
        assert_eq!(hits[0].downloads, 12345);
    }

    #[tokio::test]
    async fn project_versions_404_is_structured_error() {
        let base = spawn_mock(vec![("/v2/project/nope/version".into(), 404, "[]".into())]);
        let http = reqwest::Client::new();
        let client = ModrinthClient::with_base(&http, base);
        let err = client
            .project_versions("nope", Some("1.21.1"), None)
            .await
            .unwrap_err();
        assert!(err.contains("不存在"), "{err}");
    }

    #[tokio::test]
    async fn versions_by_ids_parses_files_and_hashes() {
        let base = spawn_mock(vec![(
            "/v2/versions".into(),
            200,
            r#"[{"id":"v1","project_id":"p1","version_number":"1.2","game_versions":["1.21.1"],"loaders":["fabric"],"dependencies":[],"files":[{"url":"https://cdn.modrinth.com/data/x.jar","filename":"x.jar","primary":true,"hashes":{"sha1":"aa","sha512":"bb"},"size":10}]}]"#.into(),
        )]);
        let http = reqwest::Client::new();
        let client = ModrinthClient::with_base(&http, base);
        let versions = client.versions_by_ids(&["v1".to_string()]).await.unwrap();
        assert_eq!(versions.len(), 1);
        let file = versions[0].primary_file().unwrap();
        assert_eq!(file.hashes.sha1, "aa");
        assert!(versions[0].covers("1.21.1", "fabric"));
        assert!(!versions[0].covers("1.20.4", "fabric"));
    }

    /// 真实上游冒烟（`cargo test --ignored`）。
    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_search_and_versions_jei() {
        let http = reqwest::Client::builder()
            .user_agent("mcha/0.2")
            .build()
            .unwrap();
        let client = ModrinthClient::new(&http);
        let hits = client
            .search(Some("jei"), Some("1.21.1"), Some("fabric"), 5)
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.slug == "jei"),
            "应检索到 jei：{:?}",
            hits.iter().map(|h| &h.slug).collect::<Vec<_>>()
        );
        let versions = client
            .project_versions("jei", Some("1.21.1"), Some("fabric"))
            .await
            .unwrap();
        let latest = versions.first().expect("1.21.1/fabric 应有兼容版本");
        let file = latest.primary_file().expect("版本应有下载文件");
        assert_eq!(file.hashes.sha1.len(), 40);
        assert_eq!(
            super::url_host(&file.url).as_deref(),
            Some(super::CDN_HOST),
            "下载 URL 必须指向官方 CDN"
        );
    }
}
