//! 上游 API 客户端（L2 易变事实）：Mojang / PaperMC / Fabric / Modrinth / Adoptium。
//!
//! 统一约束：代理与镜像在 [`HttpBase`] 一层注入（§8.4）；
//! 所有下载 URL 官方域白名单来源，镜像只做域名替换（§12）；
//! LLM 不得直接决定任何下载，只能引用这里返回的结果。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::spec::ModRef;

#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("网络请求失败：{0}")]
    Http(#[from] reqwest::Error),
    #[error("请求 {url} 失败（HTTP {status}）：{body}")]
    Status {
        url: String,
        status: u16,
        body: String,
    },
    #[error("解析 {url} 的 JSON 响应失败：{source}")]
    Json {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("文件 {file} 哈希校验失败：期望 {expected}，实际 {actual}")]
    HashMismatch {
        file: String,
        expected: String,
        actual: String,
    },
    #[error("上游响应缺少预期字段：{0}")]
    BadResponse(String),
    #[error(
        "mod {project} 没有 MC {mc}/{loader} 的可用构建（该 mod 当前最高支持 {latest_supported}）。\
         可询问玩家是否换用其它 mod、降低 MC 版本，或改玩无 mod 服务器"
    )]
    NoCompatibleVersion {
        project: String,
        mc: String,
        loader: String,
        latest_supported: String,
    },
    #[error("下载被取消")]
    Cancelled,
}

/// 待下载文件（含校验信息，是"版本校验管线"的产出物）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DownloadItem {
    pub url: String,
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub file_name: String,
    pub kind: DownloadKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadKind {
    /// 服务端主 jar（原版 / Paper / Spigot / Fabric bundle）
    ServerJar,
    /// 受管 JRE zip（§8.8）
    JavaJre,
    /// mod jar
    Mod,
}

/// 共享 HTTP 底座：代理、镜像替换、超时统一在此。
/// 可克隆（reqwest::Client 内部为引用计数），各上游客户端共享同一底座。
#[derive(Clone)]
pub struct HttpBase {
    http: reqwest::Client,
    mirrors: Vec<(String, String)>,
}

/// 常规请求默认超时（整体上限；探针类调用用 get_json_timeout 单独收紧）。
const DEFAULT_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

impl HttpBase {
    pub fn new(cfg: &AppConfig) -> Result<Self, UpstreamError> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .user_agent("mcha/0.1 (course project)");
        if !cfg.network.proxy.is_empty() {
            builder = builder.proxy(
                reqwest::Proxy::all(&cfg.network.proxy)
                    .map_err(|e| UpstreamError::BadResponse(format!("代理配置非法：{e}")))?,
            );
        }
        Ok(Self {
            http: builder.build()?,
            mirrors: cfg
                .network
                .mirrors
                .iter()
                .map(|m| (m.from.clone(), m.to.clone()))
                .collect(),
        })
    }

    /// 镜像替换：只替换命中的官方域名（§12：镜像仅替换白名单内域名）。
    pub fn apply_mirrors(&self, url: &str) -> String {
        let mut out = url.to_string();
        for (from, to) in &self.mirrors {
            if out.contains(from.as_str()) {
                out = out.replace(from.as_str(), to);
            }
        }
        out
    }

    /// GET 并解析为 JSON。瞬时网络失败自动重试（NFR-3）。
    pub async fn get_json(&self, url: &str) -> Result<serde_json::Value, UpstreamError> {
        self.get_json_timeout(url, DEFAULT_REQUEST_TIMEOUT).await
    }

    /// GET 并解析为 JSON，单请求超时独立控制（上游探针快速失败用，决议 D22
    /// v0.11.1 勘误：镜像探针曾继承客户端级 120s 超时，国内不可达时表现为
    /// 数分钟静默假死）。重试语义与 [`Self::get_json`] 一致。
    pub async fn get_json_timeout(
        &self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, UpstreamError> {
        let url = self.apply_mirrors(url);
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.http.get(&url).timeout(timeout).send().await {
                // 瞬时失败（连接/超时/中断）重试后重新发起
                Err(e) if attempt < 3 && is_transient_reqwest(&e) => {
                    tracing::warn!("GET {url} 第 {attempt} 次失败，重试：{e}");
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) => return Err(e.into()),
                Ok(resp) => {
                    let status = resp.status();
                    match resp.text().await {
                        Err(e) if attempt < 3 && is_transient_reqwest(&e) => {
                            tracing::warn!("读取 {url} 响应体第 {attempt} 次失败，重试：{e}");
                            tokio::time::sleep(backoff(attempt)).await;
                        }
                        Err(e) => return Err(e.into()),
                        Ok(text) => {
                            if !status.is_success() {
                                return Err(UpstreamError::Status {
                                    url,
                                    status: status.as_u16(),
                                    body: text,
                                });
                            }
                            return serde_json::from_str(&text)
                                .map_err(|e| UpstreamError::Json { url, source: e });
                        }
                    }
                }
            }
        }
    }

    /// GET 并直接反序列化为目标类型（内部走 get_json）。
    pub async fn get_typed<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
    ) -> Result<T, UpstreamError> {
        let url = self.apply_mirrors(url);
        let value = self.get_json(&url).await?;
        serde_json::from_value(value).map_err(|e| UpstreamError::Json { url, source: e })
    }

    /// GET 响应体为文本，带单请求超时、大小上限与取消感知（决议 D25：
    /// 执行环工具 `http_get_text` 的底座，也是 getbukkit 页面解析渠道的实现件）。
    /// 流式读取并全程守护上限，防 Content-Length 缺失或撒谎。
    pub async fn get_text_capped(
        &self,
        url: &str,
        timeout: std::time::Duration,
        max_bytes: usize,
        cancel: CancellationToken,
    ) -> Result<String, UpstreamError> {
        let url = self.apply_mirrors(url);
        let resp = self.http.get(&url).timeout(timeout).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(UpstreamError::Status {
                url,
                status: status.as_u16(),
                body: String::new(),
            });
        }
        if let Some(len) = resp.content_length()
            && len as usize > max_bytes
        {
            return Err(UpstreamError::BadResponse(format!(
                "响应体 {len} 字节超过上限 {max_bytes}（拒绝读取）"
            )));
        }
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut stream = resp.bytes_stream();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(UpstreamError::Cancelled),
                chunk = futures::StreamExt::next(&mut stream) => {
                    let Some(chunk) = chunk else { break };
                    let bytes = chunk?;
                    if buf.len() + bytes.len() > max_bytes {
                        return Err(UpstreamError::BadResponse(format!(
                            "响应体超过大小上限 {max_bytes}（已读 {} 字节）",
                            buf.len() + bytes.len()
                        )));
                    }
                    buf.extend_from_slice(&bytes);
                }
            }
        }
        String::from_utf8(buf)
            .map_err(|e| UpstreamError::BadResponse(format!("非 UTF-8 文本：{e}")))
    }

    /// 发起 GET 但**不跟随重定向**，返回 3xx 的 Location 目标（决议 D25：
    /// getbukkit `/get/<token>` → `cdn.getbukkit.org` 直链的解析件，v0.12.1
    /// 抓站实测该跳转是取得真直链的唯一可靠途径）。用一次性无重定向客户端，
    /// 不影响共享底座的重定向策略。
    pub async fn resolve_redirect(
        &self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<String, UpstreamError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent("mcha/0.1 (course project)")
            .build()?;
        let resp = client.get(url).send().await?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| UpstreamError::BadResponse("3xx 响应缺 Location 头".into()))?;
            Ok(loc.to_string())
        } else {
            Err(UpstreamError::BadResponse(format!(
                "期望 3xx 重定向，实际状态 {status}"
            )))
        }
    }

    /// 下载文件到 dest_dir：临时文件 + 哈希校验 + 原子改名（NFR-3）。
    /// 瞬时网络失败整体重试（临时文件会被重建，安全）。
    /// `on_progress(current, total)` 供上层发进度事件。
    pub async fn download(
        &self,
        item: &DownloadItem,
        dest_dir: &Path,
        cancel: CancellationToken,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<PathBuf, UpstreamError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self
                .download_once(item, dest_dir, cancel.clone(), on_progress)
                .await
            {
                Err(e) if attempt < 3 && is_transient_upstream(&e) && !cancel.is_cancelled() => {
                    tracing::warn!("下载 {} 第 {attempt} 次失败，重试：{e}", item.file_name);
                    tokio::time::sleep(backoff(attempt)).await;
                }
                other => return other,
            }
        }
    }

    async fn download_once(
        &self,
        item: &DownloadItem,
        dest_dir: &Path,
        cancel: CancellationToken,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<PathBuf, UpstreamError> {
        let url = self.apply_mirrors(&item.url);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(UpstreamError::Status {
                url,
                status: status.as_u16(),
                body: String::new(),
            });
        }
        let total = resp.content_length();

        std::fs::create_dir_all(dest_dir).map_err(|e| {
            UpstreamError::BadResponse(format!("创建下载目录 {} 失败：{e}", dest_dir.display()))
        })?;
        let tmp_path = dest_dir.join(format!("{}.part", item.file_name));
        let mut file = tokio::io::BufWriter::new(
            tokio::fs::File::create(&tmp_path)
                .await
                .map_err(|e| UpstreamError::BadResponse(format!("创建临时文件失败：{e}")))?,
        );

        let mut sha1 = sha1::Sha1::new();
        let mut sha256 = sha2::Sha256::new();
        // RustCrypto 各 crate 共享 digest::Digest trait，导入一次即可
        use sha1::Digest as _;
        let mut current: u64 = 0;
        let mut stream = resp.bytes_stream();
        let mut reported: u64 = 0;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(UpstreamError::Cancelled),
                chunk = futures::StreamExt::next(&mut stream) => {
                    let Some(chunk) = chunk else { break };
                    let bytes = chunk?;
                    sha1.update(&bytes);
                    sha256.update(&bytes);
                    current += bytes.len() as u64;
                    file.write_all(&bytes).await.map_err(|e| {
                        UpstreamError::BadResponse(format!("写入临时文件失败：{e}"))
                    })?;
                    // 每 512KB 上报一次进度，避免事件风暴
                    if current - reported >= 512 * 1024 {
                        reported = current;
                        on_progress(current, total);
                    }
                }
            }
        }
        file.flush()
            .await
            .map_err(|e| UpstreamError::BadResponse(format!("写入临时文件失败：{e}")))?;
        drop(file);
        on_progress(current, total);

        // 哈希校验（下载安全第三重，§12）
        let digest_hex = |bytes: &[u8]| hex::encode(bytes);
        if let Some(expected) = &item.sha1 {
            let actual = digest_hex(&sha1.finalize());
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(UpstreamError::HashMismatch {
                    file: item.file_name.clone(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        if let Some(expected) = &item.sha256 {
            let actual = digest_hex(&sha256.finalize());
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(UpstreamError::HashMismatch {
                    file: item.file_name.clone(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }

        let dest = dest_dir.join(&item.file_name);
        tokio::fs::rename(&tmp_path, &dest)
            .await
            .map_err(|e| UpstreamError::BadResponse(format!("落盘失败：{e}")))?;
        Ok(dest)
    }
}

// ---------------------------------------------------------------------------
// Mojang piston-meta：版本清单 + 原版服务端
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Manifest {
    versions: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
struct ManifestEntry {
    id: String,
    #[serde(rename = "type")]
    entry_type: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct VersionJson {
    downloads: serde_json::Value,
    /// 官方 Java 最低要求（Mojang 启动器同源；v0.9 起为 Java 需求事实源）。
    /// 极老版本可能缺该字段，Option 兜底。
    #[serde(rename = "javaVersion", default)]
    java_version: Option<VersionJavaInfo>,
}

#[derive(Debug, Deserialize)]
struct VersionJavaInfo {
    #[serde(rename = "majorVersion")]
    major_version: u8,
}

pub struct MojangClient {
    base: HttpBase,
}

impl MojangClient {
    pub const MANIFEST_URL: &str =
        "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

    pub fn new(base: HttpBase) -> Self {
        Self { base }
    }

    /// 拉取官方正式版清单（实时校验"该版本是否存在"的事实来源）。
    pub async fn release_versions(&self) -> Result<Vec<String>, UpstreamError> {
        let manifest: Manifest = self.base.get_typed(Self::MANIFEST_URL).await?;
        Ok(manifest
            .versions
            .into_iter()
            .filter(|v| v.entry_type == "release")
            .map(|v| v.id)
            .collect())
    }

    /// MC 版本 → 官方最低 Java 大版本（v0.9：Java 需求动态事实源，§8.4）。
    /// 链路：清单定位条目 → 条目 URL 的版本 JSON → `javaVersion.majorVersion`。
    /// 版本 JSON 不可变且本方法每任务至多调用两次（工具 + preflight），
    /// 不做进程内缓存，避免引入共享可变状态。极老版本缺字段时返回 Ok(None)。
    pub async fn version_java_major(&self, mc_version: &str) -> Result<Option<u8>, UpstreamError> {
        let manifest: Manifest = self.base.get_typed(Self::MANIFEST_URL).await?;
        let entry = manifest
            .versions
            .iter()
            .find(|v| v.id == mc_version)
            .ok_or_else(|| UpstreamError::BadResponse(format!("清单中无版本 {mc_version}")))?;
        let version_json: VersionJson = self.base.get_typed(&entry.url).await?;
        Ok(version_json.java_version.map(|j| j.major_version))
    }

    /// 原版服务端下载项（URL + sha1 均来自官方元数据）。
    pub async fn server_jar(&self, mc_version: &str) -> Result<DownloadItem, UpstreamError> {
        let manifest: Manifest = self.base.get_typed(Self::MANIFEST_URL).await?;
        let entry = manifest
            .versions
            .iter()
            .find(|v| v.id == mc_version)
            .ok_or_else(|| UpstreamError::BadResponse(format!("清单中无版本 {mc_version}")))?;

        let version_json: VersionJson = self.base.get_typed(&entry.url).await?;
        let server = version_json
            .downloads
            .get("server")
            .ok_or_else(|| UpstreamError::BadResponse(format!("{mc_version} 无官方服务端下载")))?;
        Ok(DownloadItem {
            url: server["url"].as_str().unwrap_or_default().to_string(),
            sha1: server["sha1"].as_str().map(String::from),
            sha256: None,
            file_name: format!("minecraft-server-{mc_version}.jar"),
            kind: DownloadKind::ServerJar,
        })
    }
}

// ---------------------------------------------------------------------------
// PaperMC fill v3（注：v2 API 已下线，请求返回 410 sunset，2026-08 实测）
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FillBuild {
    id: serde_json::Value,
    downloads: serde_json::Value,
}

pub struct PaperClient {
    base: HttpBase,
}

impl PaperClient {
    pub const API: &str = "https://fill.papermc.io/v3/projects/paper";

    pub fn new(base: HttpBase) -> Self {
        Self { base }
    }

    /// 指定 MC 版本最新构建的 Paper 服务端下载项（官方 fill API，带 sha256）。
    pub async fn server_jar(&self, mc_version: &str) -> Result<DownloadItem, UpstreamError> {
        let url = format!("{}/versions/{mc_version}/builds", Self::API);
        let builds: Vec<FillBuild> = self.base.get_typed(&url).await?;
        let build = builds
            .last()
            .ok_or_else(|| UpstreamError::BadResponse(format!("Paper {mc_version} 无可用构建")))?;
        let dl = &build.downloads["server:default"];
        let file_name = dl["name"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("paper-{}-{}.jar", mc_version, build.id));
        Ok(DownloadItem {
            url: dl["url"].as_str().unwrap_or_default().to_string(),
            sha1: None,
            sha256: dl["checksums"]["sha256"].as_str().map(String::from),
            file_name,
            kind: DownloadKind::ServerJar,
        })
    }
}

// ---------------------------------------------------------------------------
// getbukkit 镜像：Spigot 服务端（决议 D22）
// ---------------------------------------------------------------------------

/// Spigot 客户端：官方（SpigotMC）仅以 BuildTools 编译分发、无直链，
/// 下载走 getbukkit 第三方镜像（§11 已注明取舍与哈希口径）。
pub struct SpigotClient {
    base: HttpBase,
}

impl SpigotClient {
    pub const API: &str = "https://api.getbukkit.org/v2/download/spigot";
    pub const DOWNLOAD_ROOT: &str = "https://download.getbukkit.org/spigot";
    /// 列表页（v0.12.1 抓站实测：可直连，52 个版本行，每行下载按钮为
    /// 不透明令牌链接 `/get/<token>`，token 会变必须每次抓页解析）。
    pub const LISTING: &str = "https://getbukkit.org/download/spigot";

    pub fn new(base: HttpBase) -> Self {
        Self { base }
    }

    /// 解析列表页 HTML 的 `<版本, 令牌>` 对（v0.12.1 抓站实测页面结构：
    /// 每行 `<h4>Version</h4><h2>26.2</h2>` … `<a href="…/get/<token>">`；
    /// 出现顺序即发布顺序）。纯函数便于单测。
    fn parse_listing_tokens(html: &str) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for seg in html.split("<h4>Version</h4>").skip(1) {
            let version = seg
                .split("<h2>")
                .nth(1)
                .and_then(|s| s.split("</h2>").next())
                .map(str::trim)
                .unwrap_or_default();
            let token = seg
                .split("getbukkit.org/get/")
                .nth(1)
                .map(|s| {
                    s.chars()
                        .take_while(|c| c.is_ascii_alphanumeric())
                        .collect::<String>()
                })
                .unwrap_or_default();
            if !version.is_empty() && !token.is_empty() {
                pairs.push((version.to_string(), token));
            }
        }
        pairs
    }

    /// 渠道⓪（v0.12 D25 首选）：抓列表页解析令牌 → 302 跟随取真直链。
    /// 证据链（2026-08-31 抓站复核）：`GET /get/<token>` 返回
    /// `302 → cdn.getbukkit.org/spigot/spigot-<版本>.jar`；旧实现的
    /// `download.getbukkit.org` 为过时域名、v2 API 实测超时半死。
    pub async fn direct_url_via_page(
        &self,
        mc_version: &str,
        cancel: CancellationToken,
    ) -> Result<DownloadItem, UpstreamError> {
        let html = self
            .base
            .get_text_capped(
                Self::LISTING,
                std::time::Duration::from_secs(15),
                2 * 1024 * 1024,
                cancel,
            )
            .await?;
        let token = Self::parse_listing_tokens(&html)
            .into_iter()
            .find(|(v, _)| v == mc_version)
            .map(|(_, t)| t)
            .ok_or_else(|| {
                UpstreamError::BadResponse(format!(
                    "列表页 {LISTING} 上没有版本 {mc_version} 的下载令牌",
                    LISTING = Self::LISTING
                ))
            })?;
        let get_url = format!("https://getbukkit.org/get/{token}");
        let direct = self
            .base
            .resolve_redirect(&get_url, std::time::Duration::from_secs(15))
            .await?;
        Ok(DownloadItem {
            url: direct,
            sha1: None,
            sha256: None,
            file_name: format!("spigot-{mc_version}.jar"),
            kind: DownloadKind::ServerJar,
        })
    }

    /// 指定 MC 版本的 Spigot 服务端下载项。
    /// 渠道① getbukkit v2 API：返回直链，哈希可得即携带（有则强制校验）；
    /// 探询单请求 15s 快速失败（v0.11.1 勘误：继承 120s 超时在国内网络下
    /// 表现为静默假死）。
    /// 渠道② 直链模式回退：API 不可达时按命名规则拼 URL（无哈希，
    /// 来源会在部署轨迹中明示"第三方镜像"）。
    pub async fn server_jar(&self, mc_version: &str) -> Result<DownloadItem, UpstreamError> {
        let api_url = format!("{}/{mc_version}", Self::API);
        match self
            .base
            .get_json_timeout(&api_url, std::time::Duration::from_secs(15))
            .await
        {
            Ok(json) => {
                // v2 形态：单版本请求返回对象 {name, version, url, ...}；
                // 防御清单式响应（数组时按 version 字段定位条目）
                let entry = if json.is_array() {
                    json.as_array()
                        .and_then(|a| a.iter().find(|e| e["version"].as_str() == Some(mc_version)))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)
                } else {
                    json
                };
                let url = entry["url"].as_str().unwrap_or_default().to_string();
                if url.is_empty() {
                    tracing::warn!("getbukkit API 响应缺少 url 字段，回退直链模式");
                } else {
                    let sha256 = entry["sha256"]
                        .as_str()
                        .or_else(|| entry["checksum"].as_str())
                        .map(String::from);
                    return Ok(DownloadItem {
                        url,
                        sha1: None,
                        sha256,
                        file_name: format!("spigot-{mc_version}.jar"),
                        kind: DownloadKind::ServerJar,
                    });
                }
            }
            // 镜像明确没有该版本：语义化报错，不做无谓的直链尝试
            Err(UpstreamError::Status { status: 404, .. }) => {
                return Err(UpstreamError::BadResponse(format!(
                    "Spigot {mc_version} 在 getbukkit 镜像上无可用构建（该版本可能尚未发布或未被收录）"
                )));
            }
            Err(e) => tracing::warn!("getbukkit API 查询失败（{api_url}），回退直链模式：{e}"),
        }
        Ok(DownloadItem {
            url: format!("{}/spigot-{mc_version}.jar", Self::DOWNLOAD_ROOT),
            sha1: None,
            sha256: None,
            file_name: format!("spigot-{mc_version}.jar"),
            kind: DownloadKind::ServerJar,
        })
    }
}

// ---------------------------------------------------------------------------
// Fabric meta：loader / installer 版本与 bundle 服务端
// ---------------------------------------------------------------------------

pub struct FabricClient {
    base: HttpBase,
}

#[derive(Debug, Deserialize)]
pub struct FabricResolved {
    pub loader_version: String,
    pub installer_version: String,
    pub item: DownloadItem,
}

impl FabricClient {
    pub const META: &str = "https://meta.fabricmc.net/v2";

    pub fn new(base: HttpBase) -> Self {
        Self { base }
    }

    /// 解析给定 MC 版本的 loader / installer 并产出服务端 bundle 下载项。
    /// 注意：Fabric meta 不提供哈希，`item.sha256 = None`（文档需注明，
    /// 来源为官方 meta 域名 + HTTPS，满足官方渠道约束）。
    pub async fn resolve_server(&self, mc_version: &str) -> Result<FabricResolved, UpstreamError> {
        let loader_json = self
            .base
            .get_json(&format!("{}/versions/loader/{mc_version}", Self::META))
            .await?;
        let loader_version = loader_json
            .as_array()
            .and_then(|a| a.first())
            .and_then(|e| e["loader"]["version"].as_str())
            .ok_or_else(|| {
                UpstreamError::BadResponse(format!("Fabric 无 {mc_version} 的 loader"))
            })?;
        let installer_json = self
            .base
            .get_json(&format!("{}/versions/installer", Self::META))
            .await?;
        let installer_version = installer_json
            .as_array()
            .ok_or_else(|| UpstreamError::BadResponse("Fabric installer 清单缺失".into()))?
            .iter()
            .find(|e| e["stable"].as_bool().unwrap_or(false))
            .and_then(|e| e["version"].as_str())
            .ok_or_else(|| UpstreamError::BadResponse("Fabric 无稳定 installer".into()))?;
        Ok(FabricResolved {
            loader_version: loader_version.to_string(),
            installer_version: installer_version.to_string(),
            item: DownloadItem {
                url: format!(
                    "{}/versions/loader/{mc_version}/{loader_version}/{installer_version}/server/jar",
                    Self::META
                ),
                sha1: None,
                sha256: None,
                file_name: format!("fabric-server-{mc_version}.jar"),
                kind: DownloadKind::ServerJar,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Modrinth v2：检索、版本匹配、依赖闭包
// ---------------------------------------------------------------------------

pub struct ModrinthClient {
    base: HttpBase,
}

#[derive(Debug, Deserialize)]
struct ModrinthSearch {
    hits: Vec<SearchHit>,
}

/// project 元数据（仅取受支持版本列表，NoCompatibleVersion 说明字段用）。
#[derive(Debug, Deserialize)]
struct ModrinthProject {
    #[serde(default)]
    game_versions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    project_id: String,
    slug: String,
    title: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ModrinthVersion {
    id: String,
    files: Vec<VersionFile>,
    dependencies: Vec<VersionDep>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionFile {
    url: String,
    filename: String,
    primary: bool,
    hashes: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionDep {
    project_id: Option<String>,
    dependency_type: String,
}

impl ModrinthClient {
    pub const API: &str = "https://api.modrinth.com/v2";

    pub fn new(base: HttpBase) -> Self {
        Self { base }
    }

    /// 文本检索（别名表未命中时兜底），返回 (slug, title, project_id) 列表。
    pub async fn search(
        &self,
        query: &str,
    ) -> Result<Vec<(String, String, String)>, UpstreamError> {
        let url = format!(
            "{}/search?query={}&limit=5",
            Self::API,
            urlencoding_escape(query)
        );
        let search: ModrinthSearch = self.base.get_typed(&url).await?;
        Ok(search
            .hits
            .into_iter()
            .map(|h| (h.slug, h.title, h.project_id))
            .collect())
    }

    /// 解析一个 mod 在 (mc, loader) 下的最新版本 + 依赖闭包 → ModRef。
    /// `project` 应传 Modrinth project_id（2026-08 实测按 slug 查询已 404）；
    /// search 返回值与别名表均直接给出 project_id。
    /// 实现分两步，均无 async 递归：
    /// ① 队列迭代抓取闭包内全部版本元数据；② 纯函数递归组装树。
    pub async fn resolve_mod(
        &self,
        project: &str,
        mc_version: &str,
        loader: &str,
    ) -> Result<ModRef, UpstreamError> {
        let mut cache: std::collections::HashMap<String, FetchedMod> = Default::default();
        let mut queue: std::collections::VecDeque<String> = Default::default();
        queue.push_back(project.to_string());

        while let Some(key) = queue.pop_front() {
            if cache.contains_key(&key) {
                continue; // 共享依赖只抓一次
            }
            let fetched = self.fetch_latest_version(&key, mc_version, loader).await?;
            for dep in &fetched.required_deps {
                queue.push_back(dep.clone());
            }
            cache.insert(key, fetched);
        }

        const MAX_DEPTH: usize = 16;
        fn assemble(
            key: &str,
            cache: &std::collections::HashMap<String, FetchedMod>,
        ) -> Option<ModRef> {
            assemble_at(key, cache, 0)
        }
        fn assemble_at(
            key: &str,
            cache: &std::collections::HashMap<String, FetchedMod>,
            depth: usize,
        ) -> Option<ModRef> {
            if depth > MAX_DEPTH {
                return None; // 深度护栏：理论上 Modrinth 依赖是 DAG，此处防御成环
            }
            let fetched = cache.get(key)?;
            let deps = fetched
                .required_deps
                .iter()
                .filter_map(|d| assemble_at(d, cache, depth + 1))
                .collect();
            Some(ModRef {
                project: key.to_string(),
                version_id: fetched.version_id.clone(),
                url: fetched.url.clone(),
                sha1: fetched.sha1.clone(),
                file_name: fetched.file_name.clone(),
                deps,
            })
        }
        assemble(project, &cache).ok_or_else(|| {
            UpstreamError::BadResponse(format!("mod {project} 解析失败：无可组装的版本"))
        })
    }

    /// 抓取单个 project 的最新匹配版本（含必选依赖的 project id 列表）。
    async fn fetch_latest_version(
        &self,
        project: &str,
        mc_version: &str,
        loader: &str,
    ) -> Result<FetchedMod, UpstreamError> {
        // Modrinth 约定：game_versions / loaders 传 JSON 数组字面量（URL 编码后）
        let versions_url = format!(
            "{}/project/{project}/version?game_versions=%5B%22{mc}%22%5D&loaders=%5B%22{loader}%22%5D",
            Self::API,
            mc = mc_version.replace('"', ""),
            loader = loader.replace('"', ""),
        );
        // 2026-08 实测（§12 上游韧性）：过滤无结果时 Modrinth 稳定返回 200 空数组，
        // 间歇性返回 404 空体——两条路径都必须语义化为"无兼容版本"并附该 mod
        // 当前最高支持版本，禁止让玩家看到莫名其妙的"请求失败（HTTP 404）"。
        let versions: Vec<ModrinthVersion> = match self.base.get_typed(&versions_url).await {
            Ok(v) => v,
            Err(UpstreamError::Status { status: 404, .. }) => Vec::new(),
            Err(e) => return Err(e),
        };
        let version = match versions.first() {
            Some(v) => v,
            None => {
                let latest_supported = self
                    .latest_supported_version(project)
                    .await
                    .unwrap_or_else(|_| "未知".into());
                return Err(UpstreamError::NoCompatibleVersion {
                    project: project.to_string(),
                    mc: mc_version.to_string(),
                    loader: loader.to_string(),
                    latest_supported,
                });
            }
        };
        let file = version
            .files
            .iter()
            .find(|f| f.primary)
            .or_else(|| version.files.first())
            .ok_or(UpstreamError::BadResponse(format!(
                "mod {project} 版本无文件"
            )))?;
        Ok(FetchedMod {
            version_id: version.id.clone(),
            url: file.url.clone(),
            sha1: file.hashes["sha1"].as_str().unwrap_or_default().to_string(),
            file_name: file.filename.clone(),
            required_deps: version
                .dependencies
                .iter()
                .filter(|d| d.dependency_type == "required")
                .filter_map(|d| d.project_id.clone())
                .collect(),
        })
    }

    /// 该 mod 当前最高支持的 MC 版本（NoCompatibleVersion 的说明字段）。
    /// project 元数据的 game_versions 按时间升序排列（2026-08 实测），取末位。
    async fn latest_supported_version(&self, project: &str) -> Result<String, UpstreamError> {
        let url = format!("{}/project/{project}", Self::API);
        let meta: ModrinthProject = self.base.get_typed(&url).await?;
        meta.game_versions
            .last()
            .cloned()
            .ok_or_else(|| UpstreamError::BadResponse(format!("mod {project} 无任何受支持版本")))
    }
}

/// 抓取结果：一个 project 的选定版本元数据（供纯函数组装 ModRef 树）。
struct FetchedMod {
    version_id: String,
    url: String,
    sha1: String,
    file_name: String,
    required_deps: Vec<String>,
}

/// 简单的 URL 查询参数转义（检索词含中文/空格）。
fn urlencoding_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Adoptium v3：受管 JRE 元数据（§8.8）
// ---------------------------------------------------------------------------

pub struct AdoptiumClient {
    base: HttpBase,
}

#[derive(Debug, Deserialize)]
pub struct JreAsset {
    pub release_name: String,
    pub download_url: String,
    pub file_name: String,
    pub sha256: Option<String>,
}

impl AdoptiumClient {
    pub const API: &str = "https://api.adoptium.net/v3";

    pub fn new(base: HttpBase) -> Self {
        Self { base }
    }

    /// 当前平台的官方 JRE（Windows zip / Linux/macOS tar.gz）最新 GA 版本。
    /// 注：`/assets/latest/{major}/ga` 端点已 404（2026-08 实测），
    /// 改用 `feature_releases`；哈希字段名为 `checksum`（即 sha256）。
    pub async fn latest_jre(&self, java_major: u8) -> Result<JreAsset, UpstreamError> {
        let os = match std::env::consts::OS {
            "windows" => "windows",
            "macos" => "mac",
            _ => "linux",
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "aarch64",
            other => other,
        };
        let url = format!(
            "{API}/assets/feature_releases/{java_major}/ga?os={os}&arch={arch}&image_type=jre&page_size=1",
            API = Self::API
        );
        let json = self.base.get_json(&url).await?;
        let arr = json.as_array().and_then(|a| a.first()).ok_or_else(|| {
            UpstreamError::BadResponse(format!("Adoptium 无 Java {java_major} JRE"))
        })?;
        let release_name = arr["release_name"].as_str().unwrap_or_default().to_string();
        let package = &arr["binaries"][0]["package"];
        Ok(JreAsset {
            release_name,
            download_url: package["link"].as_str().unwrap_or_default().to_string(),
            file_name: package["name"].as_str().unwrap_or_default().to_string(),
            sha256: package["checksum"]
                .as_str()
                .or_else(|| package["sha256"].as_str())
                .map(String::from),
        })
    }
}

/// 瞬时网络错误判定（连接/超时/中断/响应体截断），可安全重试。
fn is_transient_reqwest(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_decode() || e.is_request()
}

/// 上游层可重试错误：网络类与 5xx/429；哈希不匹配、4xx 等不重试。
fn is_transient_upstream(e: &UpstreamError) -> bool {
    match e {
        UpstreamError::Http(req) => is_transient_reqwest(req),
        UpstreamError::Status { status, .. } => *status >= 500 || *status == 429,
        UpstreamError::Cancelled => false,
        _ => false,
    }
}

/// 重试退避：1s → 2s → 4s。
fn backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_secs(1u64 << (attempt - 1).min(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> HttpBase {
        HttpBase::new(&crate::config::AppConfig::default()).unwrap()
    }

    #[tokio::test]
    #[ignore = "真实上游连通性冒烟（计网络流量）：cargo test -- --ignored"]
    async fn 冒烟_mojang清单与原版下载项() {
        let client = MojangClient::new(base());
        let releases = client.release_versions().await.unwrap();
        assert!(releases.iter().any(|v| v == "1.21.1"), "清单应包含 1.21.1");
        let item = client.server_jar("1.21.1").await.unwrap();
        assert!(item.url.starts_with("https://"));
        assert!(item.sha1.is_some(), "Mojang 官方元数据必须带 sha1");
    }

    #[tokio::test]
    #[ignore = "真实上游连通性冒烟（计网络流量）：cargo test -- --ignored"]
    async fn 冒烟_年份制版本的官方java需求() {
        // v0.9：26.2 为年份制正式版，官方要求 Java 25（piston-meta + wiki 双源核实）
        let client = MojangClient::new(base());
        let releases = client.release_versions().await.unwrap();
        assert!(
            releases.iter().any(|v| v == "26.2"),
            "清单应包含年份制 26.2"
        );
        let major = client.version_java_major("26.2").await.unwrap();
        assert_eq!(major, Some(25), "26.2 官方最低 Java 大版本应为 25");
        // 1.x 时代版本同样可查（分界在 26.1）
        let major = client.version_java_major("1.21.1").await.unwrap();
        assert_eq!(major, Some(21));
    }

    #[tokio::test]
    #[ignore = "真实上游连通性冒烟（计网络流量）：cargo test -- --ignored"]
    async fn 冒烟_无兼容构建语义化报错() {
        // v0.9：过滤无结果时上游返回 404 空体，须语义化为 NoCompatibleVersion
        // 并附该 mod 当前最高支持版本（暮色森林系项目实测最高 1.21.1）
        let client = ModrinthClient::new(base());
        let err = client
            .resolve_mod("eDeSn4Ds", "26.2", "fabric")
            .await
            .unwrap_err();
        match err {
            UpstreamError::NoCompatibleVersion {
                mc,
                latest_supported,
                ..
            } => {
                assert_eq!(mc, "26.2");
                assert_eq!(latest_supported, "1.21.1");
            }
            other => panic!("应报 NoCompatibleVersion，实际 {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "真实上游连通性冒烟（计网络流量）：cargo test -- --ignored"]
    async fn 冒烟_modrinth解析与依赖闭包() {
        // Sodium 是 Modrinth 原生 mod（project_id 实测：AANobbMI）
        let client = ModrinthClient::new(base());
        let resolved = client
            .resolve_mod("AANobbMI", "1.21.1", "fabric")
            .await
            .unwrap();
        assert!(!resolved.sha1.is_empty(), "Modrinth 必须提供 sha1");
        assert!(resolved.file_name.ends_with(".jar"));
    }

    #[tokio::test]
    #[ignore = "真实上游连通性冒烟（计网络流量）：cargo test -- --ignored"]
    async fn 冒烟_adoptium元数据() {
        let client = AdoptiumClient::new(base());
        let asset = client.latest_jre(21).await.unwrap();
        assert!(asset.download_url.starts_with("https://"));
        assert!(asset.sha256.is_some(), "Adoptium 元数据必须带 sha256");
    }

    #[tokio::test]
    #[ignore = "真实上游连通性冒烟（计网络流量）：cargo test -- --ignored"]
    async fn 冒烟_spigot镜像直链() {
        // 决议 D22：Spigot 走 getbukkit 镜像；API 可用时应返回直链
        let client = SpigotClient::new(base());
        let item = client.server_jar("1.21.1").await.unwrap();
        assert!(
            item.url.starts_with("https://"),
            "直链应为 HTTPS：{}",
            item.url
        );
        assert_eq!(item.file_name, "spigot-1.21.1.jar");
        assert!(
            item.sha256.is_none_or(|h| !h.is_empty()),
            "哈希字段若返回必须非空"
        );
    }

    #[tokio::test]
    #[ignore = "真实上游连通性冒烟（计网络流量）：cargo test -- --ignored"]
    async fn 冒烟_fabric与paper() {
        let fabric = FabricClient::new(base());
        let resolved = fabric.resolve_server("1.21.1").await.unwrap();
        assert!(!resolved.loader_version.is_empty());

        let paper = PaperClient::new(base());
        let item = paper.server_jar("1.21.1").await.unwrap();
        assert!(item.url.starts_with("https://fill-data.papermc.io/"));
        assert!(item.sha256.is_some(), "fill v3 必须提供 sha256");
    }
}
