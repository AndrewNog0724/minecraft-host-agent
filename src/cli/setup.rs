//! 配置向导（决议 D113/D132）：必填 3 项 + 可选 CurseForge Key + 连接测试。
//!
//! 可重复运行：读取已有配置作为默认值（回车保留）；`.env` 合并写入（只更新
//! 被修改的 Key，其余原样保留）。
//!
//! 注意：本模块运行在主 tokio 运行时内——dialoguer 是阻塞交互，必须放入
//! `spawn_blocking`；连接测试直接 `await`，**不得**自建运行时嵌套 block_on
//! （否则 panic："Cannot start a runtime from within a runtime"）。

use anyhow::Context;
use crossterm::style::{Color, Stylize};
use dialoguer::Input;
use std::collections::BTreeMap;

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

/// 向导问题的既有值集合（预填与保留语义的数据源）。
struct ExistingValues {
    endpoint: String,
    model: String,
    api_key: Option<String>,
    curseforge_key: Option<String>,
    natfrp_token: Option<String>,
}

/// 读取已有配置（config.toml + 环境变量；不存在时全部为空）。
fn load_existing() -> ExistingValues {
    let data_dir = crate::paths::shared_data_dir()
        .cloned()
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (endpoint, model) = match AppConfig::load(&data_dir) {
        Ok(loaded) if loaded.existed => (
            loaded.config.model.endpoint.clone(),
            loaded.config.model.model.clone(),
        ),
        _ => (String::new(), String::new()),
    };
    let api_key = std::env::var("MCHA_API_KEY").ok().filter(|v| !v.is_empty());
    let curseforge_key = std::env::var("MCHA_CURSEFORGE_KEY")
        .ok()
        .filter(|v| !v.is_empty());
    let natfrp_token = std::env::var("MCHA_NATFRP_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());
    ExistingValues {
        endpoint,
        model,
        api_key,
        curseforge_key,
        natfrp_token,
    }
}

/// 运行向导；返回是否成功保存了配置。
pub async fn run_setup(data_dir: &std::path::Path) -> anyhow::Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        println!("未检测到配置，且当前不是交互终端，无法运行向导。");
        println!(
            "请手工创建 {}（模板见 README），或设置 MCHA_DATA 后重试。",
            AppConfig::config_path(data_dir).display()
        );
        return Ok(false);
    }

    let existing = load_existing();
    let has_existing = !existing.endpoint.is_empty() || existing.api_key.is_some();

    // 交互问答（endpoint / 模型名 / API Key / 可选 CurseForge Key）：
    // dialoguer 阻塞式，整体放入阻塞线程池
    let answers = {
        tokio::task::spawn_blocking(move || -> anyhow::Result<WizardAnswers> {
            if has_existing {
                println!(
                    "{}",
                    "── MCHA 配置向导（检测到已有配置，回车保留当前值）──".with(Color::Cyan)
                );
            } else {
                println!("{}", "── MCHA 首次配置向导 ──".with(Color::Cyan));
            }
            println!("必填 3 项：API Endpoint、模型名、API Key；其余保持默认即可。");
            println!();

            // 1. endpoint：预设与当前值匹配则默认选中；否则落在"自定义"
            let mut items: Vec<String> = PRESETS
                .iter()
                .map(|(name, _, _)| name.to_string())
                .collect();
            items.push("自定义 Endpoint…".to_string());
            let matched_preset = PRESETS
                .iter()
                .position(|(_, url, _)| !existing.endpoint.is_empty() && *url == existing.endpoint);
            let default_selection = matched_preset.unwrap_or(if existing.endpoint.is_empty() {
                0
            } else {
                PRESETS.len()
            });
            let selection = dialoguer::Select::new()
                .with_prompt("选择 API 提供方")
                .items(&items)
                .default(default_selection)
                .interact()
                .context("选择被中断")?;
            let endpoint = if selection < PRESETS.len() {
                PRESETS[selection].1.to_string()
            } else {
                Input::<String>::new()
                    .with_prompt("API Endpoint（如 https://api.example.com/v1）")
                    .default(existing.endpoint.clone())
                    .show_default(!existing.endpoint.is_empty())
                    .interact_text()
                    .context("输入被中断")?
            };
            let default_model = if selection < PRESETS.len() {
                PRESETS[selection].2.to_string()
            } else {
                existing.model.clone()
            };

            // 2. 模型名
            let show_default = !default_model.is_empty();
            let model: String = Input::new()
                .with_prompt("模型名")
                .default(default_model)
                .show_default(show_default)
                .interact_text()
                .context("输入被中断")?;

            // 3. API Key（隐藏输入；已有值回车保留，输入新值则覆盖）
            let api_key = ask_secret(
                "API Key（输入不回显；将写入数据目录的 .env，不入仓库）",
                existing.api_key.as_deref(),
            )?;

            // 4. 可选：CurseForge API Key（默认跳过；配置时给申请指引）
            let curseforge_key = ask_curseforge_key(existing.curseforge_key.as_deref())?;

            // 5. 可选：樱花frp 访问密钥（默认跳过；注册 / 登录双入口指引，D136）
            let natfrp_token = ask_natfrp_token(existing.natfrp_token.as_deref())?;

            Ok(WizardAnswers {
                endpoint,
                model,
                api_key,
                curseforge_key,
                natfrp_token,
            })
        })
        .await
        .context("向导线程异常退出")??
    };

    // 写配置文件（带注释模板）与 .env（合并写入，保留其他 Key）
    ensure_dir(data_dir)?;
    let config_text = AppConfig::template(&answers.endpoint, &answers.model);
    let config_path = AppConfig::config_path(data_dir);
    std::fs::write(&config_path, config_text)
        .with_context(|| format!("写入配置失败：{}", config_path.display()))?;

    let mut env_updates: BTreeMap<String, Option<String>> = BTreeMap::new();
    env_updates.insert("MCHA_API_KEY".to_string(), Some(answers.api_key.clone()));
    for (env_key, choice) in [
        ("MCHA_CURSEFORGE_KEY", &answers.curseforge_key),
        ("MCHA_NATFRP_TOKEN", &answers.natfrp_token),
    ] {
        match choice {
            OptionalKeyChoice::Keep => {}
            OptionalKeyChoice::Set(value) => {
                env_updates.insert(env_key.to_string(), Some(value.clone()));
            }
            OptionalKeyChoice::Skip => {}
        }
    }
    merge_env_file(&AppConfig::env_path(data_dir), &env_updates)?;

    println!("{}", "配置已保存。正在连接测试…".with(Color::DarkGrey));

    // 连接测试（最小对话请求；在主运行时上直接 await）
    match connection_test(&answers.endpoint, &answers.model, &answers.api_key).await {
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

struct WizardAnswers {
    endpoint: String,
    model: String,
    api_key: String,
    curseforge_key: OptionalKeyChoice,
    natfrp_token: OptionalKeyChoice,
}

/// 可选密钥的三种处置（CurseForge Key / 樱花frp token 共用）。
enum OptionalKeyChoice {
    /// 已有值且用户回车保留（不触碰 .env）。
    Keep,
    /// 用户输入了新值。
    Set(String),
    /// 未配置 / 用户跳过（不触碰 .env）。
    Skip,
}

/// 隐藏输入的密钥问答：已有值时回车保留，输入新值则覆盖。
fn ask_secret(prompt: &str, existing: Option<&str>) -> anyhow::Result<String> {
    let prompt = match existing {
        Some(_) => format!("{prompt}（已设置，回车保留）"),
        None => prompt.to_string(),
    };
    let value = dialoguer::Password::new()
        .with_prompt(prompt)
        .allow_empty_password(existing.is_some())
        .interact()
        .context("输入被中断")?;
    match (existing, value.is_empty()) {
        (Some(current), true) => Ok(current.to_string()),
        _ => Ok(value),
    }
}

/// 可选的 CurseForge Key 步骤：默认跳过；选择配置时给分步申请指引。
/// 未配置时 CF 通道自动走国内镜像，功能完整可用——此步骤纯粹是官方 API 偏好。
fn ask_curseforge_key(existing: Option<&str>) -> anyhow::Result<OptionalKeyChoice> {
    let status = match existing {
        Some(_) => "已设置（回车保留）",
        None => "未配置（CF 自动走国内镜像，功能完整；配置 Key 可改走官方 API）",
    };
    let configure = dialoguer::Confirm::new()
        .with_prompt(format!("配置 CurseForge API Key？[{status}]"))
        .default(false)
        .interact()
        .context("选择被中断")?;
    if !configure {
        return Ok(match existing {
            Some(_) => OptionalKeyChoice::Keep,
            None => OptionalKeyChoice::Skip,
        });
    }
    println!("申请指引（免费，一次性）：");
    println!(
        "  1. 打开 {} 并登录 CurseForge 账号（终端内可 Ctrl+点击）",
        crate::cli::links::clickable("https://portal.curseforge.com/")
    );
    println!("  2. 进入 API Keys → 创建应用（名称随意，如 mcha）");
    println!("  3. 复制生成的 API Key 粘贴到下面");
    let value = dialoguer::Password::new()
        .with_prompt("CurseForge API Key（输入不回显；回车返回跳过）")
        .allow_empty_password(true)
        .interact()
        .context("输入被中断")?;
    if value.is_empty() {
        return Ok(match existing {
            Some(_) => OptionalKeyChoice::Keep,
            None => OptionalKeyChoice::Skip,
        });
    }
    Ok(OptionalKeyChoice::Set(value))
}

/// 可选的樱花frp 访问密钥步骤（D136）：默认跳过；注册 / 登录双入口 +
/// 实名认证 + 密钥获取的分步可点击指引（用户补充：登录入口不可省）。
fn ask_natfrp_token(existing: Option<&str>) -> anyhow::Result<OptionalKeyChoice> {
    let status = match existing {
        Some(_) => "已设置（回车保留）",
        None => "未配置（朋友跨网络联机需要；仅局域网玩可跳过）",
    };
    let configure = dialoguer::Confirm::new()
        .with_prompt(format!("配置樱花frp 访问密钥？[{status}]"))
        .default(false)
        .interact()
        .context("选择被中断")?;
    if !configure {
        return Ok(match existing {
            Some(_) => OptionalKeyChoice::Keep,
            None => OptionalKeyChoice::Skip,
        });
    }
    println!("申请指引（免费；已有账号从第 1 步登录即可）：");
    println!(
        "  1. 注册账号 {} 或登录已有账号 {}（终端内可 Ctrl+点击）",
        crate::cli::links::clickable("https://www.natfrp.com/auth/register"),
        crate::cli::links::clickable("https://www.natfrp.com/auth/login")
    );
    println!(
        "  2. 实名认证（建隧道硬前置，面板内操作）：{}",
        crate::cli::links::clickable("https://www.natfrp.com/user/")
    );
    println!(
        "  3. 查看/重置访问密钥：{}（截图注意打码，泄露请立即重置）",
        crate::cli::links::clickable("https://www.natfrp.com/user/profile")
    );
    let value = dialoguer::Password::new()
        .with_prompt("樱花frp 访问密钥（输入不回显；回车返回跳过）")
        .allow_empty_password(true)
        .interact()
        .context("输入被中断")?;
    if value.is_empty() {
        return Ok(match existing {
            Some(_) => OptionalKeyChoice::Keep,
            None => OptionalKeyChoice::Skip,
        });
    }
    Ok(OptionalKeyChoice::Set(value))
}

/// 合并写入 .env：仅更新 updates 中出现的键（None = 删除该行），其余行原样
/// 保留（含注释）；文件不存在时创建。D132：修复旧版整文件覆盖抹掉其他 Key。
/// pub(crate)：`/token` 命令（D136）复用同一合并语义。
pub(crate) fn merge_env_file(
    path: &std::path::Path,
    updates: &BTreeMap<String, Option<String>>,
) -> anyhow::Result<()> {
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let mut handled: Vec<String> = Vec::new();
    for line in &mut lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq) = trimmed.find('=') {
            let key = trimmed[..eq].trim().to_string();
            if let Some(update) = updates.get(&key) {
                match update {
                    Some(value) => *line = format!("{key}={value}"),
                    None => *line = String::new(),
                }
                handled.push(key);
            }
        }
    }
    for (key, value) in updates {
        if handled.contains(key) {
            continue;
        }
        if let Some(value) = value {
            lines.push(format!("{key}={value}"));
        }
    }
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    std::fs::write(path, lines.join("\n") + "\n")
        .with_context(|| format!("写入 .env 失败：{}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 最小对话请求：验证 endpoint / key / 模型名三要素。
///
/// async：调用方已在 tokio 运行时内，直接 await 即可——
/// 此处若自建运行时并 block_on 会嵌套 panic（本项目实测教训）。
pub async fn connection_test(
    endpoint: &str,
    model: &str,
    api_key: &str,
) -> anyhow::Result<(u128, String)> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_env_updates_target_keys_and_preserves_others() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".env");
        std::fs::write(
            &path,
            "MCHA_API_KEY=old-llm\nMCHA_CURSEFORGE_KEY=old-cf\n# 注释保留\n",
        )
        .unwrap();

        let mut updates = BTreeMap::new();
        updates.insert("MCHA_API_KEY".to_string(), Some("new-llm".to_string()));
        merge_env_file(&path, &updates).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("MCHA_API_KEY=new-llm"), "{text}");
        assert!(
            text.contains("MCHA_CURSEFORGE_KEY=old-cf"),
            "其余 Key 应保留：{text}"
        );
        assert!(text.contains("# 注释保留"), "{text}");
    }

    #[test]
    fn merge_env_creates_file_and_appends() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sub").join(".env");
        let mut updates = BTreeMap::new();
        updates.insert(
            "MCHA_CURSEFORGE_KEY".to_string(),
            Some("cf-key".to_string()),
        );
        merge_env_file(&path, &updates).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("MCHA_CURSEFORGE_KEY=cf-key"), "{text}");
    }
}
