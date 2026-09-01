//! 数据目录与主目录定位（决议 D4/D15）。
//!
//! 数据目录 `~/.mcha/`（Windows：`%APPDATA%\mcha\`），可用环境变量 `MCHA_DATA` 覆盖。
//! 刻意不引入 dirs 等第三方库：定位规则只有两条，手写更利于答辩解释。

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;

/// 主目录：Unix 取 `$HOME`，Windows 取 `%USERPROFILE%`。
pub fn home_dir() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(profile));
    }
    anyhow::bail!("无法定位主目录（HOME / USERPROFILE 均未设置）")
}

/// 数据目录：`$MCHA_DATA` > `~/.mcha/`（Windows：`%APPDATA%\mcha\`）。
pub fn data_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MCHA_DATA").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(appdata).join("mcha"));
        }
    }
    Ok(home_dir()?.join(".mcha"))
}

/// 工作区目录：`$MCHA_WORKSPACE` > 当前目录。文件类工具的路径收敛基准之一。
pub fn workspace_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MCHA_WORKSPACE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    std::env::current_dir().context("无法确定当前目录作为工作区")
}

/// 全局共享的数据目录（进程启动时解析一次）。
pub fn shared_data_dir() -> anyhow::Result<&'static PathBuf> {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    match DIR.get() {
        Some(dir) => Ok(dir),
        None => {
            let dir = data_dir()?;
            let _ = DIR.set(dir);
            Ok(DIR.get().expect("刚刚写入"))
        }
    }
}
