//! ui 渲染：事件泵——把总线事件分流为进度条（R4）与落盘（R5/R6）。

use std::collections::HashMap;
use std::sync::Arc;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rust_decimal::Decimal;

use crate::events::{AppEvent, ProgressEvent, TaskStatus, TaskTrace, TraceEvent};
use crate::store::{SessionBackup, Store};

/// 事件泵：订阅总线，驱动进度条 + 累积任务轨迹 + 写 usage/events 落盘。
/// 同步镜像一份到仓库备份目录（v0.9.2 调试设施，备份失败只 warn 不影响主流程）。
/// `rx` 必须由调用方在发布任何事件（含 TaskStarted）之前订阅——广播不可回放，
/// 先发后订会静默丢事件导致 trace 不落盘（v0.9.3 实测勘误）。
/// 泵在 TaskFinished 或通道关闭时退出。
pub async fn pump(
    mut rx: tokio::sync::broadcast::Receiver<AppEvent>,
    store: Arc<Store>,
    bars: MultiProgress,
    backup: SessionBackup,
) -> Result<(), tokio::sync::broadcast::error::RecvError> {
    // step_id → 进度条（决议 D19：完成即收起并落滚动摘要行，进度条只表达"进行中"）
    let mut step_bars: HashMap<String, ProgressBar> = HashMap::new();
    // step_id → 标题（StepFinished 只有 id，滚动摘要行需要标题）
    let mut step_titles: HashMap<String, String> = HashMap::new();
    // 任务轨迹由泵持有并落盘（R5 的"非黑盒"主体）
    let mut trace: Option<TaskTrace> = None;
    // 本次会话累计费用展示（泵内本地累计；落盘账本 read_usage 是跨任务终身账，
    // 供 `mcha usage` 查总账，不能拿来当"本次"展示——v0.9.4 勘误）
    let mut session_cost = Decimal::ZERO;
    let cost_bar = bars.add(ProgressBar::new_spinner());
    cost_bar.set_style(spinner_style());
    cost_bar.set_message("本次费用：¥0");

    loop {
        let event = rx.recv().await?;
        match event {
            AppEvent::Progress(p) => handle_progress(&bars, &mut step_bars, &mut step_titles, &p),
            AppEvent::Usage(u) => {
                let _ = store.append_usage(&u);
                session_cost += u.cost;
                cost_bar.set_message(format!(
                    "本次费用：¥{session_cost:.4}（上次调用 in {} / out {} tok）",
                    u.input_tokens, u.output_tokens
                ));
            }
            AppEvent::Trace(t) => match t {
                TraceEvent::TaskStarted { trace: t0 } => {
                    let _ = store.save_trace(&t0);
                    backup.save_trace(&t0);
                    trace = Some(t0);
                }
                TraceEvent::StepAdded { task_id, step } => {
                    if let Some(tr) = trace.as_mut() {
                        tr.steps.push(step);
                        let _ = store.save_trace(tr);
                        backup.save_trace(tr);
                    } else {
                        let _ = store.append_event(&task_id, &serde_json::json!({"step": step}));
                        backup.append_event(&task_id, &serde_json::json!({"step": step}));
                    }
                }
                TraceEvent::SpecDrafted { task_id, .. } => {
                    let _ = store.append_event(
                        &task_id,
                        &serde_json::json!({"event": "spec_drafted", "at": chrono::Local::now().to_rfc3339()}),
                    );
                    backup.append_event(
                        &task_id,
                        &serde_json::json!({"event": "spec_drafted", "at": chrono::Local::now().to_rfc3339()}),
                    );
                }
                TraceEvent::SpecConfirmed { task_id, spec } => {
                    let _ = store.append_event(
                        &task_id,
                        &serde_json::json!({
                            "event": "spec_confirmed",
                            "spec_id": spec.spec_id,
                            "at": chrono::Local::now().to_rfc3339(),
                        }),
                    );
                    backup.append_event(
                        &task_id,
                        &serde_json::json!({
                            "event": "spec_confirmed",
                            "spec_id": spec.spec_id,
                            "at": chrono::Local::now().to_rfc3339(),
                        }),
                    );
                }
                TraceEvent::TaskFinished {
                    task_id,
                    status,
                    error,
                } => {
                    if let Some(tr) = trace.as_mut() {
                        tr.status = status;
                        tr.finished_at = Some(chrono::Local::now());
                        tr.error = error.clone();
                        let _ = store.save_trace(tr);
                        backup.save_trace(tr);
                    }
                    let _ = store.append_event(
                        &task_id,
                        &serde_json::json!({"event": "task_finished", "status": status, "error": error, "at": chrono::Local::now().to_rfc3339()}),
                    );
                    backup.append_event(
                        &task_id,
                        &serde_json::json!({"event": "task_finished", "status": status, "error": error, "at": chrono::Local::now().to_rfc3339()}),
                    );
                    return Ok(());
                }
                TraceEvent::SessionMessages { task_id, messages } => {
                    // 对话原文留痕（决议 D16）：失败排障与 R5 查看共用
                    if let Ok(json) = serde_json::to_value(&messages) {
                        let _ = store.save_messages(&task_id, &json);
                        backup.save_messages(&task_id, &json);
                    }
                }
            },
        }
    }
}

/// 模板样式：模板为编译期常量，解析失败时退回内置样式（不 panic）。
fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
}

fn handle_progress(
    bars: &MultiProgress,
    step_bars: &mut HashMap<String, ProgressBar>,
    step_titles: &mut HashMap<String, String>,
    p: &ProgressEvent,
) {
    match p {
        ProgressEvent::StepStarted { step, title, .. } => {
            let bar = bars.add(ProgressBar::new_spinner());
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            bar.set_style(spinner_style());
            bar.set_message(title.clone());
            step_bars.insert(step.clone(), bar);
            step_titles.insert(step.clone(), title.clone());
        }
        ProgressEvent::StepProgress {
            step,
            current,
            total,
            detail,
            ..
        } => {
            let Some(bar) = step_bars.get(step) else {
                return;
            };
            let detail_text = detail.clone().unwrap_or_default();
            if let Some(total) = total {
                if bar.length() != Some(*total) {
                    bar.set_length(*total);
                    bar.set_style(bar_style());
                }
                bar.set_position(*current);
                bar.set_message(detail_text);
            } else if !detail_text.is_empty() {
                bar.set_message(detail_text);
            }
        }
        ProgressEvent::StepFinished {
            step, ok, detail, ..
        } => {
            // 决议 D19：进度条原地收起，改在滚动区落一行摘要——
            // 进度条消息转瞬即逝，滚动行才是用户回头可查的记录
            if let Some(bar) = step_bars.remove(step) {
                bar.finish_and_clear();
            }
            let title = step_titles.remove(step).unwrap_or_else(|| step.clone());
            let mark = if *ok { "✔" } else { "✘" };
            match detail {
                Some(d) if !d.is_empty() => {
                    let _ = bars.println(format!("{mark} {title}：{d}"));
                }
                _ => {
                    let _ = bars.println(format!("{mark} {title}"));
                }
            }
        }
        ProgressEvent::Notice { text, .. } => {
            // 直显消息（模型澄清文本等）：挂起进度条原样打印，避免交错（决议 D17）
            let _ = bars.println(text);
        }
        ProgressEvent::LogLine { line, .. } => {
            // 服务端日志行直显（决议 D19）：滚动打印，构成启动过程留痕
            let _ = bars.println(format!("  {line}"));
        }
    }
}

/// 会话结束状态转可读文本。
pub fn status_text(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "进行中",
        TaskStatus::Done => "已完成",
        TaskStatus::Failed => "失败",
        TaskStatus::Cancelled => "已取消",
    }
}
