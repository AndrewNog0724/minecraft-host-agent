//! provision：决策树引擎 + 执行流水线（确定性，§8.5）。
//!
//! 决策树推导方案（tree.rs），执行流水线把它变成运行中的服务器
//! （exec.rs），Java 供给（java.rs）与进程托管（process.rs）为其中两环。

pub mod exec;
pub mod java;
pub mod process;
pub mod tree;

pub use exec::DeployContext;
pub use exec::deploy;
pub use tree::Answers;
pub use tree::TreeOutput;
pub use tree::derive_spec;
