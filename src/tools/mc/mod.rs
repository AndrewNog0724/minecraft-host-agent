//! 开服领域工具（M2 起注册，`mc` 模块；设计 §8.2）。
//!
//! 分层理由：领域工具是"高频复合操作的可靠封装"（一次调用 = 一个完整
//! 语义动作，内建校验与进度），降低 LLM 出错面与轮数；通用工具是逃生舱。

pub mod compat;
pub mod download;
pub mod files;
pub mod java;
pub mod mcmod;
pub mod mods;
pub mod plan;
pub mod probe;
pub mod process;
pub mod profile;
pub mod server_jar;
pub mod sys_info;
pub mod wiki;

use super::ToolRegistry;

// 领域工具统一从本模块取用框架类型（与 general/ 同一约定）。
pub use super::{Permission, Tool, ToolCtx, ToolError};

/// 注册全部开服领域工具。
pub fn register_mc_tools(registry: &mut ToolRegistry) {
    registry.register(Box::new(compat::CheckVersionCompatTool));
    registry.register(Box::new(sys_info::SysInfoTool));
    registry.register(Box::new(java::CheckJavaTool));
    registry.register(Box::new(java::EnsureJavaTool));
    registry.register(Box::new(server_jar::FetchServerJarTool));
    registry.register(Box::new(files::WriteServerFilesTool));
    let (start, status) = process::lifecycle_tools();
    registry.register(Box::new(start));
    registry.register(Box::new(status));
    registry.register(Box::new(probe::ProbePortTool));
    registry.register(Box::new(probe::McPingTool));
    registry.register(Box::new(plan::CheckPlanTool));
    registry.register(Box::new(wiki::WikiSearchTool));
    registry.register(Box::new(wiki::WikiPageTool));
    // mod 场景（设计 §8.12）：检索 / 解析 / 安装三段式
    registry.register(Box::new(mods::SearchModsTool));
    registry.register(Box::new(mods::ResolveModsTool));
    registry.register(Box::new(mods::InstallModsTool));
    // 部署档案（FR-16 / US3）
    registry.register(Box::new(profile::SaveProfileTool));
    registry.register(Box::new(profile::LoadProfileTool));
}
