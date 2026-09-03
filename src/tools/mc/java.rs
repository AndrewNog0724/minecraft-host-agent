//! Java 探测与供给（决议 D117，设计 §8.7/§8.10）。
//!
//! `check_java`（ReadOnly）：PATH / JAVA_HOME / 受管目录三处探测，`java
//! -version` 解析 major；`ensure_java`（Network，S3）：受管自动安装。
//! 供给选择顺序：① 已有匹配版本（PATH）→ ② 受管目录 → ③ 下载安装。

use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::message::ToolOutcome;

use super::download::{ExpectedHash, download_verified};
use super::{Tool, ToolCtx, ToolError};

/// 受管运行时根目录：`<数据目录>/runtime/`（设计 §8.7）。
pub(crate) fn managed_root(data_dir: &Path) -> PathBuf {
    data_dir.join("runtime")
}

/// 平台对应的 java 可执行文件：`<dir>/bin/java[.exe]`。
pub(crate) fn java_exe_in(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        dir.join("bin").join("java.exe")
    } else {
        dir.join("bin").join("java")
    }
}

/// 一处 Java 安装发现。
#[derive(Debug, Clone)]
pub(crate) struct JavaInstall {
    /// java 可执行文件绝对路径。
    pub exe: PathBuf,
    pub major: u32,
    /// 发现来源说明（PATH / JAVA_HOME / 受管目录）。
    pub source: String,
}

/// 从 `java -version` 输出解析主版本号：`21.0.4` → 21；`1.8.0_392` → 8。
pub(crate) fn parse_java_version_output(text: &str) -> Option<u32> {
    for line in text.lines() {
        let Some(open) = line.find('"') else { continue };
        let rest = &line[open + 1..];
        let Some(close) = rest.find('"') else {
            continue;
        };
        let version = &rest[..close];
        let segments: Vec<&str> = version.split('.').collect();
        // 旧格式 1.8.0_x → 8；新格式 21.0.4 / 17 → 首段
        let major_str = if segments.len() >= 2 && segments[0] == "1" {
            segments[1]
        } else {
            segments[0]
        };
        if let Ok(major) = major_str.parse::<u32>() {
            return Some(major);
        }
    }
    None
}

/// 运行 `<java> -version`（输出在 stderr）解析主版本号。
pub(crate) async fn java_major_of(exe: &Path) -> Result<u32, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(exe).arg("-version").output(),
    )
    .await
    .map_err(|_| format!("执行 {exe:?} -version 超时"))?
    .map_err(|err| format!("执行 {exe:?} 失败：{err}"))?;
    if !output.status.success() {
        return Err(format!("{exe:?} -version 退出码非零"));
    }
    let text = String::from_utf8_lossy(&output.stderr);
    parse_java_version_output(&text)
        .ok_or_else(|| format!("无法从 {exe:?} 的版本输出中解析主版本号：{text:.120}"))
}

/// 去重追加（同 exe 且同 major 视为重复）。
fn push_install(install: JavaInstall, found: &mut Vec<JavaInstall>) {
    if !found
        .iter()
        .any(|f| f.major == install.major && f.exe == install.exe)
    {
        found.push(install);
    }
}

/// 三处探测：PATH 直接执行 + `where/which` 全量、JAVA_HOME、受管目录。
pub(crate) async fn scan_installs(ctx: &ToolCtx) -> Vec<JavaInstall> {
    let mut found: Vec<JavaInstall> = Vec::new();

    // ① PATH：直接执行 + 平台命令列出全部命中
    if let Ok(major) = java_major_of(Path::new("java")).await {
        push_install(
            JavaInstall {
                exe: PathBuf::from("java"),
                major,
                source: "PATH".to_string(),
            },
            &mut found,
        );
    }
    let list_cmd = if cfg!(windows) {
        "where.exe java"
    } else {
        "which -a java"
    };
    if let Ok(output) = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
        .args(if cfg!(windows) {
            vec!["/C", list_cmd]
        } else {
            vec!["-c", list_cmd]
        })
        .output()
        .await
    {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let path = Path::new(line.trim());
            if path.is_file()
                && let Ok(major) = java_major_of(path).await
            {
                push_install(
                    JavaInstall {
                        exe: path.to_path_buf(),
                        major,
                        source: "PATH".to_string(),
                    },
                    &mut found,
                );
            }
        }
    }

    // ② JAVA_HOME
    if let Some(home) = std::env::var_os("JAVA_HOME").map(PathBuf::from) {
        let exe = java_exe_in(&home);
        if let Ok(major) = java_major_of(&exe).await {
            push_install(
                JavaInstall {
                    exe,
                    major,
                    source: "JAVA_HOME".to_string(),
                },
                &mut found,
            );
        }
    }

    // ③ 受管目录：`<数据目录>/runtime/jdk-<major>/<版本>/bin/java`
    let root = managed_root(&ctx.data_dir);
    if let Ok(entries) = std::fs::read_dir(&root) {
        for major_dir in entries.flatten() {
            let Ok(versions) = std::fs::read_dir(major_dir.path()) else {
                continue;
            };
            for version_dir in versions.flatten() {
                let exe = java_exe_in(&version_dir.path());
                if exe.is_file()
                    && let Ok(major) = java_major_of(&exe).await
                {
                    push_install(
                        JavaInstall {
                            exe,
                            major,
                            source: format!("受管目录 {}", version_dir.path().display()),
                        },
                        &mut found,
                    );
                }
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// check_java 工具
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckJavaArgs {
    /// 需要匹配的 Java 大版本（如 21）；缺省只列出全部发现
    #[serde(default)]
    pub required_major: Option<u32>,
}

pub struct CheckJavaTool;

#[async_trait::async_trait]
impl Tool for CheckJavaTool {
    fn name(&self) -> &'static str {
        "check_java"
    }
    fn description(&self) -> String {
        "探测本机 Java 环境（PATH / JAVA_HOME / mcha 受管目录），返回各安装的位置与主版本号；可传入 required_major 判断是否已有匹配版本。只读免确认。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(CheckJavaArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: CheckJavaArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let installs = scan_installs(ctx).await;
        if installs.is_empty() {
            return Ok(ToolOutcome::err(
                "未发现任何 Java 安装（PATH / JAVA_HOME / 受管目录均无）。\
                 需要时调用 ensure_java 自动安装匹配版本。"
                    .to_string(),
            ));
        }
        let mut lines = vec!["Java 环境探测结果：".to_string()];
        let mut matched = false;
        for install in &installs {
            let mark = match args.required_major {
                Some(required) if install.major == required => {
                    matched = true;
                    "✓"
                }
                Some(_) => "－",
                None => "·",
            };
            lines.push(format!(
                "{mark} Java {}（major {}）→ {}（{}）",
                install.major,
                install.major,
                install.exe.display(),
                install.source
            ));
        }
        match args.required_major {
            Some(required) if matched => lines.push(format!(
                "结论：已有 Java {required} 可用，起服可直接使用（无需下载）。"
            )),
            Some(required) => lines.push(format!(
                "结论：无 Java {required}；调用 ensure_java(major={required}) 受管安装。"
            )),
            None => lines.push("结论：传入 required_major 可判断是否已有匹配版本。".to_string()),
        }
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// ensure_java 工具（受管自动安装，决议 D2/D115，设计 §8.7）
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnsureJavaArgs {
    /// 需要的 Java 大版本（如 21）
    pub major: u32,
}

pub struct EnsureJavaTool;

#[async_trait::async_trait]
impl Tool for EnsureJavaTool {
    fn name(&self) -> &'static str {
        "ensure_java"
    }
    fn description(&self) -> String {
        "确保本机有指定大版本的 Java：已有匹配版本（PATH / 受管目录）直接复用；否则从 Adoptium 下载 Temurin JRE 免安装包（清华 TUNA 镜像优先，sha256 强校验）并解压到 mcha 受管目录，返回 java 可执行文件绝对路径。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(EnsureJavaArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Network
    }
    fn confirm_summary(&self, args: &serde_json::Value) -> Vec<String> {
        let major = args.get("major").and_then(|v| v.as_u64()).unwrap_or(0);
        vec![
            format!(
                "目标：Adoptium Temurin JRE（Java {major}），下载到 mcha 受管目录 <数据目录>/runtime/jdk-{major}/（sha256 强校验）"
            ),
            "本机已有匹配版本（PATH / 受管目录）时会自动复用并跳过下载".to_string(),
        ]
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: EnsureJavaArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }

        // ① 已有匹配版本 → 复用（尊重已有环境，重复开服零下载）
        if let Some(existing) = scan_installs(ctx)
            .await
            .into_iter()
            .find(|i| i.major == args.major)
        {
            return Ok(ToolOutcome::ok(format!(
                "已有 Java {}（{}）→ {}；跳过下载，起服直接使用该路径。",
                existing.major,
                existing.source,
                existing.exe.display()
            )));
        }

        // ② 解析最新 Temurin JRE
        let client = crate::knowledge::upstream::adoptium::AdoptiumClient::new(&ctx.http);
        let resolved = match client.latest_jre(args.major).await {
            Ok(r) => r,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };

        let jdk_root = managed_root(&ctx.data_dir).join(format!("jdk-{}", args.major));
        let final_dir = jdk_root.join(&resolved.release_name);
        if java_exe_in(&final_dir).is_file() {
            // 受管目录已有但探测未命中（罕见，如权限问题）：如实报告
            return Ok(ToolOutcome::ok(format!(
                "受管目录已存在 {} → {}；跳过下载。",
                resolved.release_name,
                java_exe_in(&final_dir).display()
            )));
        }

        // ③ 下载（镜像优先，失败回退官方；每源最多尝试 2 轮，网络抖动韧性）
        let use_mirror = !matches!(ctx.network.adoptium_mirror.as_str(), "off" | "");
        let candidates: Vec<String> = if use_mirror {
            vec![
                resolved
                    .mirror_url
                    .clone()
                    .unwrap_or_else(|| resolved.official_url.clone()),
                resolved.official_url.clone(),
            ]
        } else {
            vec![resolved.official_url.clone()]
        };
        let part_path = jdk_root.join(format!(".{}.part", resolved.file_name));
        let label = format!("下载 Temurin JRE {}", args.major);
        let mut errors: Vec<String> = Vec::new();
        let mut download_ok = false;
        let mut download_note = String::new();
        'rounds: for _round in 0..2 {
            for (attempt, url) in candidates.iter().enumerate() {
                if ctx.cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let expected = ExpectedHash::Sha256(resolved.sha256.clone());
                match download_verified(ctx, url, &part_path, &label, Some(expected)).await {
                    Ok(_) => {
                        download_ok = true;
                        download_note = if attempt == 0 && use_mirror {
                            "清华 TUNA 镜像".to_string()
                        } else {
                            "官方渠道（镜像不可达已回退）".to_string()
                        };
                        break 'rounds;
                    }
                    Err(reason) => {
                        let _ = std::fs::remove_file(&part_path);
                        errors.push(format!("{url}：{reason}"));
                    }
                }
            }
        }
        if !download_ok {
            let mut detail = errors.last().cloned().unwrap_or_default();
            if errors.len() > 1 {
                detail = format!("{detail}（全部尝试：{}）", errors.join(" | "));
            }
            return Ok(ToolOutcome::err(format!(
                "Java {major} 下载失败；最后错误：{detail}",
                major = args.major
            )));
        }

        // ④ 解压 → 落位 → 校验
        let staging = jdk_root.join(format!(".staging-{}", resolved.release_name));
        if let Err(reason) = tokio::task::spawn_blocking({
            let part = part_path.clone();
            let staging = staging.clone();
            let is_zip = resolved.file_name.ends_with(".zip");
            move || extract_archive(&part, &staging, is_zip)
        })
        .await
        .map_err(|err| ToolError::Io(format!("解压任务失败：{err}")))?
        {
            let _ = std::fs::remove_dir_all(&staging);
            let _ = std::fs::remove_file(&part_path);
            return Ok(ToolOutcome::err(format!("JRE 解压失败：{reason}")));
        }
        let _ = std::fs::remove_file(&part_path);

        let home = match find_java_home(&staging) {
            Some(dir) => dir,
            None => {
                let _ = std::fs::remove_dir_all(&staging);
                return Ok(ToolOutcome::err(
                    "解压完成但未找到 bin/java 目录结构（包内容异常）".to_string(),
                ));
            }
        };
        if let Err(err) = if home != staging {
            std::fs::rename(&home, &final_dir)
        } else {
            std::fs::rename(&staging, &final_dir)
        } {
            let _ = std::fs::remove_dir_all(&staging);
            return Ok(ToolOutcome::err(format!(
                "安装目录落位失败：{err}（{} → {}）",
                home.display(),
                final_dir.display()
            )));
        }
        let _ = std::fs::remove_dir_all(&staging);

        let exe = java_exe_in(&final_dir);
        match java_major_of(&exe).await {
            Ok(actual) if actual == args.major => Ok(ToolOutcome::ok(format!(
                "已安装 Java {actual}（Temurin JRE，{}）→ {}；起服一律使用该路径。",
                download_note,
                exe.display()
            ))),
            Ok(actual) => Ok(ToolOutcome::err(format!(
                "安装后校验不符：期望 Java {}，实际 {}（{}）",
                args.major,
                actual,
                exe.display()
            ))),
            Err(reason) => Ok(ToolOutcome::err(format!("安装后校验失败：{reason}"))),
        }
    }
}

/// 解压 JRE 免安装包到目标目录（Windows zip / Unix tar.gz）。
/// 解压库自带 zip-slip / 路径穿越防护（zip::ZipArchive::extract、tar::unpack）。
fn extract_archive(archive_path: &Path, dest: &Path, is_zip: bool) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|err| format!("创建目录失败：{err}"))?;
    if is_zip {
        let file =
            std::fs::File::open(archive_path).map_err(|err| format!("打开 zip 失败：{err}"))?;
        let mut archive =
            zip::ZipArchive::new(file).map_err(|err| format!("读取 zip 失败：{err}"))?;
        archive
            .extract(dest)
            .map_err(|err| format!("解压 zip 失败：{err}"))
    } else {
        let file =
            std::fs::File::open(archive_path).map_err(|err| format!("打开 tar.gz 失败：{err}"))?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive
            .unpack(dest)
            .map_err(|err| format!("解压 tar.gz 失败：{err}"))
    }
}

/// 在解压结果中定位包含 bin/java 的目录（Adoptium 包带单层根目录）。
fn find_java_home(staging: &Path) -> Option<PathBuf> {
    if java_exe_in(staging).is_file() {
        return Some(staging.to_path_buf());
    }
    let entries = std::fs::read_dir(staging).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path();
        if candidate.is_dir() && java_exe_in(&candidate).is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_and_legacy_version_outputs() {
        let modern =
            "openjdk version \"21.0.4\" 2024-07-16\nOpenJDK Runtime Environment Temurin-21.0.4+7";
        assert_eq!(parse_java_version_output(modern), Some(21));
        let legacy = "openjdk version \"1.8.0_392\" 2023-10-17";
        assert_eq!(parse_java_version_output(legacy), Some(8));
        let seventeen = "openjdk version \"17.0.2\" 2022-01-18";
        assert_eq!(parse_java_version_output(seventeen), Some(17));
        assert_eq!(parse_java_version_output("nothing here"), None);
        assert_eq!(parse_java_version_output(""), None);
    }

    #[test]
    fn ensure_confirmation_lines_describe_install() {
        let lines = EnsureJavaTool.confirm_summary(&serde_json::json!({ "major": 21 }));
        assert!(lines.iter().all(|l| !l.trim().is_empty()), "{lines:?}");
        let joined = lines.join("\n");
        assert!(joined.contains("Java 21"), "{joined}");
        assert!(joined.contains("runtime/jdk-21"), "{joined}");
        assert!(joined.contains("复用"), "{joined}");
    }

    #[test]
    fn managed_root_and_exe_layout() {
        let dir = Path::new("/data");
        assert_eq!(managed_root(dir), PathBuf::from("/data/runtime"));
        let exe = java_exe_in(&PathBuf::from("/data/runtime/jdk-21/temurin"));
        assert!(exe.ends_with(if cfg!(windows) { "java.exe" } else { "java" }));
        assert!(exe.to_string_lossy().contains("bin"));
    }

    fn fake_java_file(home: &Path) -> PathBuf {
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = if cfg!(windows) {
            bin.join("java.exe")
        } else {
            bin.join("java")
        };
        std::fs::write(&exe, b"fake").unwrap();
        exe
    }

    /// 平台对应的 java 文件名（夹具与 find_java_home 保持同一约定）。
    fn java_file_name() -> &'static str {
        if cfg!(windows) { "java.exe" } else { "java" }
    }

    #[test]
    fn extracts_zip_with_root_dir_and_finds_home() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("jre.zip");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
            let root = "jdk-21.0.4+7/";
            zip.add_directory(format!("{root}bin"), options).unwrap();
            zip.start_file(format!("{root}bin/{}", java_file_name()), options)
                .unwrap();
            zip.write_all(b"fake").unwrap();
            zip.finish().unwrap();
        }
        let dest = dir.path().join("out");
        extract_archive(&archive, &dest, true).unwrap();
        let home = find_java_home(&dest).expect("应定位到 home");
        assert!(home.join("bin").join(java_file_name()).is_file());
    }

    #[test]
    fn extracts_targz_and_finds_home() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("jre.tar.gz");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut tar = tar::Builder::new(encoder);
            let content: &[u8] = b"fake";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("jdk-21.0.4+7/bin/{}", java_file_name()),
                content,
            )
            .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let dest = dir.path().join("out");
        extract_archive(&archive, &dest, false).unwrap();
        let home = find_java_home(&dest).expect("应定位到 home");
        assert!(home.join("bin").join(java_file_name()).is_file());
    }

    #[test]
    fn find_java_home_accepts_flat_layout() {
        let dir = tempfile::tempdir().unwrap();
        fake_java_file(dir.path());
        assert_eq!(find_java_home(dir.path()), Some(dir.path().to_path_buf()));
        assert_eq!(find_java_home(&dir.path().join("nope")), None);
    }

    #[tokio::test]
    async fn ensure_java_reports_missing_major_shape() {
        // 只验证错误路径不 panic：不实际下载（离线可跑）
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
            network: Default::default(),
            retrieval: Default::default(),
        };
        let outcome = EnsureJavaTool
            .run(serde_json::json!({ "major": 999 }), &ctx)
            .await
            .unwrap();
        // 无 999 版本：应返回结构化错误（失败也回传，NFR-3），而不是 panic
        assert!(!outcome.is_ok(), "Java 999 不应存在：{outcome:?}");
    }

    #[tokio::test]
    #[ignore = "真实下载冒烟（约 50MB JRE）：cargo test --ignored"]
    async fn live_ensure_java_installs_into_managed_dir() {
        let (tx, _rx) = crate::events::event_channel();
        let root = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            workspace: root.path().to_path_buf(),
            data_dir: root.path().join("data"),
            http: reqwest::Client::builder()
                .user_agent("mcha/0.2")
                .build()
                .unwrap(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
        };
        let outcome = EnsureJavaTool
            .run(serde_json::json!({ "major": 21 }), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("应安装成功：{outcome:?}");
        };
        assert!(
            content.contains("bin/java") || content.contains("bin\\java"),
            "结果应含 java 路径：{content}"
        );
    }

    #[tokio::test]
    async fn scan_finds_nothing_in_empty_dir() {
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
            network: Default::default(),
            retrieval: Default::default(),
        };
        let installs = scan_installs(&ctx).await;
        // 环境里可能有真实 Java（PATH），但受管目录必为空：只断言不 panic 且元素有 source
        for install in &installs {
            assert!(!install.source.is_empty());
        }
    }
}
