//! knowledge：L1 静态知识库 + 上游 API 客户端（定制 2 的后端；设计 §8.4/§8.10）。
//!
//! 查询工具（check_version_compat 等）与领域执行工具（fetch_server_jar 等）
//! 共用本模块。版本类事实的唯一来源——红线：知识内容只经工具返回值进入
//! Agent 上下文，永不进 Prompt。

pub mod compat;
pub mod upstream;
pub mod version;
