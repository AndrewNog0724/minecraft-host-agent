//! 通用文件工具：read_file / write_file / edit_file / list_dir（设计 §8.2）。
//!
//! 全部经路径收敛（仅工作区与数据目录内）；写与编辑的确认门由 Agent 框架
//! 依据 `Permission` 统一实施（D106），工具本身不做确认。

use anyhow::Context as _;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

use crate::agent::message::ToolOutcome;

use super::confinement::resolve_in;
use super::{Tool, ToolCtx, ToolError};

/// 单次读取上限（字节）：超过则截断并注明，避免撑爆上下文。
const READ_CAP_BYTES: u64 = 512 * 1024;
/// 目录列举条目上限。
const LIST_CAP_ENTRIES: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// 文件路径（工作区相对路径或绝对路径，必须位于工作区或数据目录内）
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// 目标路径（必须位于工作区或数据目录内；父目录不存在时自动创建）
    pub path: String,
    /// 要写入的完整文本内容（覆盖写入）
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// 目标文件路径（必须已存在于工作区或数据目录内）
    pub path: String,
    /// 要被替换的精确文本（默认必须唯一匹配；多处命中会报错）
    pub old_string: String,
    /// 替换后的文本
    pub new_string: String,
    /// 为 true 时替换全部匹配（默认 false，要求唯一匹配）
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListDirArgs {
    /// 目录路径（默认工作区根；必须位于工作区或数据目录内）
    #[serde(default)]
    pub path: Option<String>,
}

/// 工作区文件工具共用的路径基准。
fn bases(ctx: &ToolCtx) -> Vec<&Path> {
    vec![ctx.workspace.as_path(), ctx.data_dir.as_path()]
}

pub struct ReadFileTool;

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> String {
        "读取工作区内文本文件的内容。适用于查看配置、代码、日志等文本；超过 512KB 会截断。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ReadFileArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: ReadFileArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        let path = resolve_in(&bases(ctx), &args.path)?;
        let meta = std::fs::metadata(&path)
            .map_err(|err| ToolError::Io(format!("{}：{}", path.display(), file_error(&err))))?;
        if meta.is_dir() {
            return Ok(ToolOutcome::err(format!(
                "{} 是目录，请用 list_dir",
                path.display()
            )));
        }
        let bytes = std::fs::read(&path)
            .map_err(|err| ToolError::Io(format!("{}：{}", path.display(), file_error(&err))))?;
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return Ok(ToolOutcome::err(
                "疑似二进制文件，read_file 不支持；如需信息请用 run_command",
            ));
        }
        if bytes.len() as u64 > READ_CAP_BYTES {
            let head = String::from_utf8_lossy(&bytes[..READ_CAP_BYTES as usize]);
            return Ok(ToolOutcome::ok(format!(
                "（文件共 {} 字节，超过上限，仅显示前 {} 字节）\n{}",
                bytes.len(),
                READ_CAP_BYTES,
                head
            )));
        }
        Ok(ToolOutcome::ok(String::from_utf8_lossy(&bytes).to_string()))
    }
}

pub struct WriteFileTool;

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> String {
        "把完整文本内容写入工作区文件（覆盖式）。新建配置、保存抓取结果等场景使用。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WriteFileArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Write
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: WriteFileArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        let path = resolve_in(&bases(ctx), &args.path)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        atomic_write(&path, args.content.as_bytes())
            .map_err(|err| ToolError::Io(format!("写入 {} 失败：{err}", path.display())))?;
        Ok(ToolOutcome::ok(format!(
            "已写入 {}（{} 字节）",
            path.display(),
            args.content.len()
        )))
    }
}

pub struct EditFileTool;

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }
    fn description(&self) -> String {
        "对工作区现有文件做精确替换编辑：old_string 必须唯一命中（多处命中报错），适合小修改。"
            .into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(EditFileArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::Write
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: EditFileArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        let path = resolve_in(&bases(ctx), &args.path)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|err| ToolError::Io(format!("读取 {} 失败：{err}", path.display())))?;
        let hits = content.matches(args.old_string.as_str()).count();
        if hits == 0 {
            return Ok(ToolOutcome::err(
                "old_string 在文件中未找到，请先 read_file 核对原文",
            ));
        }
        if hits > 1 && !args.replace_all {
            return Ok(ToolOutcome::err(format!(
                "old_string 命中 {hits} 处；请补充更多上下文使其唯一，或设置 replace_all=true"
            )));
        }
        let updated = if args.replace_all {
            content.replace(args.old_string.as_str(), &args.new_string)
        } else {
            content.replacen(args.old_string.as_str(), &args.new_string, 1)
        };
        atomic_write(&path, updated.as_bytes())
            .map_err(|err| ToolError::Io(format!("写回 {} 失败：{err}", path.display())))?;
        Ok(ToolOutcome::ok(format!(
            "已替换 {} 处（{}）",
            if args.replace_all { hits } else { 1 },
            path.display()
        )))
    }
}

pub struct ListDirTool;

#[async_trait::async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &'static str {
        "list_dir"
    }
    fn description(&self) -> String {
        "列出工作区目录内容（含子目录与文件大小）。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ListDirArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: ListDirArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        let path = match &args.path {
            Some(p) => resolve_in(&bases(ctx), p)?,
            None => ctx.workspace.clone(),
        };
        let mut entries: Vec<(String, bool, u64)> = Vec::new();
        let read_dir = std::fs::read_dir(&path)
            .map_err(|err| ToolError::Io(format!("{}：{}", path.display(), file_error(&err))))?;
        for entry in read_dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => return Err(ToolError::Io(format!("遍历目录失败：{err}"))),
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            entries.push((name, is_dir, size));
            if entries.len() >= LIST_CAP_ENTRIES {
                break;
            }
        }
        // 目录在前，同类按名称排序
        entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut lines: Vec<String> = Vec::new();
        for (name, is_dir, size) in &entries {
            if *is_dir {
                lines.push(format!("{name}/"));
            } else {
                lines.push(format!("{name}  {size} B"));
            }
        }
        if entries.len() >= LIST_CAP_ENTRIES {
            lines.push(format!("…（超过 {LIST_CAP_ENTRIES} 项，已截断）"));
        }
        let total = std::fs::read_dir(&path).map(|d| d.count()).unwrap_or(0);
        let mut out = format!("{}（共 {total} 项）\n", path.display());
        out.push_str(&if lines.is_empty() {
            "（空目录）".to_string()
        } else {
            lines.join("\n")
        });
        Ok(ToolOutcome::ok(out))
    }
}

/// 原子写：先写同目录临时文件再落正式名，避免半截文件。
fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建父目录失败：{}", parent.display()))?;
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut tmp, bytes)?;
    tmp.persist(path)
        .map_err(|err| anyhow::anyhow!("落盘失败：{err}"))?;
    Ok(())
}

/// 把 std::io::Error 转成用户可读的提示（含"下一步怎么办"倾向）。
fn file_error(err: &std::io::Error) -> String {
    match err.kind() {
        std::io::ErrorKind::NotFound => "文件不存在".to_string(),
        std::io::ErrorKind::PermissionDenied => "没有权限".to_string(),
        _ => format!("{err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelToken;
    use crate::events::event_channel;
    use crate::tools::{Interaction, InteractionError};
    use std::sync::Arc;

    struct NoInteraction;

    #[async_trait::async_trait]
    impl Interaction for NoInteraction {
        async fn confirm(
            &self,
            _req: crate::tools::ConfirmRequest,
        ) -> Result<crate::tools::ConfirmDecision, InteractionError> {
            Err(InteractionError::Failed("测试中不应触发确认".into()))
        }
        async fn ask(&self, _req: crate::tools::AskRequest) -> Result<String, InteractionError> {
            Err(InteractionError::Failed("测试中不应触发提问".into()))
        }
    }

    fn test_ctx(root: &Path) -> (ToolCtx, tempfile::TempDir) {
        let workspace = root.join("workspace");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let (tx, _rx) = event_channel();
        let ctx = ToolCtx {
            workspace: workspace.clone(),
            data_dir,
            http: reqwest::Client::new(),
            cancel: CancelToken::new(),
            interaction: Arc::new(NoInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
        };
        (ctx, tempfile::tempdir().unwrap())
    }

    async fn write_and_read_back(ctx: &ToolCtx, path: &str, content: &str) {
        let outcome = WriteFileTool
            .run(serde_json::json!({ "path": path, "content": content }), ctx)
            .await
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
        let back = ReadFileTool
            .run(serde_json::json!({ "path": path }), ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content: got } = back else {
            panic!("读回应成功：{back:?}")
        };
        assert_eq!(got, content);
    }

    #[tokio::test]
    async fn write_read_edit_roundtrip() {
        let root = tempfile::tempdir().unwrap();
        let (ctx, _guard) = test_ctx(root.path());
        write_and_read_back(&ctx, "notes/a.txt", "第一版内容").await;

        // 唯一匹配替换
        let outcome = EditFileTool
            .run(
                serde_json::json!({
                    "path": "notes/a.txt",
                    "old_string": "第一版",
                    "new_string": "第二版",
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
        let back = ReadFileTool
            .run(serde_json::json!({ "path": "notes/a.txt" }), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = back else {
            panic!("读取失败")
        };
        assert_eq!(content, "第二版内容");

        // 多处命中必须报错
        WriteFileTool
            .run(
                serde_json::json!({ "path": "notes/b.txt", "content": "x x x" }),
                &ctx,
            )
            .await
            .unwrap();
        let outcome = EditFileTool
            .run(
                serde_json::json!({
                    "path": "notes/b.txt",
                    "old_string": "x",
                    "new_string": "y",
                }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Err { error } = outcome else {
            panic!("多处命中应报错")
        };
        assert!(error.contains("3 处"), "{error}");
    }

    #[tokio::test]
    async fn list_dir_shows_entries() {
        let root = tempfile::tempdir().unwrap();
        let (ctx, _guard) = test_ctx(root.path());
        std::fs::write(ctx.workspace.join("README.md"), "hello").unwrap();
        std::fs::create_dir_all(ctx.workspace.join("sub")).unwrap();
        let outcome = ListDirTool.run(serde_json::json!({}), &ctx).await.unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("列目录应成功")
        };
        assert!(content.contains("README.md"));
        assert!(content.contains("sub/"));
    }

    #[tokio::test]
    async fn path_outside_workspace_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (ctx, _guard) = test_ctx(root.path());
        let outcome = WriteFileTool
            .run(
                serde_json::json!({ "path": "/etc/mcha-evil.txt", "content": "x" }),
                &ctx,
            )
            .await;
        match outcome {
            Err(ToolError::Confinement(_)) => {}
            other => panic!("越界路径应被拒绝：{other:?}"),
        }
    }
}
