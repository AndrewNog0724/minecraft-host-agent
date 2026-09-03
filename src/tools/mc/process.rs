//! 服务端进程生命周期（FR-14，决议 D118，设计 §8.10）。
//!
//! 三件套共享同一托管槽（同刻仅一台）：`start_server`（spawn java 直启、
//! 日志行流、就绪特征 `Done (`、Drop 守卫 `kill_on_drop`）、`stop_server`
//!（stdin `stop` 优雅停 → 超时树杀）、`server_status`（状态 + 日志尾部）。
//! mcha 退出即停托管进程（防孤儿）；长期运行以交付的 start 脚本为准。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

use crate::agent::message::ToolOutcome;
use crate::events::Event;
use crate::tools::confinement::resolve_in;

use super::{Tool, ToolCtx, ToolError};

/// 日志环形缓冲容量。
const LOG_CAPACITY: usize = 200;
/// stop 优雅停机的等待上限。
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// 托管中的服务器实例。
struct ManagedServer {
    child: tokio::process::Child,
    /// stdin（stop 命令通道；发送后 take 关闭以让服务端读到 EOF）。
    stdin: Option<tokio::process::ChildStdin>,
    pid: u32,
    server_dir: PathBuf,
    port: Option<u16>,
    started_at: std::time::Instant,
    log: Arc<AsyncMutex<VecDeque<String>>>,
}

/// 共享托管槽。
type Slot = Arc<AsyncMutex<Option<ManagedServer>>>;

/// 构造共享同一托管槽的三件套。
pub(crate) fn lifecycle_tools() -> (StartServerTool, StopServerTool, ServerStatusTool) {
    let slot: Slot = Arc::new(AsyncMutex::new(None));
    (
        StartServerTool { slot: slot.clone() },
        StopServerTool { slot: slot.clone() },
        ServerStatusTool { slot },
    )
}

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

fn tail_lines(log: &AsyncMutex<VecDeque<String>>, count: usize) -> Vec<String> {
    // 仅在持锁极短场景调用；这里直接阻塞取快照（内容为行字符串，无 await）
    match log.try_lock() {
        Ok(queue) => queue.iter().rev().take(count).rev().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

/// 读取一条输出流（stdout 或 stderr）逐行入日志缓冲并发送事件。
///
/// `emit` 为 false 时停止向事件流发送并**放弃发送端**——托管服务器的日志
/// 读取任务长期存活，若始终持有发送端，回合结束后渲染器将永不退出，
/// REPL 卡死在回合收尾无法回到提示符（用户实测教训）。此后日志继续进
/// 缓冲（server_status 可查），不再滚动刷屏。
fn spawn_reader(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    mut line_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    log: Arc<AsyncMutex<VecDeque<String>>>,
    mut events: Option<crate::events::EventTx>,
    emit: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stream).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            {
                let mut queue = log.lock().await;
                if queue.len() >= LOG_CAPACITY {
                    queue.pop_front();
                }
                queue.push_back(line.clone());
            }
            if emit.load(Ordering::SeqCst) {
                if let Some(tx) = line_tx.as_ref() {
                    let _ = tx.send(line.clone());
                }
                if let Some(events) = events.as_ref() {
                    let _ = events.send(Event::OutputLine(format!("│ {line}")));
                }
            } else {
                // 停止滚动：放弃发送端（渲染器可在回合结束后正常收尾）
                line_tx = None;
                events = None;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// start_server
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartServerArgs {
    /// java 可执行文件绝对路径（来自 check_java / ensure_java）
    pub java_path: String,
    /// 服务器目录（工作区内，默认 server）
    #[serde(default)]
    pub server_dir: Option<String>,
    /// JVM 最大内存 MB（-Xmx；与 write_server_files 时保持一致）
    #[serde(default)]
    pub jvm_memory_mb: Option<u32>,
    /// 就绪等待上限秒（默认 120，首次生成世界可能较慢）
    #[serde(default)]
    pub ready_timeout_secs: Option<u64>,
}

pub struct StartServerTool {
    slot: Slot,
}

impl StartServerTool {
    /// 确认门内容：让用户看清将以什么命令、在哪个目录启动什么。
    fn confirmation_lines(args: &serde_json::Value) -> Vec<String> {
        let java = args
            .get("java_path")
            .and_then(|v| v.as_str())
            .unwrap_or("java");
        let server_dir = args
            .get("server_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("server");
        let xmx = args
            .get("jvm_memory_mb")
            .and_then(|v| v.as_u64())
            .map(|m| format!(" -Xmx{m}M"))
            .unwrap_or_default();
        let timeout = args
            .get("ready_timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(120);
        vec![
            format!("目录：<工作区>/{server_dir}；命令：{java}{xmx} -jar server.jar nogui"),
            format!(
                "启动后将等待就绪特征（Done ( … )!），最多 {timeout} 秒；Esc / Ctrl-C 可打断等待"
            ),
        ]
    }
}

impl StartServerTool {
    /// 就绪等待循环：读到 `Done (` 即就绪；读者流关闭 = 进程退出（崩溃）。
    async fn await_ready(
        ctx: &ToolCtx,
        log: &AsyncMutex<VecDeque<String>>,
        mut lines: tokio::sync::mpsc::UnboundedReceiver<String>,
        child: &mut tokio::process::Child,
        timeout: Duration,
    ) -> Result<bool, ToolError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(false); // 未就绪但进程仍在运行
                }
                line = lines.recv() => match line {
                    Some(l) if l.contains("Done (") => {
                        let _ = l;
                        return Ok(true);
                    }
                    Some(_) => {}
                    None => {
                        // stdout/stderr 读者全部结束：进程已退出（崩溃）
                        let status = child.wait().await;
                        let tail = tail_lines(log, 15).join("\n");
                        let code = match status {
                            Ok(s) => s.code().map(|c| c.to_string()).unwrap_or_else(|| "信号终止".into()),
                            Err(err) => format!("等待进程退出失败：{err}"),
                        };
                        return Err(ToolError::Io(format!(
                            "服务器进程在就绪前退出（退出码 {code}）。日志尾部：\n{tail}"
                        )));
                    }
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Tool for StartServerTool {
    fn name(&self) -> &'static str {
        "start_server"
    }
    fn description(&self) -> String {
        "启动托管中的 Minecraft 服务器（直接以 java 启动 server.jar nogui）：流式输出日志，等待就绪特征 Done (x.xxx)! 或超时/崩溃报告。同一时刻仅支持一台；长期运行请使用交付的 start 脚本。".into()
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

        let mut slot = self.slot.lock().await;
        if slot.is_some() {
            return Ok(ToolOutcome::err(
                "已有托管中的服务器进程；先 stop_server 或用 server_status 查看",
            ));
        }
        let server_dir = resolve_in(
            &[ctx.workspace.as_path()],
            args.server_dir.as_deref().unwrap_or("server"),
        )?;
        let jar = server_dir.join("server.jar");
        if !jar.is_file() {
            return Ok(ToolOutcome::err(format!(
                "{} 不存在；先 fetch_server_jar 下载服务端",
                jar.display()
            )));
        }
        let port = parse_port(&server_dir);

        // 组装命令：java [-Xmx] -jar server.jar nogui（不经 .bat，规避编码/弹窗）
        let mut command = tokio::process::Command::new(&args.java_path);
        if let Some(xmx) = args.jvm_memory_mb {
            command.arg(format!("-Xmx{xmx}M"));
        }
        command
            .args(["-jar", "server.jar", "nogui"])
            .current_dir(&server_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true); // Drop 守卫（D118）：mcha 退出/取消时进程不残留
        #[cfg(unix)]
        command.process_group(0);

        // spawn：ETXTBSY（内核对刚写入文件立即执行有短暂拒绝）重试 3 次
        let mut child = None;
        for _ in 0..3 {
            match command.spawn() {
                Ok(c) => {
                    child = Some(c);
                    break;
                }
                Err(err) if err.raw_os_error() == Some(26) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(err) => {
                    return Err(ToolError::Io(format!("启动 java 失败：{err}")));
                }
            }
        }
        let Some(mut child) = child else {
            return Err(ToolError::Io(
                "启动 java 失败：文件被占用（ETXTBSY，已重试 3 次）".into(),
            ));
        };
        let pid = child.id().unwrap_or(0);
        let stdout = child.stdout.take().expect("stdout 已 piped");
        let stderr = child.stderr.take().expect("stderr 已 piped");
        let stdin = child.stdin.take().expect("stdin 已 piped");

        let log: Arc<AsyncMutex<VecDeque<String>>> = Arc::new(AsyncMutex::new(VecDeque::new()));
        // 日志滚动闸：就绪后停止向事件流发送（读取任务放弃发送端），日志
        // 继续进缓冲供 server_status 查询；交付语后不再有日志刷屏
        let emit = Arc::new(AtomicBool::new(true));
        let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        spawn_reader(
            stdout,
            Some(line_tx.clone()),
            log.clone(),
            Some(ctx.events.clone()),
            emit.clone(),
        );
        spawn_reader(
            stderr,
            Some(line_tx),
            log.clone(),
            Some(ctx.events.clone()),
            emit.clone(),
        );

        let timeout = Duration::from_secs(args.ready_timeout_secs.unwrap_or(120).clamp(5, 600));
        let managed = ManagedServer {
            child,
            stdin: Some(stdin),
            pid,
            server_dir: server_dir.clone(),
            port,
            started_at: std::time::Instant::now(),
            log: log.clone(),
        };
        *slot = Some(managed);

        let ready = Self::await_ready(
            ctx,
            &log,
            line_rx,
            &mut slot.as_mut().unwrap().child,
            timeout,
        )
        .await;
        // 无论就绪 / 超时 / 崩溃 / 被打断：工具已返回，日志一律停止滚动并
        // 放弃事件发送端——否则回合结束后渲染器永不退出，REPL 卡死（实测教训）
        emit.store(false, Ordering::SeqCst);
        match ready {
            Err(ToolError::Cancelled) => Err(ToolError::Cancelled), // 进程保持托管
            Err(err) => {
                // 崩溃：清空托管槽
                *slot = None;
                Ok(ToolOutcome::err(err.to_string()))
            }
            Ok(true) => {
                let elapsed = slot.as_ref().unwrap().started_at.elapsed().as_secs_f32();
                let mut lines = vec![format!(
                    "服务器就绪（{elapsed:.1}s，PID {pid}，{}）",
                    server_dir.display()
                )];
                if let Some(port) = port {
                    lines.push(format!(
                        "监听 127.0.0.1:{port}；可用 mc_ping 验证，或直接进服游玩。"
                    ));
                }
                lines.push("日志已停止滚动（server_status 可查）；长期运行/关闭 mcha 后请用交付的 start 脚本启动。".to_string());
                Ok(ToolOutcome::ok(lines.join("\n")))
            }
            Ok(false) => {
                let tail = tail_lines(&log, 10).join("\n");
                Ok(ToolOutcome::err(format!(
                    "未在 {} 秒内就绪；进程仍在运行（PID {pid}），首次生成世界可能较慢。\
                     可用 server_status 查看，或继续等待（再次以更长超时启动前需先 stop_server）。\n日志尾部：\n{tail}",
                    timeout.as_secs()
                )))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// stop_server
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StopServerArgs {
    /// 优雅停失败后是否强杀（默认 true）
    #[serde(default)]
    pub force_on_timeout: Option<bool>,
}

pub struct StopServerTool {
    slot: Slot,
}

#[async_trait::async_trait]
impl Tool for StopServerTool {
    fn name(&self) -> &'static str {
        "stop_server"
    }
    fn description(&self) -> String {
        "优雅停止托管中的服务器（stdin 发送 stop 命令，保存世界），超时则强制结束进程树。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(StopServerArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Execute
    }
    fn confirm_summary(&self, _args: &serde_json::Value) -> Vec<String> {
        vec![
            "向托管中的服务器 stdin 发送 stop 命令优雅停机（保存世界数据）".to_string(),
            format!(
                "{} 秒未退出则强制结束进程树",
                GRACEFUL_STOP_TIMEOUT.as_secs()
            ),
        ]
    }
    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: StopServerArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        let force = args.force_on_timeout.unwrap_or(true);
        let Some(mut server) = self.slot.lock().await.take() else {
            return Ok(ToolOutcome::err("当前没有托管中的服务器进程"));
        };
        use tokio::io::AsyncWriteExt;
        let write_result = match server.stdin.take() {
            Some(mut stdin) => stdin
                .write_all(b"stop\n")
                .await
                .map_err(|err| format!("写入 stop 失败：{err}"))
                .map(|_| ()),
            None => Err("stdin 已关闭（进程可能已退出）".to_string()),
        };
        let wait = tokio::time::timeout(GRACEFUL_STOP_TIMEOUT, server.child.wait()).await;
        let uptime = server.started_at.elapsed().as_secs_f32();
        let tail = tail_lines(&server.log, 5).join("\n");
        match (write_result, wait) {
            (Ok(()), Ok(Ok(status))) => Ok(ToolOutcome::ok(format!(
                "服务器已优雅停止（运行 {:.1}s，退出码 {}）。\n尾部日志：\n{tail}",
                uptime,
                status.code().unwrap_or(0)
            ))),
            (Ok(()), Err(_elapsed)) => {
                if force {
                    kill_tree(server.pid).await;
                    Ok(ToolOutcome::ok(format!(
                        "优雅停超时（{}s），已强制结束进程树（PID {}，运行 {:.1}s）",
                        GRACEFUL_STOP_TIMEOUT.as_secs(),
                        server.pid,
                        uptime
                    )))
                } else {
                    Ok(ToolOutcome::err(format!(
                        "优雅停超时且未强杀（force_on_timeout=false）；进程 PID {} 可能仍在运行",
                        server.pid
                    )))
                }
            }
            (Err(reason), _) => {
                kill_tree(server.pid).await;
                Ok(ToolOutcome::err(format!(
                    "{reason}；已强制结束进程树（PID {}）",
                    server.pid
                )))
            }
            (Ok(()), Ok(Err(err))) => Ok(ToolOutcome::err(format!("等待退出失败：{err}"))),
        }
    }
}

/// 结束进程树：Windows `taskkill /T /F`；Unix 对进程组发 SIGKILL。
async fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .await;
    }
    #[cfg(unix)]
    {
        // start_server 以 process_group(0) 启动，可对整组发信号
        libc_kill_group(pid);
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

#[cfg(unix)]
fn libc_kill_group(pid: u32) {
    // kill(-pgid, SIGKILL)：等价于对进程组全体发 SIGKILL；失败（组已不存在）忽略
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(-(pid as i32), 9);
    }
}

// ---------------------------------------------------------------------------
// server_status
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ServerStatusArgs {}

pub struct ServerStatusTool {
    slot: Slot,
}

#[async_trait::async_trait]
impl Tool for ServerStatusTool {
    fn name(&self) -> &'static str {
        "server_status"
    }
    fn description(&self) -> String {
        "查看托管中的服务器进程状态（PID / 端口 / 运行时长 / 最近日志）。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ServerStatusArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolCtx,
    ) -> Result<ToolOutcome, ToolError> {
        let mut slot = self.slot.lock().await;
        let Some(server) = slot.as_mut() else {
            return Ok(ToolOutcome::ok("当前没有托管中的服务器进程。"));
        };
        let tail = tail_lines(&server.log, 10).join("\n");
        match server.child.try_wait() {
            Ok(Some(status)) => {
                let pid = server.pid;
                let dir = server.server_dir.display().to_string();
                let uptime = server.started_at.elapsed().as_secs_f32();
                *slot = None;
                Ok(ToolOutcome::ok(format!(
                    "服务器进程已退出（PID {pid}，运行 {uptime:.1}s，退出码 {}；{dir}）。\n尾部日志：\n{tail}",
                    status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "信号".into())
                )))
            }
            Ok(None) => {
                let mut lines = vec![format!(
                    "运行中：PID {}，端口 {}，已运行 {:.1}s，目录 {}",
                    server.pid,
                    server
                        .port
                        .map(|p| format!("127.0.0.1:{p}"))
                        .unwrap_or_else(|| "未知".into()),
                    server.started_at.elapsed().as_secs_f32(),
                    server.server_dir.display()
                )];
                lines.push(format!("最近日志：\n{tail}"));
                Ok(ToolOutcome::ok(lines.join("\n")))
            }
            Err(err) => Ok(ToolOutcome::err(format!("查询进程状态失败：{err}"))),
        }
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
        };
        (ctx, root)
    }

    fn tools() -> (StartServerTool, StopServerTool, ServerStatusTool) {
        lifecycle_tools()
    }

    /// 写 server.properties（供端口解析）与假 java 脚本（打印 MC 启动日志，stop 时退出）。
    #[cfg(unix)]
    fn install_fixture(dir: &std::path::Path, script_body: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("server.properties"), "server-port=25565\n").unwrap();
        std::fs::write(dir.join("server.jar"), b"fake jar").unwrap();
        let script = dir.join("fake-java.sh");
        std::fs::write(&script, script_body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.display().to_string()
    }

    #[cfg(unix)]
    const READY_SCRIPT: &str = "#!/bin/sh\n\
        echo \"[00:00:00] [main/INFO]: Starting minecraft server version 1.21.1\"\n\
        echo \"[00:00:00] [Server thread/INFO]: Preparing level \\\"world\\\"\"\n\
        echo \"[00:00:01] [Server thread/INFO]: Done (0.100s)! For help, type \\\"help\\\"\"\n\
        while read -r line; do\n\
        case \"$line\" in\n\
        stop) echo \"[00:00:02] [Server thread/INFO]: Stopping server\"; exit 0;;\n\
        esac\n\
        done\n\
        sleep 120\n";

    #[cfg(unix)]
    const CRASH_SCRIPT: &str = "#!/bin/sh\n\
        echo \"[00:00:00] [main/ERROR]: Failed to start the minecraft server\"\n\
        exit 1\n";

    #[tokio::test]
    #[cfg(unix)]
    async fn full_lifecycle_with_fake_server() {
        let (ctx, _root) = test_ctx();
        let (start, stop, status) = tools();
        let java_path = install_fixture(&ctx.workspace.join("server"), READY_SCRIPT);

        let started = start
            .run(
                serde_json::json!({ "java_path": java_path, "jvm_memory_mb": 1024 }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = started else {
            panic!("应就绪：{started:?}");
        };
        assert!(content.contains("服务器就绪"), "{content}");
        assert!(content.contains("127.0.0.1:25565"), "{content}");

        let running = status.run(serde_json::json!({}), &ctx).await.unwrap();
        let ToolOutcome::Ok { content } = running else {
            panic!("状态应为运行中：{running:?}");
        };
        assert!(content.contains("运行中"), "{content}");

        let stopped = stop.run(serde_json::json!({}), &ctx).await.unwrap();
        let ToolOutcome::Ok { content } = stopped else {
            panic!("应优雅停止：{stopped:?}");
        };
        assert!(content.contains("优雅停止"), "{content}");

        let after = status.run(serde_json::json!({}), &ctx).await.unwrap();
        let ToolOutcome::Ok { content } = after else {
            panic!("状态查询应成功：{after:?}");
        };
        assert!(content.contains("没有托管"), "{content}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn crash_before_ready_reports_tail() {
        let (ctx, _root) = test_ctx();
        let (start, _stop, _status) = tools();
        let java_path = install_fixture(&ctx.workspace.join("server"), CRASH_SCRIPT);
        let outcome = start
            .run(serde_json::json!({ "java_path": java_path }), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Err { error } = outcome else {
            panic!("崩溃应返回结构化错误：{outcome:?}");
        };
        assert!(error.contains("就绪前退出"), "{error}");
        assert!(error.contains("Failed to start"), "{error}");
    }

    #[tokio::test]
    async fn start_requires_server_jar() {
        let (ctx, _root) = test_ctx();
        let (start, _stop, _status) = tools();
        let outcome = start
            .run(
                serde_json::json!({ "java_path": "/nonexistent/java" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "缺 server.jar 应拒绝：{outcome:?}");
    }

    #[tokio::test]
    async fn stop_without_server_reports_error() {
        let (_ctx, _root) = test_ctx();
        let (_start, stop, status) = tools();
        let outcome = stop.run(serde_json::json!({}), &_ctx).await.unwrap();
        assert!(!outcome.is_ok(), "无进程应报错：{outcome:?}");
        let outcome = status.run(serde_json::json!({}), &_ctx).await.unwrap();
        assert!(outcome.is_ok(), "状态查询只读应成功：{outcome:?}");
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

    /// v2.3 实测回归：日志停滚后读者必须放弃事件发送端，否则渲染器永不
    /// 退出、REPL 卡死在回合收尾（无法回到提示符，Ctrl-D / Ctrl-C 失效）。
    #[tokio::test]
    async fn reader_drops_sender_when_emit_disabled() {
        let log: Arc<AsyncMutex<VecDeque<String>>> = Arc::new(AsyncMutex::new(VecDeque::new()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (etx, _erx) = crate::events::event_channel();
        let emit = Arc::new(AtomicBool::new(true));
        let (mut client, server_side) = tokio::io::duplex(64);
        spawn_reader(server_side, Some(tx), log.clone(), Some(etx), emit.clone());

        use tokio::io::AsyncWriteExt;
        client.write_all(b"one\n").await.unwrap();
        // 关闸前的行照常发送
        assert_eq!(rx.recv().await.as_deref(), Some("one"));
        emit.store(false, Ordering::SeqCst);
        client.write_all(b"two\n").await.unwrap();
        // 关闸后的行不再发送；读者放弃发送端 → 通道关闭（渲染器可收尾）
        assert!(rx.recv().await.is_none());
        // 日志缓冲不受影响（server_status 仍可查全量尾部）
        assert_eq!(log.lock().await.len(), 2);
    }

    #[test]
    fn confirmation_lines_describe_start_and_stop() {
        // M2.1 实测回归：确认弹窗不得再出现空白内容
        let lines = StartServerTool::confirmation_lines(&serde_json::json!({
            "java_path": "/opt/jdk/bin/java", "jvm_memory_mb": 4096
        }));
        assert!(lines.iter().all(|l| !l.trim().is_empty()), "{lines:?}");
        let joined = lines.join("\n");
        assert!(
            joined.contains("/opt/jdk/bin/java -Xmx4096M -jar server.jar nogui"),
            "{joined}"
        );
        assert!(joined.contains("120 秒"), "{joined}");

        let (_start, stop, _status) = lifecycle_tools();
        let lines = stop.confirm_summary(&serde_json::json!({}));
        assert!(lines.iter().all(|l| !l.trim().is_empty()), "{lines:?}");
        assert!(lines.join("\n").contains("stop"), "{lines:?}");
    }
}
