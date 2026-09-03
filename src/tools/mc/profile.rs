//! Profile 存取工具（FR-16 / US3；设计 §8.12）：save_profile / load_profile。
//!
//! save_profile（Write，确认门）：Agent 填方案字段 → schemars 校验 +
//! server_dir 存在性核对 → JSON 原子落 `~/.mcha/profiles/`。
//! load_profile（ReadOnly）：profile_id 或 latest → 全文读回上下文。

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;
use crate::store::profile::{self, Profile, ProfileArtifact, ProfileMod};
use crate::tools::confinement::resolve_in;

use super::{Permission, Tool, ToolCtx, ToolError};

// ---------------------------------------------------------------------------
// save_profile
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveProfileArgs {
    /// 账号类型：all_online | all_offline | mixed
    pub account: String,
    /// 服务端软件：vanilla | paper | spigot | fabric
    pub software: String,
    /// MC 版本
    pub mc_version: String,
    /// Java 大版本要求
    pub java_major: u32,
    /// Java 运行时绝对路径（受管安装时记录；可选）
    #[serde(default)]
    pub java_path: Option<String>,
    /// JVM -Xmx（MB）
    pub jvm_memory_mb: u32,
    /// mod 清单（无 mod 时省略）
    #[serde(default)]
    pub mods: Vec<ProfileMod>,
    /// 网络方案描述（如 lan / direct:25565 / tunnel:sakura）
    #[serde(default)]
    pub network: String,
    /// 世界方案：new 或既有世界路径
    #[serde(default)]
    pub world: String,
    /// 实际产物清单（jar / 脚本 / mods 目录 / 日志等）
    #[serde(default)]
    pub artifacts: Vec<ProfileArtifact>,
    /// 风险提示等备注
    #[serde(default)]
    pub notes: String,
    /// 服务器目录（工作区内；存在性核对用，默认 server）
    #[serde(default)]
    pub server_dir: Option<String>,
}

pub struct SaveProfileTool;

#[async_trait::async_trait]
impl Tool for SaveProfileTool {
    fn name(&self) -> &'static str {
        "save_profile"
    }
    fn description(&self) -> String {
        "把当前部署方案与产物清单保存为部署档案（~/.mcha/profiles/），供日后 load_profile 复用。"
            .into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(SaveProfileArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::Write
    }
    fn confirm_summary(&self, args: &serde_json::Value) -> Vec<String> {
        let get = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let mods = args
            .get("mods")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        vec![
            format!(
                "保存部署档案：{} × MC {}（{}，-Xmx{}MB，mod {mods} 个）",
                get("software"),
                get("mc_version"),
                get("account"),
                args.get("jvm_memory_mb")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            ),
            "写入 ~/.mcha/profiles/<profile_id>.json".to_string(),
        ]
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: SaveProfileArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        // server_dir 存在性核对（提示性质，不阻断——档案可以先于目录清理保存）
        let mut warnings: Vec<String> = Vec::new();
        let server_dir: PathBuf = resolve_in(
            &[ctx.workspace.as_path()],
            args.server_dir.as_deref().unwrap_or("server"),
        )?;
        if !server_dir.exists() {
            warnings.push(format!(
                "提示：{} 当前不存在（方案可能尚未部署）",
                server_dir.display()
            ));
        }

        let account = args.account.as_str();
        if !matches!(account, "all_online" | "all_offline" | "mixed") {
            return Ok(ToolOutcome::err(format!(
                "未知账号类型「{account}」；可选 all_online | all_offline | mixed"
            )));
        }

        let profile = Profile {
            profile_id: profile::new_profile_id(),
            created_at: crate::store::now_rfc3339(),
            account: args.account,
            software: args.software,
            mc_version: args.mc_version,
            java_major: args.java_major,
            java_path: args.java_path,
            jvm_memory_mb: args.jvm_memory_mb,
            mods: args.mods,
            network: args.network,
            world: args.world,
            artifacts: args.artifacts,
            notes: args.notes,
        };
        let path = profile::save(&ctx.data_dir, &profile)
            .map_err(|err| ToolError::Io(format!("保存档案失败：{err}")))?;
        let mut lines = vec![format!(
            "已保存部署档案 {id}（{software} × MC {mc}，mod {mods} 个）",
            id = profile.profile_id,
            software = profile.software,
            mc = profile.mc_version,
            mods = profile.mods.len()
        )];
        lines.extend(warnings);
        lines.push(format!("文件：{}", path.display()));
        lines.push("日后可说「加载上次的配置档案」或用 load_profile 读回复用。".to_string());
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// load_profile
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadProfileArgs {
    /// 档案 id（save_profile 返回；省略或 latest = 最近的档案）
    #[serde(default)]
    pub profile_id: Option<String>,
}

pub struct LoadProfileTool;

#[async_trait::async_trait]
impl Tool for LoadProfileTool {
    fn name(&self) -> &'static str {
        "load_profile"
    }
    fn description(&self) -> String {
        "读取部署档案到会话上下文（含 mod 清单与产物清单），用于对照现状补差复用。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(LoadProfileArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: LoadProfileArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let wanted = args
            .profile_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let loaded = match wanted {
            None | Some("latest") => {
                let all = match profile::list(&ctx.data_dir) {
                    Ok(all) => all,
                    Err(err) => return Ok(ToolOutcome::err(format!("列出档案失败：{err}"))),
                };
                match all.first() {
                    Some(latest) => latest.clone(),
                    None => {
                        return Ok(ToolOutcome::err(
                            "尚无已保存的部署档案；可先完成一次部署并用 save_profile 保存",
                        ));
                    }
                }
            }
            Some(id) => match profile::load(&ctx.data_dir, id) {
                Ok(loaded) => loaded,
                Err(err) => return Ok(ToolOutcome::err(format!("读取档案失败：{err}"))),
            },
        };

        let mut lines = vec![format!(
            "已加载部署档案 {id}（保存于 {at}）：",
            id = loaded.profile_id,
            at = loaded.created_at
        )];
        lines.push(format!(
            "方案：{software} × MC {mc}（Java {java}+，-Xmx{mem}MB，账号 {account}）",
            software = loaded.software,
            mc = loaded.mc_version,
            java = loaded.java_major,
            mem = loaded.jvm_memory_mb,
            account = loaded.account
        ));
        if let Some(java) = &loaded.java_path {
            lines.push(format!("Java 运行时：{java}"));
        }
        lines.push(format!(
            "mod（{count} 个）：{list}",
            count = loaded.mods.len(),
            list = if loaded.mods.is_empty() {
                "无".to_string()
            } else {
                loaded
                    .mods
                    .iter()
                    .map(|m| m.slug.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            }
        ));
        if !loaded.network.is_empty() {
            lines.push(format!("网络：{}", loaded.network));
        }
        if !loaded.world.is_empty() {
            lines.push(format!("世界：{}", loaded.world));
        }
        if !loaded.artifacts.is_empty() {
            lines.push(format!("产物：{} 项", loaded.artifacts.len()));
            for artifact in &loaded.artifacts {
                lines.push(format!("  - [{}] {}", artifact.kind, artifact.path));
            }
        }
        if !loaded.notes.is_empty() {
            lines.push(format!("备注：{}", loaded.notes));
        }
        lines.push("完整档案 JSON：".to_string());
        match serde_json::to_string_pretty(&loaded) {
            Ok(text) => lines.push(text),
            Err(err) => return Ok(ToolOutcome::err(format!("序列化档案失败：{err}"))),
        }
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use crate::tools::general::tests::QuietInteraction;

    fn test_ctx(workspace: &std::path::Path) -> ToolCtx {
        let (tx, _rx) = crate::events::event_channel();
        ToolCtx {
            workspace: workspace.to_path_buf(),
            data_dir: workspace.join(".data"),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
            curseforge_key: String::new(),
        }
    }

    fn save_args() -> serde_json::Value {
        serde_json::json!({
            "account": "all_offline", "software": "fabric", "mc_version": "1.21.1",
            "java_major": 21, "jvm_memory_mb": 4096, "network": "lan",
            "mods": [{"slug": "jei", "version_id": "AbCd", "file_name": "jei.jar", "sha1": "aa"}],
            "artifacts": [{"kind": "jar", "path": "server/server.jar"}],
            "notes": "离线白名单已开启"
        })
    }

    #[tokio::test]
    async fn save_then_load_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path());
        let outcome = SaveProfileTool.run(save_args(), &ctx).await.unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("保存应成功：{outcome:?}");
        };
        assert!(content.contains("已保存部署档案"), "{content}");

        // latest 读取
        let outcome = LoadProfileTool
            .run(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("latest 读取应成功：{outcome:?}");
        };
        assert!(content.contains("mod（1 个）：jei"), "{content}");
        assert!(content.contains("完整档案 JSON"), "{content}");
    }

    #[tokio::test]
    async fn load_missing_is_structured_error() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path());
        let outcome = LoadProfileTool
            .run(serde_json::json!({"profile_id": "no-such"}), &ctx)
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "不存在的档案应结构化报错");
    }

    #[tokio::test]
    async fn save_rejects_unknown_account_and_warns_missing_dir() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_ctx(root.path());
        let mut args = save_args();
        args["account"] = serde_json::json!("half");
        let outcome = SaveProfileTool.run(args, &ctx).await.unwrap();
        assert!(!outcome.is_ok(), "未知账号类型应报错");

        // server_dir 不存在 → 提示但不阻断
        let outcome = SaveProfileTool
            .run(
                {
                    let mut a = save_args();
                    a["server_dir"] = serde_json::json!("not-yet-server");
                    a
                },
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("目录不存在应提示不阻断：{outcome:?}");
        };
        assert!(content.contains("当前不存在"), "{content}");
    }
}
