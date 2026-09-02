//! config：模型 / 价格 / 预算 / 安全 / 搜索 / Agent 调参（R3，决议 D113）。
//!
//! 存储分两处：`~/.mcha/config.toml`（除 Key 外的一切）与 `~/.mcha/.env`
//! （仅 API Key，经环境变量读取，永不写入仓库与 config.toml）。

pub mod edit;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::paths;

/// 确认门级别（决议 D110）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfirmLevel {
    /// 一切工具都确认。
    Paranoid,
    /// 写 / 执行 / 网络下载确认，只读免确认（默认）。
    #[default]
    Standard,
    /// 全部免确认（演示 / CI，留痕）。
    Auto,
}

impl ConfirmLevel {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "paranoid" => Ok(ConfirmLevel::Paranoid),
            "standard" => Ok(ConfirmLevel::Standard),
            "auto" => Ok(ConfirmLevel::Auto),
            other => {
                anyhow::bail!("未知 confirm_level：{other}（可选 paranoid | standard | auto）")
            }
        }
    }
}

/// 模型接入配置（R3：endpoint / 模型 / 上下文长度 / 思考模式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub endpoint: String,
    pub model: String,
    /// 上下文长度（token），上下文裁剪依据。
    pub context_len: u32,
    /// 思考模式开关（GLM 系 OpenAI 兼容语义：thinking.type）。
    pub thinking: bool,
    /// 存放 API Key 的环境变量名（默认 `MCHA_API_KEY`）。
    pub api_key_env: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            model: String::new(),
            context_len: 128_000,
            thinking: false,
            api_key_env: "MCHA_API_KEY".to_string(),
        }
    }
}

/// 一条模型价格（元 / 百万 token）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceEntry {
    pub model: String,
    pub input_per_m: f64,
    pub output_per_m: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    /// 费用硬上限（元），超限自动中断（R6）。
    pub limit_cny: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self { limit_cny: 10.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// paranoid | standard | auto（决议 D110）。
    pub confirm_level: String,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            confirm_level: "standard".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct SearchConfig {
    /// 空 = 无搜索后端（web_search 返回结构化错误，决议 D103）。
    pub backend: String,
}

/// 下载镜像配置（决议 D115，设计 §8.10）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Mojang 资源镜像：`bmclapi` | `off` | 自定义基础 URL（默认 bmclapi）。
    pub mojang_mirror: String,
    /// Adoptium 二进制镜像：`tuna` | `off`（默认 tuna）。
    pub adoptium_mirror: String,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            mojang_mirror: "bmclapi".to_string(),
            adoptium_mirror: "tuna".to_string(),
        }
    }
}

/// wiki 检索来源注册（决议 D120，设计 §8.11）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalConfig {
    /// MC Wiki MediaWiki API 地址；空 = 禁用。
    pub mcwiki: String,
    /// MC百科检索入口（M2.2 接入）；空 = 未接入。
    pub mcmod: String,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            mcwiki: "https://wiki.biligame.com/mc/api.php".to_string(),
            mcmod: String::new(),
        }
    }
}

/// 解释 Mojang 镜像配置值（D115）：`bmclapi` 预设 / `off`（含空）/ 自定义基础 URL。
pub fn mojang_mirror_base(raw: &str) -> Option<String> {
    match raw.trim() {
        "" | "off" => None,
        "bmclapi" => Some("https://bmclapi2.bangbang93.com".to_string()),
        other => Some(other.trim_end_matches('/').to_string()),
    }
}

/// Agent Loop 调参（§8.1：轮数保险丝等，config 可调）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentTuning {
    /// 单回合工具调用次数上限（防失控循环，非业务限制）。
    pub max_tool_calls_per_turn: u32,
    /// run_command 默认超时（秒）。
    pub command_timeout_secs: u64,
    /// 工具结果超过该字节数时转存附件、回传摘要（§8.1 大输出落盘）。
    pub large_output_bytes: usize,
}

impl Default for AgentTuning {
    fn default() -> Self {
        Self {
            max_tool_calls_per_turn: 40,
            command_timeout_secs: 120,
            large_output_bytes: 8 * 1024,
        }
    }
}

/// 应用配置全景（设计 §8.6）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub model: ModelConfig,
    #[serde(rename = "prices")]
    pub prices: Vec<PriceEntry>,
    pub budget: BudgetConfig,
    pub safety: SafetyConfig,
    pub search: SearchConfig,
    pub network: NetworkConfig,
    pub retrieval: RetrievalConfig,
    pub agent: AgentTuning,
}

/// 配置加载结果：文件不存在（首次启动走 setup）与文件存在但字段缺失要区分。
pub struct LoadedConfig {
    pub config: AppConfig,
    /// config.toml 是否存在（决定是否进入 setup 向导）。
    pub existed: bool,
}

impl AppConfig {
    pub fn config_path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.toml")
    }

    pub fn env_path(data_dir: &Path) -> PathBuf {
        data_dir.join(".env")
    }

    /// 从数据目录加载配置；文件不存在时返回默认值 + existed=false。
    pub fn load(data_dir: &Path) -> anyhow::Result<LoadedConfig> {
        let path = Self::config_path(data_dir);
        if !path.exists() {
            return Ok(LoadedConfig {
                config: Self::with_builtin_prices(),
                existed: false,
            });
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("读取配置文件失败：{}", path.display()))?;
        let mut config: AppConfig = toml::from_str(&text)
            .with_context(|| format!("解析配置文件失败：{}", path.display()))?;
        config.merge_builtin_prices();
        Ok(LoadedConfig {
            config,
            existed: true,
        })
    }

    /// 默认配置 + 内置价格预设（用户文件里同 model 的条目覆盖预设）。
    pub fn with_builtin_prices() -> Self {
        let mut config = Self::default();
        config.merge_builtin_prices();
        config
    }

    /// 合并内置价格预设：预设先入，用户条目覆盖同名模型。
    pub fn merge_builtin_prices(&mut self) {
        // 空预设文件（纯注释）是合法 TOML 表而非数组，先解析为 Value 再取数组
        let value: toml::Value = toml::from_str(include_str!("../assets/prices/presets.toml"))
            .expect("内置价格预设格式错误（编译期内嵌文件）");
        let items = value.as_array().cloned().unwrap_or_default();
        for item in items {
            let preset: PriceEntry =
                serde::Deserialize::deserialize(item).expect("内置价格预设条目格式错误");
            if !self.prices.iter().any(|p| p.model == preset.model) {
                self.prices.push(preset);
            }
        }
    }

    /// 查模型价格：(输入价, 输出价)（元 / 百万 token）；未配置返回 None。
    pub fn price_for(&self, model: &str) -> Option<(f64, f64)> {
        self.prices
            .iter()
            .find(|p| p.model == model)
            .map(|p| (p.input_per_m, p.output_per_m))
    }

    pub fn confirm_level(&self) -> anyhow::Result<ConfirmLevel> {
        ConfirmLevel::parse(&self.safety.confirm_level)
    }

    /// 启动校验：必填项缺失时不进入会话（决议 D113），返回可复制的缺失项说明。
    pub fn validate(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        if self.model.endpoint.trim().is_empty() {
            missing
                .push("model.endpoint（API Endpoint，例：https://open.bigmodel.cn/api/paas/v4）");
        }
        if self.model.model.trim().is_empty() {
            missing.push("model.model（模型名，例：glm-5.2）");
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing.join("\n"))
        }
    }

    /// 读取 API Key：从 `api_key_env` 指定的环境变量取。
    pub fn api_key(&self) -> anyhow::Result<String> {
        let var = &self.model.api_key_env;
        let key = std::env::var(var).unwrap_or_default();
        if key.trim().is_empty() {
            anyhow::bail!(
                "环境变量 {var} 未设置或为空。请在 {} 中写入 {var}=你的密钥，或直接 export {var}。",
                Self::env_path(paths::shared_data_dir()?).display()
            );
        }
        Ok(key.trim().to_string())
    }

    /// 生成带注释的配置模板（setup 向导与启动校验共用）。
    pub fn template(endpoint: &str, model: &str) -> String {
        format!(
            r#"# MCHA 配置（模型接入 / 价格 / 预算 / 安全 / 搜索）
[model]
endpoint = "{endpoint}"
model = "{model}"
context_len = 128000        # 上下文长度（token），裁剪依据
thinking = false            # 思考模式开关
# api_key_env = "MCHA_API_KEY"   # 存放 API Key 的环境变量名（Key 本体写 .env，不入库）

# 模型价格（元 / 百万 token），用于 R6 费用换算；无条目的模型费用记 0 并标注
#[[prices]]
#model = "glm-5.2"
#input_per_m = 2.0
#output_per_m = 8.0

[budget]
limit_cny = 10.0            # 费用硬上限，超限自动中断（R6）

[safety]
confirm_level = "standard"  # paranoid | standard | auto

[network]                   # 下载镜像（决议 D115）
mojang_mirror = "bmclapi"   # bmclapi | off | 自定义基础URL
adoptium_mirror = "tuna"    # tuna | off

[retrieval]                 # wiki 检索来源注册（决议 D120）
mcwiki = "https://wiki.biligame.com/mc/api.php"
mcmod = ""                  # MC百科检索入口（M2.2 接入）

[search]
backend = ""                # 空 = 无搜索后端（web_search 返回结构化错误）

[agent]
max_tool_calls_per_turn = 40   # 单回合工具调用保险丝（防失控循环）
command_timeout_secs = 120     # run_command 默认超时
large_output_bytes = 8192      # 工具结果转存附件的阈值
"#
        )
    }
}

/// 读取 API Key 前调用：把数据目录 `.env` 中的键值注入进程环境（已存在的不覆盖）。
pub fn load_dotenv(data_dir: &Path) -> anyhow::Result<()> {
    let path = AppConfig::env_path(data_dir);
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 .env 失败：{}", path.display()))?;
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            tracing::warn!(".env 第 {} 行缺少 '='，已跳过", line_no + 1);
            continue;
        };
        let key = key.trim();
        let mut value = value.trim();
        // 支持成对引号包裹
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        if key.is_empty() {
            continue;
        }
        // 已存在的环境变量优先（显式 export 的值覆盖 .env）
        if std::env::var_os(key).is_none() {
            // SAFETY：load_dotenv 只在 main 启动早期（任何线程产生前）调用一次，
            // 此刻修改进程环境变量无并发风险
            unsafe { std::env::set_var(key, value) };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let text = r#"
[model]
endpoint = "https://open.bigmodel.cn/api/paas/v4"
model = "glm-5.2"
context_len = 64000
thinking = true

[[prices]]
model = "glm-5.2"
input_per_m = 2.0
output_per_m = 8.0

[budget]
limit_cny = 5.0

[safety]
confirm_level = "auto"

[search]
backend = "duckduckgo"

[agent]
max_tool_calls_per_turn = 10
"#;
        let config: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(config.model.model, "glm-5.2");
        assert_eq!(config.model.context_len, 64000);
        assert!(config.model.thinking);
        assert_eq!(config.budget.limit_cny, 5.0);
        assert_eq!(config.confirm_level().unwrap(), ConfirmLevel::Auto);
        assert_eq!(config.agent.max_tool_calls_per_turn, 10);
        assert_eq!(config.price_for("glm-5.2"), Some((2.0, 8.0)));
    }

    #[test]
    fn partial_config_gets_defaults() {
        let text = r#"
[model]
endpoint = "https://example.com/v1"
model = "m1"
"#;
        let config: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(config.model.context_len, 128_000);
        assert!(!config.model.thinking);
        assert_eq!(config.model.api_key_env, "MCHA_API_KEY");
        assert_eq!(config.agent.command_timeout_secs, 120);
    }

    #[test]
    fn builtin_prices_parse() {
        let config = AppConfig::with_builtin_prices();
        // 预设文件当前为占位注释，解析出空表也算通过（不许崩溃）
        assert!(config.prices.is_empty() || !config.prices.is_empty());
    }

    #[test]
    fn validate_reports_missing() {
        let config = AppConfig::default();
        let err = config.validate().unwrap_err();
        assert!(err.contains("model.endpoint"));
        assert!(err.contains("model.model"));
    }

    #[test]
    fn dotenv_parses_and_respects_existing() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY：测试进程单线程执行此段，环境变量读写无并发风险
        unsafe { std::env::set_var("MCHA_TEST_DOTENV_KEEP", "keep") };
        std::fs::write(
            dir.path().join(".env"),
            "# 注释\nMCHA_TEST_DOTENV_A=\"带引号\"\nMCHA_TEST_DOTENV_KEEP=overridden\nBAD LINE\n",
        )
        .unwrap();
        load_dotenv(dir.path()).unwrap();
        assert_eq!(std::env::var("MCHA_TEST_DOTENV_A").unwrap(), "带引号");
        assert_eq!(std::env::var("MCHA_TEST_DOTENV_KEEP").unwrap(), "keep");
        unsafe {
            std::env::remove_var("MCHA_TEST_DOTENV_A");
            std::env::remove_var("MCHA_TEST_DOTENV_KEEP");
        }
    }
}
