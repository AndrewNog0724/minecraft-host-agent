//! 事件总线与核心事件类型（§8.1）。
//!
//! R4（进度）/ R5（轨迹）/ R6（用量）是同一事件流的三个视图：
//! 任何模块都只向 [`EventBus`] 发布事件，ui 与 store 各自订阅处理。

use chrono::{DateTime, Local};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::spec::{ServerSpec, ServerSpecDraft};

/// 全局任务标识：每次 Agent 任务（开服 / 诊断）一个。
pub type TaskId = String;

/// 进度事件（R4 数据基础）。`StepProgress.current/total` 直接映射
/// ui 上的 "45/120 MB" 式进度条。Step* 命名与设计文档 §8.1 冻结格式一致。
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProgressEvent {
    StepStarted {
        task_id: TaskId,
        step: String,
        title: String,
    },
    StepProgress {
        task_id: TaskId,
        step: String,
        current: u64,
        total: Option<u64>,
        /// 附加说明，如 "下载中 45/120 MB"
        detail: Option<String>,
    },
    StepFinished {
        task_id: TaskId,
        step: String,
        ok: bool,
        detail: Option<String>,
    },
    /// 面向用户的直显消息（模型澄清文本、待确认问题等，决议 D17）。
    /// 渲染层经 MultiProgress::println 原样打印，不进进度条。
    Notice { task_id: TaskId, text: String },
    /// 服务端日志行直显（决议 D19）：渲染层滚动打印，构成启动过程留痕。
    LogLine {
        task_id: TaskId,
        step: String,
        line: String,
    },
}

/// LLM 调用阶段（R6 按阶段汇总用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Requirement,
    Diagnosis,
    Chat,
}

/// 单次 LLM 调用计量（R6 数据基础）。由 `llm` 模块在响应解析处强制生成。
/// 上游不返回 usage 时，token 数记 0 并置 `usage_reported = false`，
/// 只计调用次数（对应课程 Q9）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub call_id: String,
    pub task_id: TaskId,
    pub at: DateTime<Local>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 按 config 价格表换算的成本（精确十进制，避免浮点漂移）
    pub cost: Decimal,
    pub phase: Phase,
    /// 上游是否报告了真实 token 数；false 时以上数字不可信
    pub usage_reported: bool,
}

/// 轨迹条目类别（R5 "非黑盒" 主体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    /// 一次 LLM 调用
    Llm,
    /// 一次工具执行
    Tool,
    /// 决策树的一次节点判定
    Decision,
    /// 一次确定性执行动作（下载 / 写文件 / 起进程）
    Exec,
}

/// 任务轨迹中的单步记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceStep {
    pub kind: TraceKind,
    /// 人可读摘要（会话导出时直接展示）
    pub summary: String,
    /// 关联的 UsageRecord.call_id（Llm 类）
    pub usage_refs: Vec<String>,
    pub at: DateTime<Local>,
    /// 结构化详情（工具参数与结果摘要等），JSON 便于查询
    pub detail: Option<serde_json::Value>,
}

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

/// 任务轨迹：一次任务从发起到结束的完整留痕（R5）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTrace {
    pub task_id: TaskId,
    pub title: String,
    pub spec_id: Option<String>,
    pub started_at: DateTime<Local>,
    pub finished_at: Option<DateTime<Local>>,
    pub steps: Vec<TraceStep>,
    pub status: TaskStatus,
    /// 失败原因摘要（决议 D19）：sessions show / 导出可见，不再只有 stderr 一闪而过
    #[serde(default)]
    pub error: Option<String>,
}

impl TaskTrace {
    pub fn new(task_id: TaskId, title: impl Into<String>) -> Self {
        Self {
            task_id,
            title: title.into(),
            spec_id: None,
            started_at: Local::now(),
            finished_at: None,
            steps: Vec::new(),
            status: TaskStatus::Running,
            error: None,
        }
    }
}

/// 任务生命周期轨迹事件（区别于进度：这是给 R5 落盘的结构化留痕）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceEvent {
    TaskStarted {
        trace: TaskTrace,
    },
    StepAdded {
        task_id: TaskId,
        step: TraceStep,
    },
    SpecDrafted {
        task_id: TaskId,
        draft: Box<ServerSpecDraft>,
    },
    SpecConfirmed {
        task_id: TaskId,
        spec: Box<ServerSpec>,
    },
    TaskFinished {
        task_id: TaskId,
        status: TaskStatus,
        /// 失败原因摘要（决议 D19）；成功 / 取消为 None
        error: Option<String>,
    },
    /// 需求理解环的完整对话消息（决议 D16：失败留痕；成功也落盘供 R5 查看）。
    SessionMessages {
        task_id: TaskId,
        messages: Vec<crate::llm::ChatMessage>,
    },
}

/// 统一应用事件：三个视图共享一条流。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppEvent {
    Progress(ProgressEvent),
    Usage(UsageRecord),
    Trace(TraceEvent),
}

impl From<ProgressEvent> for AppEvent {
    fn from(e: ProgressEvent) -> Self {
        AppEvent::Progress(e)
    }
}

impl From<UsageRecord> for AppEvent {
    fn from(e: UsageRecord) -> Self {
        AppEvent::Usage(e)
    }
}

impl From<TraceEvent> for AppEvent {
    fn from(e: TraceEvent) -> Self {
        AppEvent::Trace(e)
    }
}

/// 事件总线：广播发布（R4/R5/R6 三视图共享）。
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<AppEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self { tx }
    }

    /// 发布事件。当前无订阅者时返回 Err，属于正常情况，忽略即可。
    pub fn publish(&self, event: impl Into<AppEvent>) {
        let _ = self.tx.send(event.into());
    }

    /// 订阅事件流。
    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }
}
