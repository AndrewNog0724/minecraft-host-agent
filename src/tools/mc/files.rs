//! write_server_files：服务器配置生成（FR-11，设计 §8.10）。
//!
//! 产物四件：`eula.txt`（eula_accepted=false 拒绝生成——EULA 必须经
//! ask_user 确认）、`server.properties`（内置默认键表 + 参数覆盖）、
//! `whitelist.json`（离线 UUID：nameUUIDFromBytes v3 口径）、
//! `start.bat` + `start.sh` 双脚本（D118：长期运行以交付脚本为准）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::agent::message::ToolOutcome;
use crate::tools::confinement::resolve_in;

use super::{Tool, ToolCtx, ToolError};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct WhitelistArgs {
    /// 是否启用白名单（离线模式强烈建议开启）
    pub enabled: bool,
    /// 白名单玩家名（正版 1–16 字符：字母数字下划线）
    #[serde(default)]
    pub names: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct WriteServerFilesArgs {
    /// 服务器目录（工作区内，默认 server）
    #[serde(default)]
    pub server_dir: Option<String>,
    /// 服务端软件 id（记录在脚本头注释中）
    pub software: String,
    /// MC 版本（记录在脚本头注释中）
    pub mc_version: String,
    /// 用户已通过 ask_user 确认接受 Minecraft EULA（必须为 true）
    pub eula_accepted: bool,
    /// 正版验证开关（账号类型推导，勿凭空猜测）
    pub online_mode: bool,
    /// 服务端口（默认 25565）
    #[serde(default)]
    pub port: Option<u16>,
    /// 服务器列表标语（MOTD）
    #[serde(default)]
    pub motd: Option<String>,
    /// 最大玩家数（默认 20）
    #[serde(default)]
    pub max_players: Option<u32>,
    /// 白名单配置
    pub whitelist: WhitelistArgs,
    /// JVM 最大内存（MB，-Xmx）
    pub jvm_memory_mb: u32,
    /// java 可执行文件绝对路径（来自 check_java / ensure_java）
    pub java_path: String,
}

/// 离线 UUID：`nameUUIDFromBytes(("OfflinePlayer:"+name) UTF_8)` 口径——
/// MD5 摘要后置 RFC 4122 version=3、variant=IETF。
pub(crate) fn offline_uuid(name: &str) -> String {
    use md5::Digest as _;
    let mut digest = md5::Md5::digest(format!("OfflinePlayer:{name}").as_bytes()).to_vec();
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 校验玩家名（正版口径：1–16 个字母 / 数字 / 下划线）。
fn valid_player_name(name: &str) -> bool {
    (1..=16usize).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// server.properties 键值表（参数覆盖默认值；输出按 MC 惯例排序）。
pub(crate) fn server_properties(
    args: &WriteServerFilesArgs,
    port: u16,
    motd: &str,
    max_players: u32,
) -> Vec<(String, String)> {
    let offline = !args.online_mode;
    let whitelist_on = args.whitelist.enabled;
    let mut props: Vec<(String, String)> = vec![
        ("accepts-transfers".into(), "false".into()),
        ("allow-flight".into(), "false".into()),
        ("allow-nether".into(), "true".into()),
        ("broadcast-console-to-ops".into(), "true".into()),
        ("broadcast-rcon-to-ops".into(), "true".into()),
        ("difficulty".into(), "normal".into()),
        ("enable-command-block".into(), "false".into()),
        ("enable-jmx-monitoring".into(), "false".into()),
        ("enable-query".into(), "false".into()),
        ("enable-rcon".into(), "false".into()),
        ("enable-status".into(), "true".into()),
        // 离线模式没有 Mojang 签名档案，必须关闭，否则玩家无法正常进服
        ("enforce-secure-profile".into(), (!offline).to_string()),
        ("enforce-whitelist".into(), whitelist_on.to_string()),
        ("entity-broadcast-range-percentage".into(), "100".into()),
        ("force-gamemode".into(), "false".into()),
        ("function-permission-level".into(), "2".into()),
        ("gamemode".into(), "survival".into()),
        ("generate-structures".into(), "true".into()),
        ("hardcore".into(), "false".into()),
        ("level-name".into(), "world".into()),
        ("level-seed".into(), String::new()),
        ("level-type".into(), "minecraft\\:normal".into()),
        ("max-chained-neighbor-updates".into(), "1000000".into()),
        ("max-players".into(), max_players.to_string()),
        ("max-tick-time".into(), "60000".into()),
        ("max-world-size".into(), "29999984".into()),
        ("motd".into(), motd.to_string()),
        ("network-compression-threshold".into(), "256".into()),
        ("online-mode".into(), (!offline).to_string()),
        ("op-permission-level".into(), "4".into()),
        ("pause-when-empty-seconds".into(), "60".into()),
        ("player-idle-timeout".into(), "0".into()),
        ("prevent-proxy-connections".into(), "false".into()),
        ("pvp".into(), "true".into()),
        ("query.port".into(), port.to_string()),
        ("rate-limit".into(), "0".into()),
        ("rcon.password".into(), String::new()),
        ("rcon.port".into(), "25575".into()),
        ("require-resource-pack".into(), "false".into()),
        ("resource-pack".into(), String::new()),
        ("server-ip".into(), String::new()),
        ("server-port".into(), port.to_string()),
        ("simulation-distance".into(), "10".into()),
        ("spawn-monsters".into(), "true".into()),
        ("spawn-protection".into(), "16".into()),
        ("sync-chunk-writes".into(), "true".into()),
        ("view-distance".into(), "10".into()),
        ("white-list".into(), whitelist_on.to_string()),
    ];
    props.sort_by(|a, b| a.0.cmp(&b.0));
    props
}

/// 渲染 server.properties 全文。
pub(crate) fn render_properties(props: &[(String, String)]) -> String {
    let mut out = String::from("#Minecraft server properties（由 mcha 生成）\n");
    for (key, value) in props {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

/// -Xms：取 -Xmx 的一半，下限 512MB（不含超大值时的常规做法）。
pub(crate) fn xms_for(xmx_mb: u32) -> u32 {
    (xmx_mb / 2).max(512).min(xmx_mb)
}

pub struct WriteServerFilesTool;

#[async_trait::async_trait]
impl Tool for WriteServerFilesTool {
    fn name(&self) -> &'static str {
        "write_server_files"
    }
    fn description(&self) -> String {
        "生成服务器配置文件到服务器目录：eula.txt（必须先经 ask_user 征得用户同意接受 EULA）、server.properties、whitelist.json（自动计算离线 UUID）、start.bat 与 start.sh 启动脚本。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WriteServerFilesArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Write
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: WriteServerFilesArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        // EULA：未经用户确认拒绝生成（定制 1 硬规则）
        if !args.eula_accepted {
            return Ok(ToolOutcome::err(
                "eula_accepted=false：必须先经 ask_user 向用户确认接受 Minecraft EULA（https://aka.ms/MinecraftEULA）后才能写 eula.txt",
            ));
        }
        if args.jvm_memory_mb < 512 {
            return Ok(ToolOutcome::err(format!(
                "jvm_memory_mb={} 过小（下限 512）；请按 sys_info 的内存数据重新推荐",
                args.jvm_memory_mb
            )));
        }
        for name in &args.whitelist.names {
            if !valid_player_name(name) {
                return Ok(ToolOutcome::err(format!(
                    "玩家名「{name}」不合法（1–16 个字母 / 数字 / 下划线）"
                )));
            }
        }

        let server_dir = resolve_in(
            &[ctx.workspace.as_path()],
            args.server_dir.as_deref().unwrap_or("server"),
        )?;
        std::fs::create_dir_all(&server_dir)
            .map_err(|err| ToolError::Io(format!("创建服务器目录失败：{err}")))?;
        let port = args.port.unwrap_or(25565);
        let motd = args
            .motd
            .clone()
            .unwrap_or_else(|| "A Minecraft Server".to_string());
        let max_players = args.max_players.unwrap_or(20);
        let mut written: Vec<PathBuf> = Vec::new();
        let put =
            |name: &str, content: String, dir: &std::path::Path| -> Result<PathBuf, ToolError> {
                let path = dir.join(name);
                std::fs::write(&path, content)
                    .map_err(|err| ToolError::Io(format!("写入 {name} 失败：{err}")))?;
                Ok(path)
            };

        // eula.txt
        written.push(put(
            "eula.txt",
            "# 通过 eula_accepted=true 写入（mcha 已向用户确认）\neula=true\n".to_string(),
            &server_dir,
        )?);

        // server.properties
        let props = server_properties(&args, port, &motd, max_players);
        written.push(put(
            "server.properties",
            render_properties(&props),
            &server_dir,
        )?);

        // whitelist.json（离线 UUID）
        if args.whitelist.enabled {
            let entries: Vec<String> = args
                .whitelist
                .names
                .iter()
                .map(|name| {
                    format!(
                        "{{\"uuid\":\"{}\",\"name\":\"{}\"}}",
                        offline_uuid(name),
                        name
                    )
                })
                .collect();
            let json = format!("[{}\n]\n", entries.join(",\n"));
            written.push(put("whitelist.json", json, &server_dir)?);
        }

        // start.bat / start.sh 双脚本（java 绝对路径 + -Xmx + nogui）
        let xms = xms_for(args.jvm_memory_mb);
        let java_path = args.java_path.replace('\\', "\\\\");
        let bat = format!(
            "@echo off\r\ntitle MCHA Minecraft Server（{} {}）\r\ncd /d \"%~dp0\"\r\n\"{java_path}\" -Xmx{}M -Xms{xms}M -jar server.jar nogui\r\npause\r\n",
            args.software, args.mc_version, args.jvm_memory_mb
        );
        written.push(put("start.bat", bat, &server_dir)?);
        let sh = format!(
            "#!/bin/sh\n# MCHA Minecraft Server（{} {}）\ncd \"$(dirname \"$0\")\"\nexec \"{java_path}\" -Xmx{}M -Xms{xms}M -jar server.jar nogui\n",
            args.software, args.mc_version, args.jvm_memory_mb
        );
        written.push(put("start.sh", sh, &server_dir)?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                server_dir.join("start.sh"),
                std::fs::Permissions::from_mode(0o755),
            );
        }

        let listed: Vec<String> = written.iter().map(|p| p.display().to_string()).collect();
        let mut lines = vec![format!("已生成 {} 个文件：", written.len())];
        lines.extend(listed.iter().map(|p| format!("- {p}")));
        lines.push(format!(
            "要点：online-mode={}；端口 {port}；白名单 {}（{} 人）；-Xmx{}M",
            args.online_mode,
            args.whitelist.enabled,
            args.whitelist.names.len(),
            args.jvm_memory_mb
        ));
        if !args.online_mode {
            lines.push(
                "离线模式风险提示：任何人可伪装任意用户名进服，务必保持白名单开启。".to_string(),
            );
        }
        lines.push("下一步：start_server 启动并等待就绪（Done (x.xxx)!）。".to_string());
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_uuid_matches_java_name_uuid_from_bytes() {
        // 黄金值：Python 按同算法（MD5 → v3/IETF 位调整）预计算
        assert_eq!(
            offline_uuid("Notch"),
            "b50ad385-829d-3141-a216-7e7d7539ba7f"
        );
        assert_eq!(
            offline_uuid("Dream"),
            "e6135a83-d680-39d6-b4be-65a3d8bb97ad"
        );
        // 非 ASCII 用户名（UTF-8 字节口径）
        assert_eq!(
            offline_uuid("玩家甲"),
            "a623eaa5-5212-3578-914b-09dabed6ab8d"
        );
    }

    fn sample_args() -> WriteServerFilesArgs {
        serde_json::from_value(serde_json::json!({
            "software": "paper", "mc_version": "1.21.1",
            "eula_accepted": true, "online_mode": false,
            "whitelist": { "enabled": true, "names": ["Notch"] },
            "jvm_memory_mb": 4096, "java_path": "C:\\jre\\bin\\java.exe"
        }))
        .unwrap()
    }

    #[test]
    fn properties_reflect_offline_and_whitelist() {
        let args = sample_args();
        let props = server_properties(&args, 25565, "测试服", 10);
        let text = render_properties(&props);
        assert!(text.contains("online-mode=false"));
        assert!(text.contains("white-list=true"));
        assert!(text.contains("enforce-whitelist=true"));
        assert!(text.contains("enforce-secure-profile=false"));
        assert!(text.contains("server-port=25565"));
        assert!(text.contains("motd=测试服"));
        assert!(text.contains("max-players=10"));
        // 排序稳定（快照锚点）
        assert!(text.starts_with(
            "#Minecraft server properties（由 mcha 生成）\naccepts-transfers=false\n"
        ));
    }

    #[test]
    fn properties_online_mode_true() {
        let mut args = sample_args();
        args.online_mode = true;
        args.whitelist.enabled = false;
        let props = server_properties(&args, 25565, "motd", 20);
        let text = render_properties(&props);
        assert!(text.contains("online-mode=true"));
        assert!(text.contains("white-list=false"));
        assert!(text.contains("enforce-secure-profile=true"));
    }

    #[test]
    fn xms_is_half_of_xmx_with_floor() {
        assert_eq!(xms_for(4096), 2048);
        assert_eq!(xms_for(512), 512);
        assert_eq!(xms_for(1024), 512);
    }

    #[tokio::test]
    async fn rejects_without_eula_and_bad_names() {
        let mut args = sample_args();
        args.eula_accepted = false;
        let outcome = WriteServerFilesTool
            .run(serde_json::to_value(&args).unwrap(), &test_ctx().0)
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "未确认 EULA 应拒绝");

        let mut args = sample_args();
        args.whitelist.names = vec!["不合法名字!".to_string()];
        let outcome = WriteServerFilesTool
            .run(serde_json::to_value(&args).unwrap(), &test_ctx().0)
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "非法玩家名应拒绝");
    }

    fn test_ctx() -> (ToolCtx, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let (tx, _rx) = crate::events::event_channel();
        let ctx = ToolCtx {
            workspace: root.path().join("workspace"),
            data_dir: root.path().join("data"),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
        };
        (ctx, root)
    }

    #[tokio::test]
    async fn writes_all_four_files() {
        let (ctx, _root) = test_ctx();
        let args = sample_args();
        let outcome = WriteServerFilesTool
            .run(serde_json::to_value(&args).unwrap(), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("应成功：{outcome:?}");
        };
        let server_dir = ctx.workspace.join("server");
        assert!(server_dir.join("eula.txt").is_file());
        assert!(server_dir.join("server.properties").is_file());
        assert!(server_dir.join("whitelist.json").is_file());
        assert!(server_dir.join("start.bat").is_file());
        assert!(server_dir.join("start.sh").is_file());
        let whitelist = std::fs::read_to_string(server_dir.join("whitelist.json")).unwrap();
        assert!(whitelist.contains("b50ad385-829d-3141-a216-7e7d7539ba7f"));
        let bat = std::fs::read_to_string(server_dir.join("start.bat")).unwrap();
        assert!(bat.contains("-Xmx4096M") && bat.contains("server.jar nogui"));
        assert!(content.contains("离线模式风险提示"));
    }
}
