//! provision：决策树引擎（tree.rs）+ 部署编排环（agent.rs，决议 D25）+
//! 工具后端（exec.rs）+ Java 供给（java.rs）+ 进程托管（process.rs）。
//!
//! 方案由决策树推导；部署由 LLM 逐工具调用编排（失败结构化回环），
//! exec.rs 的原流水线步骤全部降格为编排环的工具实现层。

pub mod agent;
pub mod exec;
pub mod java;
pub mod process;
pub mod tree;

pub use exec::DeployContext;
pub use exec::deploy;
pub use tree::Answers;
pub use tree::TreeOutput;
pub use tree::derive_spec;
