//! tools：工具系统（设计 §8.2）——模型与真实世界之间唯一的副作用通道。
//!
//! 分层：`Tool` 统一抽象 + 注册表 + Schema 校验在本模块；`general/` 是与场景
//! 无关的通用工具集（M1）；开服领域工具（`mc/`，M2）以同一 trait 接入。

pub mod confinement;
pub mod general;
pub mod mc;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::message::ToolOutcome;
use crate::cancel::CancelToken;
use crate::events::EventTx;
use crate::llm::ToolSpec;

use thiserror::Error;

/// 工具权限分级（确认门依据，决议 D106/D110）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// 只读（免确认）。
    ReadOnly,
    /// 写文件。
    Write,
    /// 执行命令 / 起停进程。
    Execute,
    /// 网络下载（大文件落盘）。
    Network,
}

/// 框架级工具错误（不回传模型）：取消、路径越界、IO 意外。
/// 业务性失败一律以 `ToolOutcome::Err` 结构化回传，由模型自行恢复（NFR-3）。
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("操作已被用户取消")]
    Cancelled,
    #[error("路径越界：{0}")]
    Confinement(String),
    #[error("IO 错误：{0}")]
    Io(String),
}

/// 工具执行上下文：安全边界与交互通道的集合。
#[derive(Clone)]
pub struct ToolCtx {
    /// 工作区（文件类工具路径收敛基准之一）。
    pub workspace: PathBuf,
    /// 数据目录 `~/.mcha/`（另一收敛基准）。
    pub data_dir: PathBuf,
    /// 共享 HTTP 客户端。
    pub http: reqwest::Client,
    /// 取消令牌（Ctrl-C 打断，R4）。
    pub cancel: CancelToken,
    /// 用户交互通道（确认门 / ask_user）。
    pub interaction: Arc<dyn Interaction>,
    /// 事件总线（进度渲染）。
    pub events: EventTx,
    /// run_command 默认超时（秒）。
    pub command_timeout_secs: u64,
    /// 搜索后端（决议 D103：空 = 无后端）。
    pub search_backend: String,
    /// 下载镜像配置（决议 D115，领域工具用）。
    pub network: crate::config::NetworkConfig,
    /// wiki 检索来源注册（决议 D120，领域工具用；S8 起）。
    #[allow(dead_code)]
    pub retrieval: crate::config::RetrievalConfig,
    /// CurseForge API Key（.env 装配时读取；空 = 未配置，mod 覆盖仅 Modrinth）。
    pub curseforge_key: String,
    /// 樱花frp 访问密钥（.env 装配 / `/token` 写入后同步；空 = 未配置，穿透引导待配置）。
    pub natfrp_token: String,
}

/// 确认门请求：展示给用户的内容（完整命令 / 写入摘要 / 下载目标）。
#[derive(Debug, Clone)]
pub struct ConfirmRequest {
    pub title: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmDecision {
    /// y：本次允许。
    Allow,
    /// a：本会话允许此工具。
    AllowAlways,
    /// n：拒绝（结构化回传模型，由其调整方案）。
    Deny,
}

/// ask_user 请求（设计 §8.2：单选项列表或自由文本）。
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub question: String,
    pub options: Vec<String>,
    pub allow_free_text: bool,
}

#[derive(Debug, Error)]
pub enum InteractionError {
    #[error("用户中断")]
    Cancelled,
    #[error("交互失败：{0}")]
    Failed(String),
}

/// 用户交互抽象：CLI 用 dialoguer 实现；测试用脚本实现。
#[async_trait::async_trait]
pub trait Interaction: Send + Sync {
    async fn confirm(&self, req: ConfirmRequest) -> Result<ConfirmDecision, InteractionError>;
    async fn ask(&self, req: AskRequest) -> Result<String, InteractionError>;
}

/// 工具统一抽象（课程 agent-architecture.md 第三节的落地）。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    /// 写清职责——模型选错工具多因描述不清。
    fn description(&self) -> String;
    /// 参数 JSON Schema（schemars 从类型派生）。
    fn parameters_schema(&self) -> serde_json::Value;
    fn permission(&self) -> Permission;
    /// 确认门弹窗的内容行（显示给用户的关键信息）。
    ///
    /// 默认返回空 = 框架按通用规则生成（命令 / 路径 / URL 摘要）；领域工具
    /// 的参数名不同，应覆写本方法展示真正关键的方案信息，避免弹窗空白。
    fn confirm_summary(&self, _args: &serde_json::Value) -> Vec<String> {
        Vec::new()
    }
    /// 执行。返回 `ToolOutcome`（含业务失败）；框架级意外才走 `ToolError`。
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError>;
}

/// 工具注册表。
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// 模型侧的工具声明列表。
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .map(|tool| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description(),
                parameters: tool.parameters_schema(),
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.keys().cloned().collect();
        names.sort();
        names
    }
}

/// 参数 Schema 校验（D112：校验失败携错误回传模型自纠）。
pub fn validate_args(tool: &dyn Tool, args: &serde_json::Value) -> Result<(), String> {
    let schema = tool.parameters_schema();
    let validator = jsonschema::validator_for(&schema)
        .map_err(|err| format!("工具参数 Schema 编译失败：{err}"))?;
    if let Err(err) = validator.validate(args) {
        return Err(format!(
            "参数不符合 Schema：{err}；参数 Schema：{}",
            crate::agent::message::truncate_chars(&schema.to_string(), 300)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct TestArgs {
        /// 目标路径
        path: String,
        /// 次数
        count: u32,
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "echo_tool"
        }
        fn description(&self) -> String {
            "测试用工具".into()
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::to_value(schemars::schema_for!(TestArgs)).unwrap()
        }
        fn permission(&self) -> Permission {
            Permission::ReadOnly
        }
        async fn run(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutcome, ToolError> {
            Ok(ToolOutcome::ok("ok"))
        }
    }

    #[test]
    fn schema_has_descriptions_and_validation_works() {
        let tool = EchoTool;
        let schema = tool.parameters_schema();
        let text = schema.to_string();
        assert!(text.contains("目标路径"), "doc 注释应成为 description");

        let ok = serde_json::json!({ "path": "a.txt", "count": 1 });
        assert!(validate_args(&tool, &ok).is_ok());

        let bad = serde_json::json!({ "path": "a.txt" });
        let err = validate_args(&tool, &bad).unwrap_err();
        assert!(err.contains("count"), "缺少必填字段应报错：{err}");

        let bad_type = serde_json::json!({ "path": "a.txt", "count": -1 });
        assert!(validate_args(&tool, &bad_type).is_err());
    }

    #[test]
    fn registry_specs_sorted() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        assert_eq!(registry.names(), vec!["echo_tool".to_string()]);
        let specs = registry.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo_tool");
        // 确认门经 gate 查询工具权限；注册表侧验证 specs 与查询
        assert!(registry.get("echo_tool").is_some());
        assert!(registry.get("nope").is_none());
    }
}
