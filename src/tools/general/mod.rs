//! 通用工具集（M1，框架自带，与场景无关；设计 §8.2）。

pub mod ask_user;
pub mod fs_tools;
pub mod http_tools;
pub mod load_skill;
pub mod run_command;
pub mod web_search;

use super::ToolRegistry;

// 通用工具统一从本模块取用框架类型，避免各文件写 super::super::
pub use super::confinement;
pub use super::{Permission, Tool, ToolCtx, ToolError};

/// 注册全部通用工具。
pub fn register_general_tools(registry: &mut ToolRegistry) {
    registry.register(Box::new(run_command::RunCommandTool));
    registry.register(Box::new(fs_tools::ReadFileTool));
    registry.register(Box::new(fs_tools::WriteFileTool));
    registry.register(Box::new(fs_tools::EditFileTool));
    registry.register(Box::new(fs_tools::ListDirTool));
    registry.register(Box::new(http_tools::HttpGetTextTool));
    registry.register(Box::new(http_tools::HttpDownloadTool));
    registry.register(Box::new(web_search::WebSearchTool));
    registry.register(Box::new(ask_user::AskUserTool));
    registry.register(Box::new(load_skill::LoadSkillTool));
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::tools::{ConfirmDecision, ConfirmRequest, Interaction, InteractionError};

    /// 测试用静默交互：一切确认直接放行、一切提问直接报错。
    pub(crate) struct QuietInteraction;

    #[async_trait::async_trait]
    impl Interaction for QuietInteraction {
        async fn confirm(&self, _req: ConfirmRequest) -> Result<ConfirmDecision, InteractionError> {
            Ok(ConfirmDecision::Allow)
        }
        async fn ask(&self, _req: crate::tools::AskRequest) -> Result<String, InteractionError> {
            Err(InteractionError::Failed("测试中不应触发提问".into()))
        }
    }
}
