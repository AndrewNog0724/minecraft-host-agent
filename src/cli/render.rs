//! 会话界面的语义化渲染块（D107）：事件流 → 终端视觉块。
//!
//! 渲染器是事件总线的订阅者，不维护会话状态（用量累计在 Session）。
//! 不支持 Unicode 符号的环境可设 `MCHA_ASCII=1` 降级 ASCII 符号集（§8.6）。

use std::collections::HashMap;

use crossterm::style::{Attribute, Color, Stylize};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::events::Event;

/// 符号集（Unicode 默认 / ASCII 降级）。
struct Symbols {
    tool: &'static str,
    result: &'static str,
    ok: &'static str,
    err: &'static str,
    think: &'static str,
}

impl Symbols {
    fn detect() -> Self {
        if std::env::var("MCHA_ASCII")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            Self {
                tool: "*",
                result: "`",
                ok: "[ok]",
                err: "[!!]",
                think: "...",
            }
        } else {
            Self {
                tool: "⏺",
                result: "⎿",
                ok: "✓",
                err: "✗",
                think: "✻",
            }
        }
    }
}

/// 渲染任务：消费事件直到通道关闭。
pub async fn render_task(mut rx: UnboundedReceiver<Event>) {
    let symbols = Symbols::detect();
    let multi = MultiProgress::new();
    let style = ProgressStyle::with_template("{spinner} {msg}")
        .expect("spinner 模板错误")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ");
    let mut spinner: Option<ProgressBar> = None;
    let mut bars: HashMap<String, ProgressBar> = HashMap::new();
    let mut unpriced_warned = false;

    while let Some(event) = rx.recv().await {
        match event {
            Event::UsageRecorded(record) => {
                // 有用的消费：无价格预设时提示一次（R6 的"清晰展示"）
                if !record.priced && !unpriced_warned {
                    unpriced_warned = true;
                    println!(
                        "{}",
                        "  该模型无价格预设，费用将记 0（仅计 token）；可在 config.toml [[prices]] 补充".with(Color::Yellow)
                    );
                }
            }
            Event::TextDelta(text) => {
                print!("{text}");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
            }
            Event::ThinkingDelta(text) => {
                print!(
                    "{}",
                    text.with(Color::DarkGrey).attribute(Attribute::Italic)
                );
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
            }
            Event::ThinkingFinished { seconds } => {
                println!(
                    "{}",
                    format!("{0} 已思考 {seconds}s", symbols.think).with(Color::DarkGrey)
                );
            }
            Event::ToolStarted { name, args_summary } => {
                println!(
                    "{}",
                    format!("{} {}({args_summary})", symbols.tool, name).with(Color::Cyan)
                );
                let pb = multi.add(ProgressBar::new_spinner());
                pb.set_style(style.clone());
                pb.set_message(format!("{name} 运行中…（Ctrl-C 打断）"));
                pb.enable_steady_tick(std::time::Duration::from_millis(120));
                spinner = Some(pb);
            }
            Event::ToolFinished {
                ok,
                summary,
                duration_ms,
            } => {
                if let Some(pb) = spinner.take() {
                    pb.finish_and_clear();
                }
                // 下载进度条一并清理
                for (_, bar) in bars.drain() {
                    bar.finish_and_clear();
                }
                let mark = if ok {
                    symbols.ok.with(Color::Green)
                } else {
                    symbols.err.with(Color::Red)
                };
                let secs = duration_ms as f64 / 1000.0;
                println!(
                    "  {} {} {}",
                    symbols.result.with(Color::DarkGrey),
                    mark,
                    format!("{summary}（{secs:.1}s）").with(Color::DarkGrey)
                );
            }
            Event::Progress { label, done, total } => {
                let multi_for_closure = multi.clone();
                let style_for_closure = style.clone();
                let label_for_closure = label.clone();
                let bar = bars.entry(label.clone()).or_insert_with(move || {
                    let pb = multi_for_closure.add(ProgressBar::new(total.unwrap_or(0)));
                    if total.is_none() {
                        pb.set_style(style_for_closure);
                    }
                    pb.set_message(label_for_closure);
                    pb
                });
                if let Some(total) = total {
                    bar.set_length(total);
                    bar.set_position(done);
                } else {
                    bar.set_message(format!("{label}（{done} 字节）"));
                }
            }
            Event::OutputLine(line) => {
                println!("{}", line.with(Color::DarkGrey));
            }
            Event::Notice(text) => {
                println!("{}", format!("  {text}").with(Color::DarkGrey));
            }
        }
    }
}
