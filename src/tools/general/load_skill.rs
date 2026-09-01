//! load_skill：按需加载领域指南（Skills，决议 D104；内容资产自 M2 起提供）。
//!
//! 技能目录约定：`<根>/<name>/SKILL.md`。搜索顺序：数据目录 → 可执行文件
//! 旁 assets/ → 当前目录 assets/。只读注入，不影响安全边界。

use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;

use crate::agent::message::ToolOutcome;

use super::{Tool, ToolCtx, ToolError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadSkillArgs {
    /// 技能名（小写字母 / 数字 / 连字符，如 server-setup）
    pub name: String,
}

pub struct LoadSkillTool;

impl LoadSkillTool {
    /// 候选技能根目录（供工具与 system prompt 的技能清单共用）。
    fn search_roots(data_dir: &std::path::Path) -> Vec<PathBuf> {
        let mut roots = vec![data_dir.join("skills")];
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            roots.push(dir.join("assets").join("skills"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            roots.push(cwd.join("assets").join("skills"));
        }
        roots
    }

    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    }
}

#[async_trait::async_trait]
impl Tool for LoadSkillTool {
    fn name(&self) -> &'static str {
        "load_skill"
    }
    fn description(&self) -> String {
        "按需加载领域指南（Skill）：某类任务的完整操作规程。执行前若 system prompt 的技能清单里有对应条目，应先加载。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(LoadSkillArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: LoadSkillArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if !Self::valid_name(&args.name) {
            return Ok(ToolOutcome::err(format!(
                "技能名不合法：{}（仅小写字母 / 数字 / 连字符）",
                args.name
            )));
        }
        for root in Self::search_roots(&ctx.data_dir) {
            let path = root.join(&args.name).join("SKILL.md");
            if path.is_file() {
                let content = std::fs::read_to_string(&path)
                    .map_err(|err| ToolError::Io(format!("读取技能失败：{err}")))?;
                let lines = content.lines().count();
                return Ok(ToolOutcome::ok(format!(
                    "已加载技能「{}」（{lines} 行）\n\n{content}",
                    args.name
                )));
            }
        }
        let available = available_skills(&ctx.data_dir);
        let hint = if available.is_empty() {
            "当前未安装任何技能。".to_string()
        } else {
            format!("可用技能：{}", available.join("、"))
        };
        Ok(ToolOutcome::err(format!(
            "技能「{}」不存在。{hint}",
            args.name
        )))
    }
}

/// 已安装技能清单（system prompt 中只放一句话列表，按需加载全文——§8.5）。
pub fn available_skills(data_dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for root in LoadSkillTool::search_roots(data_dir) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && path.join("SKILL.md").is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && !names.iter().any(|n| n == name)
            {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names
}
