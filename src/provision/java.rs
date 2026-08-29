//! Java 自动供给（FR-02，§8.8，决议 D2：全自动受管安装，不降级）。
//!
//! 选择顺序：① 系统 PATH 已有匹配版本 → 用系统的；
//! ② 受管目录已有 → 复用；③ Adoptium 官方渠道下载 zip 免安装包。
//! 只写受管目录 `<数据目录>/runtime/jdk-<major>/`，绝不碰系统位置。

use std::path::{Path, PathBuf};

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
    #[error("JRE 解压后找不到 java 可执行文件")]
    JavaBinaryNotFound,
}

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

/// 在受管目录里找可复用的 java：返回 (java 可执行绝对路径, 版本目录名)。
fn find_managed_java(data_dir: &Path, major: u8) -> Option<(PathBuf, String)> {
    let root = managed_root(data_dir, major);
    let entries = std::fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(java_bin_relative());
        if candidate.is_file() {
            let dir_name = entry.file_name().to_string_lossy().to_string();
            return Some((candidate, dir_name));
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
    bus.publish(ProgressEvent::StepStarted {
        task_id: task_id.into(),
        step: step.into(),
        title: format!("探测系统 Java（需要 Java {required_major}）"),
    });
    match probe_system_java().await {
        Ok(Some((path, version, major))) if major == required_major => {
            let runtime = JavaRuntime::System {
                path,
                version: version.clone(),
            };
            bus.publish(ProgressEvent::StepFinished {
                task_id: task_id.into(),
                step: step.into(),
                ok: true,
                detail: Some(format!("使用系统 Java {version}")),
            });
            return Ok(runtime);
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

    // ② 受管目录复用
    if let Some((path, dir_name)) = find_managed_java(data_dir, required_major) {
        bus.publish(ProgressEvent::StepFinished {
            task_id: task_id.into(),
            step: step.into(),
            ok: true,
            detail: Some(format!("复用受管 JRE：{}", dir_name)),
        });
        return Ok(JavaRuntime::Managed {
            path: path.to_string_lossy().to_string(),
            vendor: VENDOR.into(),
            version: dir_name,
        });
    }

    // ③ Adoptium 下载安装（zip 免安装包，sha256 强制校验）
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

    let staging = std::env::temp_dir().join("mc-host-agent-jre");
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
        detail: Some("校验通过，解压到受管目录".into()),
    });
    let dest_root = managed_root(data_dir, required_major);
    let dest_dir = dest_root.join(&asset.release_name);
    // Windows 分发 zip 包；Linux/macOS 分发 tar.gz（Adoptium 官方打包形态）
    if asset.file_name.ends_with(".zip") {
        unzip(&zip_path, &dest_dir)?;
    } else {
        untar_gz(&zip_path, &dest_dir)?;
    }
    let _ = tokio::fs::remove_file(&zip_path).await; // 清理临时 zip，失败不影响结果

    let java_path = locate_java_binary(&dest_dir).ok_or(JavaError::JavaBinaryNotFound)?;
    bus.publish(ProgressEvent::StepFinished {
        task_id: task_id.into(),
        step: step.into(),
        ok: true,
        detail: Some(format!("已安装受管 JRE：{}", java_path.display())),
    });
    Ok(JavaRuntime::Managed {
        path: java_path.to_string_lossy().to_string(),
        vendor: VENDOR.into(),
        version: asset.release_name.clone(),
    })
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
        // 模拟国内网络：配置清华 TUNA 镜像（官方 GitHub 渠道不可达时的默认解法）
        let mut cfg = crate::config::AppConfig::default();
        cfg.network.adoptium_mirror = "https://mirrors.tuna.tsinghua.edu.cn/Adoptium".to_string();
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
