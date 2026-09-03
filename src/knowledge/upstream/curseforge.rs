//! CurseForge API v1 客户端（mod 双源扩展；设计 §8.12）。
//!
//! **双基址**：官方（`api.curseforge.com`，需 key）与国内镜像（`mod.mcimirror.top`，
//! 开源公益项目 mcmod-info-mirror，免 key）。`CfClient::new` 按用户是否配置
//! `MCHA_CURSEFORGE_KEY`（`.env`，随工具上下文下发，不入仓库）自动选择——
//! 有 key 走官方，无 key 自动走镜像，CF 独占 mod 开箱可用。镜像与官方 API
//! 同构（实测逐端点核验 + sha1 与实文件一致）；轨迹中如实标注通道。
//!
//! 端点：`GET /v1/mods/search`（官方文档原生支持；镜像仅支持 GET）、
//! `GET /v1/mods/{id}`、`GET /v1/mods/{id}/files`、`POST /v1/mods/files`
//! （按 fileId 批量重取，安装期权威数据源）。哈希为 sha1 单哈希（algo=1），
//! 强度低于 Modrinth 双哈希——输出轨迹如实标注。
//!
//! 已裁定排除的替代路径（答辩备查）：自建 key 池中心代理（违反条款 / 配额单
//! 桶 / 单点 / key 公开）与网页抓取（Cloudflare / 无权威哈希 / 脆弱）。

use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{read_json, urlencode};

/// 官方 API 基址。
pub const OFFICIAL_API: &str = "https://api.curseforge.com";
/// 国内镜像基址（开源公益项目 mcmod-info-mirror；与官方 API v1 同构，免 key）。
pub const MIRROR_API: &str = "https://mod.mcimirror.top/curseforge";
/// Minecraft 的 gameId（CurseForge 平台唯一）。
pub const MINECRAFT_GAME_ID: i64 = 432;
/// Fabric 的 modLoaderType 枚举值。
pub const FABRIC_LOADER_TYPE: i64 = 4;
/// 下载 CDN 域（对 API 下发 `downloadUrl` 的域强校验；官方 API 实测返回
/// edge 域，重定向落到 mediafilez 域，两者同入白名单）。
pub const CDN_HOSTS: &[&str] = &[
    "edge.forgecdn.net",
    "mediafilez.forgecdn.net",
    "media.forgecdn.net",
];

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
    /// 是否走国内镜像（无 key 自动切换；轨迹标注用）。
    mirror: bool,
}

impl<'a> CfClient<'a> {
    /// 按用户配置自动选择通道：有 key → 官方 API；无 key → 国内镜像（免 key）。
    pub fn new(http: &'a reqwest::Client, key: String) -> Self {
        let mirror = key.trim().is_empty();
        Self {
            http,
            api_base: if mirror {
                MIRROR_API.to_string()
            } else {
                OFFICIAL_API.to_string()
            },
            key,
            mirror,
        }
    }

    /// 测试注入：自定义 API 基址（本地 mock；key 可空）。
    #[allow(dead_code)]
    pub fn with_base(http: &'a reqwest::Client, api_base: String, key: String) -> Self {
        Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            key,
            mirror: false,
        }
    }

    /// 通道标注（轨迹/错误消息中如实呈现数据来源）。
    fn channel(&self) -> &'static str {
        if self.mirror {
            "经社区镜像"
        } else {
            "官方 API"
        }
    }

    async fn get(&self, url: &str, what: &str) -> Result<reqwest::Response, String> {
        let mut request = self.http.get(url).header("Accept", "application/json");
        if !self.key.trim().is_empty() {
            request = request.header("x-api-key", self.key.trim());
        }
        let response = tokio::time::timeout(REQUEST_TIMEOUT, request.send())
            .await
            .map_err(|_| format!("请求超时（{url}）"))?
            .map_err(|err| format!("请求失败（{url}）：{err}"))?;
        let status = response.status();
        if status.as_u16() == 403 {
            return Err(format!(
                "{what} 被拒绝（HTTP 403）：{}",
                if self.mirror {
                    "镜像拒绝访问，请稍后重试或改用官方 Key 通道"
                } else {
                    "CurseForge Key 无效或未配置"
                }
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
        let mut request = self.http.post(url).header("Accept", "application/json");
        if !self.key.trim().is_empty() {
            request = request.header("x-api-key", self.key.trim());
        }
        let response = tokio::time::timeout(REQUEST_TIMEOUT, request.json(body).send())
            .await
            .map_err(|_| format!("请求超时（{url}）"))?
            .map_err(|err| format!("请求失败（{url}）：{err}"))?;
        let status = response.status();
        if status.as_u16() == 403 {
            return Err(format!(
                "{what} 被拒绝（HTTP 403）：{}",
                if self.mirror {
                    "镜像拒绝访问，请稍后重试或改用官方 Key 通道"
                } else {
                    "CurseForge Key 无效或未配置"
                }
            ));
        }
        if status.as_u16() == 429 {
            return Err(format!("{what} 触发限额（HTTP 429）：请稍后重试"));
        }
        read_json(response, what).await
    }

    /// 检索 mod（名称 / slug 过滤 + MC 版本 + Fabric）。
    ///
    /// 统一用 `GET /v1/mods/search`：官方文档原生支持；镜像仅支持 GET。
    pub async fn search(
        &self,
        filter: &str,
        game_version: &str,
        limit: usize,
    ) -> Result<Vec<CfProject>, String> {
        let url = format!(
            "{}/v1/mods/search?gameId={}&searchFilter={}&gameVersion={}&modLoaderType={}&index=0&pageSize={}",
            self.api_base,
            MINECRAFT_GAME_ID,
            urlencode(filter),
            urlencode(game_version),
            FABRIC_LOADER_TYPE,
            limit.clamp(1, 50)
        );
        let response = self
            .get(
                &url,
                &format!("CurseForge 检索（{filter}）· {}", self.channel()),
            )
            .await?;
        if response.status().as_u16() == 404 {
            return Err(format!(
                "CurseForge 检索端点不可用（HTTP 404，通道：{}）",
                self.channel()
            ));
        }
        let parsed: SearchResponse = read_json(
            response,
            &format!("CurseForge 检索（{filter}）· {}", self.channel()),
        )
        .await?;
        Ok(parsed.data)
    }

    /// 项目详情（依赖闭包回查 slug 用）。
    pub async fn mod_detail(&self, mod_id: i64) -> Result<CfProject, String> {
        let url = format!("{}/v1/mods/{}", self.api_base, mod_id);
        let what = format!("CurseForge 项目（{mod_id}）· {}", self.channel());
        let response = self.get(&url, &what).await?;
        if response.status().as_u16() == 404 {
            return Err(format!("CurseForge 上不存在项目 {mod_id}"));
        }
        let parsed: DataOne<CfProject> = read_json(response, &what).await?;
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
        let what = format!("CurseForge 文件列表（{mod_id}）· {}", self.channel());
        let response = self.get(&url, &what).await?;
        if response.status().as_u16() == 404 {
            return Err(format!("CurseForge 上不存在项目 {mod_id}"));
        }
        let parsed: DataVec<CfFile> = read_json(response, &what).await?;
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
                &format!("CurseForge 批量文件 · {}", self.channel()),
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

    #[test]
    fn channel_selection_by_key_presence() {
        let http = reqwest::Client::new();
        // 有 key → 官方基址；无 key → 国内镜像基址
        let official = CfClient::new(&http, "some-key".to_string());
        assert_eq!(official.api_base, OFFICIAL_API);
        assert_eq!(official.channel(), "官方 API");
        let mirror = CfClient::new(&http, String::new());
        assert_eq!(mirror.api_base, MIRROR_API);
        assert_eq!(mirror.channel(), "经社区镜像");
        // 测试注入基址不受 key 有无影响
        let custom = CfClient::with_base(&http, "http://127.0.0.1:9/".into(), String::new());
        assert_eq!(custom.api_base, "http://127.0.0.1:9");
        assert_eq!(custom.channel(), "官方 API");
    }

    /// 镜像通道 mock：免 key（不带 x-api-key 也放行）；POST 一律 404
    ///（镜像不支持 POST search 的回归护栏）。
    fn spawn_mirror_mock() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let search_body = r#"{"data":[{"id":227639,"slug":"the-twilight-forest","name":"The Twilight Forest","summary":"一座魔法森林","download_count":9}]}"#;
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf);
                let method = request.split_whitespace().next().unwrap_or("");
                let target = request.split_whitespace().nth(1).unwrap_or("");
                let (status, body) = if method == "POST" {
                    ("404 Not Found", String::new())
                } else if target.starts_with("/v1/mods/search") {
                    ("200 OK", search_body.to_string())
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
    async fn mirror_channel_get_search_without_key() {
        let base = spawn_mirror_mock();
        let http = reqwest::Client::new();
        let client = CfClient::with_base(&http, base, String::new());
        let projects = client
            .search("twilight forest", "1.21.1", 10)
            .await
            .unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].slug, "the-twilight-forest");
        // 镜像不支持 POST search：批量端点应得到 404 的结构化报错而非 panic
        let err = client.files_by_ids(&[5566]).await.unwrap_err();
        assert!(err.contains("失败") || err.contains("404"), "{err}");
    }
}
