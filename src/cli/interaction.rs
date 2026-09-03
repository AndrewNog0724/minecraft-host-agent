//! 终端交互实现：确认门（y/a/n 单键）与 ask_user（dialoguer）。

use crossterm::event::{Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers, read};
use crossterm::style::{Attribute, Color, Stylize};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::tools::{AskRequest, ConfirmDecision, ConfirmRequest, Interaction, InteractionError};

pub struct TerminalInteraction;

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

#[async_trait::async_trait]
impl Interaction for TerminalInteraction {
    /// 确认门（D110）：y 本次允许 / a 本会话允许此工具 / n 拒绝，单键确认。
    async fn confirm(&self, req: ConfirmRequest) -> Result<ConfirmDecision, InteractionError> {
        tokio::task::spawn_blocking(move || {
            println!(
                "{}",
                format!("┌─ {}", req.title)
                    .with(Color::Yellow)
                    .attribute(Attribute::Bold)
            );
            for line in &req.lines {
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
