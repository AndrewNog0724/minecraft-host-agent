//! 导出打码（NFR-2）：会话导出时自动遮蔽 API 密钥与公网 IPv4 地址。
//!
//! 不引入 regex 依赖，逐字符扫描实现（规则简单、答辩可解释）：
//! - `sk-` 开头的密钥样式 token → `sk-****（已打码）`
//! - 四段 0–255 的 IPv4 → 前三段保留、末段打码（`203.0.113.***`）
//! - 调用方还可传入已知敏感串（如当前配置的 API Key 原文）做精确替换

/// 打码一段文本。
pub fn mask_sensitive(text: &str, extra_secrets: &[String]) -> String {
    let mut out = mask_keys_and_ips(text);
    for secret in extra_secrets {
        if secret.len() >= 8 && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "****（已打码）");
        }
    }
    out
}

fn mask_keys_and_ips(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        // 密钥样式：sk- 后跟至少 8 个 [A-Za-z0-9_-]
        if bytes[i..].starts_with(b"sk-") {
            let mut end = i + 3;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'-')
            {
                end += 1;
            }
            if end - i >= 11 {
                out.push_str("sk-****（已打码）");
                i = end;
                continue;
            }
        }
        // IPv4：digit(1-3).digit(1-3).digit(1-3).digit(1-3)，各段 ≤ 255
        if bytes[i].is_ascii_digit()
            && let Some(len) = match_ipv4(&bytes[i..])
        {
            let octets: Vec<u8> = text[i..i + len]
                .split('.')
                // turbofish 显式标注：Windows 依赖树中的 encode_unicode 为
                // Vec<u8> 提供了额外的 FromIterator 实现，会让推断歧义（E0283）
                .filter_map(|s| s.parse::<u8>().ok())
                .collect();
            if octets.len() == 4 {
                // 私有地址（127.* / 10.* / 192.168.* / 172.16-31.*）不打码，
                // 它们是排障信息的关键部分；公网地址遮蔽末段
                let public = !is_private_octets(&octets);
                if public {
                    out.push_str(&format!("{}.*.*.***", octets[0]));
                } else {
                    out.push_str(&text[i..i + len]);
                }
                i += len;
                continue;
            }
        }
        // 非 ASCII 字符按字节逐个复制（UTF-8 序列不会与 ASCII 规则冲突）
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn is_private_octets(o: &[u8]) -> bool {
    match o[0] {
        10 | 127 => true,
        169 => o[1] == 254,
        172 => (16..=31).contains(&o[1]),
        192 => o[1] == 168,
        _ => false,
    }
}

/// 返回匹配的 IPv4 字符串长度；不匹配返回 None。
fn match_ipv4(bytes: &[u8]) -> Option<usize> {
    let mut pos = 0;
    let mut octets = Vec::with_capacity(4);
    for part in 0..4 {
        let start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() && pos - start < 3 {
            pos += 1;
        }
        if pos == start {
            return None;
        }
        let value: u32 = std::str::from_utf8(&bytes[start..pos]).ok()?.parse().ok()?;
        if value > 255 {
            return None;
        }
        octets.push(value);
        if part < 3 {
            if pos < bytes.len() && bytes[pos] == b'.' {
                pos += 1;
            } else {
                return None;
            }
        }
    }
    // 后面紧跟字母/数字/点说明不是独立 IP（如版本号 1.2.3.4.5）
    if pos < bytes.len() && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'.') {
        return None;
    }
    Some(pos)
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_public_ipv4_but_keeps_private() {
        let out = mask_sensitive("服务器在 203.0.113.7 上", &[]);
        assert_eq!(out, "服务器在 203.*.*.*** 上");
        assert!(out.contains("203."));
        assert!(!out.contains("203.0.113.7"));

        let private = mask_sensitive("本机 127.0.0.1 与 192.168.1.5", &[]);
        assert_eq!(private, "本机 127.0.0.1 与 192.168.1.5");
    }

    #[test]
    fn masks_sk_keys() {
        let out = mask_sensitive("用 key sk-abcdefgh12345678 调用", &[]);
        assert!(!out.contains("abcdefgh12345678"));
        assert!(out.contains("sk-****"));

        // 过短的 sk- 不算密钥（如 "task-1"）
        let keep = mask_sensitive("task-1 项目", &[]);
        assert_eq!(keep, "task-1 项目");
    }

    #[test]
    fn masks_exact_secret() {
        let out = mask_sensitive(
            "key 是 supersecret123 保密",
            &["supersecret123".to_string()],
        );
        assert!(!out.contains("supersecret123"));
    }

    #[test]
    fn version_numbers_not_masked() {
        let out = mask_sensitive("版本 1.21.1 与 2.5.0 都正常", &[]);
        assert_eq!(out, "版本 1.21.1 与 2.5.0 都正常");
    }
}
