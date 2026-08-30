//! knowledge：静态知识库（L1）+ 上游 API 客户端（L2）+ 版本校验管线（§8.4）。
//!
//! 设计红线（§8.9）：版本类事实永远不进 Prompt——
//! LLM 只能经本模块的工具返回值获得事实，自己没有凭记忆作答的通道。

pub mod upstream;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::spec::{HybridAuth, ModRef, ServerSoftware};

/// 编译期嵌入的知识库数据（随包分发，带版本号与来源日期，可独立更新）。
pub const JAVA_MAP_TOML: &str = include_str!("../assets/knowledge/java_map.toml");
pub const ALIASES_TOML: &str = include_str!("../assets/knowledge/aliases.toml");
pub const ERROR_PATTERNS_TOML: &str = include_str!("../assets/knowledge/error_patterns.toml");

#[derive(Debug, Error)]
pub enum KnowledgeError {
    #[error("版本号非法：{input}。请给出 MC 正式版版本号（如 26.2 或 1.21.1）")]
    BadVersion { input: String },
    #[error("查询上游 API 失败：{0}")]
    Upstream(#[from] upstream::UpstreamError),
    #[error("mod {0} 在 Modrinth 上不存在（或无匹配版本）。检索无结果")]
    ModNotFound(String),
}

/// Java 需求口径来源（v0.9：动态事实优先，静态表只兜底）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JavaMajorSource {
    /// piston-meta 版本 JSON 的 `javaVersion` 字段（Mojang 官方启动器同源）
    Manifest,
    /// 上游不可达/字段缺失时的 L1 静态表兜底
    L1Fallback,
    /// 动态与静态都拿不到
    Unknown,
}

/// Java 需求解析（v0.9，"能查就不猜"）：动态事实优先，L1 兜底。
/// 返回 (Java 大版本, 口径来源)。纯函数，便于离线单测。
pub fn resolve_java_major(
    manifest_major: Option<u8>,
    kb: &KnowledgeBase,
    mc_version: &str,
) -> (Option<u8>, JavaMajorSource) {
    if let Some(major) = manifest_major {
        return (Some(major), JavaMajorSource::Manifest);
    }
    match kb.java_major_for(mc_version) {
        Some(major) => (Some(major), JavaMajorSource::L1Fallback),
        None => (None, JavaMajorSource::Unknown),
    }
}

/// MC → Java 映射条目。
#[derive(Debug, Clone, Deserialize)]
pub struct JavaMapEntry {
    pub min_version: String,
    pub java_major: u8,
}

/// mod 中文别名条目：`project` 为 Modrinth project_id（API 事实键），
/// `slug` 为可读名（用于 spec_id 等展示）。
#[derive(Debug, Clone, Deserialize)]
pub struct ModAlias {
    pub zh: String,
    pub project: String,
    pub slug: String,
}

/// 崩溃错误模式（L-1，diagnose 确定性匹配用）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ErrorPattern {
    pub id: String,
    pub regex: String,
    pub cause: String,
    #[serde(default)]
    pub fix: String,
}

/// 静态知识库：运行时从嵌入 TOML 加载，只读。
#[derive(Debug, Clone)]
pub struct KnowledgeBase {
    pub java_map: Vec<JavaMapEntry>,
    pub aliases: Vec<ModAlias>,
    /// L-1 错误模式库：diagnose 模块（P1 交付）的确定性匹配数据，
    /// MVP 阶段仅随包分发与测试解析，暂无读取方。
    #[allow(dead_code)]
    pub error_patterns: Vec<ErrorPattern>,
}

/// TOML 顶层表包装（`[[entries]]` 等数组的反序列化载体）。
#[derive(Deserialize)]
struct JavaMapFile {
    entries: Vec<JavaMapEntry>,
}

#[derive(Deserialize)]
struct AliasFile {
    aliases: Vec<ModAlias>,
}

#[derive(Deserialize)]
struct PatternFile {
    patterns: Vec<ErrorPattern>,
}

impl KnowledgeBase {
    /// 加载嵌入的静态知识（每次构造即解析，量小无性能顾虑）。
    pub fn embedded() -> Result<Self, toml::de::Error> {
        Ok(Self {
            java_map: toml::from_str::<JavaMapFile>(JAVA_MAP_TOML)?.entries,
            aliases: toml::from_str::<AliasFile>(ALIASES_TOML)?.aliases,
            error_patterns: toml::from_str::<PatternFile>(ERROR_PATTERNS_TOML)?.patterns,
        })
    }

    /// MC 版本 → 所需 Java 大版本（知识库查表，非 LLM 猜测）。
    pub fn java_major_for(&self, mc_version: &str) -> Option<u8> {
        let version = normalize_version(mc_version).ok()?;
        let mut best: Option<(semver::Version, u8)> = None;
        for entry in &self.java_map {
            let Ok(min) = normalize_version(&entry.min_version) else {
                continue;
            };
            if version >= min {
                // 取满足条件的最大 min_version（最后一条适用规则）
                if best.as_ref().is_none_or(|(v, _)| min >= *v) {
                    best = Some((min, entry.java_major));
                }
            }
        }
        best.map(|(_, major)| major)
    }

    /// mod 中文名 → Modrinth project_id（2026-08 实测 API 仅认 project_id）。
    pub fn alias_lookup(&self, name: &str) -> Option<String> {
        self.aliases
            .iter()
            .find(|a| a.zh == name)
            .map(|a| a.project.clone())
    }

    /// mod 中文名 → 可读 slug（生成 spec_id 等展示用）。
    pub fn alias_slug(&self, name: &str) -> Option<String> {
        self.aliases
            .iter()
            .find(|a| a.zh == name)
            .map(|a| a.slug.clone())
    }
}

/// 归一化 MC 版本号：补全 patch 段（"1.21" → "1.21.0"），
/// 非法输入（如快照名、乱码）返回 [`KnowledgeError::BadVersion`]。
pub fn normalize_version(input: &str) -> Result<semver::Version, KnowledgeError> {
    let trimmed = input.trim().trim_start_matches('v');
    if trimmed.is_empty() {
        return Err(KnowledgeError::BadVersion {
            input: input.to_string(),
        });
    }
    let dotted = match trimmed.split('.').count() {
        1 => format!("{trimmed}.0.0"),
        2 => format!("{trimmed}.0"),
        _ => trimmed.to_string(),
    };
    // 只接受 x.y.z 纯数字正式版：快照（25w14a）与胡编输入在此被拒
    semver::Version::parse(&dotted).map_err(|_| KnowledgeError::BadVersion {
        input: input.to_string(),
    })
}

/// 版本校验结论（决策树与 LLM 工具的返回格式）。
#[derive(Debug, Clone, Serialize)]
pub struct CompatReport {
    pub mc_version: String,
    pub exists: bool,
    pub java_major: Option<u8>,
    /// Java 需求口径来源（v0.9）：manifest = 官方动态值，l1_fallback = 静态表
    pub java_major_source: JavaMajorSource,
    pub software: String,
    /// 不通过的说明（供 ui / LLM 澄清）
    pub issues: Vec<String>,
    /// 建议的相近版本（拒绝幻觉版本号时给用户选择）
    pub suggestions: Vec<String>,
}

/// 给出被拒版本号的相近建议：主/次版本距离最近者优先。
pub fn suggest_versions(available: &[String], rejected: &str, limit: usize) -> Vec<String> {
    let Ok(rejected_v) = normalize_version(rejected) else {
        return available.iter().rev().take(limit).cloned().collect();
    };
    let mut scored: Vec<(i64, &String)> = available
        .iter()
        .filter_map(|v| {
            let parsed = normalize_version(v).ok()?;
            // 距离 = 主版本差 * 1000 + 次版本差；取最小
            let dist = (parsed.major as i64 - rejected_v.major as i64).abs() * 1000
                + (parsed.minor as i64 - rejected_v.minor as i64).abs();
            Some((dist, v))
        })
        .collect();
    scored.sort_by_key(|(dist, _)| *dist);
    scored.truncate(limit);
    scored.into_iter().map(|(_, v)| v.clone()).collect()
}

/// 加载器名称 → spec 软件类型（决策树节点 2 的模糊匹配收敛点）。
pub fn parse_software(name: &str) -> Option<ServerSoftware> {
    match name.trim().to_lowercase().as_str() {
        "vanilla" | "原版" => Some(ServerSoftware::Vanilla),
        "paper" => Some(ServerSoftware::Paper { build: None }),
        "fabric" => Some(ServerSoftware::Fabric {
            loader_version: String::new(),
            installer_version: String::new(),
        }),
        _ => None,
    }
}

/// 混合认证方案由服务端类型决定（决策树：Paper→插件，Fabric→EasyAuth）。
pub fn hybrid_auth_for(software: &ServerSoftware) -> Option<HybridAuth> {
    match software {
        ServerSoftware::Paper { .. } => Some(HybridAuth::Plugin),
        ServerSoftware::Fabric { .. } => Some(HybridAuth::EasyAuth),
        ServerSoftware::Vanilla => None,
    }
}

/// 把 mod 依赖闭包展平为下载清单（去重，拓扑序：依赖在前）。
pub fn flatten_mods(mods: &[ModRef]) -> Vec<ModRef> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    fn walk(m: &ModRef, seen: &mut std::collections::HashSet<String>, out: &mut Vec<ModRef>) {
        for dep in &m.deps {
            walk(dep, seen, out);
        }
        if seen.insert(m.version_id.clone()) {
            out.push(m.clone());
        }
    }
    for m in mods {
        walk(m, &mut seen, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KnowledgeBase {
        KnowledgeBase::embedded().unwrap()
    }

    #[test]
    fn 版本映射查表() {
        let kb = kb();
        // v0.9：年份制版本——26.1 起官方要求 Java 25（piston-meta 实测）
        assert_eq!(kb.java_major_for("26.2"), Some(25));
        assert_eq!(kb.java_major_for("26.1"), Some(25));
        assert_eq!(kb.java_major_for("26.1.2"), Some(25));
        // 1.x 时代规则不受影响
        assert_eq!(kb.java_major_for("1.21.1"), Some(21));
        assert_eq!(kb.java_major_for("1.21"), Some(21));
        assert_eq!(kb.java_major_for("1.20.4"), Some(17));
        assert_eq!(kb.java_major_for("1.20.5"), Some(21));
        assert_eq!(kb.java_major_for("1.18.2"), Some(17));
        assert_eq!(kb.java_major_for("1.17.1"), Some(16));
        assert_eq!(kb.java_major_for("1.16.5"), Some(8));
        assert_eq!(kb.java_major_for("25w14a"), None, "快照版不受理");
        assert_eq!(
            kb.java_major_for("26.3-snapshot-10"),
            None,
            "年份制快照同样不受理"
        );
    }

    #[test]
    fn java需求解析_动态优先静态兜底() {
        let kb = kb();
        // 官方动态值可用 → manifest 口径
        let (major, source) = resolve_java_major(Some(25), &kb, "26.2");
        assert_eq!((major, source), (Some(25), JavaMajorSource::Manifest));
        // 动态缺失但静态表命中 → l1_fallback 口径
        let (major, source) = resolve_java_major(None, &kb, "26.2");
        assert_eq!((major, source), (Some(25), JavaMajorSource::L1Fallback));
        // 两处都拿不到 → unknown（semver 数值比较下 1.5 低于最早收录版本）
        let (major, source) = resolve_java_major(None, &kb, "1.5");
        assert_eq!((major, source), (None, JavaMajorSource::Unknown));
    }

    #[test]
    fn 别名查表() {
        let kb = kb();
        // project 为 Modrinth project_id（API 事实键），slug 为可读名
        // v0.9 复核：暮色森林改登 eDeSn4Ds（原 TeamTwilight 已不在 Modrinth）
        assert_eq!(kb.alias_lookup("暮色森林").as_deref(), Some("eDeSn4Ds"));
        assert_eq!(kb.alias_slug("暮色森林").as_deref(), Some("twilightforest"));
        assert_eq!(kb.alias_lookup("不存在的mod"), None);
    }

    #[test]
    fn 形式合法版本被接受_存在性交上游判定() {
        // v0.9 勘误：26.2 是年份制正式版（2026-06 发布），非幻觉；形式合法，
        // 存在性由上游清单判定。真正的非法输入是快照名与胡编。
        assert!(normalize_version("26.2").is_ok());
        assert!(normalize_version("26.1.2").is_ok());
        assert!(normalize_version(" bananas").is_err());
        assert!(normalize_version("").is_err());
        assert!(normalize_version("25w14a").is_err(), "快照名非法");
        assert!(
            normalize_version("26.3-snapshot-10").is_err(),
            "年份制快照非法"
        );
    }

    #[test]
    fn 就近版本建议() {
        let mut available: Vec<String> = ["1.21.1", "1.21", "1.20.6", "1.20.4", "1.19.2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let suggestions = suggest_versions(&available, "1.20.99", 3);
        assert_eq!(suggestions.first().map(String::as_str), Some("1.20.6"));
        // 年份制：1.x 拼错的建议不会窜到 26.x，反之亦然
        available = ["26.2", "26.1", "1.21.1", "1.20.6"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let suggestions = suggest_versions(&available, "26.3", 2);
        assert_eq!(suggestions.first().map(String::as_str), Some("26.2"));
        let suggestions = suggest_versions(&available, "1.20.99", 2);
        assert_eq!(suggestions.first().map(String::as_str), Some("1.20.6"));
    }

    #[test]
    fn 依赖闭包展平去重() {
        let leaf = ModRef {
            project: "a".into(),
            version_id: "va".into(),
            url: String::new(),
            sha1: String::new(),
            file_name: "a.jar".into(),
            deps: vec![],
        };
        let mid = ModRef {
            project: "b".into(),
            version_id: "vb".into(),
            url: String::new(),
            sha1: String::new(),
            file_name: "b.jar".into(),
            deps: vec![leaf.clone()],
        };
        let top = ModRef {
            project: "c".into(),
            version_id: "vc".into(),
            url: String::new(),
            sha1: String::new(),
            file_name: "c.jar".into(),
            deps: vec![mid.clone(), leaf],
        };
        let flat = flatten_mods(&[top.clone(), mid]);
        let ids: Vec<&str> = flat.iter().map(|m| m.version_id.as_str()).collect();
        assert_eq!(ids, vec!["va", "vb", "vc"], "依赖在前、无重复");
    }

    #[test]
    fn 错误模式库可解析且关键模式存在() {
        let kb = kb();
        let ids: Vec<&str> = kb.error_patterns.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"java-version-mismatch"));
        assert!(ids.contains(&"port-in-use"));
        assert!(ids.contains(&"mod-incompatible"));
    }
}
