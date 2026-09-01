//! 终端交互实现：确认门（y/a/n）与 ask_user（dialoguer）。

use crossterm::style::{Attribute, Color, Stylize};

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
    /// 确认门（D110）：y 本次允许 / a 本会话允许此工具 / n 拒绝。
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
