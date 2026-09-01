//! 首次启动配置向导（决议 D113）：必填 3 项 + 连接测试。

use anyhow::Context;
use crossterm::style::{Color, Stylize};
use dialoguer::Input;

use crate::agent::message::Message;
use crate::config::AppConfig;
use crate::llm::{ChatRequest, LlmClient, OpenAiCompatClient};
use crate::store::ensure_dir;

/// endpoint 预设快捷项。
const PRESETS: &[(&str, &str, &str)] = &[
    (
        "智谱 GLM",
        "https://open.bigmodel.cn/api/paas/v4",
        "glm-5.2",
    ),
    ("DeepSeek", "https://api.deepseek.com/v1", "deepseek-chat"),
    ("OpenAI", "https://api.openai.com/v1", "gpt-4o-mini"),
];

/// 运行向导；返回是否成功保存了配置。
pub fn run_setup(data_dir: &std::path::Path) -> anyhow::Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        println!("未检测到配置，且当前不是交互终端，无法运行向导。");
        println!(
            "请手工创建 {}（模板见 README），或设置 MCHA_DATA 后重试。",
            AppConfig::config_path(data_dir).display()
        );
        return Ok(false);
    }

    println!("{}", "── MCHA 首次配置向导 ──".with(Color::Cyan));
    println!("必填 3 项：API Endpoint、模型名、API Key；其余保持默认即可。");
    println!();

    // 1. endpoint
    let mut items: Vec<String> = PRESETS
        .iter()
        .map(|(name, _, _)| name.to_string())
        .collect();
    items.push("自定义 Endpoint…".to_string());
    let selection = dialoguer::Select::new()
        .with_prompt("选择 API 提供方")
        .items(&items)
        .default(0)
        .interact()
        .context("选择被中断")?;
    let endpoint = if selection < PRESETS.len() {
        PRESETS[selection].1.to_string()
    } else {
        Input::<String>::new()
            .with_prompt("API Endpoint（如 https://api.example.com/v1）")
            .interact_text()
            .context("输入被中断")?
    };
    let default_model = if selection < PRESETS.len() {
        PRESETS[selection].2.to_string()
    } else {
        String::new()
    };

    // 2. 模型名
    let show_default = !default_model.is_empty();
    let model: String = Input::new()
        .with_prompt("模型名")
        .default(default_model)
        .show_default(show_default)
        .interact_text()
        .context("输入被中断")?;

    // 3. API Key（隐藏输入，写 .env）
    let api_key: String = dialoguer::Password::new()
        .with_prompt("API Key（输入不回显；将写入数据目录的 .env，不入仓库）")
        .interact()
        .context("输入被中断")?;

    // 写配置文件（带注释模板）与 .env
    ensure_dir(data_dir)?;
    let config_text = AppConfig::template(&endpoint, &model);
    let config_path = AppConfig::config_path(data_dir);
    std::fs::write(&config_path, config_text)
        .with_context(|| format!("写入配置失败：{}", config_path.display()))?;
    let env_path = AppConfig::env_path(data_dir);
    std::fs::write(&env_path, format!("MCHA_API_KEY={api_key}\n"))
        .with_context(|| format!("写入 .env 失败：{}", env_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
    }

    println!("{}", "配置已保存。正在连接测试…".with(Color::DarkGrey));

    // 连接测试（最小对话请求）
    match connection_test(&endpoint, &model, &api_key) {
        Ok((latency_ms, reply)) => {
            println!(
                "{}",
                format!("✓ 连接成功（{latency_ms} ms），模型应答：{reply}").with(Color::Green)
            );
            println!("现在可以开始会话了。预算等高级选项见 config.toml 注释。");
            Ok(true)
        }
        Err(err) => {
            println!("{}", format!("✗ 连接测试失败：{err}").with(Color::Red));
            println!("配置已保存，但请检查后修正：");
            println!("  mcha config set model.endpoint <地址>");
            println!("  mcha config set model.model <模型名>");
            println!(
                "或重新编辑 {} 后运行 mcha config test 重测。",
                config_path.display()
            );
            Ok(true)
        }
    }
}

/// 最小对话请求：验证 endpoint / key / 模型名三要素。
pub fn connection_test(
    endpoint: &str,
    model: &str,
    api_key: &str,
) -> anyhow::Result<(u128, String)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建运行时失败")?;
    runtime.block_on(async {
        let client = OpenAiCompatClient::new(endpoint.to_string(), api_key.to_string())?;
        let request = ChatRequest {
            model: model.to_string(),
            messages: vec![Message::user("这是连接测试，请只回复：OK")],
            tools: vec![],
            thinking: false,
        };
        let started = std::time::Instant::now();
        let response = client
            .chat(request, None)
            .await
            .map_err(|failure| anyhow::anyhow!("{failure}"))?;
        let latency = started.elapsed().as_millis();
        let reply = response
            .reply
            .content
            .unwrap_or_else(|| "（模型无文本回复）".to_string());
        Ok((latency, crate::agent::message::truncate_chars(&reply, 80)))
    })
}
