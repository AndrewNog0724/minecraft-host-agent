//! 终端超链接（setup 引导等用户直出文本用）：OSC 8 显式链接 + 纯文本回退。
//!
//! 两层"可点击"：
//! 1. **纯文本 URL**——现代终端（Windows Terminal / iTerm2 / VTE 系 / mintty
//!    等）会自动识别并渲染为可点击，是保底形态；
//! 2. **OSC 8 转义序列**（`\x1b]8;;URL\x07文本\x1b]8;;\x07`）——链接文本与
//!    目标解耦，兼容性更稳。不支持的终端（老 conhost 等）打印转义序列会出
//!    乱码，必须降级。
//!
//! 支持检测走"已知支持终端白名单，不确定即回退纯文本"的保守策略；
//! `MCHA_NO_HYPERLINK=1` 可强制关闭（与 `MCHA_ASCII` 同一降级约定，§8.6）。

/// 当前终端是否支持 OSC 8 超链接。
pub fn supports_hyperlinks() -> bool {
    supports_hyperlinks_env(&|key| std::env::var_os(key))
}

/// 检测实现（环境变量读取可注入，便于测试）。
fn supports_hyperlinks_env(get: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    if get("MCHA_NO_HYPERLINK").is_some() {
        return false;
    }
    // Windows Terminal（WT_SESSION）与 mintty（Git Bash 默认终端）
    if get("WT_SESSION").is_some() {
        return true;
    }
    if let Some(program) = get("TERM_PROGRAM").and_then(|v| v.into_string().ok()) {
        match program.as_str() {
            "iTerm.app" | "WezTerm" | "vscode" | "Hyper" | "ghostty" | "mintty" => return true,
            _ => {}
        }
    }
    if get("KITTY_WINDOW_ID").is_some() {
        return true;
    }
    // VTE 系（GNOME Terminal / Konsole 等）：版本号十进制文本，4000 = 16.0
    let vte_version = get("VTE_VERSION")
        .and_then(|v| v.into_string().ok())
        .and_then(|v| v.trim().parse::<u32>().ok());
    if vte_version.is_some_and(|version| version >= 4000) {
        return true;
    }
    false
}

/// 生成用户可点击的链接：支持 OSC 8 时包装（显示文本 = URL），否则纯文本。
pub fn clickable(url: &str) -> String {
    clickable_with(supports_hyperlinks(), url, url)
}

/// 指定显示文本与支持与否的全参数形式（测试与降级逻辑共用）。
fn clickable_with(supported: bool, url: &str, text: &str) -> String {
    if supported {
        format!("\x1b]8;;{url}\x07{text}\x1b]8;;\x07")
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.into())
    }

    #[test]
    fn known_terminals_are_detected() {
        assert!(supports_hyperlinks_env(&env_of(&[("WT_SESSION", "x")])));
        assert!(supports_hyperlinks_env(&env_of(&[(
            "TERM_PROGRAM",
            "iTerm.app"
        )])));
        assert!(supports_hyperlinks_env(&env_of(&[("VTE_VERSION", "6003")])));
        assert!(supports_hyperlinks_env(&env_of(&[(
            "KITTY_WINDOW_ID",
            "1"
        )])));
    }

    #[test]
    fn unknown_or_disabled_falls_back() {
        // 无任何已知标记（如老 conhost）→ 保守回退
        assert!(!supports_hyperlinks_env(&env_of(&[])));
        // 显式关闭优先于已知支持
        assert!(!supports_hyperlinks_env(&env_of(&[
            ("WT_SESSION", "x"),
            ("MCHA_NO_HYPERLINK", "1")
        ])));
        // 版本过低的 VTE 不算支持
        assert!(!supports_hyperlinks_env(&env_of(&[(
            "VTE_VERSION",
            "3902"
        )])));
    }

    #[test]
    fn clickable_wraps_when_supported_and_plain_otherwise() {
        let url = "https://portal.curseforge.com/";
        let wrapped = clickable_with(true, url, url);
        assert!(wrapped.starts_with("\x1b]8;;"));
        assert!(wrapped.ends_with("\x1b]8;;\x07"));
        assert!(wrapped.contains(url));
        // 回退形态必须是纯 URL（现代终端自动识别为可点击）
        assert_eq!(clickable_with(false, url, url), url);
    }
}
