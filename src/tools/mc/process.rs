//! 服务端生命周期（FR-14，决议 D118 → D134，设计 §8.10）。
//!
//! `start_server`：在**独立终端窗口**运行交付的 start 脚本（与用户手动
//! 双击完全一致，D118"长期运行以交付脚本为准"由此统一）——服务器日志
//! 只在窗口中滚动，Agent 界面不再展示；就绪判定轮询 `logs/latest.log`
//!（`Done (` 特征 + mtime 晚于启动时刻快照，防旧日志误判就绪）。停服
//! 由用户在服务器窗口 Ctrl+C（或输入 stop），mcha 退出不影响服务器。
//! `server_status`：轻量只读（端口探测 + latest.log 尾部推断）。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;
use crate::events::Event;
use crate::tools::confinement::resolve_in;

use super::{Tool, ToolCtx, ToolError};

/// 就绪轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 进度播报间隔秒数（R4：超过 3 秒的任务须实时渲染进度）。
const PROGRESS_EVERY: u64 = 5;

/// 从 server.properties 读 server-port（缺失返回 None）。
pub(crate) fn parse_port(server_dir: &std::path::Path) -> Option<u16> {
    let text = std::fs::read_to_string(server_dir.join("server.properties")).ok()?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("server-port=") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// 读取 logs/latest.log 尾部 n 行（文件缺失返回空）。
fn read_log_tail(server_dir: &Path, count: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(server_dir.join("logs").join("latest.log")) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].iter().map(|line| line.to_string()).collect()
}

/// 轮询一次日志，返回 (本轮是否有新写入, 是否出现就绪特征)。
///
/// mtime 必须晚于启动前快照：latest.log 为上一轮运行遗留时，其中已有
/// `Done (`（实测教训——启动等待被旧日志瞬间"假就绪"），故先验证确为
/// 本轮新写入再查特征。
fn probe_log(log_path: &Path, stale_mtime: Option<std::time::SystemTime>) -> (bool, bool) {
    let Ok(meta) = std::fs::metadata(log_path) else {
        return (false, false);
    };
    let fresh = match meta.modified().ok() {
        Some(current) => match stale_mtime {
            Some(old) => current > old,
            None => true, // 此前不存在，本轮新建
        },
        None => false,
    };
    if !fresh {
        return (false, false);
    }
    let done = std::fs::read_to_string(log_path)
        .map(|text| text.contains("Done ("))
        .unwrap_or(false);
    (true, done)
}

/// 启动器：把交付的 start 脚本在新终端窗口拉起（参数：服务器目录、脚本路径）。
/// 抽象为闭包供测试注入（CI / 无桌面环境无法弹窗）。
type Launcher = Arc<dyn Fn(&Path, &Path) -> Result<(), String> + Send + Sync>;

/// 真实启动器：Windows 以 CREATE_NEW_CONSOLE 新开 cmd 窗口跑 start.bat；
/// Unix 依次探测常见终端模拟器跑 start.sh，无图形环境则明确报错。
fn real_launch(server_dir: &Path, script: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        let _ = script; // bat 固定从服务器目录启动，无需脚本路径
        std::process::Command::new("cmd")
            .args(["/c", "start.bat"])
            .current_dir(server_dir)
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map(|_| ())
            .map_err(|err| format!("新开窗口启动 start.bat 失败：{err}"))
    }
    #[cfg(unix)]
    {
        // (终端模拟器, 传参风格)；-e / -- / -x 语义均为"执行其后的命令"
        let candidates = [
            ("x-terminal-emulator", "-e"),
            ("gnome-terminal", "--"),
            ("konsole", "-e"),
            ("xfce4-terminal", "-x"),
            ("xterm", "-e"),
        ];
        for (term, flag) in candidates {
            if find_in_path(term).is_some() {
                return std::process::Command::new(term)
                    .args([flag])
                    .arg(script)
                    .current_dir(server_dir)
                    .spawn()
                    .map(|_| ())
                    .map_err(|err| format!("在 {term} 中启动服务器失败：{err}"));
            }
        }
        Err(
            "未找到可用的图形终端（探测过 x-terminal-emulator / gnome-terminal / konsole / xfce4-terminal / xterm）；请在桌面环境运行，或手动执行 start 脚本"
                .into(),
        )
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (server_dir, script);
        Err("当前平台不支持自动弹窗启动；请手动执行 start 脚本".into())
    }
}

/// 在 PATH 各目录中查找可执行文件。
#[cfg(unix)]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

// ---------------------------------------------------------------------------
// start_server
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartServerArgs {
    /// 服务器目录（工作区内，默认 server）
    #[serde(default)]
    pub server_dir: Option<String>,
    /// 就绪等待上限秒（默认 120，首次生成世界可能较慢）
    #[serde(default)]
    pub ready_timeout_secs: Option<u64>,
}

pub struct StartServerTool {
    launcher: Launcher,
}

/// 构造生命周期二件套。
pub(crate) fn lifecycle_tools() -> (StartServerTool, ServerStatusTool) {
    (
        StartServerTool {
            launcher: Arc::new(real_launch),
        },
        ServerStatusTool,
    )
}

impl StartServerTool {
    /// 确认门内容：让用户看清将在哪个目录、以什么方式启动。
    fn confirmation_lines(args: &serde_json::Value) -> Vec<String> {
        let server_dir = args
            .get("server_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("server");
        let timeout = args
            .get("ready_timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);
        vec![
            format!(
                "目录：<工作区>/{server_dir}；在新终端窗口运行交付的 start 脚本（与手动双击一致），服务器日志在窗口中滚动"
            ),
            format!(
                "启动后等待就绪特征（Done ( … )!），最多 {timeout} 秒；Esc / Ctrl-C 仅打断等待，不影响服务器窗口"
            ),
        ]
    }
}

#[async_trait::async_trait]
impl Tool for StartServerTool {
    fn name(&self) -> &'static str {
        "start_server"
    }
    fn description(&self) -> String {
        "在新终端窗口运行交付的 start 脚本启动 Minecraft 服务器（与手动双击一致；日志只在窗口滚动，Agent 界面不展示），轮询 logs/latest.log 等待就绪特征 Done (x.xxx)! 或超时报告。端口已被监听即拒绝（防重复开服）；停服由用户在服务器窗口操作。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(StartServerArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Execute
    }
    fn confirm_summary(&self, args: &serde_json::Value) -> Vec<String> {
        Self::confirmation_lines(args)
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: StartServerArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        let server_dir = resolve_in(
            &[ctx.workspace.as_path()],
            args.server_dir.as_deref().unwrap_or("server"),
        )?;
        // start 脚本是唯一启动路径（write_server_files 交付，java 参数已固化）
        let script_name = if cfg!(windows) {
            "start.bat"
        } else {
            "start.sh"
        };
        let script = server_dir.join(script_name);
        if !script.is_file() {
            return Ok(ToolOutcome::err(format!(
                "{} 不存在；先 write_server_files 生成启动脚本",
                script.display()
            )));
        }

        // 重复开服守卫：独立窗口模型下 mcha 不持进程句柄，以端口监听推断
        let port = parse_port(&server_dir);
        if let Some(port) = port {
            let addr = format!("127.0.0.1:{port}");
            let occupied = tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::TcpStream::connect(&addr),
            )
            .await;
            if matches!(occupied, Ok(Ok(_))) {
                return Ok(ToolOutcome::err(format!(
                    "端口 {addr} 已有服务监听，疑似服务器已在独立窗口运行；请先在该窗口 Ctrl+C（或输入 stop）停服后再启动"
                )));
            }
        }

        // 旧日志快照：就绪判定要求 latest.log 的 mtime 晚于此快照
        let log_path = server_dir.join("logs").join("latest.log");
        let stale_mtime = std::fs::metadata(&log_path)
            .ok()
            .and_then(|meta| meta.modified().ok());
        let t0 = std::time::Instant::now();

        (self.launcher)(&server_dir, &script).map_err(ToolError::Io)?;

        // 就绪轮询：mtime 更新 + Done ( 特征；每 5 秒报一次进度（R4）
        let timeout = Duration::from_secs(args.ready_timeout_secs.unwrap_or(120).clamp(1, 600));
        let deadline = tokio::time::Instant::now() + timeout;
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next_progress = PROGRESS_EVERY;
        let ready = loop {
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => {
                    // 只打断等待：服务器窗口不受影响，就绪与否以窗口为准
                    let _ = ctx.events.send(Event::OutputLine(
                        "│ 等待已打断；服务器窗口仍在启动，是否就绪以窗口为准".into(),
                    ));
                    return Err(ToolError::Cancelled);
                }
                _ = tokio::time::sleep_until(deadline) => break None,
                _ = tick.tick() => {
                    let (_, done) = probe_log(&log_path, stale_mtime);
                    if done {
                        break Some(());
                    }
                    let elapsed = t0.elapsed().as_secs();
                    if elapsed >= next_progress {
                        let _ = ctx.events.send(Event::OutputLine(format!(
                            "│ 已等待 {elapsed}s：服务器日志在独立窗口滚动，此处静默等待"
                        )));
                        next_progress += PROGRESS_EVERY;
                    }
                }
            }
        };

        match ready {
            Some(()) => {
                let elapsed = t0.elapsed().as_secs_f32();
                let mut lines = vec![format!(
                    "服务器已在独立窗口启动并就绪（{elapsed:.1}s），日志正在该窗口中滚动。"
                )];
                if let Some(port) = port {
                    lines.push(format!(
                        "监听 127.0.0.1:{port}；可用 mc_ping 验证，或直接进服游玩。"
                    ));
                }
                lines.push(
                    "停服方法：切到服务器窗口按 Ctrl+C（或在窗口输入 stop 后回车），世界会自动保存；mcha 无需也无法远程停服。"
                        .to_string(),
                );
                Ok(ToolOutcome::ok(lines.join("\n")))
            }
            None => {
                let (fresh, _) = probe_log(&log_path, stale_mtime);
                let mut message = if fresh {
                    format!(
                        "未在 {} 秒内就绪；服务器窗口可能仍在启动（首次生成世界较慢）或已报错退出，请查看窗口内容。",
                        timeout.as_secs()
                    )
                } else {
                    format!(
                        "未在 {} 秒内就绪，且日志毫无新写入——服务器窗口可能启动即失败（如 start 脚本中的 Java 路径失效），请查看窗口回显。",
                        timeout.as_secs()
                    )
                };
                if fresh {
                    let tail = read_log_tail(&server_dir, 10);
                    if !tail.is_empty() {
                        message.push_str(&format!("\nlogs/latest.log 尾部：\n{}", tail.join("\n")));
                    }
                }
                Ok(ToolOutcome::err(message))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// server_status
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServerStatusArgs {
    /// 服务器目录（工作区内，默认 server）
    #[serde(default)]
    pub server_dir: Option<String>,
}

pub struct ServerStatusTool;

#[async_trait::async_trait]
impl Tool for ServerStatusTool {
    fn name(&self) -> &'static str {
        "server_status"
    }
    fn description(&self) -> String {
        "查看服务器运行状态：探测端口是否有服务监听 + 读取 logs/latest.log 尾部。独立窗口模型下 mcha 不托管进程，本工具为轻量只读推断。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ServerStatusArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: ServerStatusArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        let server_dir = resolve_in(
            &[ctx.workspace.as_path()],
            args.server_dir.as_deref().unwrap_or("server"),
        )?;

        let mut lines = Vec::new();
        match parse_port(&server_dir) {
            Some(port) => {
                let addr = format!("127.0.0.1:{port}");
                let probe = tokio::time::timeout(
                    Duration::from_secs(1),
                    tokio::net::TcpStream::connect(&addr),
                )
                .await;
                match probe {
                    Ok(Ok(_)) => lines.push(format!(
                        "端口 {addr} 有服务监听（疑似服务器运行中，独立窗口内）"
                    )),
                    Ok(Err(_)) | Err(_) => lines.push(format!(
                        "端口 {addr} 无监听（服务器未运行，或仍在启动早期）"
                    )),
                }
            }
            None => lines.push("server.properties 缺失或未配置 server-port，无法探测端口".into()),
        }
        let tail = read_log_tail(&server_dir, 10);
        if tail.is_empty() {
            lines.push("（暂无 logs/latest.log 日志）".into());
        } else {
            lines.push(format!("logs/latest.log 尾部：\n{}", tail.join("\n")));
        }
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            curseforge_key: String::new(),
            natfrp_token: String::new(),
        };
        (ctx, root)
    }

    /// 无头启动器：CI 无图形终端，直接以 sh 后台执行 start.sh（写 latest.log）。
    #[cfg(unix)]
    fn headless_tools() -> (StartServerTool, ServerStatusTool) {
        let launcher: Launcher = Arc::new(|dir, script| {
            std::process::Command::new("sh")
                .arg(script)
                .current_dir(dir)
                .spawn()
                .map(|_| ())
                .map_err(|err| err.to_string())
        });
        (StartServerTool { launcher }, ServerStatusTool)
    }

    /// 写 server.properties 与 start.sh 假脚本（往 logs/latest.log 写启动日志）。
    #[cfg(unix)]
    fn install_fixture(dir: &std::path::Path, port: u16, script_body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("server.properties"),
            format!("server-port={port}\n"),
        )
        .unwrap();
        std::fs::write(dir.join("start.sh"), script_body).unwrap();
    }

    #[cfg(unix)]
    const READY_SCRIPT: &str = "#!/bin/sh\n\
        mkdir -p logs\n\
        {\n\
        echo \"[00:00:00] [main/INFO]: Starting minecraft server version 1.21.1\"\n\
        echo \"[00:00:01] [Server thread/INFO]: Done (0.100s)! For help, type \\\"help\\\"\"\n\
        } > logs/latest.log\n\
        sleep 30\n";

    #[cfg(unix)]
    const CRASH_SCRIPT: &str = "#!/bin/sh\n\
        mkdir -p logs\n\
        echo \"[00:00:00] [main/ERROR]: Failed to start the minecraft server\" > logs/latest.log\n\
        exit 1\n";

    #[cfg(unix)]
    const SILENT_SCRIPT: &str = "#!/bin/sh\nexit 1\n";

    #[tokio::test]
    #[cfg(unix)]
    async fn start_pops_window_and_reports_ready() {
        let (ctx, _root) = test_ctx();
        let (start, _status) = headless_tools();
        let server_dir = ctx.workspace.join("server");
        install_fixture(&server_dir, 25991, READY_SCRIPT);

        let started = start
            .run(
                serde_json::json!({ "server_dir": "server", "ready_timeout_secs": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = started else {
            panic!("应就绪：{started:?}");
        };
        assert!(content.contains("独立窗口"), "{content}");
        assert!(content.contains("127.0.0.1:25991"), "{content}");
        // 用户要求：成功回复中必须说明停服方法
        assert!(
            content.contains("停服方法") && content.contains("Ctrl+C"),
            "{content}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn crash_before_ready_reports_tail_on_timeout() {
        let (ctx, _root) = test_ctx();
        let (start, _status) = headless_tools();
        let server_dir = ctx.workspace.join("server");
        install_fixture(&server_dir, 25992, CRASH_SCRIPT);

        let outcome = start
            .run(
                serde_json::json!({ "server_dir": "server", "ready_timeout_secs": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Err { error } = outcome else {
            panic!("崩溃应返回结构化错误：{outcome:?}");
        };
        assert!(error.contains("未在 1 秒内就绪"), "{error}");
        assert!(error.contains("Failed to start"), "{error}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn silent_failure_reports_no_log_activity() {
        let (ctx, _root) = test_ctx();
        let (start, _status) = headless_tools();
        let server_dir = ctx.workspace.join("server");
        install_fixture(&server_dir, 25993, SILENT_SCRIPT);

        let outcome = start
            .run(
                serde_json::json!({ "server_dir": "server", "ready_timeout_secs": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Err { error } = outcome else {
            panic!("无日志应返回结构化错误：{outcome:?}");
        };
        assert!(error.contains("日志毫无新写入"), "{error}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn occupied_port_rejects_duplicate_start() {
        let (ctx, _root) = test_ctx();
        let (start, _status) = headless_tools();
        let server_dir = ctx.workspace.join("server");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        install_fixture(&server_dir, port, READY_SCRIPT);

        let outcome = start
            .run(
                serde_json::json!({ "server_dir": "server", "ready_timeout_secs": 1 }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Err { error } = outcome else {
            panic!("端口被占应拒绝：{outcome:?}");
        };
        assert!(
            error.contains("已在运行") || error.contains("已有服务监听"),
            "{error}"
        );
        drop(listener);
    }

    #[tokio::test]
    async fn start_requires_start_script() {
        let (ctx, _root) = test_ctx();
        let (start, _status) = lifecycle_tools();
        let outcome = start
            .run(serde_json::json!({ "server_dir": "server" }), &ctx)
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "缺 start 脚本应拒绝：{outcome:?}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn status_reports_listening_port_and_log_tail() {
        let (ctx, _root) = test_ctx();
        let (_start, status) = headless_tools();
        let server_dir = ctx.workspace.join("server");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        install_fixture(&server_dir, port, SILENT_SCRIPT);
        std::fs::create_dir_all(server_dir.join("logs")).unwrap();
        std::fs::write(
            server_dir.join("logs").join("latest.log"),
            "[00:00:00] [Server thread/INFO]: Done (1.0s)! For help\n",
        )
        .unwrap();

        let outcome = status
            .run(serde_json::json!({ "server_dir": "server" }), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("状态查询应成功：{outcome:?}");
        };
        assert!(content.contains("有服务监听"), "{content}");
        assert!(content.contains("Done (1.0s)"), "{content}");
        drop(listener);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn status_reports_silent_port() {
        let (ctx, _root) = test_ctx();
        let (_start, status) = headless_tools();
        let server_dir = ctx.workspace.join("server");
        // 占住再释放一个随机端口，确保其当前无监听
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        tokio::time::sleep(Duration::from_millis(100)).await;
        install_fixture(&server_dir, port, SILENT_SCRIPT);

        let outcome = status
            .run(serde_json::json!({ "server_dir": "server" }), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("状态查询应成功：{outcome:?}");
        };
        assert!(content.contains("无监听"), "{content}");
    }

    #[test]
    fn parse_port_reads_properties() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("server.properties"),
            "motd=x\nserver-port=30000\n",
        )
        .unwrap();
        assert_eq!(parse_port(dir.path()), Some(30000));
        std::fs::write(dir.path().join("server.properties"), "motd=x\n").unwrap();
        assert_eq!(parse_port(dir.path()), None);
    }

    #[test]
    fn probe_log_ignores_stale_done_marker() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("latest.log");
        // 旧日志：上一轮遗留，已含 Done ( 特征
        std::fs::write(&log_path, "[old] Done (9.9s)!\n").unwrap();
        // 快照取"未来"时间不可行，这里反向验证：无快照（新建）才可信，
        // 有快照时 mtime 未更新则一律视为未刷新
        let old_stamp = std::fs::metadata(&log_path).unwrap().modified().unwrap();
        assert_eq!(probe_log(&log_path, Some(old_stamp)), (false, false));
        // 无快照（文件为本轮新建）→ 可信，且含就绪特征
        assert_eq!(probe_log(&log_path, None), (true, true));
        // 文件不存在
        assert_eq!(
            probe_log(&dir.path().join("nope.log"), None),
            (false, false)
        );
    }

    #[test]
    fn confirmation_lines_describe_window_start() {
        // M2.1 实测回归：确认弹窗不得再出现空白内容
        let lines = StartServerTool::confirmation_lines(&serde_json::json!({
            "server_dir": "server"
        }));
        assert!(lines.iter().all(|l| !l.trim().is_empty()), "{lines:?}");
        let joined = lines.join("\n");
        assert!(joined.contains("start 脚本"), "{joined}");
        assert!(joined.contains("120 秒"), "{joined}");
        assert!(joined.contains("不影响服务器窗口"), "{joined}");
    }
}
