//! 核心数据结构：`ServerSpec` 与 `ServerSpecDraft`（§8.1）。
//!
//! 要点：LLM 只能产出 [`ServerSpecDraft`]（未校验态），
//! [`ServerSpec`] 的唯一构造者是 provision 的决策树引擎——
//! 结构上保证设计原则 1/2（副作用不出 Rust、能查就不猜）。

use chrono::{DateTime, Local};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 一次开服方案的完整描述（R5 档案主体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSpec {
    /// 语义化 id（如 "twilight-5p"），档案文件名与隧道命名都用它
    pub spec_id: String,
    pub created_at: DateTime<Local>,
    pub account: AccountPolicy,
    pub software: ServerSoftware,
    /// 语义化版本号，必须经 knowledge 校验存在
    pub mc_version: String,
    pub java: JavaPlan,
    /// 决策树按玩家数与机器内存推导的 JVM 上限
    pub jvm_memory_mb: u32,
    /// mod 依赖闭包解析结果（流水线安装阶段回填）
    pub mods: Vec<ModRef>,
    /// 玩家原始提到的 mod 名称（可能为中文别名；流水线按此解析到 mods）
    #[serde(default)]
    pub mod_names: Vec<String>,
    pub network: NetworkPlan,
    pub world: WorldPlan,
    /// 风险与注意事项，ui 必须展示
    pub notes: Vec<String>,
    /// 服务端工作目录（执行流水线回填）
    #[serde(default)]
    pub server_dir: Option<String>,
    /// 服务端监听端口
    pub port: u16,
    /// 最大玩家数
    pub max_players: u32,
}

/// 账号策略（决策树节点 1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountPolicy {
    /// 全正版：online-mode=true
    Online,
    /// 全离线：online-mode=false + 白名单必选
    Offline { whitelist: Vec<String> },
    /// 混合：online-mode=false + 认证方案
    Hybrid {
        auth: HybridAuth,
        whitelist: Vec<String>,
    },
}

/// 混合认证方案：Paper 走登录插件，Fabric 走 EasyAuth mod。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HybridAuth {
    /// Paper 登录插件（如 LibreLogin）
    Plugin,
    /// Fabric EasyAuth
    EasyAuth,
}

/// 服务端软件选型（决策树节点 2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerSoftware {
    /// 原版（无 mod / 插件需求）
    Vanilla,
    /// Paper 插件服；build 为 None 表示最新稳定构建
    Paper { build: Option<u32> },
    /// Fabric mod 服：loader 与 installer 版本均经 Fabric meta 校验
    Fabric {
        loader_version: String,
        installer_version: String,
    },
}

/// Java 供给计划（§8.8）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaPlan {
    /// MC 版本所需的 Java 大版本（知识库 L1 查表结论）
    pub required_major: u8,
    pub runtime: JavaRuntime,
}

/// Java 运行时来源：优先系统，其次受管目录复用，最后自动安装。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JavaRuntime {
    /// 系统 PATH 中探测到并校验可用
    System { path: String, version: String },
    /// 受管安装：<数据目录>/runtime/jdk-<major>/<版本>/
    Managed {
        path: String,
        vendor: String,
        version: String,
    },
    /// 尚未确定（决策树推导阶段）
    Pending,
}

/// 一个 mod 的引用（含依赖闭包解析结果）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModRef {
    /// Modrinth project id 或 slug
    pub project: String,
    /// 选定的 Modrinth version id
    pub version_id: String,
    /// 下载 URL
    pub url: String,
    /// sha1 校验值（Modrinth 提供）
    pub sha1: String,
    pub file_name: String,
    /// 依赖（已递归展开为闭包）
    pub deps: Vec<ModRef>,
}

/// 网络方案（决策树节点：网络拓扑）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetworkPlan {
    /// 同一局域网直连
    LanOnly,
    /// 有公网 IP：端口映射 + 防火墙（给指引，不自动改防火墙）
    Direct { firewall_hint: String },
    /// 内网穿透（P1，默认樱花frp）
    Tunnel { provider: TunnelProvider },
}

/// 穿透方案提供方（决议 D9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelProvider {
    /// 樱花frp（默认）
    Natfrp,
}

/// 存档方案。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorldPlan {
    /// 新建世界（可选种子）
    New { seed: Option<String> },
    /// 复用已有存档
    Existing { path: String },
}

impl ServerSpec {
    pub fn new(spec_id: impl Into<String>) -> Self {
        Self {
            spec_id: spec_id.into(),
            created_at: Local::now(),
            account: AccountPolicy::Online,
            software: ServerSoftware::Vanilla,
            mc_version: String::new(),
            java: JavaPlan {
                required_major: 0,
                runtime: JavaRuntime::Pending,
            },
            jvm_memory_mb: 2048,
            mods: Vec::new(),
            mod_names: Vec::new(),
            network: NetworkPlan::LanOnly,
            world: WorldPlan::New { seed: None },
            notes: Vec::new(),
            server_dir: None,
            port: 25565,
            max_players: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// ServerSpecDraft：LLM 需求理解环的产出（未校验态）
// ---------------------------------------------------------------------------

/// 澄清问题：决策树发现 `Missing(节点)` 时，经 ui 问用户。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    /// 问题主题，如 "account"（账号类型）
    pub topic: String,
    /// 面向玩家的自然语言问题
    pub text: String,
    /// 可选项（空表示自由文本输入）
    pub options: Vec<String>,
}

/// 需求理解环的产出：部分方案 + 待澄清问题。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ServerSpecDraft {
    pub partial: PartialSpec,
    #[serde(default)]
    pub questions: Vec<Question>,
}

/// ServerSpec 的可空子集：LLM 允许填的字段全部可空，
/// 缺失项由决策树生成澄清问题或按规则补全。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct PartialSpec {
    /// 玩家用自然语言给出的语义化 spec 名（无则自动生成）
    pub spec_id: Option<String>,
    /// 正版玩家数 / 离线玩家数（原始需求表达）
    pub online_players: Option<u32>,
    pub offline_players: Option<u32>,
    /// 账号类型倾向：online / offline / hybrid
    pub account_kind: Option<String>,
    /// 服务端软件：vanilla / paper / fabric
    pub software: Option<String>,
    pub mc_version: Option<String>,
    /// mod 名称列表（可能是中文别名，如 "暮色森林"）
    #[serde(default)]
    pub mods: Vec<String>,
    /// 是否跨网络联机（true = 需要穿透或端口映射）
    pub cross_network: Option<bool>,
    /// 玩家自报的机器内存 MB（用于 JVM 推导）
    pub machine_memory_mb: Option<u32>,
    pub max_players: Option<u32>,
    /// 玩家提到的其它要求（自由文本）
    pub extra: Option<String>,
}
