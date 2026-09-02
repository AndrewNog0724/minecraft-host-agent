//! Fabric meta 客户端：服务端启动器 jar 解析（设计 §8.10）。
//!
//! 端点：`/v2/versions/loader/{game}`（该游戏版本的 loader 列表，新到旧）、
//! `/v2/versions/installer`（安装器列表）、
//! `/v2/versions/loader/{game}/{loader}/{installer}/server/jar`（整合启动器）。
//! 整包无官方哈希——下载后计算 sha256 留痕（trust_note 如实标注）。

use serde::Deserialize;

use super::{read_json, send_get, urlencode};

/// 官方 meta 基址。
pub const OFFICIAL_META: &str = "https://meta.fabricmc.net";

#[derive(Debug, Deserialize)]
pub struct VersionEntry {
    pub version: String,
    pub stable: bool,
}

/// loader 列表条目：loader 字段嵌套（`{loader: {...}, intermediary: {...}, ...}`）。
#[derive(Debug, Deserialize)]
struct LoaderListEntry {
    loader: VersionEntry,
}

/// Fabric 服务端解析结果。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedFabric {
    pub loader: String,
    pub installer: String,
    /// 服务端启动器 jar 下载 URL（S4 fetch 使用）。
    pub url: String,
}

pub struct FabricClient<'a> {
    http: &'a reqwest::Client,
    meta_base: String,
}

impl<'a> FabricClient<'a> {
    pub fn new(http: &'a reqwest::Client) -> Self {
        Self {
            http,
            meta_base: OFFICIAL_META.to_string(),
        }
    }

    /// 测试注入：自定义 meta 基址（本地 mock）。
    #[allow(dead_code)]
    pub fn with_base(http: &'a reqwest::Client, meta_base: String) -> Self {
        Self {
            http,
            meta_base: meta_base.trim_end_matches('/').to_string(),
        }
    }

    /// 解析某 MC 版本的 Fabric 服务端启动器（最新稳定 loader + 最新 installer）。
    pub async fn resolve_server(&self, mc_version: &str) -> Result<ResolvedFabric, String> {
        let loaders_url = format!(
            "{}/v2/versions/loader/{}",
            self.meta_base,
            urlencode(mc_version)
        );
        let response = send_get(self.http, &loaders_url).await?;
        if response.status().as_u16() == 404 {
            return Err(format!(
                "Fabric 不支持 MC {mc_version}（loader 列表为空或版本不存在）"
            ));
        }
        let wrapped: Vec<LoaderListEntry> =
            read_json(response, &format!("Fabric loader 列表（{mc_version}）")).await?;
        let loader = wrapped
            .iter()
            .map(|e| &e.loader)
            .find(|e| e.stable)
            .map(|e| e.version.clone())
            .ok_or_else(|| format!("Fabric 对 MC {mc_version} 暂无稳定版 loader"))?;

        let installer_url = format!("{}/v2/versions/installer", self.meta_base);
        let response = send_get(self.http, &installer_url).await?;
        let installers: Vec<VersionEntry> = read_json(response, "Fabric installer 列表").await?;
        let installer = installers
            .iter()
            .find(|e| e.stable)
            .or_else(|| installers.first())
            .map(|e| e.version.clone())
            .ok_or_else(|| "Fabric installer 列表为空".to_string())?;

        Ok(ResolvedFabric {
            url: format!(
                "{}/v2/versions/loader/{}/{}/{}/server/jar",
                self.meta_base,
                urlencode(mc_version),
                urlencode(&loader),
                urlencode(&installer)
            ),
            loader,
            installer,
        })
    }
}
