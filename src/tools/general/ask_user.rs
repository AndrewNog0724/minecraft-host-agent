//! ask_user：Agent 获取用户输入的唯一通道（设计 §8.2）。
//!
//! 单选项列表或自由文本（默认允许选项外自由输入）；用户 Ctrl-C = 打断当前
//! 回合（同全局语义），以 `ToolError::Cancelled` 透传。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;
use crate::tools::{AskRequest, InteractionError};

use super::{Tool, ToolCtx, ToolError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskUserArgs {
    /// 要问用户的问题（一句话说清在决定什么）
    pub question: String,
    /// 选项列表（可空 = 自由文本输入）
    #[serde(default)]
    pub options: Vec<String>,
    /// 是否允许用户输入选项之外的自由文本（默认允许）
    #[serde(default = "default_allow_free_text")]
    pub allow_free_text: bool,
}

fn default_allow_free_text() -> bool {
    true
}

pub struct AskUserTool;

#[async_trait::async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &'static str {
        "ask_user"
    }
    fn description(&self) -> String {
        "向用户提问：给出选项列表（用户也可自由输入）或开放文本输入。需要用户决策 / 提供信息时使用。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(AskUserArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: AskUserArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        let request = AskRequest {
            question: args.question,
            options: args.options,
            allow_free_text: args.allow_free_text,
        };
        match ctx.interaction.ask(request).await {
            Ok(answer) => Ok(ToolOutcome::ok(answer)),
            Err(InteractionError::Cancelled) => Err(ToolError::Cancelled),
            Err(err) => Ok(ToolOutcome::err(format!("提问失败：{err}"))),
        }
    }
}
