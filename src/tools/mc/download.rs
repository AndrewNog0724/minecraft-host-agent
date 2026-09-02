//! 领域工具共用的"下载 + 强校验"助手（设计 §8.10/§12）。
//!
//! 约定：流式写盘 + 进度事件；官方哈希（sha1/sha256）强校验，不符即删文件
//! 失败回环；无官方哈希的渠道返回落地计算的 sha256 留痕。

use std::path::Path;
use std::time::{Duration, Instant};

use sha1::Digest as _;

use crate::events::Event;

use super::ToolCtx;

/// 期望哈希（官方提供时强校验）。
#[derive(Debug, Clone)]
pub(crate) enum ExpectedHash {
    Sha1(String),
    Sha256(String),
}

/// 下载结果：字节数 + sha256（落地计算留痕）+ sha1（需要时计算）。
pub(crate) struct DownloadResult {
    pub bytes: u64,
    pub sha256: String,
    pub sha1: Option<String>,
}

/// 下载 url 到 dest：进度事件（label 区分进度条）、取消检查、哈希校验。
/// 校验失败删除 dest 并返回 Err（失败回环）。
pub(crate) async fn download_verified(
    ctx: &ToolCtx,
    url: &str,
    dest: &Path,
    label: &str,
    expected: Option<ExpectedHash>,
) -> Result<DownloadResult, String> {
    let response = tokio::time::timeout(
        Duration::from_secs(60),
        crate::knowledge::upstream::send_get(&ctx.http, url),
    )
    .await
    .map_err(|_| format!("连接超时（60 秒；{url}）"))?
    .map_err(|err| format!("连接失败（{url}）：{err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}（{url}）", status.as_u16()));
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| format!("创建目录失败：{err}"))?;
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|err| format!("创建文件失败（{}）：{err}", dest.display()))?;

    let total = response.content_length();
    let mut hasher256 = sha2::Sha256::new();
    let mut hasher1 = expected
        .as_ref()
        .map(|kind| matches!(kind, ExpectedHash::Sha1(_)))
        .unwrap_or(false)
        .then(sha1::Sha1::new);
    let mut written: u64 = 0;
    let mut last_progress = Instant::now();
    let mut stream = std::pin::pin!(response.bytes_stream());

    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        if ctx.cancel.is_cancelled() {
            let _ = std::fs::remove_file(dest);
            return Err("已被用户取消".to_string());
        }
        let chunk = chunk.map_err(|err| format!("下载中断：{err}"))?;
        hasher256.update(&chunk);
        if let Some(hasher) = hasher1.as_mut() {
            hasher.update(&chunk);
        }
        file.write_all(&chunk)
            .await
            .map_err(|err| format!("写盘失败：{err}"))?;
        written += chunk.len() as u64;
        if last_progress.elapsed() >= Duration::from_millis(200) {
            last_progress = Instant::now();
            let _ = ctx.events.send(Event::Progress {
                label: label.to_string(),
                done: written,
                total: total.filter(|t| *t > 0),
            });
        }
    }
    file.flush()
        .await
        .map_err(|err| format!("刷盘失败：{err}"))?;

    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let sha256 = hex(&hasher256.finalize());
    let sha1 = hasher1.map(|h| hex(&h.finalize()));

    match &expected {
        Some(ExpectedHash::Sha256(want)) if *want != sha256 => {
            let _ = std::fs::remove_file(dest);
            return Err(format!(
                "sha256 校验失败：期望 {want}，实际 {sha256}（文件已删除）"
            ));
        }
        Some(ExpectedHash::Sha1(want)) if sha1.as_deref() != Some(want.as_str()) => {
            let _ = std::fs::remove_file(dest);
            let actual = sha1.clone().unwrap_or_default();
            return Err(format!(
                "sha1 校验失败：期望 {want}，实际 {actual}（文件已删除）"
            ));
        }
        _ => {}
    }
    Ok(DownloadResult {
        bytes: written,
        sha256,
        sha1,
    })
}
