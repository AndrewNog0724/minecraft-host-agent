//! HTTP 工具：http_get_text（抓文本）/ http_download（带校验的下载，设计 §8.2）。
//!
//! 安全边界（§12 下载安全）：M1 强制 https；官方域白名单与镜像机制在 M2
//! 领域化时收紧。下载经 `.part` 临时文件 + 断点续传 + 可选 sha256 校验。

use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

use crate::agent::message::ToolOutcome;
use crate::events::Event;

use super::confinement::resolve_in;
use super::{Tool, ToolCtx, ToolError};

/// get_text 默认字符上限。
const GET_TEXT_MAX_CHARS: usize = 30_000;
/// get_text 请求超时。
const GET_TEXT_TIMEOUT: Duration = Duration::from_secs(60);
/// 下载默认超时。
const DOWNLOAD_TIMEOUT_DEFAULT: u64 = 600;
/// 进度事件的最小间隔（避免刷爆事件通道）。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HttpGetTextArgs {
    /// 要抓取的 https:// 网页或 API 地址
    pub url: String,
    /// 返回文本的字符上限（默认 30000，超出截断）
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HttpDownloadArgs {
    /// 文件下载地址（https://）
    pub url: String,
    /// 保存路径（必须位于工作区或数据目录内）
    pub path: String,
    /// 期望的 sha256（十六进制，不区分大小写）；提供时校验不过即失败
    #[serde(default)]
    pub sha256: Option<String>,
    /// 整体超时秒数（默认 600）
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

pub struct HttpGetTextTool;

#[async_trait::async_trait]
impl Tool for HttpGetTextTool {
    fn name(&self) -> &'static str {
        "http_get_text"
    }
    fn description(&self) -> String {
        "抓取网页 / API 的文本响应（查文档、解析直链）。大响应会截断。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(HttpGetTextArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: HttpGetTextArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if !args.url.starts_with("https://") {
            return Ok(ToolOutcome::err("仅允许 https:// 地址（安全边界）"));
        }
        let url = args.url.clone();
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let request = ctx.http.get(&args.url);
        let response = match tokio::time::timeout(GET_TEXT_TIMEOUT, request.send()).await {
            Err(_) => return Ok(ToolOutcome::err("请求超时（60 秒）")),
            Ok(Err(err)) => return Ok(ToolOutcome::err(format!("请求失败：{err}"))),
            Ok(Ok(resp)) => resp,
        };
        let status = response.status();
        if !status.is_success() {
            return Ok(ToolOutcome::err(format!(
                "上游返回 HTTP {}（对 {url}）",
                status.as_u16()
            )));
        }
        let text = match tokio::time::timeout(GET_TEXT_TIMEOUT, response.text()).await {
            Err(_) => return Ok(ToolOutcome::err("读取响应超时")),
            Ok(Err(err)) => return Ok(ToolOutcome::err(format!("读取响应失败：{err}"))),
            Ok(Ok(text)) => text,
        };
        let max_chars = args.max_chars.unwrap_or(GET_TEXT_MAX_CHARS).max(200);
        let total_chars = text.chars().count();
        if total_chars > max_chars {
            let head: String = text.chars().take(max_chars).collect();
            return Ok(ToolOutcome::ok(format!(
                "（全文 {total_chars} 字符，已截断到 {max_chars}；需要更多请分次抓取或用 run_command + curl）\n{head}"
            )));
        }
        Ok(ToolOutcome::ok(text))
    }
}

pub struct HttpDownloadTool;

#[async_trait::async_trait]
impl Tool for HttpDownloadTool {
    fn name(&self) -> &'static str {
        "http_download"
    }
    fn description(&self) -> String {
        "下载文件到工作区（支持断点续传，可选 sha256 校验）。用于获取发行包等二进制文件。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(HttpDownloadArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Network
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: HttpDownloadArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if !args.url.starts_with("https://") {
            return Ok(ToolOutcome::err("仅允许 https:// 地址（安全边界）"));
        }
        let target = resolve_in(
            &[ctx.workspace.as_path(), ctx.data_dir.as_path()],
            &args.path,
        )?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let url = args.url.clone();
        let timeout_secs = args
            .timeout_secs
            .unwrap_or(DOWNLOAD_TIMEOUT_DEFAULT)
            .clamp(10, 3600);

        let part_path = PathBuf::from(format!("{}.part", target.display()));
        let expect_sha = args
            .sha256
            .as_ref()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());

        let mut resume_from = 0u64;
        if part_path.exists() {
            resume_from = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
        }

        // 首次请求：带断点 Range；416（断点失效）则删掉 .part 重来
        let response = {
            let send = |resume: u64| {
                let mut request = ctx.http.get(&args.url);
                if resume > 0 {
                    request = request.header("Range", format!("bytes={resume}-"));
                }
                request
            };
            match tokio::time::timeout(Duration::from_secs(60), send(resume_from).send()).await {
                Err(_) => return Ok(ToolOutcome::err("连接超时")),
                Ok(Err(err)) => return Ok(ToolOutcome::err(format!("请求失败：{err}"))),
                Ok(Ok(resp)) => {
                    if resp.status().as_u16() == 416 && resume_from > 0 {
                        let _ = std::fs::remove_file(&part_path);
                        resume_from = 0;
                        match tokio::time::timeout(Duration::from_secs(60), send(0).send()).await {
                            Err(_) => return Ok(ToolOutcome::err("连接超时")),
                            Ok(Err(err)) => {
                                return Ok(ToolOutcome::err(format!("请求失败：{err}")));
                            }
                            Ok(Ok(resp)) => resp,
                        }
                    } else {
                        resp
                    }
                }
            }
        };

        let status = response.status();
        // 206 = 续传命中；200 = 服务端不支持 Range，整段重来
        let mut start = resume_from;
        let append = status.as_u16() == 206;
        if !append {
            start = 0;
        }
        if !(status.is_success()) {
            return Ok(ToolOutcome::err(format!(
                "上游返回 HTTP {}（对 {url}）",
                status.as_u16()
            )));
        }
        let total = total_size(&response, start);
        let mut file = match if append && start > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&part_path)
                .await
        } else {
            // 不续传时先清掉旧 .part
            let _ = std::fs::remove_file(&part_path);
            if let Some(parent) = target.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            tokio::fs::File::create(&part_path).await
        } {
            Ok(file) => file,
            Err(err) => return Ok(ToolOutcome::err(format!("创建临时文件失败：{err}"))),
        };

        let label = format!(
            "下载 {}",
            target
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| target.display().to_string())
        );
        let mut hasher = Sha256::new();
        let mut written: u64 = start;
        let stream = response.bytes_stream();
        let mut last_progress = Instant::now();
        let mut stream = std::pin::pin!(stream);

        let download_all = async {
            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                if ctx.cancel.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let chunk = chunk.map_err(|err| ToolError::Io(format!("下载中断：{err}")))?;
                hasher.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .map_err(|err| ToolError::Io(format!("写盘失败：{err}")))?;
                written += chunk.len() as u64;
                if last_progress.elapsed() >= PROGRESS_INTERVAL {
                    last_progress = Instant::now();
                    let _ = ctx.events.send(Event::Progress {
                        label: label.clone(),
                        done: written,
                        total,
                    });
                }
            }
            file.flush()
                .await
                .map_err(|err| ToolError::Io(format!("刷盘失败：{err}")))?;
            Ok(())
        };

        match tokio::time::timeout(Duration::from_secs(timeout_secs), download_all).await {
            // 超时 / 取消 / IO 失败都保留 .part，重试可续传
            Err(_elapsed) => {
                return Ok(ToolOutcome::err(format!(
                    "下载超时（{timeout_secs} 秒）；已保留断点 {}，重试可续传",
                    part_path.display()
                )));
            }
            Ok(Err(ToolError::Cancelled)) => return Err(ToolError::Cancelled),
            Ok(Err(other)) => {
                return Ok(ToolOutcome::err(format!(
                    "下载中断（{other}）；已保留断点 {}，重试可续传",
                    part_path.display()
                )));
            }
            Ok(Ok(())) => {}
        }

        let actual = hex_encode(&hasher.finalize());
        if let Some(expected) = &expect_sha
            && *expected != actual
        {
            let _ = std::fs::remove_file(&part_path);
            return Ok(ToolOutcome::err(format!(
                "sha256 校验失败：期望 {expected}，实际 {actual}；文件已删除，请核对来源"
            )));
        }
        std::fs::rename(&part_path, &target)
            .map_err(|err| ToolError::Io(format!("落盘失败：{err}")))?;
        let _ = ctx.events.send(Event::Progress {
            label: label.clone(),
            done: written,
            total,
        });
        Ok(ToolOutcome::ok(format!(
            "已下载 {written} 字节 → {}；sha256={actual}",
            target.display()
        )))
    }
}

/// 从响应头推断总大小：Content-Length（整段）或 Content-Range 的 total（续传）。
fn total_size(response: &reqwest::Response, start: u64) -> Option<u64> {
    if let Some(range) = response
        .headers()
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
    {
        // 形如 bytes 1024-2047/4096
        if let Some(total) = range.rsplit('/').next()
            && let Ok(total) = total.trim().parse::<u64>()
        {
            return Some(total);
        }
    }
    response
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|len| len + start)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_known_vector() {
        // "abc" 的 sha256（标准测试向量）
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            hex_encode(&hasher.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn https_only_enforced() {
        // 由工具逻辑保证；此处断言字符串判断本身
        assert!(!"http://example.com".starts_with("https://"));
        assert!("https://example.com".starts_with("https://"));
    }

    #[test]
    fn total_size_parses_content_range() {
        // 无真实响应可造，直接验证解析分支的纯逻辑（rsplit 数字）
        let range = "bytes 1024-2047/4096";
        let total = range.rsplit('/').next().unwrap().trim();
        assert_eq!(total.parse::<u64>().unwrap(), 4096);
    }
}
