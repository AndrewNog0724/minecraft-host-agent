//! MC 版本号解析与比较（设计 §8.10）。
//!
//! MC 稳定版版本号为 2–3 段纯数字点分串（`1.21`、`1.21.1`）。自写数值段
//! 比较器（缺段补 0）而非 semver crate：`1.21` 不是合法 semver 却是合法
//! MC 版本，自写约 40 行且答辩可解释。快照（`24w14a`）与非数字输入在解析
//! 层拒绝；`26.2` 这类形状合法但不存在于上游清单的输入由存在性核对拒绝
//! （check_version_compat 给就近建议）。

/// 一个 MC 版本号（稳定版语义）。
#[derive(Debug, Clone, Eq)]
pub struct McVersion {
    /// 规范化后的原始输入（去空白）。
    pub raw: String,
    /// 数值段（固定 3 位，缺段补 0，便于比较）。
    segments: [u32; 3],
}

impl McVersion {
    /// 解析：2–3 段纯数字；拒绝空段、非数字、快照、超长段。
    pub fn parse(input: &str) -> Result<Self, String> {
        let trimmed = input.trim();
        let mut parts = Vec::new();
        for segment in trimmed.split('.') {
            if segment.is_empty()
                || segment.len() > 5
                || !segment.chars().all(|c| c.is_ascii_digit())
            {
                return Err(format!(
                    "非法 MC 版本号「{input}」：应为 1.21 / 1.21.1 形式的 2–3 段纯数字（快照与预发布版不支持）"
                ));
            }
            // 段内全是 ASCII 数字且长度受限，parse 不会失败；仍显式处理
            let value: u32 = segment
                .parse()
                .map_err(|_| format!("非法 MC 版本号「{input}」：数字段超出范围"))?;
            parts.push(value);
        }
        if parts.len() < 2 || parts.len() > 3 {
            return Err(format!(
                "非法 MC 版本号「{input}」：应为 2–3 段数字（如 1.21、1.21.1）"
            ));
        }
        let mut segments = [0u32; 3];
        segments[..parts.len()].copy_from_slice(&parts);
        Ok(Self {
            raw: trimmed.to_string(),
            segments,
        })
    }
}

impl PartialEq for McVersion {
    fn eq(&self, other: &Self) -> bool {
        self.segments == other.segments
    }
}

impl PartialOrd for McVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for McVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.segments.cmp(&other.segments)
    }
}

impl std::fmt::Display for McVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_versions() {
        let v = McVersion::parse(" 1.21.1 ").unwrap();
        assert_eq!(v.raw, "1.21.1");
        let v = McVersion::parse("1.21").unwrap();
        assert_eq!(v.raw, "1.21");
        // 两段与三段表示等值（缺段补 0）
        assert_eq!(
            McVersion::parse("1.21").unwrap(),
            McVersion::parse("1.21.0").unwrap()
        );
    }

    #[test]
    fn rejects_malformed_versions() {
        for bad in [
            "", "abc", "1", "1.2.3.4", "24w14a", "1.21.x", "1.x.1", "1.-2", "26.2.",
        ] {
            assert!(McVersion::parse(bad).is_err(), "应拒绝：{bad}");
        }
    }

    #[test]
    fn orders_versions_numerically() {
        let v = |s: &str| McVersion::parse(s).unwrap();
        assert!(v("1.20.4") < v("1.20.5"));
        assert!(v("1.9") < v("1.10")); // 数值比较，非字典序
        assert!(v("1.21.1") > v("1.21"));
        assert!(v("1.21") == v("1.21.0"));
        assert!(v("2.0") > v("1.21.9"));
    }
}
