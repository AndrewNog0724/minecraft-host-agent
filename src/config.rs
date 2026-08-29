//! 应用配置（R3）：模型、价格表、预算、网络代理与镜像、穿透 token。
//!
//! 来源：`<数据目录>/config.toml` + `.env`（仅 API Key）。
//! 数据目录：`~/.mc-host-agent/`（Windows：`%APPDATA%\mc-host-agent\`，决议 D4）。

use std::fmt;
use std::path::{Path, PathBuf};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const ENV_API_KEY: &str = "AGENT_API_KEY";
pub const CONFIG_FILE: &str = "config.toml";
pub const ENV_FILE: &str = ".env";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("读取配置文件 {path} 失败：{source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("解析配置文件 {path} 失败：{source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("写入配置文件 {path} 失败：{source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("配置缺少必填项：{0}（可运行 `agent config init` 生成模板后填写）")]
    Missing(String),
    #[error("配置项取值非法：{0}")]
    Invalid(String),
}

/// 模型配置（R3 核心项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// OpenAI 兼容 Chat API 地址（如 https://open.bigmodel.cn/api/paas/v4）
    #[serde(default)]
    pub endpoint: String,
    /// 模型名（如 glm-5.2）
    #[serde(default)]
    pub model: String,
    /// API Key 所在环境变量名（默认 AGENT_API_KEY，值放 .env）
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// 上下文长度（token）：发送前按此裁剪历史
    #[serde(default = "default_context_len")]
    pub context_len: u32,
    /// 思考模式开关（透传为请求体 thinking 字段，按 endpoint 兼容性）
    #[serde(default)]
    pub thinking: bool,
    /// 请求超时秒数
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_api_key_env() -> String {
    ENV_API_KEY.to_string()
}

fn default_context_len() -> u32 {
    128_000
}

fn default_timeout_secs() -> u64 {
    300
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            model: String::new(),
            api_key_env: default_api_key_env(),
            context_len: default_context_len(),
            thinking: false,
            timeout_secs: default_timeout_secs(),
        }
    }
}

/// 单个模型的价格条目：每 1M token 的单价。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceEntry {
    pub model: String,
    pub input_per_m: Decimal,
    pub output_per_m: Decimal,
    #[serde(default = "default_currency")]
    pub currency: String,
}

fn default_currency() -> String {
    "CNY".to_string()
}

/// 预算上限（R6）：超限自动中断任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// 费用上限；0 表示不限制
    #[serde(default)]
    pub limit: Decimal,
    #[serde(default = "default_currency")]
    pub currency: String,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            limit: Decimal::ZERO,
            currency: default_currency(),
        }
    }
}

/// 网络配置：全局代理 + 官方域镜像替换（§12 下载安全：镜像仅替换白名单内域名）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkConfig {
    /// HTTP(S) 代理，如 http://127.0.0.1:7890；空 = 直连
    #[serde(default)]
    pub proxy: String,
    /// 域名镜像映射：官方域 → 镜像域（如 piston-meta → 国内镜像）
    #[serde(default)]
    pub mirrors: Vec<MirrorRule>,
    /// Adoptium JRE 下载镜像根地址（§8.8，国内推荐清华 TUNA：
    /// https://mirrors.tuna.tsinghua.edu.cn/Adoptium）；空 = 仅官方渠道
    #[serde(default)]
    pub adoptium_mirror: String,
}

/// 一条镜像规则：把对 `from` 的请求改写到 `to`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorRule {
    pub from: String,
    pub to: String,
}

/// 穿透配置（P1 樱花frp；MVP 只保留 token 字段）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TunnelConfig {
    /// 樱花frp 访问密钥（仅存本地，导出打码）
    #[serde(default)]
    pub natfrp_token: String,
}

/// 聚合配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub model: ModelConfig,
    /// 用户自定义价格表，优先于内置预设
    #[serde(default, rename = "prices")]
    pub prices: Vec<PriceEntry>,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub tunnel: TunnelConfig,
}

/// 数据目录定位（决议 D4）。
pub fn data_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("MC_HOST_AGENT_DATA") {
        return PathBuf::from(custom);
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        // Windows：%APPDATA%\mc-host-agent
        return Path::new(&appdata).join("mc-host-agent");
    }
    // Linux / macOS：~/.mc-host-agent
    match std::env::var("HOME") {
        Ok(home) => Path::new(&home).join(".mc-host-agent"),
        Err(_) => PathBuf::from(".mc-host-agent"),
    }
}

pub fn config_path() -> PathBuf {
    data_dir().join(CONFIG_FILE)
}

impl AppConfig {
    /// 加载配置：确保数据目录存在；config.toml 缺失时生成带注释模板；
    /// 同时加载 .env（存在则注入进程环境，供 API Key 读取）。
    pub fn load() -> Result<Self, ConfigError> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir).map_err(|source| ConfigError::Read {
            path: dir.clone(),
            source,
        })?;

        let env_path = dir.join(ENV_FILE);
        load_dotenv(&env_path);

        let path = config_path();
        if !path.exists() {
            let template = render_template();
            std::fs::write(&path, template).map_err(|source| ConfigError::Write {
                path: path.clone(),
                source,
            })?;
            tracing::info!("已生成配置模板 {}", path.display());
        }

        let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        // 注意：加载不做校验——`agent plan` 等不需要 LLM 的流程不应被模型配置阻塞；
        // 需要 LLM 的流程在入口处显式调用 validate()
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })
    }

    /// 启动校验：缺项报错并给出可执行的下一步（§8.7）。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.model.endpoint.trim().is_empty() {
            return Err(ConfigError::Missing(
                "model.endpoint（模型 API 地址）".into(),
            ));
        }
        if self.model.model.trim().is_empty() {
            return Err(ConfigError::Missing("model.model（模型名称）".into()));
        }
        if self.api_key().map(|k| k.trim().is_empty()).unwrap_or(true) {
            return Err(ConfigError::Missing(format!(
                "{}（API Key，写在 {} 中）",
                self.model.api_key_env,
                data_dir().join(ENV_FILE).display()
            )));
        }
        if self.model.context_len == 0 {
            return Err(ConfigError::Invalid("model.context_len 必须大于 0".into()));
        }
        Ok(())
    }

    /// 从进程环境读取 API Key（.env 已在加载时注入）。
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.model.api_key_env).ok()
    }

    /// 价格查询：用户自定义优先，其次内置预设。
    pub fn rate_for(&self, model: &str) -> Option<PriceEntry> {
        self.prices
            .iter()
            .find(|p| p.model == model)
            .cloned()
            .or_else(|| builtin_price(model))
    }
}

/// 极简 .env 加载：KEY=VALUE 逐行，# 开头为注释；已存在的环境变量不覆盖。
fn load_dotenv(path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        // 安全性：仅在进程启动早期、尚未派生任何线程前调用（见 load_dotenv 的调用点），
        // 此时修改进程环境不会与其它线程的 getenv 竞争。
        if std::env::var_os(key).is_none() {
            unsafe { std::env::set_var(key, value) };
        }
    }
}

/// 内置价格预设（决议 D3）。单位：每 1M token。
/// 来源：各厂商官网公布价，采集日期 2026-08-28；随包分发，用户可覆盖。
fn builtin_price(model: &str) -> Option<PriceEntry> {
    fn presets() -> Vec<PriceEntry> {
        vec![
            // GLM 系列（bigmodel.cn 官网价，CNY）
            entry("glm-5.2", "2", "8", "CNY"),
            entry("glm-5.2-flash", "0", "0", "CNY"),
            entry("glm-4.7", "2", "8", "CNY"),
            entry("glm-4.5-air", "0.2", "2", "CNY"),
            // DeepSeek 官网价（CNY）
            entry("deepseek-chat", "2", "8", "CNY"),
            entry("deepseek-reasoner", "4", "16", "CNY"),
            // OpenAI 官网价（USD）
            entry("gpt-4o-mini", "0.15", "0.6", "USD"),
            entry("gpt-4o", "2.5", "10", "USD"),
        ]
    }
    let model_lower = model.to_lowercase();
    presets().into_iter().find(|p| p.model == model_lower)
}

fn entry(model: &str, input: &str, output: &str, currency: &str) -> PriceEntry {
    PriceEntry {
        model: model.to_string(),
        input_per_m: input.parse().unwrap_or(Decimal::ZERO),
        output_per_m: output.parse().unwrap_or(Decimal::ZERO),
        currency: currency.to_string(),
    }
}

/// 生成带注释的配置模板（缺项时给用户可复制的样例）。
fn render_template() -> String {
    format!(
        r#"# mc-host-agent 配置文件（R3）
# 完整说明见 README「配置」一节。改动后保存即生效（下次运行读取）。

[model]
# OpenAI 兼容 Chat API 地址（不含 /chat/completions 后缀）
endpoint = "https://open.bigmodel.cn/api/paas/v4"
# 模型名称
model = "glm-5.2"
# API Key 的环境变量名；实际值写在本目录 .env 文件里（不进仓库）
api_key_env = "{env_key}"
# 上下文长度（token），发送前按此裁剪会话历史
context_len = 128000
# 思考模式（部分模型支持，如 GLM；不支持时请保持 false）
thinking = false
# 单次请求超时（秒）
timeout_secs = 300

# 价格表：每 1M token 单价。内置常见模型预设（GLM/DeepSeek/OpenAI，
# 采集日期 2026-08-28，来源为各厂商官网）；此处可覆盖或补充。
# 示例（去掉行首 # 即生效）：
# [[prices]]
# model = "my-model"
# input_per_m = 2.0
# output_per_m = 8.0
# currency = "CNY"

# 预算上限（R6）：本次安装累计费用达到上限后自动中断任务；0 = 不限制。
[budget]
limit = 0
currency = "CNY"

# 网络配置：代理与官方域镜像（国内网络不可达时启用）。
[network]
proxy = ""
# Adoptium JRE 下载镜像（§8.8）。国内强烈建议清华 TUNA 镜像（官方 GitHub 渠道
# 在国内经常不可达）：取消下一行注释即启用。
# adoptium_mirror = "https://mirrors.tuna.tsinghua.edu.cn/Adoptium"
# 镜像规则示例：
# [[network.mirrors]]
# from = "piston-meta.mojang.com"
# to = "mirror.example.com/piston-meta"

# 内网穿透（P1 樱花frp）：访问密钥见管理面板，仅存本地。
[tunnel]
natfrp_token = ""
"#,
        env_key = ENV_API_KEY,
    )
}

impl fmt::Display for AppConfig {
    /// 展示用（打码密钥，NFR-2）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "[model]")?;
        writeln!(f, "endpoint = {}", self.model.endpoint)?;
        writeln!(f, "model = {}", self.model.model)?;
        writeln!(f, "api_key_env = {}", self.model.api_key_env)?;
        writeln!(f, "context_len = {}", self.model.context_len)?;
        writeln!(f, "thinking = {}", self.model.thinking)?;
        writeln!(f, "[budget]")?;
        writeln!(f, "limit = {} {}", self.budget.limit, self.budget.currency)?;
        writeln!(f, "[network]")?;
        writeln!(
            f,
            "proxy = {}",
            if self.network.proxy.is_empty() {
                "<直连>"
            } else {
                "<已配置>"
            }
        )?;
        writeln!(f, "mirror 规则数 = {}", self.network.mirrors.len())?;
        writeln!(f, "[tunnel]")?;
        write!(
            f,
            "natfrp_token = {}",
            if self.tunnel.natfrp_token.is_empty() {
                "<未配置>"
            } else {
                "<已配置>"
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 内置价格可查且用户覆盖优先() {
        let mut cfg = AppConfig::default();
        assert!(cfg.rate_for("glm-5.2").is_some(), "内置预设应含 glm-5.2");
        assert!(cfg.rate_for("unknown-model").is_none());

        cfg.prices.push(entry("glm-5.2", "9", "9", "CNY"));
        let rate = cfg.rate_for("glm-5.2").unwrap();
        assert_eq!(rate.input_per_m, Decimal::from(9));
    }

    #[test]
    fn 校验缺项报错() {
        let cfg = AppConfig::default();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("endpoint"));
    }

    #[test]
    fn 模板可被解析() {
        let cfg: AppConfig = toml::from_str(&render_template()).unwrap();
        assert_eq!(cfg.model.model, "glm-5.2");
        assert_eq!(cfg.model.context_len, 128_000);
    }
}
