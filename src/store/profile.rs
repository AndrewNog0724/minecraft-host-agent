//! 部署档案 Profile（R5 / US3；设计 §8.6/§8.12）。
//!
//! Profile = 部署方案的结构化快照（方案 + 产物清单 + 时间戳），由 Agent 经
//! `save_profile` 工具落盘、`load_profile` 读回会话上下文；定位是记录产物
//! 与复用载体，不是流程关卡。落盘 `~/.mcha/profiles/<profile_id>.json`。

use anyhow::Context;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::ensure_dir;

/// 档案中的单个 mod 记录。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProfileMod {
    /// Modrinth 项目 slug
    pub slug: String,
    /// Modrinth 版本 id
    pub version_id: String,
    /// 落盘文件名
    pub file_name: String,
    /// 安装时校验的 sha1
    pub sha1: String,
}

/// 档案中的实际产物记录。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProfileArtifact {
    /// 产物类型（jar | script | mods_dir | properties | whitelist | log …）
    pub kind: String,
    /// 相对 / 绝对路径
    pub path: String,
}

/// 部署方案快照（字段定稿见设计 §8.6）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Profile {
    /// 档案 id（`YYYYMMDD-HHMMSS`，保存时生成）
    pub profile_id: String,
    /// 保存时间（RFC3339）
    pub created_at: String,
    /// 账号类型：all_online | all_offline | mixed
    pub account: String,
    /// 服务端软件：vanilla | paper | spigot | fabric
    pub software: String,
    /// MC 版本
    pub mc_version: String,
    /// Java 大版本要求
    pub java_major: u32,
    /// Java 运行时绝对路径（受管安装时记录）
    #[serde(default)]
    pub java_path: Option<String>,
    /// JVM -Xmx（MB）
    pub jvm_memory_mb: u32,
    /// mod 清单（无 mod 时为空）
    #[serde(default)]
    pub mods: Vec<ProfileMod>,
    /// 网络方案描述（如 lan / direct:25565 / tunnel:sakura）
    #[serde(default)]
    pub network: String,
    /// 世界方案：new 或既有世界路径
    #[serde(default)]
    pub world: String,
    /// 实际产物清单
    #[serde(default)]
    pub artifacts: Vec<ProfileArtifact>,
    /// 风险提示等备注
    #[serde(default)]
    pub notes: String,
}

/// profiles 目录：`<数据目录>/profiles/`。
pub fn profiles_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("profiles")
}

/// 落盘档案（profile_id 已在结构中）。
pub fn save(data_dir: &Path, profile: &Profile) -> anyhow::Result<PathBuf> {
    let dir = profiles_dir(data_dir);
    ensure_dir(&dir)?;
    let path = dir.join(format!("{}.json", profile.profile_id));
    let text = serde_json::to_string_pretty(profile).context("序列化档案失败")?;
    std::fs::write(&path, text).with_context(|| format!("写出档案失败：{}", path.display()))?;
    Ok(path)
}

/// 读取单个档案。
pub fn load(data_dir: &Path, profile_id: &str) -> anyhow::Result<Profile> {
    let path = profiles_dir(data_dir).join(format!("{}.json", profile_id));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取档案失败：{}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("解析档案失败：{}", path.display()))
}

/// 列出全部档案（按 id 降序 = 新到旧）。
pub fn list(data_dir: &Path) -> anyhow::Result<Vec<Profile>> {
    let dir = profiles_dir(data_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut profiles = Vec::new();
    for entry in
        std::fs::read_dir(&dir).with_context(|| format!("读取目录失败：{}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("跳过无法读取的档案 {}：{err}", path.display());
                continue;
            }
        };
        match serde_json::from_str::<Profile>(&text) {
            Ok(profile) => profiles.push(profile),
            Err(err) => eprintln!("跳过格式非法的档案 {}：{err}", path.display()),
        }
    }
    profiles.sort_by(|a, b| b.profile_id.cmp(&a.profile_id));
    Ok(profiles)
}

/// 生成 profile_id：`YYYYMMDD-HHMMSS`（本地时区，秒级）。
pub fn new_profile_id() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> Profile {
        Profile {
            profile_id: id.to_string(),
            created_at: "2026-09-03T12:00:00+08:00".to_string(),
            account: "all_offline".to_string(),
            software: "fabric".to_string(),
            mc_version: "1.21.1".to_string(),
            java_major: 21,
            java_path: Some("/data/runtime/jdk-21/21.0.4/bin/java".to_string()),
            jvm_memory_mb: 4096,
            mods: vec![ProfileMod {
                slug: "jei".to_string(),
                version_id: "AbCd".to_string(),
                file_name: "jei.jar".to_string(),
                sha1: "aa".to_string(),
            }],
            network: "lan".to_string(),
            world: "new".to_string(),
            artifacts: vec![ProfileArtifact {
                kind: "jar".to_string(),
                path: "server/server.jar".to_string(),
            }],
            notes: "离线模式已开启白名单".to_string(),
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let profile = sample("20260903-120000");
        let path = save(root.path(), &profile).unwrap();
        assert!(path.exists());
        let loaded = load(root.path(), "20260903-120000").unwrap();
        assert_eq!(loaded.software, "fabric");
        assert_eq!(loaded.mods.len(), 1);
        assert_eq!(loaded.mods[0].slug, "jei");
    }

    #[test]
    fn list_orders_newest_first_and_tolerates_bad_files() {
        let root = tempfile::tempdir().unwrap();
        save(root.path(), &sample("20260903-120000")).unwrap();
        save(root.path(), &sample("20260904-130000")).unwrap();
        std::fs::write(profiles_dir(root.path()).join("broken.json"), "not json").unwrap();
        let list = list(root.path()).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].profile_id, "20260904-130000");
    }

    #[test]
    fn load_missing_reports_path() {
        let root = tempfile::tempdir().unwrap();
        let err = load(root.path(), "no-such").unwrap_err();
        assert!(err.to_string().contains("no-such"));
    }
}
