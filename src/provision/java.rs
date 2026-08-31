//! Java 自动供给（FR-02，§8.8，决议 D2：全自动受管安装，不降级）。
//!
//! 选择顺序（v0.11 决议 D21 修订）：① 系统 PATH 已有匹配版本 → 用系统的；
//! ② Windows 扫描 `C:\Program Files\Java\*`（用户硬性要求的统一安装位置）；
//! ③ 数据目录受管目录已有 → 复用（历史兼容）；
//! ④ Adoptium 官方渠道下载 zip 免安装包（镜像优先，决议 D24）。
//! Windows 安装根为 `C:\Program Files\Java\<版本>\`——普通权限写不进时
//! 经 PowerShell 提权一次（UAC），拒绝/失败回退数据目录受管目录并留痕；
//! 只写该子目录，不改注册表、不改系统 PATH。其余平台只写数据目录受管位置。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::events::{EventBus, ProgressEvent};
use crate::knowledge::upstream::{
    AdoptiumClient, DownloadItem, DownloadKind, HttpBase, UpstreamError,
};
use crate::spec::JavaRuntime;

const VENDOR: &str = "temurin";

#[derive(Debug, Error)]
pub enum JavaError {
    #[error("探测系统 Java 失败：{0}")]
    Probe(String),
    #[error("Java 供给失败：{0}")]
    Upstream(#[from] UpstreamError),
    #[error("解压 JRE 包失败：{0}")]
    Unzip(String),
    #[error("JRE 安装失败：{0}")]
    Install(String),
    #[error("JRE 解压后找不到 java 可执行文件")]
    JavaBinaryNotFound,
    #[error("操作已被用户取消")]
    Cancelled,
}

/// UAC 提权等待上限（决议 D27）：弹窗未被确认时绝不永久挂起。
const ELEVATION_TIMEOUT_SECS: u64 = 120;

/// 平台 java 可执行相对路径。
fn java_bin_relative() -> &'static str {
    if cfg!(windows) {
        "bin/java.exe"
    } else {
        "bin/java"
    }
}

/// 环境探测报告（agent 工具 `probe_environment` 的返回值，只读无副作用）。
pub async fn probe_environment_report() -> serde_json::Value {
    let java = match probe_system_java().await {
        Ok(Some((path, version, major))) => serde_json::json!({
            "found": true, "path": path, "version": version, "major": major
        }),
        _ => serde_json::json!({ "found": false }),
    };
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "java": java,
    })
}

/// 解析 `java -version` 输出中的版本号：
/// `openjdk version "21.0.4" ...` → 21；`java version "1.8.0_392"` → 8。
fn parse_java_major(output: &str) -> Option<u8> {
    let quoted = output.split('"').nth(1)?;
    let mut parts = quoted.split('.');
    let first = parts.next()?.trim();
    if first == "1" {
        // 旧版本命名：1.8.0_x → 8
        parts.next()?.parse().ok()
    } else {
        first.parse().ok()
    }
}

/// 探测系统 PATH 上的 Java：返回 (可执行名, 完整版本串, 大版本)。
async fn probe_system_java() -> Result<Option<(String, String, u8)>, JavaError> {
    let output = tokio::process::Command::new("java")
        .arg("-version")
        .output()
        .await
        .map_err(|e| JavaError::Probe(format!("无法执行 java：{e}")))?;
    // 注意：`java -version` 把版本信息打到 stderr
    let text = String::from_utf8_lossy(&output.stderr);
    let Some(version_line) = text.lines().next() else {
        return Ok(None);
    };
    let quoted = version_line
        .split('"')
        .nth(1)
        .unwrap_or_default()
        .to_string();
    match parse_java_major(&text) {
        Some(major) => Ok(Some(("java".into(), quoted, major))),
        None => Ok(None),
    }
}

/// 受管目录布局：`<data>/runtime/jdk-<major>/<release_name>/`。
fn managed_root(data_dir: &Path, major: u8) -> PathBuf {
    data_dir.join("runtime").join(format!("jdk-{major}"))
}

/// Windows 受管 JRE 统一安装根（决议 D21，用户硬性要求）：
/// `C:\Program Files\Java\`，与官方安装器默认位置一致。
fn program_files_java_root() -> PathBuf {
    PathBuf::from(r"C:\Program Files\Java")
}

/// 列出安装根下的 java 可执行文件候选：根即版本目录（root/bin/java）
/// 与一级子目录（root/<版本>/bin/java）两种形态都识别。
fn java_candidates_in_root(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let direct = root.join(java_bin_relative());
    if direct.is_file() {
        out.push(direct);
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join(java_bin_relative());
        if candidate.is_file() {
            out.push(candidate);
        }
    }
    out
}

/// 运行 java -version 并解析主版本；无法运行或解析失败返回 None。
async fn java_major_of(java: &Path) -> Option<u8> {
    let output = tokio::process::Command::new(java)
        .arg("-version")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // `java -version` 把版本信息打到 stderr
    parse_java_major(&String::from_utf8_lossy(&output.stderr))
}

/// 扫描安装根：候选逐一以 `java -version` 核实主版本（目录名不作依据），
/// 返回首个匹配的绝对路径（决议 D21 的 ② 复用查找）。
async fn find_java_in_root(root: &Path, major: u8) -> Option<PathBuf> {
    for candidate in java_candidates_in_root(root) {
        if java_major_of(&candidate).await == Some(major) {
            return Some(candidate);
        }
    }
    None
}

/// 在受管目录里找可复用的 java：返回 (java 可执行绝对路径, 版本目录名)。
/// v0.10.1 勘误：Adoptium 压缩包自带顶层目录（如 `jdk-21.0.12.1+1-jre/`），
/// 解压后实为 `<release>/<release>-jre/bin/java` 双层嵌套——此前只查一层，
/// 永远找不到已装的 JRE，导致每个任务都重复下载。此处按两层探测。
fn find_managed_java(data_dir: &Path, major: u8) -> Option<(PathBuf, String)> {
    let root = managed_root(data_dir, major);
    let entries = std::fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        let bin = java_bin_relative();
        // 形态一：直接 <release>/bin/java
        if dir.join(bin).is_file() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            return Some((dir.join(bin), dir_name));
        }
        // 形态二：压缩包自带顶层目录 → <release>/<inner>/bin/java
        if let Ok(nested) = std::fs::read_dir(&dir) {
            for inner in nested.flatten() {
                let candidate = inner.path().join(bin);
                if candidate.is_file() {
                    let dir_name = entry.file_name().to_string_lossy().to_string();
                    return Some((candidate, dir_name));
                }
            }
        }
    }
    None
}

/// 解析受管 JRE 的 java 绝对路径（写入 JavaPlan，运行服务端一律用它）。
pub fn managed_java_path(runtime: &JavaRuntime) -> Option<String> {
    match runtime {
        JavaRuntime::System { path, .. } | JavaRuntime::Managed { path, .. } => Some(path.clone()),
        JavaRuntime::Pending => None,
    }
}

/// 主入口：按 §8.8 选择顺序拿到满足 `required_major` 的 Java 运行时。
/// 步骤的开始/结束事件由调用方（exec::deploy）统一发布；本函数只发过程性
/// StepProgress（渠道、字节进度等），避免同名步骤的双重进度条（v0.10）。
pub async fn resolve_java(
    required_major: u8,
    cfg: &crate::config::AppConfig,
    data_dir: &Path,
    base: &HttpBase,
    bus: &EventBus,
    task_id: &str,
    cancel: CancellationToken,
) -> Result<JavaRuntime, JavaError> {
    let step = "java";

    // ① 系统 PATH
    match probe_system_java().await {
        Ok(Some((path, version, major))) if major == required_major => {
            return Ok(JavaRuntime::System {
                path,
                version: version.clone(),
            });
        }
        Ok(found) => {
            let detail = found
                .map(|(_, v, m)| format!("系统 Java 为 {v}（Java {m}），不匹配，转受管安装"))
                .unwrap_or_else(|| "未检测到系统 Java，转受管安装".into());
            bus.publish(ProgressEvent::StepProgress {
                task_id: task_id.into(),
                step: step.into(),
                current: 0,
                total: None,
                detail: Some(detail),
            });
        }
        Err(e) => tracing::warn!("系统 Java 探测失败（忽略并转受管安装）：{e}"),
    }

    // ② Windows：扫描统一安装根 Program Files\Java（决议 D21）
    if cfg!(windows) {
        let root = program_files_java_root();
        match find_java_in_root(&root, required_major).await {
            Some(path) => {
                return Ok(JavaRuntime::Managed {
                    path: path.to_string_lossy().to_string(),
                    vendor: VENDOR.into(),
                    version: dir_name_of(&path),
                });
            }
            None => bus.publish(ProgressEvent::StepProgress {
                task_id: task_id.into(),
                step: step.into(),
                current: 0,
                total: None,
                detail: Some(format!(
                    "{} 下未发现 Java {required_major}，转受管安装",
                    root.display()
                )),
            }),
        }
    }

    // ③ 受管目录复用（历史兼容：数据目录 runtime/）。
    // 决议 D21 v0.11.1：Windows 下先尝试把旧受管 JRE 迁移到统一安装根
    // Program Files\Java\；UAC 被拒后记旗标不再反复弹窗。
    if let Some((path, _dir_name)) = find_managed_java(data_dir, required_major) {
        let mut final_path = path.clone();
        if cfg!(windows) {
            let declined_marker = data_dir.join("runtime").join("migrate-declined.flag");
            let declined_before = declined_marker.is_file();
            let migrated = if declined_before {
                Ok(None)
            } else {
                migrate_managed_to_program_files(&path, required_major, bus, task_id, step, &cancel)
                    .await
            };
            // D27：用户取消必须中止任务，不得降级继续
            let migrated = match migrated {
                Ok(m) => m,
                Err(JavaError::Cancelled) => return Err(JavaError::Cancelled),
                Err(e) => {
                    tracing::warn!("迁移流程异常（降级用原位置）：{e}");
                    None
                }
            };
            match migrated {
                Some(new_path) => {
                    final_path = new_path.clone();
                    let _ = std::fs::remove_file(&declined_marker);
                    bus.publish(ProgressEvent::StepProgress {
                        task_id: task_id.into(),
                        step: step.into(),
                        current: 0,
                        total: None,
                        detail: Some(format!(
                            "旧受管 JRE 已迁移到统一安装根：{}",
                            new_path.display()
                        )),
                    });
                }
                None => {
                    let detail = if declined_before {
                        format!(
                            "此前已拒绝迁移，继续使用原位置：{path}（删除 {marker} 可重新启用自动迁移）",
                            path = path.display(),
                            marker = declined_marker.display()
                        )
                    } else {
                        let _ = std::fs::write(&declined_marker, b"");
                        format!(
                            "JRE 迁移到 {} 未完成（UAC 被拒或移动失败），继续使用原位置：{}",
                            program_files_java_root().display(),
                            path.display()
                        )
                    };
                    bus.publish(ProgressEvent::StepProgress {
                        task_id: task_id.into(),
                        step: step.into(),
                        current: 0,
                        total: None,
                        detail: Some(detail),
                    });
                }
            }
        }
        return Ok(JavaRuntime::Managed {
            path: final_path.to_string_lossy().to_string(),
            vendor: VENDOR.into(),
            version: dir_name_of(&final_path),
        });
    }

    // ④ Adoptium 下载安装（zip 免安装包，sha256 强制校验）
    bus.publish(ProgressEvent::StepProgress {
        task_id: task_id.into(),
        step: step.into(),
        current: 0,
        total: None,
        detail: Some("从 Adoptium 官方渠道获取 JRE 元数据".into()),
    });
    let adoptium = AdoptiumClient::new(base.clone());
    let asset = adoptium.latest_jre(required_major).await?;

    // 下载渠道：配置了 adoptium_mirror 时镜像优先（§8.8，国内网络默认解法），
    // 失败回退官方渠道；两条渠道校验同一 sha256。
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "mac",
        _ => "linux",
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        other => other,
    };
    let mut urls = Vec::new();
    if !cfg.network.adoptium_mirror.is_empty() {
        urls.push(format!(
            "{mirror}/{major}/jre/{arch}/{os}/{file}",
            mirror = cfg.network.adoptium_mirror.trim_end_matches('/'),
            major = required_major,
            file = asset.file_name,
        ));
    }
    urls.push(asset.download_url.clone());

    let staging = std::env::temp_dir().join("mcha-jre");
    let mut zip_path: Option<std::path::PathBuf> = None;
    let mut last_err: Option<UpstreamError> = None;
    for url in urls {
        let item = DownloadItem {
            url,
            sha1: None,
            sha256: asset.sha256.clone(),
            file_name: asset.file_name.clone(),
            kind: DownloadKind::JavaJre,
        };
        bus.publish(ProgressEvent::StepProgress {
            task_id: task_id.into(),
            step: step.into(),
            current: 0,
            total: None,
            detail: Some(format!("下载渠道：{}", item.url)),
        });
        match base
            .download(&item, &staging, cancel.clone(), &|current, total| {
                bus.publish(ProgressEvent::StepProgress {
                    task_id: task_id.into(),
                    step: step.into(),
                    current,
                    total,
                    detail: Some(format!(
                        "下载 JRE {}/{} 字节",
                        current,
                        total.map(|t| t.to_string()).unwrap_or_else(|| "?".into())
                    )),
                });
            })
            .await
        {
            Ok(path) => {
                zip_path = Some(path);
                break;
            }
            Err(UpstreamError::Cancelled) => {
                return Err(JavaError::Upstream(UpstreamError::Cancelled));
            }
            Err(e) => {
                tracing::warn!("渠道 {} 下载失败：{e}", item.url);
                last_err = Some(e);
            }
        }
    }
    let Some(zip_path) = zip_path else {
        return Err(last_err
            .map(JavaError::Upstream)
            .unwrap_or(JavaError::JavaBinaryNotFound));
    };

    bus.publish(ProgressEvent::StepProgress {
        task_id: task_id.into(),
        step: step.into(),
        current: 0,
        total: None,
        detail: Some("校验通过，安装到目标位置".into()),
    });
    // 安装落位（决议 D21）：Windows 优先统一安装根 `C:\Program Files\Java\`，
    // 普通权限写不进时 UAC 提权一次；被拒/失败回退数据目录受管目录并留痕。
    // 其余平台直接装数据目录受管目录。
    let mut java_path: Option<PathBuf> = None;
    if cfg!(windows) {
        let root = program_files_java_root();
        match install_windows_zip(
            &zip_path,
            &root,
            required_major,
            bus,
            task_id,
            step,
            &cancel,
        )
        .await
        {
            Ok(path) => java_path = Some(path),
            // D27：用户取消必须中止任务，不得降级继续装数据目录
            Err(JavaError::Cancelled) => return Err(JavaError::Cancelled),
            Err(e) => {
                tracing::warn!("Program Files 安装未完成，回退数据目录受管安装：{e}");
                bus.publish(ProgressEvent::StepProgress {
                    task_id: task_id.into(),
                    step: step.into(),
                    current: 0,
                    total: None,
                    detail: Some(format!(
                        "Program Files 写入未完成（{e}），回退数据目录受管安装"
                    )),
                });
            }
        }
    }
    if java_path.is_none() {
        let dest_root = managed_root(data_dir, required_major);
        let dest_dir = dest_root.join(&asset.release_name);
        // Windows 分发 zip 包；Linux/macOS 分发 tar.gz（Adoptium 官方打包形态）
        if asset.file_name.ends_with(".zip") {
            unzip(&zip_path, &dest_dir)?;
        } else {
            untar_gz(&zip_path, &dest_dir)?;
        }
        java_path = Some(locate_java_binary(&dest_dir).ok_or(JavaError::JavaBinaryNotFound)?);
    }
    let _ = tokio::fs::remove_file(&zip_path).await; // 清理临时 zip，失败不影响结果

    let java_path = java_path.ok_or(JavaError::JavaBinaryNotFound)?;
    Ok(JavaRuntime::Managed {
        path: java_path.to_string_lossy().to_string(),
        vendor: VENDOR.into(),
        version: dir_name_of(&java_path),
    })
}

/// 从 java 绝对路径反推版本目录名（`.../<版本>/bin/java` 的 `<版本>` 段）。
/// 目录名不作假设——Program Files 与受管目录的实际落位名以解压结果为准。
fn dir_name_of(java_path: &Path) -> String {
    java_path
        .parent()
        .and_then(|bin| bin.parent())
        .and_then(|dir| dir.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Windows 安装：zip 解压到统一安装根（决议 D21）。
/// 直接写入失败（普通权限进程写 Program Files）→ UAC 提权一次；
/// 成功后在根目录按主版本定位 java 绝对路径返回。
async fn install_windows_zip(
    zip_path: &Path,
    root: &Path,
    required_major: u8,
    bus: &EventBus,
    task_id: &str,
    step: &str,
    cancel: &CancellationToken,
) -> Result<PathBuf, JavaError> {
    if let Err(e) = unzip(zip_path, root) {
        tracing::warn!(
            "直接写入 {} 失败（{e}），弹 UAC 提权安装（用户拒绝则回退数据目录）",
            root.display()
        );
        elevate_expand_archive(zip_path, root, bus, task_id, step, cancel).await?;
    }
    find_java_in_root(root, required_major)
        .await
        .ok_or(JavaError::JavaBinaryNotFound)
}

/// 生成提权解压脚本（决议 D21）。独立纯函数便于单测。
/// 退出码约定：0 成功；3 解压后根下找不到 java.exe；4 解压异常。
fn expand_archive_script(zip_path: &Path, root: &Path) -> String {
    let zip_str = zip_path.display().to_string();
    let root_str = root.display().to_string();
    let lines = [
        "$ErrorActionPreference = 'Stop'".to_string(),
        format!("New-Item -ItemType Directory -Force -Path '{root_str}' | Out-Null"),
        "try {".to_string(),
        format!("  Expand-Archive -LiteralPath '{zip_str}' -DestinationPath '{root_str}' -Force"),
        format!(
            "  $java = Get-ChildItem -LiteralPath '{root_str}' -Recurse -Filter 'java.exe' | Select-Object -First 1"
        ),
        "  if ($null -eq $java) { exit 3 }".to_string(),
        "  exit 0".to_string(),
        "} catch {".to_string(),
        "  exit 4".to_string(),
        "}".to_string(),
    ];
    // UTF-8 with BOM：Windows PowerShell 5.1 对无 BOM 文件按 GBK 误读（§8.7 同款约束）
    format!("\u{feff}{}\n", lines.join("\n"))
}

/// 经 UAC 提权运行一段 PowerShell 脚本（决议 D21 公共件；D27 加固）。
/// 临时 .ps1（UTF-8 BOM，由调用方的脚本生成函数保证）→ 外层 powershell 以
/// `Start-Process -Verb RunAs` 拉起内层执行并透传退出码。返回内层退出码。
/// D27：等待外层进程整体受 120s 超时与取消令牌约束——UAC 弹窗未被确认
/// 时超时杀进程返回 [`JavaError::Install`]，用户取消时返回
/// [`JavaError::Cancelled`]，绝不永久挂起（v0.11.1 实测挂死根因）。
/// `tag` 用于临时脚本命名（jre-install / jre-move）。
async fn run_elevated_ps(
    script_content: &str,
    tag: &str,
    cancel: &CancellationToken,
) -> Result<i32, JavaError> {
    let script_path = std::env::temp_dir().join(format!("mcha-{tag}.ps1"));
    std::fs::write(&script_path, script_content)
        .map_err(|e| JavaError::Install(format!("写提权脚本失败：{e}")))?;
    let outer = format!(
        "$p = Start-Process powershell -Verb RunAs -Wait -PassThru \
         -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','{}'; exit $p.ExitCode",
        script_path.display()
    );
    let mut child = match tokio::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &outer,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_file(&script_path);
            return Err(JavaError::Install(format!(
                "无法启动 PowerShell 提权流程：{e}"
            )));
        }
    };

    /// select 三路竞争的结果；等待 future 在块内 drop，借用在块外结束，
    /// 之后才能终止子进程（D27）。
    enum Outcome {
        Code(i32),
        TimedOut,
        Cancelled,
    }
    let outcome = {
        let wait = child.wait();
        tokio::pin!(wait);
        tokio::select! {
            res = &mut wait => Outcome::Code(
                res.map_err(|e| JavaError::Install(format!("提权进程异常退出：{e}")))?
                    .code()
                    .ok_or_else(|| JavaError::Install("提权进程被信号终止".into()))?,
            ),
            _ = tokio::time::sleep(Duration::from_secs(ELEVATION_TIMEOUT_SECS)) => Outcome::TimedOut,
            _ = cancel.cancelled() => Outcome::Cancelled,
        }
    };
    match outcome {
        Outcome::Code(code) => {
            let _ = std::fs::remove_file(&script_path);
            Ok(code)
        }
        Outcome::TimedOut => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = std::fs::remove_file(&script_path);
            Err(JavaError::Install(format!(
                "提权等待超时（{ELEVATION_TIMEOUT_SECS} 秒）：UAC 弹窗未确认，已中止等待"
            )))
        }
        Outcome::Cancelled => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = std::fs::remove_file(&script_path);
            Err(JavaError::Cancelled)
        }
    }
}

/// 经 UAC 提权把 JRE zip 解压到安装根（决议 D21；D27：前置预告 + 超时/取消）。
async fn elevate_expand_archive(
    zip_path: &Path,
    root: &Path,
    bus: &EventBus,
    task_id: &str,
    step: &str,
    cancel: &CancellationToken,
) -> Result<(), JavaError> {
    bus.publish(ProgressEvent::StepProgress {
        task_id: task_id.into(),
        step: step.into(),
        current: 0,
        total: None,
        detail: Some(format!(
            "即将弹出 UAC 窗口请求管理员授权（写入 {}），请在弹窗中点“是”；{} 秒内未确认将自动中止",
            root.display(),
            ELEVATION_TIMEOUT_SECS
        )),
    });
    let code = run_elevated_ps(
        &expand_archive_script(zip_path, root),
        "jre-install",
        cancel,
    )
    .await?;
    if code == 0 {
        return Ok(());
    }
    Err(JavaError::Install(format!(
        "提权安装未完成（脚本退出码 {code}；UAC 被拒或脚本执行失败）"
    )))
}

/// 生成提权移动目录脚本（决议 D21 v0.11.1：旧受管 JRE 迁移到统一安装根）。
/// 独立纯函数便于单测。退出码约定：0 成功；3 移动后目标缺 java.exe；
/// 4 移动异常；5 目标已存在（不覆盖）。
fn move_dir_script(src: &Path, dst: &Path) -> String {
    let src_str = src.display().to_string();
    let dst_str = dst.display().to_string();
    let dst_parent = dst
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let lines = [
        "$ErrorActionPreference = 'Stop'".to_string(),
        format!("New-Item -ItemType Directory -Force -Path '{dst_parent}' | Out-Null"),
        "try {".to_string(),
        format!("  if (Test-Path -LiteralPath '{dst_str}') {{ exit 5 }}"),
        format!("  Move-Item -LiteralPath '{src_str}' -Destination '{dst_str}' -Force"),
        format!("  if (-not (Test-Path -LiteralPath '{dst_str}\\bin\\java.exe')) {{ exit 3 }}"),
        "  exit 0".to_string(),
        "} catch {".to_string(),
        "  exit 4".to_string(),
        "}".to_string(),
    ];
    // UTF-8 with BOM：Windows PowerShell 5.1 对无 BOM 文件按 GBK 误读（§8.7 同款约束）
    format!("\u{feff}{}\n", lines.join("\n"))
}

/// 决议 D21 v0.11.1：Windows 下把数据目录里的旧受管 JRE 一次性迁移到
/// 统一安装根 `C:\Program Files\Java\`。`Ok(Some(path))` = 迁移后的 java
/// 绝对路径；`Ok(None)` = 迁移未完成（UAC 被拒 / 超时 / 移动失败 / 目标已
/// 存在但扫描未命中），调用方回退继续用原路径并留痕（D27：降级不阻塞）；
/// `Err(Cancelled)` = 用户取消，必须向上中止任务。
async fn migrate_managed_to_program_files(
    java_path: &Path,
    required_major: u8,
    bus: &EventBus,
    task_id: &str,
    step: &str,
    cancel: &CancellationToken,
) -> Result<Option<PathBuf>, JavaError> {
    let root = program_files_java_root();
    if java_path.starts_with(&root) {
        return Ok(Some(java_path.to_path_buf())); // 已在统一安装根
    }
    // 源版本目录 = java 路径上两级（.../<版本>/bin/java → <版本>）
    let Some(src_dir) = java_path.parent().and_then(|bin| bin.parent()) else {
        return Ok(None);
    };
    let Some(dir_name) = src_dir.file_name().map(|s| s.to_string_lossy().to_string()) else {
        return Ok(None);
    };
    let target = root.join(&dir_name);

    // 直接移动（同卷且进程可写 Program Files 时成功；普通权限会失败 → 提权）
    if std::fs::rename(src_dir, &target).is_ok() {
        return Ok(find_java_in_root(&root, required_major).await);
    }
    bus.publish(ProgressEvent::StepProgress {
        task_id: task_id.into(),
        step: step.into(),
        current: 0,
        total: None,
        detail: Some(format!(
            "即将弹出 UAC 窗口请求管理员授权（把旧受管 JRE 迁移到 {}），请在弹窗中点“是”；{} 秒内未确认将自动中止",
            root.display(),
            ELEVATION_TIMEOUT_SECS
        )),
    });
    let code = match run_elevated_ps(&move_dir_script(src_dir, &target), "jre-move", cancel).await {
        Ok(code) => code,
        Err(JavaError::Cancelled) => return Err(JavaError::Cancelled),
        // D27：超时 / 启动失败等一律降级，不阻塞任务
        Err(e) => {
            tracing::warn!("提权迁移中止（降级用原位置）：{e}");
            return Ok(None);
        }
    };
    match code {
        // 移动成功，或目标已有同名版本（不覆盖、改用现成安装）：
        // 两种情况都以统一安装根的实际扫描结果为准
        0 | 5 => Ok(find_java_in_root(&root, required_major).await),
        1 => {
            tracing::info!("UAC 被拒绝，JRE 保持原位置");
            Ok(None)
        }
        code => {
            tracing::warn!("提权迁移失败（脚本退出码 {code}）");
            Ok(None)
        }
    }
}

/// 在解压目录里定位 java 可执行文件：官方压缩包均带顶层版本目录
/// （如 jdk-21.0.12.1+1-jre/bin/java），zip/tar.gz 两种形态都先查一层。
fn locate_java_binary(dest_dir: &Path) -> Option<PathBuf> {
    let direct = dest_dir.join(java_bin_relative());
    if direct.is_file() {
        return Some(direct);
    }
    let entries = std::fs::read_dir(dest_dir).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(java_bin_relative());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 纯 Rust 解压 tar.gz（Linux/macOS 的 JRE 分发形态）。
/// tar crate 的路径检查（strip 父引用）防止路径逃逸。
pub fn untar_gz(tgz_path: &Path, dest_dir: &Path) -> Result<(), JavaError> {
    let file = std::fs::File::open(tgz_path)
        .map_err(|e| JavaError::Unzip(format!("打开 {tgz_path:?}：{e}")))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    std::fs::create_dir_all(dest_dir).map_err(|e| JavaError::Unzip(format!("创建目录：{e}")))?;
    archive
        .unpack(dest_dir)
        .map_err(|e| JavaError::Unzip(format!("解压 tar.gz：{e}")))
}

/// 纯 Rust 解压 zip（zip-slip 防护：拒绝逃逸目标目录的条目名）。
pub fn unzip(zip_path: &Path, dest_dir: &Path) -> Result<(), JavaError> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| JavaError::Unzip(format!("打开 {zip_path:?}：{e}")))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| JavaError::Unzip(format!("读取 zip：{e}")))?;
    std::fs::create_dir_all(dest_dir).map_err(|e| JavaError::Unzip(format!("创建目录：{e}")))?;
    let dest_abs = dest_dir
        .canonicalize()
        .map_err(|e| JavaError::Unzip(format!("解析目录：{e}")))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| JavaError::Unzip(format!("遍历条目：{e}")))?;
        let Some(name) = entry.enclosed_name() else {
            // 含 ".." 或绝对路径的条目 → zip-slip，直接拒绝
            return Err(JavaError::Unzip(format!(
                "zip 条目名非法：{}",
                entry.name()
            )));
        };
        let out_path = dest_abs.join(name);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| JavaError::Unzip(format!("创建子目录：{e}")))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| JavaError::Unzip(format!("创建子目录：{e}")))?;
            }
            let mut out = std::fs::File::create(&out_path)
                .map_err(|e| JavaError::Unzip(format!("创建文件：{e}")))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|e| JavaError::Unzip(format!("解压写入：{e}")))?;
            // 保留可执行位（Unix 下 java/bin 需要执行权限）
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = entry.unix_mode() {
                    let _ =
                        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod integration {
    use super::*;
    use crate::knowledge::upstream::HttpBase;

    #[tokio::test]
    #[ignore = "真实下载受管 JRE（约 50MB 网络流量）：cargo test -- --ignored"]
    async fn 受管_java_供给全链路() {
        // 决议 D24：adoptium_mirror 默认即清华 TUNA 镜像（国内开箱即用），
        // 本用例同时验证默认镜像渠道与官方回退路径的同一 sha256 校验。
        let cfg = crate::config::AppConfig::default();
        let data = tempfile::tempdir().unwrap();
        let base = HttpBase::new(&cfg).unwrap();
        let bus = crate::events::EventBus::new();
        let cancel = CancellationToken::new();

        let runtime = resolve_java(21, &cfg, data.path(), &base, &bus, "t-java", cancel)
            .await
            .unwrap_or_else(|e| panic!("供给失败：{e}"));

        let java_path =
            managed_java_path(&runtime).unwrap_or_else(|| panic!("应得到可用 java 路径"));
        let output = tokio::process::Command::new(&java_path)
            .arg("-version")
            .output()
            .await
            .unwrap_or_else(|e| panic!("运行受管 java 失败：{e}"));
        let text = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            parse_java_major(&text),
            Some(21),
            "受管 JRE 应为 Java 21，实际：{text}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.10.1 勘误回归：双层嵌套（Adoptium zip 自带顶层目录）的受管 JRE
    /// 必须能被复用查找命中——此前只查一层，导致每个任务重复下载。
    #[test]
    fn 受管java复用_双层嵌套与直接形态() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("runtime").join("jdk-21");

        // 形态一：直接 <release>/bin/java（tar.gz 解压或修正后的落盘布局）
        let direct = root.join("a-release");
        std::fs::create_dir_all(direct.join("bin")).unwrap();
        std::fs::write(direct.join("bin").join("java"), b"").unwrap();
        let (path, name) = find_managed_java(tmp.path(), 21).unwrap();
        assert_eq!(name, "a-release");
        assert!(path.ends_with("a-release/bin/java"));

        // 形态二：双层嵌套 <release>/<release>-jre/bin/java（实测现场布局）
        std::fs::remove_dir_all(direct).unwrap();
        let nested = root.join("b-release").join("b-release-jre");
        std::fs::create_dir_all(nested.join("bin")).unwrap();
        std::fs::write(nested.join("bin").join("java"), b"").unwrap();
        let (path, name) = find_managed_java(tmp.path(), 21).unwrap();
        assert_eq!(name, "b-release");
        assert!(path.ends_with("b-release-jre/bin/java"));
    }

    /// 决议 D21：统一安装根扫描——根即版本目录与一级版本子目录两种形态
    /// 都要产出候选（主版本核实由 java -version 运行时完成，不入单测）。
    #[test]
    fn 安装根候选扫描_两种形态() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Java");
        let bin_rel = Path::new(java_bin_relative());

        // 形态一：根即版本目录（root/bin/java[.exe]）
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join(bin_rel), b"").unwrap();
        let cands = java_candidates_in_root(&root);
        assert_eq!(cands.len(), 1);

        // 形态二：一级版本子目录（root/<版本>/bin/java[.exe]）
        let sub = root.join("jdk-25.0.1+12-jre");
        std::fs::create_dir_all(sub.join("bin")).unwrap();
        std::fs::write(sub.join(bin_rel), b"").unwrap();
        let cands = java_candidates_in_root(&root);
        assert_eq!(cands.len(), 2);
        assert!(
            cands
                .iter()
                .any(|p| p.to_string_lossy().contains("jdk-25.0.1+12-jre"))
        );

        // 空目录 / 不存在目录不报错
        assert!(java_candidates_in_root(&tmp.path().join("absent")).is_empty());
    }

    /// 决议 D21：提权脚本必须是 UTF-8 with BOM（Windows PowerShell 5.1
    /// 对无 BOM 文件按 GBK 误读），且包含路径与退出码约定。
    #[test]
    fn 提权脚本内容_bom路径与退出码() {
        let script = expand_archive_script(
            Path::new(r"C:\Users\a\AppData\Local\Temp\mcha-jre\jre.zip"),
            Path::new(r"C:\Program Files\Java"),
        );
        assert!(script.starts_with('\u{feff}'), "必须带 UTF-8 BOM");
        assert!(script.contains(r"C:\Users\a\AppData\Local\Temp\mcha-jre\jre.zip"));
        assert!(script.contains(r"C:\Program Files\Java"));
        assert!(script.contains("Expand-Archive"));
        assert!(script.contains("exit 3"), "解压后无 java.exe 的失败码");
        assert!(script.contains("exit 4"), "解压异常的失败码");
    }

    /// 决议 D21 v0.11.1：迁移脚本——BOM、不覆盖目标（exit 5）、
    /// 移动后 java.exe 校验（exit 3）。
    #[test]
    fn 迁移脚本内容_不覆盖与校验() {
        let script = move_dir_script(
            Path::new(r"C:\Users\a\AppData\Roaming\mcha\runtime\jdk-25\jdk-25.0.4.1+1-jre"),
            Path::new(r"C:\Program Files\Java\jdk-25.0.4.1+1-jre"),
        );
        assert!(script.starts_with('\u{feff}'), "必须带 UTF-8 BOM");
        assert!(script.contains(r"Move-Item -LiteralPath 'C:\Users\a\AppData\Roaming\mcha\runtime\jdk-25\jdk-25.0.4.1+1-jre'"));
        assert!(script.contains(r"C:\Program Files\Java\jdk-25.0.4.1+1-jre"));
        assert!(script.contains("exit 5"), "目标已存在必须不覆盖");
        assert!(script.contains("bin\\java.exe"), "移动后须校验 java.exe");
    }

    #[test]
    fn 版本目录名反推() {
        assert_eq!(
            dir_name_of(Path::new("/x/jdk-25.0.1+12-jre/bin/java")),
            "jdk-25.0.1+12-jre"
        );
        assert_eq!(dir_name_of(Path::new("java")), "");
    }

    #[test]
    fn java版本解析() {
        assert_eq!(
            parse_java_major("openjdk version \"21.0.4\" 2024-07-16"),
            Some(21)
        );
        assert_eq!(parse_java_major("java version \"1.8.0_392\""), Some(8));
        assert_eq!(parse_java_major("openjdk version \"17.0.2\""), Some(17));
        assert_eq!(parse_java_major("没有版本信息"), None);
    }

    #[test]
    fn zip_slip防护() {
        // 构造一个含逃逸条目名的 zip：用 zip crate 写一个带 ".." 的归档
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("evil.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file("../evil.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut writer, b"pwn").unwrap();
            writer.finish().unwrap();
        }
        let dest = tempfile::tempdir().unwrap();
        let err = unzip(&zip_path, dest.path()).unwrap_err();
        assert!(err.to_string().contains("非法"), "应拒绝逃逸条目");
    }
}
