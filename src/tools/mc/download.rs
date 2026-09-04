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
    Sha512(String),
    /// SakuraFrp frpc 官方分发为 MD5（D135 事实基线）。
    Md5(String),
}

/// 下载结果：字节数 + sha256（落地计算留痕）+ sha1（需要时计算）。
/// sha512 校验在函数内完成，不外泄。
pub(crate) struct DownloadResult {
    pub bytes: u64,
    pub sha256: String,
    pub sha1: Option<String>,
}

/// 计算既有文件的 sha1（mod 安装冲突比对用：同名文件哈希一致则跳过）。
pub(crate) fn sha1_of_file(path: &Path) -> Result<String, String> {
    use sha1::Digest as _;
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)
        .map_err(|err| format!("读取既有文件失败（{}）：{err}", path.display()))?;
    let mut hasher = sha1::Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|err| format!("读取失败：{err}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 下载 url 到 dest：进度事件（label 区分进度条）、取消检查、哈希校验。
/// expected 可同时携带多种哈希（如 Modrinth 的 sha1 + sha512 双校验），
/// 全部通过才算成功；任一不符即删除 dest 并返回 Err（失败回环）。
pub(crate) async fn download_verified(
    ctx: &ToolCtx,
    url: &str,
    dest: &Path,
    label: &str,
    expected: &[ExpectedHash],
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
        .iter()
        .any(|k| matches!(k, ExpectedHash::Sha1(_)))
        .then(sha1::Sha1::new);
    let mut hasher512 = expected
        .iter()
        .any(|k| matches!(k, ExpectedHash::Sha512(_)))
        .then(sha2::Sha512::new);
    let mut hasher_md5 = expected
        .iter()
        .any(|k| matches!(k, ExpectedHash::Md5(_)))
        .then(md5::Md5::new);
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
        if let Some(hasher) = hasher512.as_mut() {
            hasher.update(&chunk);
        }
        if let Some(hasher) = hasher_md5.as_mut() {
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
    let sha512 = hasher512.map(|h| hex(&h.finalize()));
    let md5_hex = hasher_md5.map(|h| hex(&h.finalize()));

    for kind in expected {
        match kind {
            ExpectedHash::Sha256(want) if *want != sha256 => {
                let _ = std::fs::remove_file(dest);
                return Err(format!(
                    "sha256 校验失败：期望 {want}，实际 {sha256}（文件已删除）"
                ));
            }
            ExpectedHash::Sha1(want) if sha1.as_deref() != Some(want.as_str()) => {
                let _ = std::fs::remove_file(dest);
                let actual = sha1.clone().unwrap_or_default();
                return Err(format!(
                    "sha1 校验失败：期望 {want}，实际 {actual}（文件已删除）"
                ));
            }
            ExpectedHash::Sha512(want) if sha512.as_deref() != Some(want.as_str()) => {
                let _ = std::fs::remove_file(dest);
                let actual = sha512.clone().unwrap_or_default();
                return Err(format!(
                    "sha512 校验失败：期望 {want}，实际 {actual}（文件已删除）"
                ));
            }
            ExpectedHash::Md5(want) if md5_hex.as_deref() != Some(want.as_str()) => {
                let _ = std::fs::remove_file(dest);
                let actual = md5_hex.clone().unwrap_or_default();
                return Err(format!(
                    "MD5 校验失败：期望 {want}，实际 {actual}（文件已删除）"
                ));
            }
            _ => {}
        }
    }
    Ok(DownloadResult {
        bytes: written,
        sha256,
        sha1,
    })
}
