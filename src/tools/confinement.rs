//! 路径收敛（FR-04 / NFR-2）：文件类工具的目标路径必须落在允许的基准目录内。
//!
//! 采用词法规范化（消解 `.` 与 `..`）+ 前缀校验；不触盘、不解析符号链接
//! （M1 取舍：目标平台 Windows 上符号链接少见，README 会注明该边界）。

use std::path::{Component, Path, PathBuf};

use super::ToolError;

/// 词法规范化路径：消解 `.`、`..` 与重复分隔符。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // 越出根时 pop 到空，后续前缀检查会拒绝
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 把用户 / 模型给的路径解析到允许的基准目录内。
///
/// - 相对路径相对 `bases[0]`（工作区）解析；
/// - 绝对路径原样规范化后做前缀校验；
/// - 越界返回 `ToolError::Confinement`（结构化回传模型）。
pub fn resolve_in(bases: &[&Path], raw: &str) -> Result<PathBuf, ToolError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ToolError::Confinement("路径为空".to_string()));
    }
    let raw_path = Path::new(raw);
    let joined = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        bases[0].join(raw_path)
    };
    let normalized = normalize(&joined);
    for base in bases {
        let base_norm = normalize(base);
        if normalized.starts_with(&base_norm) {
            return Ok(normalized);
        }
    }
    let allowed = bases
        .iter()
        .map(|b| b.display().to_string())
        .collect::<Vec<_>>()
        .join(" 或 ");
    Err(ToolError::Confinement(format!(
        "路径“{raw}”必须位于 {allowed} 之内"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let data = dir.path().join("data");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        (dir, workspace, data)
    }

    #[cfg(unix)]
    #[test]
    fn relative_paths_stay_inside() {
        let (_dir, workspace, data) = setup();
        let bases = [workspace.as_path(), data.as_path()];

        let p = resolve_in(&bases, "a/b.txt").unwrap();
        assert_eq!(p, workspace.join("a/b.txt"));

        let p = resolve_in(&bases, "./a/../b.txt").unwrap();
        assert_eq!(p, workspace.join("b.txt"));

        // 数据目录内的绝对路径也允许
        let p = resolve_in(&bases, &data.join("usage/x.jsonl").display().to_string()).unwrap();
        assert_eq!(p, data.join("usage/x.jsonl"));
    }

    #[cfg(unix)]
    #[test]
    fn traversal_and_outside_rejected() {
        let (_dir, workspace, data) = setup();
        let bases = [workspace.as_path(), data.as_path()];

        // 经典穿越：即便词法上还在工作区内也合法，越出即拒绝
        assert!(resolve_in(&bases, "../../etc/passwd").is_err());
        assert!(resolve_in(&bases, "/etc/passwd").is_err());
        assert!(resolve_in(&bases, "").is_err());

        // 词法留在工作区内但形式可疑的，规范化后应放行
        let ok = resolve_in(&bases, "sub/../inside.txt").unwrap();
        assert_eq!(ok, workspace.join("inside.txt"));

        // 两级上跳会越出工作区，必须拒绝
        assert!(resolve_in(&bases, "sub/../../inside.txt").is_err());
    }

    #[test]
    fn normalize_dedups() {
        assert_eq!(
            normalize(Path::new("/a/./b//c/../d")),
            PathBuf::from("/a/b/d")
        );
    }
}
