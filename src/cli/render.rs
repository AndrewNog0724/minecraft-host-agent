//! 会话界面的语义化渲染块（D107）：事件流 → 终端视觉块。
//!
//! 渲染器是事件总线的订阅者，仅维护界面级状态（板块间隔、折叠计数），
//! 不参与会话状态（用量累计在 Session）。显示层的折叠 / 省略只影响终端
//! 输出：完整内容照常落盘（R5）、照常回传模型。
//! 不支持 Unicode 符号的环境可设 `MCHA_ASCII=1` 降级 ASCII 符号集（§8.6）。

use std::collections::HashMap;
use std::io::Write as _;

use crossterm::style::{Attribute, Color, Stylize};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::events::Event;

/// 思考流最多直显的行数，超出折叠（仅影响显示）。
const THINKING_MAX_LINES: usize = 4;
/// 思考流直显的字符数保险丝，应对整段无换行的思考（仅影响显示）。
const THINKING_MAX_CHARS: usize = 2000;
/// 每次工具调用最多直显的输出行数，超出省略（仅影响显示）。
const OUTPUT_MAX_LINES: usize = 12;

/// 当前渲染中的板块类型（跨板块时补空行，同板块连续内容不加）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Thinking,
    Text,
    Tool,
}

/// 渲染器的流式状态：只在单个回合的渲染任务内存活，纯界面用途。
#[derive(Default)]
struct StreamState {
    last_block: Option<Block>,
    /// 思考流是否已开启（收到首个增量时置位）。
    thinking_open: bool,
    /// 思考流的当前行是否未换行（结束时需要补一个换行）。
    thinking_line_open: bool,
    thinking_lines: usize,
    thinking_chars: usize,
    thinking_folded: bool,
    /// 助理文本流是否已开启（决定新文本块前是否补空行）。
    text_open: bool,
    /// 助理文本的当前行是否未换行（回合结束时补齐，避免粘连后续内容）。
    text_line_open: bool,
    /// 当前工具调用已直显 / 已省略的输出行数。
    out_lines_shown: usize,
    out_lines_hidden: usize,
}

impl StreamState {
    /// 进入新板块：回合内跨板块时先补一个空行；
    /// 本回合首个板块不补（REPL 在回合开始处已预留空行）。
    /// 若上一文本行未收尾，先补换行再留空行，避免粘连。
    fn enter_block(&mut self, block: Block) {
        if self.last_block.is_some() && self.last_block != Some(block) {
            if self.text_line_open {
                println!();
                self.text_line_open = false;
            }
            println!();
        }
        if block != Block::Text {
            self.text_open = false;
        }
        self.last_block = Some(block);
    }
}

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

/// 工具名 → 动词短语：给调用行配上自然语言解释，用户不用猜工具名含义。
fn tool_verb(name: &str) -> &'static str {
    match name {
        "run_command" => "运行命令",
        "read_file" => "读取文件",
        "write_file" => "写入文件",
        "edit_file" => "编辑文件",
        "list_dir" => "浏览目录",
        "http_get_text" => "抓取网页",
        "http_download" => "下载文件",
        "web_search" => "搜索网页",
        "ask_user" => "向你提问",
        "load_skill" => "加载技能",
        _ => "调用工具",
    }
}

/// 结果摘要压成单行（换行折叠为空格），避免结果行在终端上跨多行。
fn collapse_lines(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
    let mut state = StreamState::default();

    while let Some(event) = rx.recv().await {
        match event {
            Event::UsageRecorded(_) => {
                // 用量与"无价格预设"提示都不在会话中出现，
                // 统一在退出汇总一次性给出（决议 D108）
            }
            Event::ThinkingDelta(text) => {
                if !state.thinking_open {
                    state.enter_block(Block::Thinking);
                    print!("{} ", symbols.think.with(Color::DarkGrey));
                    state.thinking_open = true;
                    state.thinking_line_open = true;
                    state.thinking_lines = 0;
                    state.thinking_chars = 0;
                    state.thinking_folded = false;
                }
                if state.thinking_folded {
                    // 已折叠：静默丢弃后续增量（计量与落盘不受影响）
                    continue;
                }
                state.thinking_lines += text.matches('\n').count();
                state.thinking_chars += text.chars().count();
                // 换行数达到上限意味着第 MAX+1 行已经开始，立即折叠
                if state.thinking_lines >= THINKING_MAX_LINES
                    || state.thinking_chars > THINKING_MAX_CHARS
                {
                    state.thinking_folded = true;
                    state.thinking_line_open = false;
                    println!("\n  {}", "…（思考内容较长，已折叠）".with(Color::DarkGrey));
                } else {
                    print!(
                        "{}",
                        text.with(Color::DarkGrey).attribute(Attribute::Italic)
                    );
                }
                let _ = std::io::stdout().flush();
            }
            Event::ThinkingFinished { seconds } => {
                if state.thinking_line_open {
                    println!();
                }
                println!(
                    "  {}",
                    format!("{} 已思考 {seconds}s", symbols.result)
                        .with(Color::DarkGrey)
                        .attribute(Attribute::Italic)
                );
                state.thinking_open = false;
                state.thinking_line_open = false;
            }
            Event::TextDelta(text) => {
                if !state.text_open {
                    state.enter_block(Block::Text);
                    state.text_open = true;
                }
                print!("{text}");
                state.text_line_open = !text.ends_with('\n');
                let _ = std::io::stdout().flush();
            }
            Event::ToolStarted { name, args_summary } => {
                state.enter_block(Block::Tool);
                state.out_lines_shown = 0;
                state.out_lines_hidden = 0;
                println!(
                    "{} {} {}",
                    symbols.tool.with(Color::Cyan).attribute(Attribute::Bold),
                    tool_verb(&name)
                        .with(Color::Cyan)
                        .attribute(Attribute::Bold),
                    format!("{name}({args_summary})").with(Color::Cyan)
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
                let mut timing = format!("{secs:.1}s");
                if state.out_lines_hidden > 0 {
                    timing.push_str(&format!(" · 已省略 {} 行输出", state.out_lines_hidden));
                }
                let summary = collapse_lines(&summary);
                // 渲染层再截一刀：模型返回的完整摘要在会话日志里，界面上点到为止
                let summary = crate::agent::message::truncate_chars(&summary, 120);
                let summary = if ok {
                    format!("{summary}（{timing}）").with(Color::DarkGrey)
                } else {
                    format!("{summary}（{timing}）").with(Color::Red)
                };
                println!(
                    "  {} {} {}",
                    symbols.result.with(Color::DarkGrey),
                    mark,
                    summary
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
                if state.out_lines_shown < OUTPUT_MAX_LINES {
                    state.out_lines_shown += 1;
                    println!(
                        "  {} {}",
                        "│".with(Color::DarkGrey),
                        line.with(Color::DarkGrey)
                    );
                } else {
                    state.out_lines_hidden += 1;
                }
            }
            Event::Blank => {
                println!();
            }
            Event::Notice(text) => {
                println!("  · {}", text.with(Color::DarkGrey));
            }
        }
    }
    // 回合结束：助理文本若未以换行收尾，补一个换行（后续内容从新行开始）
    if state.text_line_open {
        println!();
    }
}
