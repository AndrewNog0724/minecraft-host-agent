//! 服务端进程管理（FR-06）：启动、就绪检测（日志 "Done"）、停止。
//!
//! Drop 守卫保证取消/退出时先停服务端进程，不留孤儿（R4 打断语义）。

use std::path::Path;
use std::sync::Mutex;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("启动服务端失败：{0}")]
    Spawn(String),
    #[error("等待服务端就绪超时（{secs} 秒）。最近日志（完整日志见 {log_path}）：\n{tail}")]
    ReadyTimeout {
        secs: u64,
        log_path: String,
        tail: String,
    },
    #[error("服务端在就绪前退出。最近日志（完整日志见 {log_path}）：\n{tail}")]
    ExitedEarly { log_path: String, tail: String },
    #[error("任务已取消")]
    Cancelled,
}

/// 托管中的服务端进程。
pub struct ServerProcess {
    child: Mutex<Option<Child>>,
    /// 就绪检测时收集的最近日志（stdout+stderr 合并，失败时供诊断，决议 D19）
    recent_log: Mutex<Vec<String>>,
}

impl ServerProcess {
    /// 启动服务端：`java <jvm_args> -jar <jar> nogui`。
    pub async fn spawn(
        java_path: &str,
        jvm_args: &[String],
        jar: &Path,
        workdir: &Path,
    ) -> Result<Self, ProcessError> {
        let mut cmd = tokio::process::Command::new(java_path);
        cmd.args(jvm_args)
            .arg("-jar")
            .arg(jar)
            .arg("nogui")
            .current_dir(workdir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());
        // Unix 下 java 需可执行权限；Windows 无此概念
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(java_path) {
                let mut perm = meta.permissions();
                perm.set_mode(perm.mode() | 0o111);
                let _ = std::fs::set_permissions(java_path, perm);
            }
        }
        let child = cmd
            .spawn()
            .map_err(|e| ProcessError::Spawn(format!("{java_path} -jar {}：{e}", jar.display())))?;
        Ok(Self {
            child: Mutex::new(Some(child)),
            recent_log: Mutex::new(Vec::new()),
        })
    }

    /// 阻塞直至日志出现就绪标记（`Done (x.xxx)s!...`）或超时/取消/早退。
    ///
    /// 决议 D19（v0.10 实测复盘）：
    /// - **stdout 与 stderr 并读**：此前 stderr 管道接而不读——JVM 崩溃信息
    ///   （走 stderr）全部丢失，且缓冲区写满会卡死子进程直到超时；
    /// - 全部输出（stderr 行带 `[stderr]` 前缀）实时追加到 `log_path`；
    /// - 每 5 秒回调一次 `on_tick(已等待秒数)`，让界面显示"已等待 N 秒"；
    /// - 超时 / 提前退出的错误消息附最近日志尾部，排障有据可查。
    pub async fn wait_ready(
        &self,
        timeout_secs: u64,
        cancel: CancellationToken,
        log_path: &Path,
        mut on_line: impl FnMut(&str),
        mut on_tick: impl FnMut(u64),
    ) -> Result<(), ProcessError> {
        let (stdout, stderr) = {
            let mut guard = self
                .child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(child) = guard.as_mut() else {
                return Err(self.early_exit_error(log_path));
            };
            (child.stdout.take(), child.stderr.take())
        };
        let Some(stdout) = stdout else {
            return Err(ProcessError::Spawn("stdout 不可读".into()));
        };
        let Some(stderr) = stderr else {
            return Err(ProcessError::Spawn("stderr 不可读".into()));
        };
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut log_file = std::fs::File::create(log_path).map_err(|e| {
            ProcessError::Spawn(format!("创建启动日志 {} 失败：{e}", log_path.display()))
        })?;

        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(timeout_secs);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.reset(); // 首个 tick 立即触发，跳过；从第 5 秒起上报
        let mut stdout_open = true;
        let mut stderr_open = true;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProcessError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(self.ready_timeout_error(timeout_secs, log_path));
                }
                _ = tick.tick() => {
                    let elapsed = start.elapsed().as_secs();
                    on_tick(elapsed);
                }
                line = stdout_lines.next_line(), if stdout_open => {
                    match line {
                        Ok(Some(line)) => {
                            let rendered = self.record_line(&line, false, &mut log_file);
                            on_line(&rendered);
                            // vanilla/paper/fabric 共用的就绪标记；
                            // stdout 行带 [HH:MM:SS] 时间戳前缀，用包含匹配
                            if line.contains("Done (") {
                                return Ok(());
                            }
                            // EULA 缺失会被写 eula 前置检查拦住；此处仍兜底识别
                            if line.contains("You need to agree to the EULA") {
                                return Err(ProcessError::Spawn("EULA 未同意".into()));
                            }
                        }
                        Ok(None) => stdout_open = false,
                        Err(e) => return Err(ProcessError::Spawn(format!("读取日志失败：{e}"))),
                    }
                }
                line = stderr_lines.next_line(), if stderr_open => {
                    match line {
                        Ok(Some(line)) => {
                            let rendered = self.record_line(&line, true, &mut log_file);
                            on_line(&rendered);
                        }
                        Ok(None) => stderr_open = false,
                        Err(e) => return Err(ProcessError::Spawn(format!("读取错误流失败：{e}"))),
                    }
                }
            }
            if !stdout_open && !stderr_open {
                // 两条流都关闭：进程已退出且未见就绪标记
                return Err(self.early_exit_error(log_path));
            }
        }
    }

    /// 记录一行日志：stdout 原样、stderr 加 `[stderr]` 前缀；
    /// 写文件 + 进内存环形缓冲，返回渲染后的行供回调直显。
    fn record_line(&self, line: &str, is_stderr: bool, log_file: &mut std::fs::File) -> String {
        let rendered = if is_stderr {
            format!("[stderr] {line}")
        } else {
            line.to_string()
        };
        use std::io::Write as _;
        let _ = writeln!(log_file, "{rendered}"); // 写日志失败不影响主流程
        let mut log = self.recent_log.lock().unwrap_or_else(|p| p.into_inner());
        if log.len() >= 200 {
            log.remove(0);
        }
        log.push(rendered.clone());
        rendered
    }

    /// 最近日志尾部（最多 `n` 行）；无输出时给出占位说明。
    fn log_tail(&self, n: usize) -> String {
        let log = self.recent_log.lock().unwrap_or_else(|p| p.into_inner());
        if log.is_empty() {
            "（服务端未输出任何日志就退出了——常见于 java 路径无效或参数错误）".into()
        } else {
            let start = log.len().saturating_sub(n);
            log[start..].join("\n")
        }
    }

    fn ready_timeout_error(&self, secs: u64, log_path: &Path) -> ProcessError {
        ProcessError::ReadyTimeout {
            secs,
            log_path: log_path.display().to_string(),
            tail: self.log_tail(15),
        }
    }

    fn early_exit_error(&self, log_path: &Path) -> ProcessError {
        ProcessError::ExitedEarly {
            log_path: log_path.display().to_string(),
            tail: self.log_tail(15),
        }
    }

    /// 停止进程（先尝试 stdin `stop`，超时强杀；MVP 简化为直接杀）。
    pub fn stop(&self) {
        let mut guard = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(child) = guard.as_mut() {
            let _ = child.start_kill();
        }
        *guard = None;
    }
}

/// Drop 守卫：任何路径退出都确保子进程被终止（R4：不留孤儿进程）。
impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// v0.10 实测回归（决议 D19）：进程在就绪前退出、崩溃信息只走 stderr——
    /// 此前该形态表现为"终端静默直至超时"，现在应立即报错并附 stderr 日志尾部。
    #[tokio::test]
    async fn 进程早退_错误应带stderr日志尾部() {
        let workdir = tempfile::tempdir().unwrap();
        // spawn 的命令形状固定为 `java <args> -jar <jar> nogui`；
        // 用 sh 顶替 java：`sh -c '脚本' -jar <jar> nogui`，脚本即崩溃服务端的形态
        let process = ServerProcess::spawn(
            "sh",
            &[
                "-c".into(),
                "echo 'Error: unable to launch JVM' >&2; exit 1".into(),
            ],
            Path::new("unused.jar"),
            workdir.path(),
        )
        .await
        .unwrap();
        let log_path = workdir.path().join("mcha-launch.log");
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let lines_cb = lines.clone();
        let err = process
            .wait_ready(
                10,
                CancellationToken::new(),
                &log_path,
                move |line| lines_cb.lock().unwrap().push(line.to_string()),
                |_| {},
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("unable to launch JVM") && text.contains("[stderr]"),
            "错误消息应附 stderr 日志尾部：{text}"
        );
        assert!(
            text.contains("mcha-launch.log"),
            "应指出完整日志位置：{text}"
        );
        let saved = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            saved.contains("[stderr] Error: unable to launch JVM"),
            "启动日志应落盘并含 stderr 行：{saved}"
        );
        assert!(
            lines.lock().unwrap().iter().any(|l| l.contains("[stderr]")),
            "on_line 回调也应收到 stderr 行（直显）"
        );
    }

    /// 就绪标记命中：stdout 出现 `Done (` 应立即返回成功。
    #[tokio::test]
    async fn 就绪标记命中返回成功() {
        let workdir = tempfile::tempdir().unwrap();
        let process = ServerProcess::spawn(
            "sh",
            &[
                "-c".into(),
                "echo '[12:00:00] Done (3.14s)! For help, type \"help\"'; sleep 30".into(),
            ],
            Path::new("unused.jar"),
            workdir.path(),
        )
        .await
        .unwrap();
        let log_path = workdir.path().join("mcha-launch.log");
        let result = process
            .wait_ready(10, CancellationToken::new(), &log_path, |_| {}, |_| {})
            .await;
        assert!(result.is_ok(), "应识别 Done ( 就绪标记：{result:?}");
        process.stop();
    }

    /// 超时路径：一直无输出时应按配置的超时秒数失败并说明日志位置。
    #[tokio::test]
    async fn 无输出超时报错() {
        let workdir = tempfile::tempdir().unwrap();
        let process = ServerProcess::spawn(
            "sh",
            &["-c".into(), "sleep 30".into()],
            Path::new("unused.jar"),
            workdir.path(),
        )
        .await
        .unwrap();
        let log_path = workdir.path().join("mcha-launch.log");
        let err = process
            .wait_ready(1, CancellationToken::new(), &log_path, |_| {}, |_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProcessError::ReadyTimeout { secs: 1, .. }),
            "应为就绪超时错误：{err}"
        );
        process.stop();
    }
}
