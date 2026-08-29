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
    #[error("等待服务端就绪超时（{0} 秒）。可查看最新日志手动判断")]
    ReadyTimeout(u64),
    #[error("服务端在就绪前退出（exit code 见日志）")]
    ExitedEarly,
    #[error("任务已取消")]
    Cancelled,
}

/// 托管中的服务端进程。
pub struct ServerProcess {
    child: Mutex<Option<Child>>,
    /// 就绪检测时收集的最近日志（失败时供诊断）
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
    /// 期间把 stdout 行写进 recent_log，并回调 `on_line` 供进度渲染。
    pub async fn wait_ready(
        &self,
        timeout_secs: u64,
        cancel: CancellationToken,
        mut on_line: impl FnMut(&str),
    ) -> Result<(), ProcessError> {
        let stdout = {
            let mut guard = self
                .child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.as_mut() {
                Some(child) => child.stdout.take(),
                None => return Err(ProcessError::ExitedEarly),
            }
        };
        let Some(stdout) = stdout else {
            return Err(ProcessError::Spawn("stdout 不可读".into()));
        };
        let mut lines = BufReader::new(stdout).lines();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProcessError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(ProcessError::ReadyTimeout(timeout_secs));
                }
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            on_line(&line);
                            {
                                let mut log = self.recent_log.lock().unwrap_or_else(|p| p.into_inner());
                                if log.len() >= 200 {
                                    log.remove(0);
                                }
                                log.push(line.clone());
                            }
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
                        Ok(None) => {
                            // stdout 关闭：进程退出
                            return Err(ProcessError::ExitedEarly);
                        }
                        Err(e) => return Err(ProcessError::Spawn(format!("读取日志失败：{e}"))),
                    }
                }
            }
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
