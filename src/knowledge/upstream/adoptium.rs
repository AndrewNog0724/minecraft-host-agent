//! Adoptium v3 API 客户端：Temurin JRE 解析（决议 D2/D115，设计 §8.7/§8.10）。
//!
//! 元数据走官方 API（含 sha256，元数据与二进制同源、校验可信）；二进制
//! 默认走清华 TUNA 镜像，路径规则 `/Adoptium/{major}/{image}/{arch}/{os}/
//! {文件名}`（实测 2026-09-02），镜像 404 时调用方回退官方 link。

use serde::Deserialize;

use super::{read_json, send_get};

/// 官方 API 基址。
pub const OFFICIAL_API: &str = "https://api.adoptium.net/v3";
/// 清华 TUNA 镜像基址。
pub const TUNA_BASE: &str = "https://mirrors.tuna.tsinghua.edu.cn/Adoptium";

#[derive(Debug, Deserialize)]
struct AssetsResponse {
    binary: BinaryJson,
    #[serde(rename = "release_name")]
    release_name: String,
    version: VersionJson,
}

#[derive(Debug, Deserialize)]
struct BinaryJson {
    #[serde(rename = "package")]
    package: PackageJson,
}

#[derive(Debug, Deserialize)]
struct PackageJson {
    name: String,
    link: String,
    checksum: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct VersionJson {
    major: u32,
}

/// Temurin JRE 解析结果。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedJre {
    pub major: u32,
    /// 发布名（如 jdk-21.0.12.1+1），作受管安装目录名。
    pub release_name: String,
    /// 官方下载 link（GitHub releases）。
    pub official_url: String,
    /// TUNA 镜像 URL（None = 该包名无法构造镜像路径，用官方）。
    pub mirror_url: Option<String>,
    /// 官方 sha256（hex，小写）。
    pub sha256: String,
    pub size: u64,
    pub file_name: String,
}

/// 本机架构 → Adoptium API 架构值。
fn adoptium_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        "x86" => "x86",
        other => other,
    }
}

/// 本机 OS → Adoptium API os 值。
fn adoptium_os() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "linux" => "linux",
        "macos" => "mac",
        other => other,
    }
}

/// 由文件名构造 TUNA 镜像 URL（路径规则实测见模块注释）。
pub(crate) fn tuna_url(
    major: u32,
    image_type: &str,
    arch: &str,
    os: &str,
    file_name: &str,
) -> String {
    format!("{TUNA_BASE}/{major}/{image_type}/{arch}/{os}/{file_name}")
}

pub struct AdoptiumClient<'a> {
    http: &'a reqwest::Client,
    api_base: String,
}

impl<'a> AdoptiumClient<'a> {
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

    /// 解析指定大版本的最新 Temurin **JRE**（hotspot，本机 arch/os）。
    pub async fn latest_jre(&self, major: u32) -> Result<ResolvedJre, String> {
        let arch = adoptium_arch();
        let os = adoptium_os();
        let url = format!(
            "{}/assets/latest/{major}/hotspot?architecture={arch}&image_type=jre&os={os}&vendor=eclipse",
            self.api_base
        );
        let response = send_get(self.http, &url).await?;
        if response.status().as_u16() == 404 {
            return Err(format!(
                "Adoptium 无 Java {major} 的 JRE 包（本机 {os}/{arch}）"
            ));
        }
        let assets: Vec<AssetsResponse> =
            read_json(response, &format!("Adoptium Java {major} 资产列表")).await?;
        let asset = assets
            .first()
            .ok_or_else(|| format!("Adoptium 对 Java {major} 返回空资产列表"))?;
        Ok(ResolvedJre {
            major: asset.version.major,
            release_name: asset.release_name.clone(),
            official_url: asset.binary.package.link.clone(),
            mirror_url: Some(tuna_url(major, "jre", arch, os, &asset.binary.package.name)),
            sha256: asset.binary.package.checksum.to_ascii_lowercase(),
            size: asset.binary.package.size,
            file_name: asset.binary.package.name.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuna_path_rule() {
        assert_eq!(
            tuna_url(
                21,
                "jre",
                "x64",
                "linux",
                "OpenJDK21U-jre_x64_linux_hotspot_21.0.12.1_1.tar.gz"
            ),
            "https://mirrors.tuna.tsinghua.edu.cn/Adoptium/21/jre/x64/linux/OpenJDK21U-jre_x64_linux_hotspot_21.0.12.1_1.tar.gz"
        );
    }

    #[test]
    fn arch_and_os_mapping() {
        // 本机映射落在已知集合内
        assert!(matches!(adoptium_arch(), "x64" | "aarch64" | "x86"));
        assert!(matches!(adoptium_os(), "windows" | "linux" | "mac"));
    }
}
