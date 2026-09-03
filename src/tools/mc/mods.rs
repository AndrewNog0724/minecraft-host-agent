//! mod 场景工具（设计 §8.12）：search_mods / resolve_mod / install_mods。
//!
//! 三段式分工：检索（可能多轮试探）/ 解析（确定性：别名 → 精确匹配 → 版本
//! 匹配 → 依赖闭包 → 意图清单）/ 安装（唯一落盘点）。事实红线：下载 URL 与
//! 哈希只信**安装时实时重取**的上游 API 返回——意图清单（source + 源专属 id）
//! 经 LLM 上下文转手，抄错 / 篡改均不影响正确性。
//!
//! 双源（Modrinth 优先，自动降级）：别名标注 source="curseforge" 的项目直达
//! CF 通道；其余先走 Modrinth，零命中且 Key 已配置时自动转 CF。两源都零命中
//! → 结构化报错；CF 独占但 Key 未配置 → 结构化说明 + 分步申请指引。
//! 冲突策略两源一致：同名同哈希跳过（幂等），不一致报错不覆盖。

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;
use crate::knowledge::upstream::curseforge::{CDN_HOSTS, CfClient, CfFile};
use crate::knowledge::upstream::modrinth::{self, CDN_HOST, ModVersion, ModrinthClient, SearchHit};
use crate::tools::confinement::resolve_in;

use super::download::{ExpectedHash, download_verified, sha1_of_file};
use super::{Permission, Tool, ToolCtx, ToolError};

/// 依赖闭包深度上限（防清单爆炸，设计 §8.12）。
const CLOSURE_MAX_DEPTH: usize = 10;
/// 精确匹配检索时的候选窗口。
const RESOLVE_SEARCH_LIMIT: u32 = 10;

// ---------------------------------------------------------------------------
// L1 中文别名表
// ---------------------------------------------------------------------------

const ALIASES_TOML: &str = include_str!("../../assets/knowledge/mod_aliases.toml");

#[derive(Debug, Deserialize)]
struct AliasFile {
    updated: String,
    #[serde(default)]
    mods: Vec<AliasEntry>,
    #[serde(default)]
    unlisted: Vec<UnlistedEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AliasEntry {
    pub slug: String,
    pub name: String,
    pub aliases: Vec<String>,
    /// 收录源："modrinth"（默认）| "curseforge"（独占项目直达 CF 通道）。
    #[serde(default)]
    source: Option<String>,
}

impl AliasEntry {
    pub(crate) fn source(&self) -> &'static str {
        match self.source.as_deref() {
            Some("curseforge") => "curseforge",
            _ => "modrinth",
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct UnlistedEntry {
    pub name: String,
    pub aliases: Vec<String>,
    pub reason: String,
}

/// 别名表（编译期内嵌，OnceLock 惰性解析）。
pub(crate) struct AliasTable {
    pub updated: String,
    entries: Vec<AliasEntry>,
    unlisted: Vec<UnlistedEntry>,
}

impl AliasTable {
    pub(crate) fn builtin() -> &'static Self {
        static T: OnceLock<AliasTable> = OnceLock::new();
        T.get_or_init(|| {
            let file: AliasFile =
                toml::from_str(ALIASES_TOML).expect("内置 mod_aliases.toml 格式错误（编译期内嵌）");
            AliasTable {
                updated: file.updated,
                entries: file.mods,
                unlisted: file.unlisted,
            }
        })
    }

    fn normalize(raw: &str) -> String {
        raw.trim().to_lowercase()
    }

    /// 精确命中别名 / slug / 项目名。
    pub(crate) fn hit(&self, name: &str) -> Option<&AliasEntry> {
        let key = Self::normalize(name);
        self.entries.iter().find(|e| {
            Self::normalize(&e.slug) == key
                || Self::normalize(&e.name) == key
                || e.aliases.iter().any(|a| Self::normalize(a) == key)
        })
    }

    /// 命中「无收录」名单（两源都不存在的常见需求）。
    pub(crate) fn unlisted(&self, name: &str) -> Option<&UnlistedEntry> {
        let key = Self::normalize(name);
        self.unlisted.iter().find(|e| {
            Self::normalize(&e.name) == key || e.aliases.iter().any(|a| Self::normalize(a) == key)
        })
    }
}

// ---------------------------------------------------------------------------
// 上游客户端构造与下载域
// ---------------------------------------------------------------------------

/// Modrinth 客户端：`[network] modrinth_api` 非空时指向自定义基址
/// （集成测试注入本地 mock / 高级用户自建代理）。
pub(crate) fn modrinth_client<'a>(ctx: &'a ToolCtx) -> ModrinthClient<'a> {
    match ctx.network.modrinth_api.trim() {
        "" => ModrinthClient::new(&ctx.http),
        base => ModrinthClient::with_base(&ctx.http, base.to_string()),
    }
}

/// CurseForge 客户端：`[network] curseforge_api` 非空时指向自定义基址
/// （测试注入 mock / 高级用户自建代理）；默认按 key 有无自动选通道——
/// 有 key 走官方 API，无 key 自动走国内镜像（§8.12）。
fn cf_client(ctx: &ToolCtx) -> CfClient<'_> {
    match ctx.network.curseforge_api.trim() {
        "" => CfClient::new(&ctx.http, ctx.curseforge_key.clone()),
        base => CfClient::with_base(&ctx.http, base.to_string(), ctx.curseforge_key.clone()),
    }
}

fn base_host(base: &str) -> Option<String> {
    let base = base.trim();
    if base.is_empty() {
        None
    } else {
        modrinth::url_host(base)
    }
}

/// Modrinth 允许的下载域：官方 CDN + API 基址同域（测试 / 代理）。
fn modrinth_allowed_hosts(ctx: &ToolCtx) -> Vec<String> {
    let mut hosts = vec![CDN_HOST.to_string()];
    if let Some(host) = base_host(&ctx.network.modrinth_api) {
        hosts.push(host);
    }
    hosts
}

/// CurseForge 允许的下载域：官方 CDN 两域 + API 基址同域（测试 / 代理）。
fn curseforge_allowed_hosts(ctx: &ToolCtx) -> Vec<String> {
    let mut hosts: Vec<String> = CDN_HOSTS.iter().map(|h| h.to_string()).collect();
    if let Some(host) = base_host(&ctx.network.curseforge_api) {
        hosts.push(host);
    }
    hosts
}

fn host_allowed(host: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// 下载量千分位展示（1_234_567 → "1,234,567"）。
fn format_downloads(n: u64) -> String {
    let text = n.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (idx, ch) in text.chars().enumerate() {
        if idx > 0 && (text.len() - idx).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------
// 源抽象：统一 Modrinth / CurseForge 的版本与依赖表示
// ---------------------------------------------------------------------------

/// 收录源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Source {
    Modrinth,
    Curseforge,
}

impl Source {
    fn parse(raw: &str) -> Option<Source> {
        match raw.trim().to_lowercase().as_str() {
            "modrinth" => Some(Source::Modrinth),
            "curseforge" => Some(Source::Curseforge),
            _ => None,
        }
    }
}

/// 依赖引用（跨源闭包的队列 / 去重键）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DepRef {
    Modrinth(String),
    Curseforge(i64),
}

/// 已解析版本（源专属数据的薄包装）。
#[derive(Debug, Clone)]
enum ResolvedVersion {
    Modrinth(ModVersion),
    Curseforge(CfFile),
}

impl ResolvedVersion {
    fn file_name(&self) -> Option<String> {
        match self {
            ResolvedVersion::Modrinth(v) => v.primary_file().map(|f| f.filename.clone()),
            ResolvedVersion::Curseforge(f) => Some(f.file_name.clone()),
        }
    }

    fn size(&self) -> Option<u64> {
        match self {
            ResolvedVersion::Modrinth(v) => v.primary_file().map(|f| f.size),
            ResolvedVersion::Curseforge(f) => Some(f.file_length),
        }
    }

    fn display_version(&self) -> String {
        match self {
            ResolvedVersion::Modrinth(v) => v.version_number.clone(),
            ResolvedVersion::Curseforge(f) => f.display_name.clone(),
        }
    }

    /// required 依赖（归一化为跨源引用）。
    fn dependencies(&self) -> Vec<DepRef> {
        match self {
            ResolvedVersion::Modrinth(v) => v
                .dependencies
                .iter()
                .filter(|d| d.dependency_type.as_deref() == Some("required"))
                .filter_map(|d| d.project_id.clone().map(DepRef::Modrinth))
                .collect(),
            ResolvedVersion::Curseforge(f) => f
                .required_dependencies()
                .into_iter()
                .map(DepRef::Curseforge)
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// search_mods
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchModsArgs {
    /// mod 名称（中文别名 / 英文名 / slug 皆可）
    pub query: String,
    /// 过滤：MC 版本（可选，如 1.21.1）
    #[serde(default)]
    pub mc_version: Option<String>,
    /// 过滤：加载器（可选，如 fabric）
    #[serde(default)]
    pub loader: Option<String>,
    /// 返回条数上限（默认 5，最多 20）
    #[serde(default)]
    pub limit: Option<u32>,
}

/// 单行摘要：`- slug ｜ 标题 ｜ 简介 ｜ 下载 N ｜ 分类`
fn format_search_hits(hits: &[SearchHit], query: &str, filter: &str) -> String {
    if hits.is_empty() {
        if let Some(entry) = AliasTable::builtin().unlisted(query) {
            return format!("两源均未收录「{query}」：{reason}", reason = entry.reason);
        }
        return format!(
            "Modrinth 未检索到「{query}」（可换关键词或英文名重试；别名表更新于 {updated}）",
            updated = AliasTable::builtin().updated
        );
    }
    let mut lines = vec![format!(
        "Modrinth 检索「{query}」（{filter}，{n} 条）：",
        n = hits.len()
    )];
    for hit in hits {
        let short_desc: String = hit.description.chars().take(60).collect();
        lines.push(format!(
            "- {slug} ｜ {title} ｜ {desc}｜ 下载 {dl} ｜ 分类: {cats}",
            slug = hit.slug,
            title = hit.title,
            desc = short_desc,
            dl = format_downloads(hit.downloads),
            cats = if hit.categories.is_empty() {
                "—".to_string()
            } else {
                hit.categories.join(",")
            }
        ));
    }
    lines.push("检索结果只作候选；安装前必须经 resolve_mod 解析版本与依赖。".to_string());
    lines.join("\n")
}

pub struct SearchModsTool;

#[async_trait::async_trait]
impl Tool for SearchModsTool {
    fn name(&self) -> &'static str {
        "search_mods"
    }
    fn description(&self) -> String {
        "在 Modrinth 检索 mod（支持中文别名，如「机械动力」）：返回候选清单（slug / 简介 / 下载量）。CurseForge 独占项目用 resolve_mod 直接解析。只读；安装前需 resolve_mod。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(SearchModsArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: SearchModsArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let aliases = AliasTable::builtin();
        let client = modrinth_client(ctx);
        let limit = args.limit.unwrap_or(5).clamp(1, 20);
        let filter = match (&args.mc_version, &args.loader) {
            (Some(v), Some(l)) => format!("MC {v} / {l}"),
            (Some(v), None) => format!("MC {v}"),
            (None, Some(l)) => l.clone(),
            (None, None) => "无过滤".to_string(),
        };

        // ① 别名命中（Modrinth 源）→ 直接定位项目（免检索歧义）
        if let Some(entry) = aliases.hit(&args.query) {
            if entry.source() == "curseforge" {
                let cf = cf_client(ctx);
                let outcome =
                    cf_search_detail(&cf, &entry.slug, &args.query, args.mc_version.as_deref())
                        .await;
                return Ok(match outcome {
                    Ok(content) => ToolOutcome::ok(content),
                    Err(reason) => ToolOutcome::err(reason),
                });
            }
            return match client.project(&entry.slug).await {
                Ok(project) => {
                    let mut lines = vec![format!(
                        "「{query}」命中别名表 → {slug}（{title}）",
                        query = args.query,
                        slug = project.slug,
                        title = project.title
                    )];
                    lines.push(format!(
                        "简介：{desc}｜ 下载 {dl}",
                        desc = project.description,
                        dl = format_downloads(project.downloads)
                    ));
                    if let (Some(mc), Some(loader)) =
                        (args.mc_version.as_deref(), args.loader.as_deref())
                    {
                        match client
                            .project_versions(&entry.slug, Some(mc), Some(loader))
                            .await
                        {
                            Ok(versions) => {
                                if let Some(latest) = versions.first() {
                                    lines.push(format!(
                                        "MC {mc} / {loader} 的最新兼容版本：{ver}（version_id={id}）",
                                        ver = latest.version_number,
                                        id = latest.id
                                    ));
                                } else {
                                    lines.push(format!(
                                        "注意：该项目没有 MC {mc} / {loader} 的兼容版本"
                                    ));
                                }
                            }
                            Err(reason) => lines.push(format!("版本查询失败：{reason}")),
                        }
                    }
                    lines.push(
                        "检索结果只作候选；安装前必须经 resolve_mod 解析版本与依赖。".to_string(),
                    );
                    Ok(ToolOutcome::ok(lines.join("\n")))
                }
                Err(reason) => Ok(ToolOutcome::err(format!(
                    "别名表命中 {slug} 但查询失败：{reason}",
                    slug = entry.slug
                ))),
            };
        }

        // ② 「无收录」名单先答（OptiFine 等两源皆无，检索只会带来噪声）
        if let Some(entry) = aliases.unlisted(&args.query) {
            return Ok(ToolOutcome::ok(format!(
                "两源均未收录「{query}」：{reason}",
                query = args.query,
                reason = entry.reason
            )));
        }

        // ③ 常规检索（Modrinth）
        match client
            .search(
                Some(&args.query),
                args.mc_version.as_deref(),
                args.loader.as_deref(),
                limit,
            )
            .await
        {
            Ok(hits) => Ok(ToolOutcome::ok(format_search_hits(
                &hits,
                &args.query,
                &filter,
            ))),
            Err(reason) => Ok(ToolOutcome::err(reason)),
        }
    }
}

/// CF 别名直达的项目详情（search_mods 用）。
async fn cf_search_detail(
    cf: &CfClient<'_>,
    slug: &str,
    query: &str,
    mc_version: Option<&str>,
) -> Result<String, String> {
    let hits = cf.search(slug, mc_version.unwrap_or(""), 10).await?;
    let key = AliasTable::normalize(slug);
    let Some(project) = hits.iter().find(|p| AliasTable::normalize(&p.slug) == key) else {
        return Ok(format!(
            "「{query}」标注为 CurseForge 项目（{slug}），但 CurseForge 检索未命中（可能名称已变更）"
        ));
    };
    let mut lines = vec![format!(
        "「{query}」命中别名表（CurseForge 独占）→ {slug}（{title}）",
        slug = project.slug,
        title = project.name
    )];
    lines.push(format!(
        "简介：{desc}｜ 下载 {dl}",
        desc = project.summary,
        dl = format_downloads(project.download_count)
    ));
    if let Some(mc) = mc_version {
        match cf.mod_files(project.id, mc).await {
            Ok(files) => {
                if let Some(latest) = files.first() {
                    lines.push(format!(
                        "MC {mc} / fabric 的最新兼容文件：{display}（file_id={id}）",
                        display = latest.display_name,
                        id = latest.id
                    ));
                } else {
                    lines.push(format!("注意：该项目没有 MC {mc} / fabric 的兼容文件"));
                }
            }
            Err(reason) => lines.push(format!("文件查询失败：{reason}")),
        }
    }
    lines.push("检索结果只作候选；安装前必须经 resolve_mod 解析版本与依赖。".to_string());
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// resolve_mod
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolveModsArgs {
    /// mod 清单（中文别名 / 英文名 / slug 混排，如 ["暮色森林", "jei"]）
    pub mods: Vec<String>,
    /// 目标 MC 版本（如 1.21.1）
    pub mc_version: String,
    /// 目标加载器（当前支持 fabric）
    #[serde(default)]
    pub loader: Option<String>,
}

/// 意图清单条目（resolve_mod 产出 → install_mods 原样回传；不含 URL / 哈希）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct ManifestEntry {
    /// 收录源：modrinth（默认）| curseforge
    #[serde(default = "default_manifest_source")]
    pub source: String,
    /// 项目 slug
    pub slug: String,
    /// Modrinth 版本 id（source=modrinth 时必填）
    #[serde(default)]
    pub version_id: Option<String>,
    /// CurseForge 项目 id（source=curseforge 时必填）
    #[serde(default)]
    pub mod_id: Option<i64>,
    /// CurseForge 文件 id（source=curseforge 时必填）
    #[serde(default)]
    pub file_id: Option<i64>,
    /// 文件名（展示用；实际以安装期 API 重取为准）
    #[serde(default)]
    pub file_name: Option<String>,
}

fn default_manifest_source() -> String {
    "modrinth".to_string()
}

/// 解析结果条目（含入选理由，轨迹可回放）。
struct Resolved {
    slug: String,
    source: Source,
    version: ResolvedVersion,
    reason: String,
    depth: usize,
}

/// 单个 mod 解析失败的结构化原因。
enum ResolveFail {
    /// 多个候选，需 Agent 经 ask_user 澄清。
    Ambiguous {
        name: String,
        candidates: Vec<(String, String)>,
    },
    /// 无收录 / 无匹配版本 / Key 缺失等，附建议。
    NotFound { name: String, hint: String },
    /// 上游查询失败（网络等）。
    Upstream(String),
}

impl ResolveFail {
    fn not_found(name: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::NotFound {
            name: name.into(),
            hint: hint.into(),
        }
    }
}

/// CurseForge 检索精确匹配：slug 或名称唯一命中才通过。
async fn cf_resolve_exact(
    cf: &CfClient<'_>,
    name: &str,
    mc_version: &str,
) -> Result<(String, CfFile, String), ResolveFail> {
    let hits = cf
        .search(name, mc_version, RESOLVE_SEARCH_LIMIT as usize)
        .await
        .map_err(ResolveFail::Upstream)?;
    let key = AliasTable::normalize(name);
    let candidates: Vec<_> = hits
        .iter()
        .filter(|p| {
            AliasTable::normalize(&p.slug) == key
                || p.name.trim().to_lowercase() == name.trim().to_lowercase()
        })
        .collect();
    let project = match candidates.as_slice() {
        [one] => one,
        [] => {
            return Err(ResolveFail::not_found(
                name,
                format!(
                    "Modrinth 与 CurseForge 均无精确命中；可换英文名重试（检索到 {total} 条相关，均不同名）",
                    total = hits.len()
                ),
            ));
        }
        many => {
            return Err(ResolveFail::Ambiguous {
                name: name.to_string(),
                candidates: many
                    .iter()
                    .map(|p| (p.slug.clone(), p.name.clone()))
                    .collect(),
            });
        }
    };
    let files = cf
        .mod_files(project.id, mc_version)
        .await
        .map_err(ResolveFail::Upstream)?;
    let Some(latest) = files.first() else {
        return Err(ResolveFail::not_found(
            name,
            format!(
                "「{slug}」没有 MC {mc_version} / fabric 的文件；可调整 MC 版本或从 CurseForge 页面手动下载",
                slug = project.slug,
                mc_version = mc_version
            ),
        ));
    };
    let reason = format!(
        "CurseForge 唯一命中；版本匹配 MC {mc} / fabric（最新 {display}）",
        mc = mc_version,
        display = latest.display_name
    );
    Ok((project.slug.clone(), latest.clone(), reason))
}

/// 解析单个 mod：别名 → Modrinth 精确匹配 →（降级）CurseForge 精确匹配。
async fn resolve_one(
    ctx: &ToolCtx,
    name: &str,
    mc_version: &str,
    loader: &str,
) -> Result<(Source, String, ResolvedVersion, String), ResolveFail> {
    let aliases = AliasTable::builtin();

    // ① 别名命中 → 按标注源直达
    if let Some(entry) = aliases.hit(name) {
        let via_alias = true;
        return match entry.source() {
            "curseforge" => {
                let cf = cf_client(ctx);
                let (slug, file, mut reason) =
                    cf_resolve_exact(&cf, &entry.slug, mc_version).await?;
                if via_alias {
                    reason = format!("别名命中（CurseForge 独占）；{}", reason);
                }
                Ok((
                    Source::Curseforge,
                    slug,
                    ResolvedVersion::Curseforge(file),
                    reason,
                ))
            }
            _ => {
                let client = modrinth_client(ctx);
                let versions = client
                    .project_versions(&entry.slug, Some(mc_version), Some(loader))
                    .await
                    .map_err(ResolveFail::Upstream)?;
                match versions.first() {
                    Some(latest) => Ok((
                        Source::Modrinth,
                        entry.slug.clone(),
                        ResolvedVersion::Modrinth(latest.clone()),
                        format!(
                            "别名命中；版本匹配 MC {mc} / {loader}（最新 {ver}）",
                            mc = mc_version,
                            ver = latest.version_number
                        ),
                    )),
                    None => {
                        Err(no_compatible_version(&client, &entry.slug, mc_version, loader).await)
                    }
                }
            }
        };
    }

    // ② 「无收录」名单（两源皆无）
    if let Some(un) = aliases.unlisted(name) {
        return Err(ResolveFail::not_found(name, un.reason.clone()));
    }

    // ③ Modrinth 精确匹配（唯一命中才通过）
    let client = modrinth_client(ctx);
    let hits = client
        .search(
            Some(name),
            Some(mc_version),
            Some(loader),
            RESOLVE_SEARCH_LIMIT,
        )
        .await
        .map_err(ResolveFail::Upstream)?;
    let key = AliasTable::normalize(name);
    let candidates: Vec<&SearchHit> = hits
        .iter()
        .filter(|h| {
            AliasTable::normalize(&h.slug) == key
                || h.title.trim().to_lowercase() == name.trim().to_lowercase()
        })
        .collect();
    let modrinth_slug = match candidates.as_slice() {
        [one] => one.slug.clone(),
        [] => {
            // ④ 零命中 → 自动转 CurseForge（官方或镜像通道；检索失败如实报错）
            let cf = cf_client(ctx);
            let (slug, file, reason) = cf_resolve_exact(&cf, name, mc_version).await?;
            return Ok((
                Source::Curseforge,
                slug,
                ResolvedVersion::Curseforge(file),
                reason,
            ));
        }
        many => {
            return Err(ResolveFail::Ambiguous {
                name: name.to_string(),
                candidates: many
                    .iter()
                    .map(|h| (h.slug.clone(), h.title.clone()))
                    .collect(),
            });
        }
    };

    // ⑤ Modrinth 版本匹配
    let versions = client
        .project_versions(&modrinth_slug, Some(mc_version), Some(loader))
        .await
        .map_err(ResolveFail::Upstream)?;
    match versions.first() {
        Some(latest) => Ok((
            Source::Modrinth,
            modrinth_slug,
            ResolvedVersion::Modrinth(latest.clone()),
            format!(
                "检索唯一命中；版本匹配 MC {mc} / {loader}（最新 {ver}）",
                mc = mc_version,
                ver = latest.version_number
            ),
        )),
        None => Err(no_compatible_version(&client, &modrinth_slug, mc_version, loader).await),
    }
}

/// Modrinth 项目无匹配版本时的最近兼容建议。
async fn no_compatible_version(
    client: &ModrinthClient<'_>,
    slug: &str,
    mc_version: &str,
    loader: &str,
) -> ResolveFail {
    let all = client.project_versions(slug, None, None).await;
    let hint = match all.as_deref().map(|v| v.first()) {
        Ok(Some(v)) => format!(
            "「{slug}」没有 MC {mc} / {loader} 的版本；最新版本 {ver} 支持 {gvs}（加载器 {loaders}），可考虑调整 MC 版本或加载器",
            mc = mc_version,
            ver = v.version_number,
            gvs = v
                .game_versions
                .iter()
                .rev()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("、"),
            loaders = v.loaders.join("、")
        ),
        _ => format!("「{slug}」在 Modrinth 上没有任何版本"),
    };
    ResolveFail::not_found(slug, hint)
}

/// 依赖闭包（跨源）：BFS + 去重 + 环检测 + 深度上限。
async fn resolve_with_closure(
    ctx: &ToolCtx,
    roots: Vec<(Source, String, ResolvedVersion, String)>,
    mc_version: &str,
    _loader: &str,
) -> Result<Vec<Resolved>, String> {
    let mut results: Vec<Resolved> = Vec::new();
    let mut visited: HashSet<DepRef> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    for (source, slug, version, reason) in roots {
        let dep_key = match (&source, &version) {
            (Source::Modrinth, ResolvedVersion::Modrinth(v)) => {
                DepRef::Modrinth(v.project_id.clone())
            }
            (Source::Curseforge, ResolvedVersion::Curseforge(f)) => DepRef::Curseforge(f.mod_id),
            _ => continue,
        };
        if visited.insert(dep_key) {
            queue.push_back((slug.clone(), 0));
            results.push(Resolved {
                slug,
                source,
                version,
                reason,
                depth: 0,
            });
        }
    }

    // CF 客户端始终可用（无 key 自动走镜像），CF 依赖可正常解析
    let cf = cf_client(ctx);
    let client = modrinth_client(ctx);

    while let Some((parent_slug, depth)) = queue.pop_front() {
        if depth >= CLOSURE_MAX_DEPTH {
            return Err(format!(
                "依赖闭包超过深度上限 {CLOSURE_MAX_DEPTH}（自 {parent_slug} 起）；清单异常，已中止"
            ));
        }
        let parent = results
            .iter()
            .find(|r| r.slug == parent_slug)
            .map(|r| r.version.clone())
            .ok_or_else(|| format!("内部状态错误：找不到已解析的 {parent_slug}"))?;
        for dep in parent.dependencies() {
            if !visited.insert(dep.clone()) {
                continue;
            }
            match dep {
                DepRef::Modrinth(pid) => {
                    let versions = client
                        .project_versions(&pid, Some(mc_version), Some(_loader))
                        .await
                        .map_err(|reason| {
                            format!("查询依赖 {pid}（{parent_slug} 的依赖）失败：{reason}")
                        })?;
                    let Some(version) = versions.first() else {
                        return Err(format!(
                            "依赖不满足：{parent_slug} 的必需依赖 {pid} 没有 MC {mc_version} / {_loader} 的版本；请调整目标版本或更换 mod"
                        ));
                    };
                    // 依赖条目只带 project_id：回查项目详情取 slug
                    let slug = match client.project(&pid).await {
                        Ok(project) => project.slug,
                        Err(_) => pid.clone(),
                    };
                    results.push(Resolved {
                        slug: slug.clone(),
                        source: Source::Modrinth,
                        version: ResolvedVersion::Modrinth(version.clone()),
                        reason: format!("必需依赖（{parent_slug}）"),
                        depth: depth + 1,
                    });
                    queue.push_back((slug, depth + 1));
                }
                DepRef::Curseforge(mid) => {
                    let files = cf.mod_files(mid, mc_version).await.map_err(|reason| {
                        format!("查询依赖（{parent_slug} 的 CurseForge 依赖）失败：{reason}")
                    })?;
                    let Some(file) = files.first() else {
                        return Err(format!(
                            "依赖不满足：{parent_slug} 的必需依赖（CurseForge {mid}）没有 MC {mc_version} / fabric 的文件"
                        ));
                    };
                    let slug = match cf.mod_detail(mid).await {
                        Ok(project) => project.slug,
                        Err(_) => format!("curseforge-{mid}"),
                    };
                    results.push(Resolved {
                        slug: slug.clone(),
                        source: Source::Curseforge,
                        version: ResolvedVersion::Curseforge(file.clone()),
                        reason: format!("必需依赖（{parent_slug}）"),
                        depth: depth + 1,
                    });
                    queue.push_back((slug, depth + 1));
                }
            }
        }
    }
    Ok(results)
}

pub struct ResolveModsTool;

#[async_trait::async_trait]
impl Tool for ResolveModsTool {
    fn name(&self) -> &'static str {
        "resolve_mod"
    }
    fn description(&self) -> String {
        "解析 mod 清单（确定性）：中文别名 → Modrinth 精确匹配（零命中自动降级 CurseForge）→ 版本匹配（MC × 加载器）→ 依赖闭包 → 意图清单。返回的 manifest JSON 原样传给 install_mods。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ResolveModsArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: ResolveModsArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if args.mods.is_empty() {
            return Ok(ToolOutcome::err("mod 清单为空；请先向用户确认要装哪些 mod"));
        }
        let loader = args
            .loader
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or("fabric")
            .to_ascii_lowercase();
        if loader != "fabric" {
            return Ok(ToolOutcome::err(format!(
                "加载器「{loader}」暂不支持自动安装（当前仅 fabric；Forge 为指导模式）"
            )));
        }

        let mut roots: Vec<(Source, String, ResolvedVersion, String)> = Vec::new();
        for name in &args.mods {
            if name.trim().is_empty() {
                continue;
            }
            match resolve_one(ctx, name, &args.mc_version, &loader).await {
                Ok(resolved) => roots.push(resolved),
                Err(fail) => {
                    return Ok(ToolOutcome::err(match fail {
                        ResolveFail::Ambiguous { name, candidates } => format!(
                            "「{name}」命中多个项目，无法唯一确定；候选：{}。请用 ask_user 请用户选择后重试",
                            candidates
                                .iter()
                                .map(|(slug, title)| format!("{slug}（{title}）"))
                                .collect::<Vec<_>>()
                                .join("、")
                        ),
                        ResolveFail::NotFound { name, hint } => {
                            format!("「{name}」解析失败：{hint}")
                        }
                        ResolveFail::Upstream(reason) => format!("上游查询失败：{reason}"),
                    }));
                }
            }
        }

        let resolved = match resolve_with_closure(ctx, roots, &args.mc_version, &loader).await {
            Ok(list) => list,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };

        // 输出：解析报告 + 意图清单 JSON
        let mut lines = vec![format!(
            "解析成功：{total} 项（要求 {roots_n} + 依赖 {deps_n}），目标 MC {mc} / {loader}：",
            total = resolved.len(),
            roots_n = resolved.iter().filter(|r| r.depth == 0).count(),
            deps_n = resolved.iter().filter(|r| r.depth > 0).count(),
            mc = args.mc_version
        )];
        let mut manifest: Vec<ManifestEntry> = Vec::new();
        for (idx, item) in resolved.iter().enumerate() {
            let file = item.version.file_name();
            let source_label = match item.source {
                Source::Modrinth => "Modrinth",
                Source::Curseforge => "CurseForge",
            };
            lines.push(format!(
                "{idx}. [{kind}·{source_label}] {slug} → {ver}（{file}{size}）｜ {reason}",
                idx = idx + 1,
                kind = if item.depth == 0 { "要求" } else { "依赖" },
                slug = item.slug,
                ver = item.version.display_version(),
                file = file.as_deref().unwrap_or("无文件"),
                size = item
                    .version
                    .size()
                    .map(|s| format!(", {:.1} MB", s as f64 / 1_048_576.0))
                    .unwrap_or_default(),
                reason = item.reason
            ));
            if file.is_none() {
                return Ok(ToolOutcome::err(
                    "部分版本没有可下载文件（上游数据异常），已中止；请调整清单重试",
                ));
            }
            let entry = match item.source {
                Source::Modrinth => {
                    let ResolvedVersion::Modrinth(v) = &item.version else {
                        continue;
                    };
                    ManifestEntry {
                        source: "modrinth".to_string(),
                        slug: item.slug.clone(),
                        version_id: Some(v.id.clone()),
                        mod_id: None,
                        file_id: None,
                        file_name: file,
                    }
                }
                Source::Curseforge => {
                    let ResolvedVersion::Curseforge(f) = &item.version else {
                        continue;
                    };
                    ManifestEntry {
                        source: "curseforge".to_string(),
                        slug: item.slug.clone(),
                        version_id: None,
                        mod_id: Some(f.mod_id),
                        file_id: Some(f.id),
                        file_name: file,
                    }
                }
            };
            manifest.push(entry);
        }
        lines.push(
            "意图清单（install_mods 的 manifest 参数，原样传递；URL 与哈希由安装时实时重取）："
                .to_string(),
        );
        lines.push(serde_json::to_string(&manifest).unwrap_or_else(|_| "[]".to_string()));
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// install_mods
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstallModsArgs {
    /// 服务器目录（工作区内相对路径，如 server）
    pub server_dir: String,
    /// 意图清单（resolve_mod 输出，原样传递）
    pub manifest: Vec<ManifestEntry>,
}

/// 下载单个文件到 mods 目录（临时文件 + 原子重命名 + 源专属哈希校验）。
/// 返回 Ok(true) = 已存在且哈希一致（跳过）；Ok(false) = 新装。
async fn download_into_mods_dir(
    ctx: &ToolCtx,
    mods_dir: &Path,
    url: &str,
    file_name: &str,
    sha1: &str,
    sha512: Option<&str>,
    allowed_hosts: &[String],
) -> Result<bool, String> {
    // 下载域强校验（设计 §12：按源白名单）
    let host = modrinth::url_host(url).ok_or_else(|| format!("下载 URL 非法：{url}"))?;
    if !host_allowed(&host, allowed_hosts) {
        return Err(format!(
            "下载 URL 域「{host}」不在白名单（仅允许 {allowed}）；已拒绝",
            allowed = allowed_hosts.join("、")
        ));
    }

    let dest = mods_dir.join(file_name);
    if dest.exists() {
        let existing = sha1_of_file(&dest)?;
        if existing == sha1 {
            return Ok(true);
        }
        return Err(format!(
            "冲突：{file} 已存在且内容不同（现有 sha1 {existing}，目标 {want}）。不覆盖用户手动安装的文件；请向用户确认后手动处理",
            file = file_name,
            existing = &existing[..12.min(existing.len())],
            want = &sha1[..12.min(sha1.len())]
        ));
    }

    // 校验集：sha1 必校验；sha512 有则双校验（Modrinth）
    let mut expected = vec![ExpectedHash::Sha1(sha1.to_string())];
    if let Some(sha512) = sha512 {
        expected.push(ExpectedHash::Sha512(sha512.to_string()));
    }

    let tmp = mods_dir.join(format!(".mcha-part-{file_name}"));
    download_verified(ctx, url, &tmp, &format!("安装 {file_name}"), &expected).await?;
    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|err| format!("落位失败（{}）：{err}", dest.display()))?;
    Ok(false)
}

/// 安装单条意图：按源实时重取权威数据（URL / 哈希）后落盘。
async fn install_entry(
    ctx: &ToolCtx,
    mods_dir: &Path,
    entry: &ManifestEntry,
) -> Result<bool, String> {
    let source = Source::parse(&entry.source).ok_or_else(|| {
        format!(
            "意图清单 source 非法：「{}」（应为 modrinth | curseforge）",
            entry.source
        )
    })?;
    match source {
        Source::Modrinth => {
            let version_id = entry.version_id.as_deref().ok_or_else(|| {
                format!(
                    "{}：source=modrinth 缺少 version_id（请用 resolve_mod 重新生成清单）",
                    entry.slug
                )
            })?;
            let client = modrinth_client(ctx);
            let versions = client
                .versions_by_ids(&[version_id.to_string()])
                .await
                .map_err(|reason| format!("重取 {slug} 版本失败：{reason}", slug = entry.slug))?;
            // 按 version_id 精确匹配（防御性过滤防串版）
            let version = versions
                .iter()
                .find(|v| v.id == version_id)
                .ok_or_else(|| format!("重取 {slug} 失败：Modrinth 无版本 {id}（清单可能过期，请重新 resolve_mod）", slug = entry.slug, id = version_id))?;
            let file = version.primary_file().ok_or_else(|| {
                format!(
                    "{slug} 的版本 {id} 没有可下载文件",
                    slug = entry.slug,
                    id = version_id
                )
            })?;
            download_into_mods_dir(
                ctx,
                mods_dir,
                &file.url,
                &file.filename,
                &file.hashes.sha1,
                Some(&file.hashes.sha512),
                &modrinth_allowed_hosts(ctx),
            )
            .await
        }
        Source::Curseforge => {
            let file_id = entry.file_id.ok_or_else(|| {
                format!(
                    "{}：source=curseforge 缺少 file_id（请用 resolve_mod 重新生成清单）",
                    entry.slug
                )
            })?;
            let client = cf_client(ctx);
            let files = client
                .files_by_ids(&[file_id])
                .await
                .map_err(|reason| format!("重取 {slug} 文件失败：{reason}", slug = entry.slug))?;
            let file = files
                .iter()
                .find(|f| f.id == file_id)
                .ok_or_else(|| format!("重取 {slug} 失败：CurseForge 无文件 {file_id}（清单可能过期，请重新 resolve_mod）", slug = entry.slug))?;
            let url = file.download_url.clone().ok_or_else(|| {
                format!(
                    "{slug}：该文件未开放第三方分发（无下载 URL）；请从 https://www.curseforge.com/projects/{slug}/files/{file_id} 手动下载放入 mods 目录",
                    slug = entry.slug
                )
            })?;
            let slug_for_err = entry.slug.clone();
            let sha1 = file
                .sha1()
                .ok_or_else(|| {
                    format!("{slug_for_err}：CurseForge 未返回 sha1，无法校验；已拒绝安装")
                })?
                .to_string();
            download_into_mods_dir(
                ctx,
                mods_dir,
                &url,
                &file.file_name,
                &sha1,
                None, // CF 仅 sha1 单哈希（强度差异在输出轨迹如实标注）
                &curseforge_allowed_hosts(ctx),
            )
            .await
        }
    }
}

pub struct InstallModsTool;

#[async_trait::async_trait]
impl Tool for InstallModsTool {
    fn name(&self) -> &'static str {
        "install_mods"
    }
    fn description(&self) -> String {
        "安装 mod 到服务器 mods 目录：按意图清单实时重取上游权威数据（Modrinth / CurseForge），下载并做哈希校验后原子落盘。同名文件哈希一致跳过、不一致报错（不覆盖）。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(InstallModsArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::Network
    }
    fn confirm_summary(&self, args: &serde_json::Value) -> Vec<String> {
        let server_dir = args
            .get("server_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("server");
        let manifest = args.get("manifest").and_then(|v| v.as_array());
        let count = manifest.map(|a| a.len()).unwrap_or(0);
        let names: Vec<String> = manifest
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        let slug = e.get("slug").and_then(|s| s.as_str())?;
                        let source = e
                            .get("source")
                            .and_then(|s| s.as_str())
                            .unwrap_or("modrinth");
                        Some(format!("{slug}[{source}]"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        vec![
            format!("向 {server_dir}/mods/ 安装 {count} 个 mod"),
            format!(
                "清单：{}",
                if names.is_empty() {
                    "（空）".to_string()
                } else {
                    names.join("、")
                }
            ),
            "下载自 Modrinth CDN（sha1+sha512）/ CurseForge CDN（sha1），安装时实时重取"
                .to_string(),
        ]
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: InstallModsArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if args.manifest.is_empty() {
            return Ok(ToolOutcome::err("意图清单为空；请先经 resolve_mod 解析"));
        }
        let server_dir = resolve_in(&[ctx.workspace.as_path()], &args.server_dir)?;
        let mods_dir: PathBuf = server_dir.join("mods");
        tokio::fs::create_dir_all(&mods_dir)
            .await
            .map_err(|err| ToolError::Io(format!("创建 mods 目录失败：{err}")))?;

        let mut installed: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut conflicts: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for entry in &args.manifest {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            match install_entry(ctx, &mods_dir, entry).await {
                Ok(true) => skipped.push(
                    entry
                        .file_name
                        .clone()
                        .unwrap_or_else(|| entry.slug.clone()),
                ),
                Ok(false) => installed.push(
                    entry
                        .file_name
                        .clone()
                        .unwrap_or_else(|| entry.slug.clone()),
                ),
                Err(reason) => {
                    if reason.starts_with("冲突：") {
                        conflicts.push(reason);
                    } else {
                        failures.push(format!(
                            "{name}：{reason}",
                            name = entry
                                .file_name
                                .clone()
                                .unwrap_or_else(|| entry.slug.clone())
                        ));
                    }
                }
            }
        }

        let mut lines = vec![format!(
            "安装结果：新装 {installed} / 跳过（已存在同哈希）{skipped} / 冲突 {conflicts} / 失败 {failures}",
            installed = installed.len(),
            skipped = skipped.len(),
            conflicts = conflicts.len(),
            failures = failures.len()
        )];
        for name in &installed {
            lines.push(format!("+ {name}"));
        }
        for name in &skipped {
            lines.push(format!("= {name}（已存在且一致）"));
        }
        lines.extend(conflicts.iter().map(|c| format!("! {c}")));
        lines.extend(failures.iter().map(|f| format!("! {f}")));
        if conflicts.is_empty() && failures.is_empty() {
            lines.push(format!(
                "全部就位：{}；重启服务器后生效（start_server / 手动脚本）",
                mods_dir.display()
            ));
            Ok(ToolOutcome::ok(lines.join("\n")))
        } else {
            lines.push(
                "存在未就位文件：解决冲突 / 失败后可重跑 install_mods（同哈希自动跳过，幂等）"
                    .to_string(),
            );
            Ok(ToolOutcome::err(lines.join("\n")))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tools::general::tests::QuietInteraction;

    /// 测试 ToolCtx（mock 基址注入 network.*_api；可选 CF key）。
    pub(crate) fn test_ctx_with_key(
        workspace: &Path,
        modrinth_api: &str,
        curseforge_api: &str,
        curseforge_key: &str,
    ) -> ToolCtx {
        let (tx, _rx) = crate::events::event_channel();
        let mut network = crate::config::NetworkConfig::default();
        network.modrinth_api = modrinth_api.to_string();
        network.curseforge_api = curseforge_api.to_string();
        ToolCtx {
            workspace: workspace.to_path_buf(),
            data_dir: workspace.join(".data"),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network,
            retrieval: Default::default(),
            curseforge_key: curseforge_key.to_string(),
        }
    }

    pub(crate) fn test_ctx(workspace: &Path, modrinth_api: &str) -> ToolCtx {
        test_ctx_with_key(workspace, modrinth_api, "", "")
    }

    #[test]
    fn alias_hits_sources_and_unlisted() {
        let table = AliasTable::builtin();
        // 暮色森林：标注 curseforge 源的正册条目
        let tf = table.hit("暮色森林").expect("暮色森林应在正册");
        assert_eq!(tf.slug, "the-twilight-forest");
        assert_eq!(tf.source(), "curseforge");
        assert_eq!(table.hit("Twilight Forest").unwrap().source(), "curseforge");
        // OptiFine：两源皆无，留在无收录名单
        assert!(table.hit("OptiFine").is_none());
        assert!(table.unlisted("OptiFine").is_some());
        assert!(table.hit("JEI").is_none() || table.hit("JEI").unwrap().source() == "modrinth");
        let jei = table.hit("JEI").expect("JEI 别名应命中");
        assert_eq!(jei.slug, "jei");
        assert_eq!(jei.source(), "modrinth");
        assert_eq!(table.hit("机械动力").unwrap().slug, "create");
        assert_eq!(
            table.hit("FABRIC-API").unwrap().slug,
            "fabric-api",
            "大小写归一"
        );
        assert!(table.hit("不存在的mod").is_none());
    }

    #[test]
    fn downloads_formatting() {
        assert_eq!(format_downloads(0), "0");
        assert_eq!(format_downloads(1234567), "1,234,567");
        assert_eq!(format_downloads(999), "999");
    }

    #[tokio::test]
    async fn search_mods_reports_unlisted_honestly() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path(), "http://127.0.0.1:1");
        let outcome = SearchModsTool
            .run(serde_json::json!({"query": "高清修复"}), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("unlisted 应为结构化说明而非错误：{outcome:?}");
        };
        assert!(content.contains("两源均未收录"), "{content}");
    }

    #[tokio::test]
    async fn curseforge_alias_without_key_uses_mirror_channel() {
        let root = tempfile::tempdir().unwrap();
        let base = spawn_cf_mock();
        // 无 key + mock 基址：别名命中暮色森林 → 直接走 CF 通道（镜像语义）
        let ctx = test_ctx_with_key(root.path(), "", &base, "");
        let outcome = SearchModsTool
            .run(serde_json::json!({"query": "暮色森林"}), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("无 key 时 CF 通道应可用（镜像语义）：{outcome:?}");
        };
        assert!(content.contains("the-twilight-forest"), "{content}");

        // resolve 同样闭环
        let outcome = ResolveModsTool
            .run(
                serde_json::json!({"mods": ["暮色森林"], "mc_version": "1.21.1", "loader": "fabric"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("resolve 无 key 应走镜像闭环：{outcome:?}");
        };
        assert!(content.contains("curseforge"), "{content}");
    }

    #[tokio::test]
    async fn resolve_falls_back_to_cf_on_modrinth_miss() {
        // Modrinth 检索有结果但不同名（零精确命中）→ 自动转 CF 解析暮色森林
        let root = tempfile::tempdir().unwrap();
        let mr_mock = spawn_modrinth_mock();
        let cf_mock = spawn_cf_mock();
        let ctx = test_ctx_with_key(root.path(), &mr_mock, &cf_mock, "");
        let outcome = ResolveModsTool
            .run(
                serde_json::json!({"mods": ["Twilight Forest"], "mc_version": "1.21.1", "loader": "fabric"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("Modrinth 零命中应转 CF 解析：{outcome:?}");
        };
        assert!(content.contains("the-twilight-forest"), "{content}");
        assert!(content.contains("curseforge"), "{content}");
    }

    /// 本地 mock Modrinth：jei（无依赖）与 sodium（依赖 jei）。
    pub(crate) fn spawn_modrinth_mock() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            fn sha1_hex(data: &[u8]) -> String {
                use sha1::Digest as _;
                let mut h = sha1::Sha1::new();
                h.update(data);
                h.finalize().iter().map(|b| format!("{b:02x}")).collect()
            }
            fn sha512_hex(data: &[u8]) -> String {
                use sha2::Digest as _;
                let mut h = sha2::Sha512::new();
                h.update(data);
                h.finalize().iter().map(|b| format!("{b:02x}")).collect()
            }
            fn percent_decode(s: &str) -> String {
                let bytes = s.as_bytes();
                let mut out = Vec::new();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == b'%' && i + 2 < bytes.len() {
                        if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                            out.push(v);
                            i += 3;
                            continue;
                        }
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
                String::from_utf8_lossy(&out).to_string()
            }

            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf);
                let Some(target) = request.split_whitespace().nth(1) else {
                    continue;
                };
                let (path, _) = target.split_once('?').unwrap_or((target, ""));
                let path = percent_decode(path);
                let base = format!("http://{addr}");

                let (status, body) = if let Some(slug) = path
                    .strip_prefix("/v2/project/")
                    .and_then(|s| s.strip_suffix("/version"))
                {
                    let deps = |pid: &str| {
                        format!(r#"{{"project_id":"{pid}","dependency_type":"required"}}"#)
                    };
                    let version = |id: &str, pid: &str, number: &str, deps: &str| {
                        format!(
                            r#"{{"id":"{id}","project_id":"{pid}","version_number":"{number}","game_versions":["1.21.1"],"loaders":["fabric"],"dependencies":[{deps}],"files":[{{"url":"{base}/files/{id}.jar","filename":"{id}.jar","primary":true,"hashes":{{"sha1":"{sha1}","sha512":"{sha512}"}},"size":11}}]}}"#,
                            sha1 = sha1_hex(format!("{id}.jar").as_bytes()),
                            sha512 = sha512_hex(format!("{id}.jar").as_bytes())
                        )
                    };
                    // 依赖闭包按 project_id（pid- 前缀）查询，需与 slug 等价处理
                    match slug.trim_start_matches("pid-") {
                        "jei" => (
                            "200 OK",
                            format!("[{}]", version("jeiV1", "pid-jei", "1.0", "")),
                        ),
                        "sodium" => (
                            "200 OK",
                            format!(
                                "[{}]",
                                version("sodV1", "pid-sodium", "2.0", &deps("pid-jei"))
                            ),
                        ),
                        _ => ("200 OK", "[]".to_string()),
                    }
                } else if let Some(id_or_slug) = path.strip_prefix("/v2/project/") {
                    let known = match id_or_slug {
                        "jei" => Some(("jei", "pid-jei")),
                        "sodium" => Some(("sodium", "pid-sodium")),
                        "pid-jei" => Some(("jei", "pid-jei")),
                        "pid-sodium" => Some(("sodium", "pid-sodium")),
                        _ => None,
                    };
                    match known {
                        None => ("404 Not Found", String::new()),
                        Some((slug, pid)) => (
                            "200 OK",
                            format!(
                                r#"{{"project_id":"{pid}","slug":"{slug}","title":"{slug} title","description":"desc","downloads":100}}"#
                            ),
                        ),
                    }
                } else if path.starts_with("/v2/versions") {
                    let version = |id: &str, pid: &str| {
                        format!(
                            r#"{{"id":"{id}","project_id":"{pid}","version_number":"{id}","game_versions":["1.21.1"],"loaders":["fabric"],"dependencies":[],"files":[{{"url":"{base}/files/{id}.jar","filename":"{id}.jar","primary":true,"hashes":{{"sha1":"{sha1}","sha512":"{sha512}"}},"size":11}}]}}"#,
                            base = base,
                            sha1 = sha1_hex(format!("{id}.jar").as_bytes()),
                            sha512 = sha512_hex(format!("{id}.jar").as_bytes())
                        )
                    };
                    (
                        "200 OK",
                        format!(
                            "[{},{}]",
                            version("jeiV1", "pid-jei"),
                            version("sodV1", "pid-sodium")
                        ),
                    )
                } else if path.starts_with("/v2/search") {
                    let slug = if path.contains("jei") {
                        "jei"
                    } else {
                        "sodium"
                    };
                    (
                        "200 OK",
                        format!(
                            r#"{{"hits":[{{"project_id":"pid-{slug}","slug":"{slug}","title":"{slug} title","description":"d","categories":["fabric"],"downloads":9}}],"offset":0,"limit":10,"total_hits":1}}"#
                        ),
                    )
                } else if let Some(name) = path.strip_prefix("/files/") {
                    ("200 OK", name.to_string())
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

    /// 本地 mock CurseForge：the-twilight-forest（依赖 CF 777 → 简化为无依赖
    /// 场景时用 file deps=[]； twilight 文件名 tf.jar，内容 = 文件名字节）。
    pub(crate) fn spawn_cf_mock() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use sha1::Digest as _;
            use std::io::{Read as _, Write as _};
            fn percent_decode(s: &str) -> String {
                let bytes = s.as_bytes();
                let mut out = Vec::new();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == b'%' && i + 2 < bytes.len() {
                        if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                            out.push(v);
                            i += 3;
                            continue;
                        }
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
                String::from_utf8_lossy(&out).to_string()
            }
            let sha1_hex = |data: &[u8]| {
                let mut h = sha1::Sha1::new();
                h.update(data);
                h.finalize()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            };
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf);
                let target = request.split_whitespace().nth(1).unwrap_or("");
                let (path, _) = target.split_once('?').unwrap_or((target, ""));
                let path = percent_decode(path);
                let base = format!("http://{addr}");
                let sha1_tf = sha1_hex(b"tf-1.21.1.jar");

                let (status, body) = if path.starts_with("/v1/mods/search") {
                    (
                        "200 OK",
                        format!(
                            r#"{{"data":[{{"id":227639,"slug":"the-twilight-forest","name":"The Twilight Forest","summary":"一座魔法森林","downloadCount":99}}]}}"#
                        ),
                    )
                } else if path.contains("/files") {
                    (
                        "200 OK",
                        format!(
                            r#"{{"data":[{{"id":5566,"modId":227639,"displayName":"TF 1.0","fileName":"tf-1.21.1.jar","downloadUrl":"{base}/cfiles/tf-1.21.1.jar","fileLength":15,"hashes":[{{"algo":1,"value":"{sha1_tf}"}}],"gameVersions":["1.21.1","Fabric"],"dependencies":[]}}]}}"#
                        ),
                    )
                } else if path.starts_with("/v1/mods/227639") {
                    (
                        "200 OK",
                        r#"{"data":{"id":227639,"slug":"the-twilight-forest","name":"The Twilight Forest","summary":"s","downloadCount":99}}"#.to_string(),
                    )
                } else if let Some(name) = path.strip_prefix("/cfiles/") {
                    ("200 OK", name.to_string())
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
    async fn resolve_closure_and_install_roundtrip() {
        let mock = spawn_modrinth_mock();
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path(), &mock);

        // ① resolve：sodium（依赖 jei）→ 闭包应包含两项目
        let outcome = ResolveModsTool
            .run(
                serde_json::json!({"mods": ["Sodium"], "mc_version": "1.21.1", "loader": "fabric"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("resolve 应成功：{outcome:?}");
        };
        assert!(content.contains("必需依赖（sodium）"), "{content}");
        assert!(content.contains("jeiV1"), "{content}");
        let manifest_line = content.lines().last().expect("最后一行是 manifest JSON");
        let manifest: Vec<ManifestEntry> = serde_json::from_str(manifest_line).unwrap();
        assert_eq!(manifest.len(), 2);
        assert!(manifest.iter().all(|m| m.source == "modrinth"));

        // ② install：按 manifest 安装到 mods 目录
        let outcome = InstallModsTool
            .run(
                serde_json::json!({"server_dir": "server", "manifest": manifest}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("install 应成功：{outcome:?}");
        };
        assert!(content.contains("新装 2"), "{content}");
        let mods_dir = root.path().join("server").join("mods");
        assert!(mods_dir.join("jeiV1.jar").exists());
        assert!(mods_dir.join("sodV1.jar").exists());

        // ③ 幂等重跑：全部跳过
        let manifest_again: Vec<ManifestEntry> = vec![
            ManifestEntry {
                source: "modrinth".into(),
                slug: "jei".into(),
                version_id: Some("jeiV1".into()),
                mod_id: None,
                file_id: None,
                file_name: Some("jeiV1.jar".into()),
            },
            ManifestEntry {
                source: "modrinth".into(),
                slug: "sodium".into(),
                version_id: Some("sodV1".into()),
                mod_id: None,
                file_id: None,
                file_name: Some("sodV1.jar".into()),
            },
        ];
        let outcome = InstallModsTool
            .run(
                serde_json::json!({"server_dir": "server", "manifest": manifest_again}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("幂等重跑应成功：{outcome:?}");
        };
        assert!(content.contains("跳过（已存在同哈希）2"), "{content}");
    }

    #[tokio::test]
    async fn curseforge_resolve_and_install_roundtrip() {
        let cf_mock = spawn_cf_mock();
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx_with_key(root.path(), "http://127.0.0.1:1", &cf_mock, "test-key");

        // ① resolve：别名直达 CurseForge
        let outcome = ResolveModsTool
            .run(
                serde_json::json!({"mods": ["暮色森林"], "mc_version": "1.21.1", "loader": "fabric"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("CF resolve 应成功：{outcome:?}");
        };
        assert!(content.contains("CurseForge 独占"), "{content}");
        let manifest_line = content.lines().last().unwrap();
        let manifest: Vec<ManifestEntry> = serde_json::from_str(manifest_line).unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].source, "curseforge");
        assert_eq!(manifest[0].mod_id, Some(227639));
        assert_eq!(manifest[0].file_id, Some(5566));

        // ② install：从 CF mock（同域）下载
        let outcome = InstallModsTool
            .run(
                serde_json::json!({"server_dir": "server", "manifest": manifest}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("CF install 应成功：{outcome:?}");
        };
        assert!(content.contains("新装 1"), "{content}");
        assert!(root.path().join("server/mods/tf-1.21.1.jar").exists());
    }

    #[tokio::test]
    async fn install_conflicting_file_is_reported_not_overwritten() {
        let mock = spawn_modrinth_mock();
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path(), &mock);

        // 域校验放行逻辑：mock 基址同域应放行
        let client = modrinth_client(&ctx);
        let versions = client
            .versions_by_ids(&["jeiV1".to_string()])
            .await
            .unwrap();
        let file = versions[0].primary_file().unwrap();
        let host = modrinth::url_host(&file.url).unwrap();
        assert_eq!(host, modrinth::url_host(&mock).unwrap(), "mock 同域应放行");

        // 冲突：预先放入同名但内容不同的文件 → 报冲突且不覆盖
        let mods_dir = root.path().join("server").join("mods");
        tokio::fs::create_dir_all(&mods_dir).await.unwrap();
        tokio::fs::write(mods_dir.join("jeiV1.jar"), b"different-bytes")
            .await
            .unwrap();
        let args = serde_json::json!({
            "server_dir": "server",
            "manifest": [{"slug": "jei", "version_id": "jeiV1", "file_name": "jeiV1.jar"}]
        });
        let outcome = InstallModsTool.run(args, &ctx).await.unwrap();
        let ToolOutcome::Err { error } = outcome else {
            panic!("内容不同应报冲突：{outcome:?}");
        };
        assert!(error.contains("冲突："), "{error}");
        let kept = tokio::fs::read(mods_dir.join("jeiV1.jar")).await.unwrap();
        assert_eq!(kept, b"different-bytes");
    }

    #[tokio::test]
    async fn search_mods_hits_alias_directly() {
        let mock = spawn_modrinth_mock();
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path(), &mock);
        let outcome = SearchModsTool
            .run(
                serde_json::json!({"query": "物品管理器", "mc_version": "1.21.1", "loader": "fabric"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("别名命中应成功：{outcome:?}");
        };
        assert!(content.contains("命中别名表"), "{content}");
        assert!(content.contains("jeiV1"), "{content}");
    }

    #[tokio::test]
    async fn resolve_rejects_non_fabric_loader() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path(), "");
        let outcome = ResolveModsTool
            .run(
                serde_json::json!({"mods": ["jei"], "mc_version": "1.21.1", "loader": "forge"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "forge 应结构化拒绝");
    }

    /// 真实上游冒烟：Modrinth resolve + install（`--ignored`）。
    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_resolve_and_install_jei_sodium() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path(), "");
        let resolved = ResolveModsTool
            .run(
                serde_json::json!({"mods": ["JEI", "sodium"], "mc_version": "1.21.1", "loader": "fabric"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = resolved else {
            panic!("live resolve 失败：{resolved:?}");
        };
        let manifest_line = content.lines().last().unwrap();
        let manifest: Vec<ManifestEntry> = serde_json::from_str(manifest_line).unwrap();
        assert!(manifest.len() >= 2, "{content}");
        let outcome = InstallModsTool
            .run(
                serde_json::json!({"server_dir": "server", "manifest": manifest}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(outcome.is_ok(), "live install 失败：{outcome:?}");
    }

    /// 真实上游冒烟：暮色森林 ← CurseForge。无 key 走国内镜像（默认通道），
    /// 有 key 走官方 API——两种通道都应闭环（`cargo test --ignored`）。
    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_resolve_twilight_forest_from_curseforge() {
        let key = std::env::var("MCHA_CURSEFORGE_KEY").unwrap_or_default();
        let channel = if key.trim().is_empty() {
            "国内镜像（无 key）"
        } else {
            "官方 API"
        };
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx_with_key(root.path(), "", "", &key);
        let outcome = ResolveModsTool
            .run(
                serde_json::json!({"mods": ["暮色森林"], "mc_version": "1.21.1", "loader": "fabric"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("live CF resolve（{channel}）失败：{outcome:?}");
        };
        assert!(content.contains("the-twilight-forest"), "{content}");
        assert!(content.contains("curseforge"), "{content}");
        eprintln!("暮色森林解析成功（通道：{channel}）");
    }

    /// 真实上游冒烟：暮色森林镜像通道安装闭环（resolve → install 落盘 +
    /// sha1 校验；无 key，走国内镜像）。
    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_install_twilight_forest_via_mirror() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path(), "");
        let resolved = ResolveModsTool
            .run(
                serde_json::json!({"mods": ["暮色森林"], "mc_version": "1.21.1", "loader": "fabric"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = resolved else {
            panic!("live 镜像 resolve 失败：{resolved:?}");
        };
        let manifest_line = content.lines().last().unwrap();
        let manifest: Vec<ManifestEntry> = serde_json::from_str(manifest_line).unwrap();
        assert_eq!(manifest[0].source, "curseforge");
        let outcome = InstallModsTool
            .run(
                serde_json::json!({"server_dir": "server", "manifest": manifest}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(outcome.is_ok(), "live 镜像 install 失败：{outcome:?}");
        let jar = root.path().join("server/mods").read_dir().unwrap().count();
        assert!(jar >= 1, "mods 目录应有文件");
    }
}
