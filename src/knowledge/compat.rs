//! L1 静态兼容知识（设计 §8.4/§8.10）：`java_compat.toml`（MC 版本段 →
//! Java 大版本）与 `server_software.toml`（服务端软件 × 支持范围 × 渠道）。
//!
//! 随包编译期内嵌（include_str!），带更新日期与来源注释；只经查询工具
//! （check_version_compat）返回值进入 Agent 上下文，永不进 Prompt（红线）。

use serde::Deserialize;
use std::sync::OnceLock;

use super::version::McVersion;

const JAVA_COMPAT_TOML: &str = include_str!("../assets/knowledge/java_compat.toml");
const SERVER_SOFTWARE_TOML: &str = include_str!("../assets/knowledge/server_software.toml");

// ---------------------------------------------------------------------------
// java_compat.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct JavaCompatFile {
    updated: String,
    ranges: Vec<JavaRangeEntry>,
}

#[derive(Debug, Deserialize)]
pub struct JavaRangeEntry {
    pub min_mc: String,
    /// 空 = 开区间（覆盖到最新版本）。
    pub max_mc: String,
    pub java_major: u32,
    pub note: String,
}

/// 一条 Java 版本要求查询结果。
#[derive(Debug, Clone)]
pub struct JavaRequirement {
    pub java_major: u32,
    /// 命中的知识表区间（人类可读）。
    pub source_range: String,
    pub note: String,
}

/// MC 版本段 → Java 大版本知识表。
pub struct JavaCompat {
    pub updated: String,
    ranges: Vec<JavaRangeEntry>,
}

impl JavaCompat {
    pub fn builtin() -> &'static Self {
        static K: OnceLock<JavaCompat> = OnceLock::new();
        K.get_or_init(|| {
            let file: JavaCompatFile = toml::from_str(JAVA_COMPAT_TOML)
                .expect("内置 java_compat.toml 格式错误（编译期内嵌文件）");
            JavaCompat {
                updated: file.updated,
                ranges: file.ranges,
            }
        })
    }

    /// 查版本对应的 Java 大版本要求；不在任何区间时返回 None。
    pub fn lookup(&self, version: &McVersion) -> Option<JavaRequirement> {
        for range in &self.ranges {
            let min = McVersion::parse(&range.min_mc).ok()?;
            if version < &min {
                continue;
            }
            let within_max = range.max_mc.is_empty()
                || McVersion::parse(&range.max_mc).is_ok_and(|max| version <= &max);
            if within_max {
                let max_text = if range.max_mc.is_empty() {
                    "最新".to_string()
                } else {
                    range.max_mc.clone()
                };
                return Some(JavaRequirement {
                    java_major: range.java_major,
                    source_range: format!("{} ~ {}", range.min_mc, max_text),
                    note: range.note.clone(),
                });
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// server_software.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SoftwareFile {
    updated: String,
    software: Vec<SoftwareEntry>,
}

/// 一条服务端软件知识（渠道与哈希可信度如实标注）。
#[derive(Debug, Deserialize, Clone)]
pub struct SoftwareEntry {
    /// 机器标识：vanilla | paper | spigot | fabric | forge。
    pub id: String,
    pub name: String,
    /// mojang | papermc | getbukkit | fabricmeta | guided。
    pub channel: String,
    pub min_mc: String,
    /// 空 = 开区间。
    pub max_mc: String,
    /// official-sha1 | official-sha256 | computed-sha256 | none。
    pub hash: String,
    pub note: String,
}

impl SoftwareEntry {
    /// 该软件对指定 MC 版本是否在支持范围内。
    pub fn supports(&self, version: &McVersion) -> bool {
        let Ok(min) = McVersion::parse(&self.min_mc) else {
            return false;
        };
        if version < &min {
            return false;
        }
        self.max_mc.is_empty() || McVersion::parse(&self.max_mc).is_ok_and(|max| version <= &max)
    }
}

/// 服务端软件知识表。
pub struct SoftwareCatalog {
    pub updated: String,
    pub software: Vec<SoftwareEntry>,
}

impl SoftwareCatalog {
    pub fn builtin() -> &'static Self {
        static K: OnceLock<SoftwareCatalog> = OnceLock::new();
        K.get_or_init(|| {
            let file: SoftwareFile = toml::from_str(SERVER_SOFTWARE_TOML)
                .expect("内置 server_software.toml 格式错误（编译期内嵌文件）");
            SoftwareCatalog {
                updated: file.updated,
                software: file.software,
            }
        })
    }

    /// 按 id 查软件条目。
    pub fn find(&self, id: &str) -> Option<&SoftwareEntry> {
        self.software.iter().find(|s| s.id == id)
    }
}

impl SoftwareEntry {
    /// 展示名（工具结果渲染用）。
    #[allow(dead_code)]
    pub fn display_name(&self) -> &str {
        &self.name
    }

    /// 哈希可信度标识（S4 fetch 使用：official-* 强校验，其余落地计算）。
    #[allow(dead_code)]
    pub fn hash_kind(&self) -> &str {
        &self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> McVersion {
        McVersion::parse(s).unwrap()
    }

    #[test]
    fn java_lookup_hits_each_range() {
        let compat = JavaCompat::builtin();
        assert_eq!(compat.lookup(&v("1.16.5")).unwrap().java_major, 8);
        assert_eq!(compat.lookup(&v("1.17")).unwrap().java_major, 16);
        assert_eq!(compat.lookup(&v("1.18")).unwrap().java_major, 17);
        assert_eq!(compat.lookup(&v("1.20.4")).unwrap().java_major, 17);
        assert_eq!(compat.lookup(&v("1.20.5")).unwrap().java_major, 21);
        assert_eq!(compat.lookup(&v("1.21.1")).unwrap().java_major, 21);
        // 1.x 上界内不落入年份版本线
        assert_eq!(compat.lookup(&v("1.22")).unwrap().java_major, 21);
        // 年份版本线（26.x → Java 25，实测 26.2）
        assert_eq!(compat.lookup(&v("26.2")).unwrap().java_major, 25);
    }

    #[test]
    fn java_lookup_misses_below_all_ranges() {
        let compat = JavaCompat::builtin();
        assert!(compat.lookup(&v("0.30")).is_none());
    }

    #[test]
    fn software_catalog_supports_ranges() {
        let catalog = SoftwareCatalog::builtin();
        let fabric = catalog.find("fabric").unwrap();
        assert!(!fabric.supports(&v("1.13.2")));
        assert!(fabric.supports(&v("1.14")));
        assert!(fabric.supports(&v("1.21.1")));
        let forge = catalog.find("forge").unwrap();
        assert_eq!(forge.channel, "guided");
        assert!(catalog.find("bukkit").is_none());
    }
}
