//! web_search：关键词搜索（决议 D103）。
//!
//! P0 只定义接口：默认无后端（结构化错误如实告知），后端实现在后续版本提供；
//! 领域事实的主通道是知识库 + 上游 API（M2），搜索只是兜底。

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;

use super::{Tool, ToolCtx, ToolError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    /// 搜索关键词
    pub query: String,
}

pub struct WebSearchTool;

#[async_trait::async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn description(&self) -> String {
        "关键词搜索，返回标题 / 链接 / 摘要列表。未配置搜索后端时会返回说明（可改用 http_get_text 抓取已知页面）。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WebSearchArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: WebSearchArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.search_backend.is_empty() {
            return Ok(ToolOutcome::err(format!(
                "搜索「{}」不可用：未配置搜索后端。可选：① 直接用 http_get_text 抓取已知页面；\
                 ② 让用户在 config.toml [search] backend 中配置后端（当前版本尚未内置实现）。",
                args.query
            )));
        }
        Ok(ToolOutcome::err(format!(
            "搜索「{}」不可用：后端「{}」尚未实现；请改用 http_get_text 抓取已知页面。",
            args.query, ctx.search_backend
        )))
    }
}
