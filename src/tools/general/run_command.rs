//! run_command：执行 shell 命令（设计 §8.2）。
//!
//! 边界：超时（默认取 ctx.command_timeout_secs，可传参覆盖，上限 600s）；
//! 输出截断（200 行 / 8KB，保头尾）；进程 kill_on_drop——取消或超时时子进程
//! 被终止，不留孤儿（M1 简化：不追杀整个进程树，见 README 已知边界）。

use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use crate::agent::message::ToolOutcome;
use crate::events::{Event, EventTx};

use super::confinement::resolve_in;
use super::{Tool, ToolCtx, ToolError};

/// 输出保底行数（超过 200 行时保留头尾各一半）。
const OUTPUT_MAX_LINES: usize = 200;
/// 输出字节上限。
const OUTPUT_MAX_BYTES: usize = 8 * 1024;
/// timeout_secs 参数上限。
const TIMEOUT_CAP_SECS: u64 = 600;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunCommandArgs {
    /// 要执行的 shell 命令（Windows 经 powershell -NoProfile -Command，Unix 经 sh -c）
    pub command: String,
    /// 超时秒数（默认 120，上限 600；超时进程会被终止）
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 子进程工作目录（工作区相对路径；默认工作区根）
    #[serde(default)]
    pub cwd: Option<String>,
}

pub struct RunCommandTool;

#[async_trait::async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &'static str {
        "run_command"
    }
    fn description(&self) -> String {
        "在工作区内执行一条 shell 命令，返回退出码与 stdout/stderr。用于系统探测、构建运行等；有超时限制。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(RunCommandArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Execute
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: RunCommandArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let cwd = match &args.cwd {
            Some(cwd) => resolve_in(&[ctx.workspace.as_path()], cwd)?,
            None => ctx.workspace.clone(),
        };
        let timeout_secs = args
            .timeout_secs
            .unwrap_or(ctx.command_timeout_secs)
            .clamp(1, TIMEOUT_CAP_SECS);

        let mut command = shell_command(&args.command);
        command
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|err| ToolError::Io(format!("启动命令失败：{err}")))?;
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        // 输出行边产出边渲染（R4：命令输出行原样滚动）
        let stdout_lines = drain_lines(stdout_pipe, ctx.events.clone());
        let stderr_lines = drain_lines(stderr_pipe, ctx.events.clone());

        let wait_all = async {
            let (out, err) = tokio::join!(stdout_lines, stderr_lines);
            let status = child
                .wait()
                .await
                .map_err(|err| ToolError::Io(format!("等待进程失败：{err}")))?;
            Ok::<_, ToolError>((out, err, status))
        };

        match tokio::time::timeout(Duration::from_secs(timeout_secs), wait_all).await {
            Err(_) => {
                // 超时：kill_on_drop 在 child drop 时终止进程
                Ok(ToolOutcome::err(format!(
                    "命令超时（{timeout_secs} 秒），进程已终止。可考虑缩短任务或调大 timeout_secs"
                )))
            }
            Ok(Err(err)) => Err(err),
            Ok(Ok((out, err, status))) => {
                let exit = match status.code() {
                    Some(code) => code.to_string(),
                    #[cfg(unix)]
                    None => format!(
                        "被信号终止（{}）",
                        status.signal().map(|s| s.to_string()).unwrap_or_default()
                    ),
                    #[cfg(windows)]
                    None => "进程被终止（无退出码）".to_string(),
                };
                Ok(ToolOutcome::ok(compose_output(&exit, &out, &err)))
            }
        }
    }
}

/// 按平台包装 shell。
#[cfg(windows)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-Command", command]);
    cmd
}

#[cfg(unix)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", command]);
    cmd
}

/// 持续读取管道：每行发 OutputLine 事件，同时收集全部行。
async fn drain_lines(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    events: EventTx,
) -> Vec<String> {
    let Some(pipe) = pipe else {
        return Vec::new();
    };
    let mut reader = BufReader::new(pipe);
    let mut lines = Vec::new();
    let mut buf = Vec::with_capacity(256);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
            buf.pop();
        }
        if buf.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(&buf).to_string();
        let _ = events.send(Event::OutputLine(line.clone()));
        lines.push(line);
    }
    lines
}

/// 输出截断：先按行（200 行保头尾），再按字节（8KB 保头尾）。
fn compose_output(exit: &str, stdout: &[String], stderr: &[String]) -> String {
    let mut out = format!("退出码：{exit}\n");
    out.push_str("--- stdout ---\n");
    out.push_str(&cap_text(&stdout.join("\n")));
    out.push_str("\n--- stderr ---\n");
    out.push_str(&cap_text(&stderr.join("\n")));
    out
}

fn cap_text(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut capped = if lines.len() > OUTPUT_MAX_LINES {
        let half = OUTPUT_MAX_LINES / 2;
        let omitted = lines.len() - OUTPUT_MAX_LINES;
        let mut kept: Vec<String> = lines[..half].iter().map(|s| s.to_string()).collect();
        kept.push(format!("…（中间省略 {omitted} 行）…"));
        kept.extend(lines[lines.len() - half..].iter().map(|s| s.to_string()));
        kept.join("\n")
    } else {
        text.to_string()
    };
    if capped.len() > OUTPUT_MAX_BYTES {
        let bytes = capped.as_bytes();
        let mid = OUTPUT_MAX_BYTES / 2;
        let head = valid_prefix(&bytes[..mid.min(bytes.len())]);
        let tail = valid_prefix(&bytes[bytes.len() - mid.min(bytes.len())..]);
        capped = format!("{head}\n…（中间省略，超出 8KB）…\n{tail}");
    }
    capped
}

/// 取字节切片中最长的合法 UTF-8 前缀（截断发生在多字节字符中间时丢弃残字）。
fn valid_prefix(bytes: &[u8]) -> &str {
    match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => std::str::from_utf8(&bytes[..err.valid_up_to()]).unwrap_or(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_text_truncates_lines_and_bytes() {
        let long: String = (0..500)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let capped = cap_text(&long);
        assert!(capped.contains("中间省略 300 行"));
        assert!(capped.starts_with("line0"));
        assert!(capped.ends_with("line499"));

        let huge = "字".repeat(20_000); // 60KB UTF-8
        let capped = cap_text(&huge);
        assert!(capped.len() < 10_000);
        assert!(capped.contains("超出 8KB"));
    }

    #[tokio::test]
    async fn runs_command_and_captures_output() {
        // 不需要真实 ctx 的交互 / 事件：只用到 workspace 与 events
        let (tx, _rx) = crate::events::event_channel();
        let root = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            workspace: root.path().to_path_buf(),
            data_dir: root.path().join("data"),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
        };
        let tool = RunCommandTool;
        let outcome = tool
            .run(
                serde_json::json!({ "command": "printf hello", "cwd": "." }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("命令应成功：{outcome:?}")
        };
        assert!(content.contains("退出码：0"));
        assert!(content.contains("hello"));
    }
}
