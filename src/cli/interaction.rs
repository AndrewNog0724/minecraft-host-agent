//! 终端交互实现：确认门（y/a/n 单键）与 ask_user（dialoguer）。
//!
//! 交互与渲染器分属不同线程——交互激活期间通过共享的 `ui_active` 闸让
//! 渲染器停靠（打印前自旋等待），避免并发输出打乱 dialoguer 的绘制
//! （用户实测：提问提示语反复换行、键入内容被覆盖不可见）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers, read};
use crossterm::style::{Attribute, Color, Stylize};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::tools::{AskRequest, ConfirmDecision, ConfirmRequest, Interaction, InteractionError};

pub struct TerminalInteraction {
    /// 交互激活闸：true = 渲染器暂停打印。
    ui_active: Arc<AtomicBool>,
}

impl TerminalInteraction {
    pub fn new(ui_active: Arc<AtomicBool>) -> Self {
        Self { ui_active }
    }

    /// 作用域守卫：进入置位，任何退出路径复位。
    fn guard(&self) -> UiActiveGuard<'_> {
        self.ui_active.store(true, Ordering::SeqCst);
        UiActiveGuard {
            flag: &self.ui_active,
        }
    }
}

struct UiActiveGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for UiActiveGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

fn read_line_sync(prompt: &str) -> std::io::Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// 单键读取（raw 模式，按一下即生效、无需回车）。
/// 非 tty 环境（管道 / CI）enable_raw_mode 失败，由调用方退化为行读取。
fn read_confirm_key() -> std::io::Result<Option<char>> {
    enable_raw_mode()?;
    let picked = loop {
        match read() {
            // Windows 终端会同时上报按下与抬起：只认 Press，避免一次按键触发两次
            Ok(CtEvent::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char(c @ ('y' | 'Y' | 'a' | 'A' | 'n' | 'N')) => break Some(c),
                KeyCode::Esc => break None,
                // raw 模式下 Ctrl-C 不产生 SIGINT：转为取消
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break None,
                _ => {}
            },
            Ok(_) => {}
            Err(err) => {
                let _ = disable_raw_mode();
                return Err(err);
            }
        }
    };
    let _ = disable_raw_mode();
    Ok(picked.map(|c| c.to_ascii_lowercase()))
}

fn map_dialoguer_error<E: std::fmt::Display>(err: E) -> InteractionError {
    let text = err.to_string();
    // dialoguer 在 Ctrl-C / ESC 时报 interrupted / canceled
    if text.contains("interrupted") || text.contains("canceled") || text.contains("cancelled") {
        InteractionError::Cancelled
    } else {
        InteractionError::Failed(text)
    }
}

/// 框行展开：内容内嵌换行时拆为多行，保证每个物理行都带 `│ ` 框线前缀
/// （"替换为"这类多行内容不再破坏边框，v2.4 实测）。
pub(crate) fn expand_box_lines(lines: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();
    for line in lines {
        for part in line.split('\n') {
            expanded.push(part.to_string());
        }
    }
    expanded
}

#[async_trait::async_trait]
impl Interaction for TerminalInteraction {
    /// 确认门（D110）：y 本次允许 / a 本会话允许此工具 / n 拒绝，单键确认。
    async fn confirm(&self, req: ConfirmRequest) -> Result<ConfirmDecision, InteractionError> {
        let _guard = self.guard();
        tokio::task::spawn_blocking(move || {
            println!(
                "{}",
                format!("┌─ {}", req.title)
                    .with(Color::Yellow)
                    .attribute(Attribute::Bold)
            );
            for line in expand_box_lines(&req.lines) {
                println!("{}", format!("│ {line}").with(Color::Yellow));
            }
            println!("{}", "└─".with(Color::Yellow));
            // 引导词：单键确认无行提示，必须明示可选按键（用户实测反馈）
            println!(
                "{}",
                "请按一个键：[y] 本次允许 · [a] 本会话允许此工具 · [n] 拒绝 · [Esc/Ctrl-C] 取消"
                    .with(Color::Yellow)
            );
            // 首选单键确认；raw 模式不可用时退化为行读取（管道 / 重定向输入）
            match read_confirm_key() {
                Ok(Some(c)) => {
                    println!("{}", c.to_string().with(Color::Green));
                    return Ok(match c {
                        'a' => ConfirmDecision::AllowAlways,
                        'n' => ConfirmDecision::Deny,
                        _ => ConfirmDecision::Allow,
                    });
                }
                Ok(None) => return Err(InteractionError::Cancelled),
                Err(_) => {}
            }
            loop {
                let line =
                    read_line_sync("允许本次操作？[y] 本次 / [a] 本会话允许此工具 / [n] 拒绝：")
                        .map_err(|err| InteractionError::Failed(err.to_string()))?;
                match line.trim().to_ascii_lowercase().as_str() {
                    "y" | "yes" | "" => return Ok(ConfirmDecision::Allow),
                    "a" | "always" => return Ok(ConfirmDecision::AllowAlways),
                    "n" | "no" => return Ok(ConfirmDecision::Deny),
                    other => {
                        println!(
                            "{}",
                            format!("无法识别「{other}」，请输入 y / a / n").with(Color::DarkGrey)
                        );
                    }
                }
            }
        })
        .await
        .map_err(|err| InteractionError::Failed(err.to_string()))?
    }

    /// ask_user：选项列表（可自由输入）或开放文本。
    async fn ask(&self, req: AskRequest) -> Result<String, InteractionError> {
        let _guard = self.guard();
        tokio::task::spawn_blocking(move || {
            if req.options.is_empty() {
                return dialoguer::Input::<String>::new()
                    .with_prompt(format!("◇ {}", req.question))
                    .allow_empty(true)
                    .interact_text()
                    .map_err(map_dialoguer_error);
            }
            let mut items: Vec<String> = req.options.clone();
            if req.allow_free_text {
                items.push("✎ 自由输入…".to_string());
            }
            let selection = dialoguer::Select::new()
                .with_prompt(format!("◇ {}", req.question))
                .items(&items)
                .default(0)
                .interact()
                .map_err(map_dialoguer_error)?;
            if req.allow_free_text && selection == req.options.len() {
                dialoguer::Input::<String>::new()
                    .with_prompt("  你的回答")
                    .allow_empty(true)
                    .interact_text()
                    .map_err(map_dialoguer_error)
            } else {
                Ok(items[selection].clone())
            }
        })
        .await
        .map_err(|err| InteractionError::Failed(err.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_lines_expand_embedded_newlines() {
        let lines = vec![
            "文件：server/whitelist.json".to_string(),
            "替换为：[{\"name\":\"A\"},\n{\"name\":\"B\"}]\n]".to_string(),
        ];
        let expanded = expand_box_lines(&lines);
        assert_eq!(expanded.len(), 4);
        assert!(expanded.iter().all(|l| !l.contains('\n')), "{expanded:?}");
        assert_eq!(expanded[2], "{\"name\":\"B\"}]");
        assert_eq!(expanded[3], "]");
    }
}
