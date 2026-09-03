//! CurseForge 官方 API v1 客户端（mod 双源扩展；设计 §8.12）。
//!
//! key 为每用户一次性可选配置（`.env` 的 `MCHA_CURSEFORGE_KEY`，随工具上下
//! 文下发，不入仓库）。端点：`POST /v1/mods/search`（gameId=432 + Fabric 过
//! 滤）、`GET /v1/mods/{id}`、`GET /v1/mods/{id}/files`、`POST /v1/mods/files`
//! （按 fileId 批量重取，安装期权威数据源）。哈希为 sha1 单哈希（algo=1），
//! 强度低于 Modrinth 双哈希——输出轨迹如实标注。
//!
//! 已裁定排除的替代路径（答辩备查）：中心代理（违反条款 / 配额单桶 / 单点 /
//! key 公开）与网页抓取（Cloudflare / 无权威哈希 / 脆弱）。

use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{read_json, urlencode};

/// 官方 API 基址。
pub const OFFICIAL_API: &str = "https://api.curseforge.com";
/// Minecraft 的 gameId（CurseForge 平台唯一）。
pub const MINECRAFT_GAME_ID: i64 = 432;
/// Fabric 的 modLoaderType 枚举值。
pub const FABRIC_LOADER_TYPE: i64 = 4;
/// 下载 CDN 域（对 API 下发 `downloadUrl` 的域强校验）。
pub const CDN_HOSTS: &[&str] = &["mediafilez.forgecdn.net", "media.forgecdn.net"];

/// 单请求超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// 响应类型
// ---------------------------------------------------------------------------

/// 项目（mod）记录（search / 详情共用字段）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfProject {
    pub id: i64,
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub download_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<CfProject>,
}

/// 文件哈希条目（algo：1 = sha1，2 = md5）。
#[derive(Debug, Clone, Deserialize)]
pub struct CfHash {
    pub algo: i32,
    pub value: String,
}

/// 文件依赖（relationType：3 = required）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfDependency {
    pub mod_id: i64,
    #[serde(default)]
    pub relation_type: i32,
}

/// 项目文件记录。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CfFile {
    pub id: i64,
    #[serde(default)]
    pub mod_id: i64,
    #[serde(default)]
    pub display_name: String,
    pub file_name: String,
    /// 项目未开放第三方分发时为 null → 结构化报错指向手动下载页。
    #[serde(default)]
    pub download_url: Option<String>,
    #[serde(default)]
    pub file_length: u64,
    #[serde(default)]
    pub hashes: Vec<CfHash>,
    /// 含 MC 版本与加载器名（如 "1.21.1"、"Fabric"），兼容复核用。
    #[serde(default)]
    pub game_versions: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<CfDependency>,
}

impl CfFile {
    /// sha1（algo=1）；缺失时由调用方如实报错。
    pub fn sha1(&self) -> Option<&str> {
        self.hashes
            .iter()
            .find(|h| h.algo == 1)
            .map(|h| h.value.as_str())
    }

    /// 是否覆盖指定 MC 版本与加载器（gameVersions 同时含两者）。
    pub fn covers(&self, mc_version: &str, loader: &str) -> bool {
        self.game_versions
            .iter()
            .any(|v| v.eq_ignore_ascii_case(mc_version))
            && self
                .game_versions
                .iter()
                .any(|v| v.eq_ignore_ascii_case(loader))
    }

    /// required 依赖（relationType=3）的 modId 列表。
    pub fn required_dependencies(&self) -> Vec<i64> {
        self.dependencies
            .iter()
            .filter(|d| d.relation_type == 3)
            .map(|d| d.mod_id)
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct SearchBody<'a> {
    #[serde(rename = "gameId")]
    game_id: i64,
    #[serde(rename = "searchFilter")]
    search_filter: &'a str,
    #[serde(rename = "gameVersion")]
    game_version: &'a str,
    #[serde(rename = "modLoaderType")]
    mod_loader_type: i64,
    pagination: Pagination,
}

#[derive(Debug, Serialize)]
struct Pagination {
    index: i64,
    #[serde(rename = "pageSize")]
    page_size: i64,
}

#[derive(Debug, Serialize)]
struct FilesBody {
    #[serde(rename = "fileIds")]
    file_ids: Vec<i64>,
}

/// 包一层响应壳（{ "data": [...] } 形态）。
#[derive(Debug, Deserialize)]
struct DataVec<T> {
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct DataOne<T> {
    data: T,
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

pub struct CfClient<'a> {
    http: &'a reqwest::Client,
    api_base: String,
    key: String,
}

impl<'a> CfClient<'a> {
    pub fn new(http: &'a reqwest::Client, key: String) -> Self {
        Self {
            http,
            api_base: OFFICIAL_API.to_string(),
            key,
        }
    }

    /// 测试注入：自定义 API 基址（本地 mock；key 随意）。
    #[allow(dead_code)]
    pub fn with_base(http: &'a reqwest::Client, api_base: String, key: String) -> Self {
        Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            key,
        }
    }

    async fn get(&self, url: &str, what: &str) -> Result<reqwest::Response, String> {
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.http
                .get(url)
                .header("x-api-key", &self.key)
                .header("Accept", "application/json")
                .send(),
        )
        .await
        .map_err(|_| format!("请求超时（{url}）"))?
        .map_err(|err| format!("请求失败（{url}）：{err}"))?;
        let status = response.status();
        if status.as_u16() == 403 {
            return Err(format!(
                "{what} 被拒绝（HTTP 403）：CurseForge Key 无效或未配置"
            ));
        }
        if status.as_u16() == 429 {
            return Err(format!("{what} 触发限额（HTTP 429）：请稍后重试"));
        }
        Ok(response)
    }

    async fn post_json<T: serde::de::DeserializeOwned, B: Serialize>(
        &self,
        url: &str,
        what: &str,
        body: &B,
    ) -> Result<T, String> {
        let response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.http
                .post(url)
                .header("x-api-key", &self.key)
                .header("Accept", "application/json")
                .json(body)
                .send(),
        )
        .await
        .map_err(|_| format!("请求超时（{url}）"))?
        .map_err(|err| format!("请求失败（{url}）：{err}"))?;
        let status = response.status();
        if status.as_u16() == 403 {
            return Err(format!(
                "{what} 被拒绝（HTTP 403）：CurseForge Key 无效或未配置"
            ));
        }
        if status.as_u16() == 429 {
            return Err(format!("{what} 触发限额（HTTP 429）：请稍后重试"));
        }
        read_json(response, what).await
    }

    /// 检索 mod（名称 / slug 过滤 + MC 版本 + Fabric）。
    pub async fn search(
        &self,
        filter: &str,
        game_version: &str,
        limit: usize,
    ) -> Result<Vec<CfProject>, String> {
        let url = format!("{}/v1/mods/search", self.api_base);
        let body = SearchBody {
            game_id: MINECRAFT_GAME_ID,
            search_filter: filter,
            game_version,
            mod_loader_type: FABRIC_LOADER_TYPE,
            pagination: Pagination {
                index: 0,
                page_size: limit.clamp(1, 50) as i64,
            },
        };
        let response: SearchResponse = self
            .post_json(&url, &format!("CurseForge 检索（{filter}）"), &body)
            .await?;
        Ok(response.data)
    }

    /// 项目详情（依赖闭包回查 slug 用）。
    pub async fn mod_detail(&self, mod_id: i64) -> Result<CfProject, String> {
        let url = format!("{}/v1/mods/{}", self.api_base, mod_id);
        let response = self
            .get(&url, &format!("CurseForge 项目（{mod_id}）"))
            .await?;
        if response.status().as_u16() == 404 {
            return Err(format!("CurseForge 上不存在项目 {mod_id}"));
        }
        let parsed: DataOne<CfProject> =
            read_json(response, &format!("CurseForge 项目（{mod_id}）")).await?;
        Ok(parsed.data)
    }

    /// 项目文件列表（MC 版本 + Fabric 过滤，新到旧）。
    pub async fn mod_files(&self, mod_id: i64, game_version: &str) -> Result<Vec<CfFile>, String> {
        let url = format!(
            "{}/v1/mods/{}/files?gameVersion={}&modLoaderType={}",
            self.api_base,
            mod_id,
            urlencode(game_version),
            FABRIC_LOADER_TYPE
        );
        let response = self
            .get(&url, &format!("CurseForge 文件列表（{mod_id}）"))
            .await?;
        if response.status().as_u16() == 404 {
            return Err(format!("CurseForge 上不存在项目 {mod_id}"));
        }
        let parsed: DataVec<CfFile> =
            read_json(response, &format!("CurseForge 文件列表（{mod_id}）")).await?;
        Ok(parsed.data)
    }

    /// 按 fileId 批量取文件（安装期权威重取；顺序不保证与入参一致）。
    pub async fn files_by_ids(&self, ids: &[i64]) -> Result<Vec<CfFile>, String> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/v1/mods/files", self.api_base);
        let parsed: DataVec<CfFile> = self
            .post_json(
                &url,
                "CurseForge 批量文件",
                &FilesBody {
                    file_ids: ids.to_vec(),
                },
            )
            .await?;
        Ok(parsed.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_helpers_extract_sha1_and_required_deps() {
        let file = CfFile {
            id: 1,
            mod_id: 42,
            display_name: "TF 1.2".into(),
            file_name: "twilightforest-1.2.jar".into(),
            download_url: Some("https://mediafilez.forgecdn.net/files/a.jar".into()),
            file_length: 10,
            hashes: vec![
                CfHash {
                    algo: 1,
                    value: "aa".into(),
                },
                CfHash {
                    algo: 2,
                    value: "bb".into(),
                },
            ],
            game_versions: vec!["1.21.1".into(), "Fabric".into()],
            dependencies: vec![
                CfDependency {
                    mod_id: 7,
                    relation_type: 3,
                },
                CfDependency {
                    mod_id: 9,
                    relation_type: 2,
                },
            ],
        };
        assert_eq!(file.sha1(), Some("aa"));
        assert!(file.covers("1.21.1", "fabric"), "加载器比较应忽略大小写");
        assert_eq!(file.required_dependencies(), vec![7]);
    }

    /// 最小 HTTP mock：POST /v1/mods/search、GET files、POST files。
    fn spawn_cf_mock() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let search_body = r#"{"data":[{"id":227639,"slug":"the-twilight-forest","name":"The Twilight Forest","summary":"一座魔法森林","download_count":9}]}"#;
            let files_body = r#"{"data":[{"id":5566,"displayName":"TF","fileName":"tf.jar","downloadUrl":"http://ADDR/files/tf.jar","fileLength":8,"hashes":[{"algo":1,"value":"9a"}],"gameVersions":["1.21.1","Fabric"],"dependencies":[{"modId":777,"relationType":3}]}]}"#.to_string();
            let files_body = files_body.replace("ADDR", &format!("{addr}"));
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf);
                let target = request.split_whitespace().nth(1).unwrap_or("");
                let has_key = request.contains("x-api-key: test-key");
                let (status, body) = if !has_key {
                    ("403 Forbidden", String::new())
                } else if target.starts_with("/v1/mods/search") {
                    ("200 OK", search_body.to_string())
                } else if target.contains("/files") && !target.starts_with("/v1/mods/files") {
                    ("200 OK", files_body.clone())
                } else if target.starts_with("/v1/mods/files") {
                    ("200 OK", files_body.clone())
                } else {
                    ("404 Not Found", String::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn search_and_files_roundtrip() {
        let base = spawn_cf_mock();
        let http = reqwest::Client::new();
        let client = CfClient::with_base(&http, base, "test-key".to_string());
        let projects = client
            .search("twilight forest", "1.21.1", 10)
            .await
            .unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].slug, "the-twilight-forest");

        let files = client.mod_files(227639, "1.21.1").await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].sha1(), Some("9a"));
        assert_eq!(files[0].required_dependencies(), vec![777]);

        let by_ids = client.files_by_ids(&[5566]).await.unwrap();
        assert_eq!(by_ids[0].file_name, "tf.jar");
    }

    #[tokio::test]
    async fn bad_key_is_structured_error() {
        let base = spawn_cf_mock();
        let http = reqwest::Client::new();
        let client = CfClient::with_base(&http, base, "wrong".to_string());
        let err = client.search("x", "1.21.1", 5).await.unwrap_err();
        assert!(err.contains("403"), "{err}");
    }
}
