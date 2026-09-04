//! 内网穿透编排（FR-17 / 定制 3，设计 §8.8，决议 D135–D139）。
//!
//! 七件套分工沿用"探测 / 执行拆分 + 确认门只落真实副作用"纪律：
//! check_tunnel（账号快照）/ ensure_frpc（唯一下载点）/ select_tunnel_node
//! （确定性打分）/ create_tunnel（查重复用 + 创建）/ start_tunnel（独立窗口
//! 拉起 + online 轮询 + 端到端验证）/ tunnel_status（诊断快照）/ delete_tunnel。
//!
//! 关键语义（D135）：frpc 与服务器同为独立终端窗口、与 mcha 生命周期解耦；
//! 就绪信号轮询 API `online=true`（免解析 frpc 日志）；隧道名约定
//! `mcha-mc<本地端口>`，创建前查重复用（幂等自律）。访问密钥不出现在
//! 确认门与工具回传文本中（`-f ***:<id>` 形态，D139）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent::message::ToolOutcome;
use crate::events::Event;
use crate::knowledge::upstream::natfrp::{self, NatfrpClient, NodeEntry, NodeStat, UserInfo};

use super::download::{ExpectedHash, download_verified};
use super::probe;
use super::{Permission, Tool, ToolCtx, ToolError};

/// 就绪轮询间隔（API `online=true`）。
const ONLINE_POLL: Duration = Duration::from_secs(2);
/// 进度播报间隔秒数（R4）。
const PROGRESS_EVERY: u64 = 5;
/// 端到端验证（TCP connect / SLP ping）单次超时。
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// 隧道名前缀；完整名 `mcha-mc<本地端口>`（确定性命名，查重复用依据）。
pub(crate) const TUNNEL_NAME_PREFIX: &str = "mcha-mc";

pub(crate) fn tunnel_name(local_port: u16) -> String {
    format!("{TUNNEL_NAME_PREFIX}{local_port}")
}

// ---------------------------------------------------------------------------
// 共用助手
// ---------------------------------------------------------------------------

/// 客户端构造：`[network] natfrp_api` 非空时指向自定义基址（测试注入 mock）。
fn client(ctx: &ToolCtx) -> NatfrpClient<'_> {
    match ctx.network.natfrp_api.trim() {
        "" => NatfrpClient::new(&ctx.http, ctx.natfrp_token.clone()),
        base => NatfrpClient::with_base(&ctx.http, base.to_string(), ctx.natfrp_token.clone()),
    }
}

/// token 缺失的结构化引导（D136：入口 = /token 或 mcha setup）。
fn token_missing() -> ToolOutcome {
    ToolOutcome::err(
        "未配置樱花frp 访问密钥。请引导用户完成一次性配置（任选其一）：\n\
         1. 会话内直接输入 /token 命令补配（推荐，不用退出会话）；\n\
         2. 或退出后运行 mcha setup（可选步骤）。\n\
         申请入口：注册 https://www.natfrp.com/auth/register 或登录 \
         https://www.natfrp.com/auth/login；实名认证在管理面板 \
         https://www.natfrp.com/user/ 完成（建隧道硬前置）；访问密钥在 \
         https://www.natfrp.com/user/profile 查看复制。\n\
         配置完成后重新调用本工具继续编排。",
    )
}

/// 字节数人性化（≥1GB 显示 GB，其余 MB）。
fn human_bytes(n: i64) -> String {
    let n = n.max(0) as f64;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else {
        format!("{n} B")
    }
}

fn frpc_root(data_dir: &Path) -> PathBuf {
    data_dir.join("runtime").join("frpc")
}

fn frpc_binary_name() -> &'static str {
    if cfg!(windows) { "frpc.exe" } else { "frpc" }
}

fn installed_frpc_path(data_dir: &Path, version: &str) -> PathBuf {
    frpc_root(data_dir).join(version).join(frpc_binary_name())
}

/// 扫描受管目录中已安装的 frpc（取最高版本目录）。
fn find_installed_frpc(data_dir: &Path) -> Option<(String, PathBuf)> {
    let mut versions: Vec<PathBuf> = std::fs::read_dir(frpc_root(data_dir))
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    for dir in versions.iter().rev() {
        let binary = dir.join(frpc_binary_name());
        if binary.is_file() {
            return Some((dir.file_name()?.to_string_lossy().to_string(), binary));
        }
    }
    None
}

/// frpc 下载域白名单：官方 CDN + 自定义基址同域（测试 / 自建代理，D115 同理）。
fn allowed_download_hosts(ctx: &ToolCtx) -> Vec<String> {
    let mut hosts = vec![natfrp::FRPC_CDN_HOST.to_string()];
    let base = ctx.network.natfrp_api.trim();
    if !base.is_empty()
        && let Some(host) = crate::knowledge::upstream::modrinth::url_host(base)
    {
        hosts.push(host);
    }
    hosts
}

/// 节点 id → host（构建连接端点用）。
async fn node_host(api: &NatfrpClient<'_>, node_id: i64) -> Option<String> {
    api.nodes()
        .await
        .ok()?
        .into_iter()
        .find(|n| n.id == node_id)
        .map(|n| n.host)
}

// ---------------------------------------------------------------------------
// 节点打分（D138：硬过滤 + 排序，确定性算法下沉工具内部）
// ---------------------------------------------------------------------------

/// 打分结果条目（附排序特征；展示理由由 format 推导）。
pub(crate) struct ScoredNode {
    pub node: NodeEntry,
    pub load: Option<f64>,
    pub mainland: bool,
    pub beta: bool,
    pub uptime: i64,
}

/// 硬过滤 + 排序。
///
/// 过滤（不满足即剔除）：可建隧道（bit2）、非强制访问认证（bit8）、非私有
/// （bit6）、非离线（bit9）、用户组等级 ≥ 节点 VIP、节点状态在线（/node/stats
/// online ≥ 0，缺失视作未知放行）。排序：内地优先 → 负载低 → 非 BETA →
/// uptime 高 → id 稳定序。返回 (入选[已排序], 剔除原因列表)。
pub(crate) fn score_nodes(
    nodes: Vec<NodeEntry>,
    stats: &HashMap<i64, NodeStat>,
    user_level: i64,
) -> (Vec<ScoredNode>, Vec<String>) {
    let mut kept = Vec::new();
    let mut excluded = Vec::new();
    for node in nodes {
        if !node.creatable() {
            excluded.push(format!("节点 {}（{}）满载或不可建隧道", node.name, node.id));
            continue;
        }
        if node.force_auth() {
            excluded.push(format!(
                "节点 {}（{}）强制访问认证，会拦截朋友直连",
                node.name, node.id
            ));
            continue;
        }
        if node.private_node() {
            excluded.push(format!("节点 {}（{}）为私有节点", node.name, node.id));
            continue;
        }
        if node.offline() {
            excluded.push(format!("节点 {}（{}）离线", node.name, node.id));
            continue;
        }
        if node.vip > user_level {
            excluded.push(format!(
                "节点 {}（{}）需要用户组等级 {}（当前 {user_level}）",
                node.name, node.id, node.vip
            ));
            continue;
        }
        if let Some(stat) = stats.get(&node.id)
            && stat.online < 0
        {
            excluded.push(format!("节点 {}（{}）状态上报离线", node.name, node.id));
            continue;
        }
        kept.push(ScoredNode {
            mainland: node.mainland(),
            beta: node.beta(),
            load: stats.get(&node.id).map(|s| s.load),
            uptime: stats.get(&node.id).map(|s| s.uptime).unwrap_or(0),
            node,
        });
    }
    kept.sort_by(|a, b| {
        b.mainland
            .cmp(&a.mainland)
            .then(
                a.load
                    .unwrap_or(f64::MAX)
                    .total_cmp(&b.load.unwrap_or(f64::MAX)),
            )
            .then(a.beta.cmp(&b.beta))
            .then(b.uptime.cmp(&a.uptime))
            .then(a.node.id.cmp(&b.node.id))
    });
    (kept, excluded)
}

/// 打分结果 → 展示行（附入选理由，轨迹可回放）。
fn format_scored(scored: &[ScoredNode]) -> Vec<String> {
    scored
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let mut reasons = Vec::new();
            if item.mainland {
                reasons.push("内地优先");
            }
            if item.load.is_some_and(|l| l < 50.0) {
                reasons.push("负载较低");
            }
            if item.beta {
                reasons.push("BETA（实验性）");
            }
            let reason = if reasons.is_empty() {
                "综合最优".to_string()
            } else {
                reasons.join(" · ")
            };
            format!(
                "{idx}. 节点 ID {id}｜{name}｜{host}｜{region}｜负载 {load}｜{reason}",
                id = item.node.id,
                name = item.node.name,
                host = item.node.host,
                region = if item.mainland { "内地" } else { "海外" },
                load = item
                    .load
                    .map(|l| format!("{l:.0}%"))
                    .unwrap_or_else(|| "—".to_string()),
                reason = reason
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// check_tunnel
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckTunnelArgs {}

pub struct CheckTunnelTool;

#[async_trait::async_trait]
impl Tool for CheckTunnelTool {
    fn name(&self) -> &'static str {
        "check_tunnel"
    }
    fn description(&self) -> String {
        "检查樱花frp 账号与隧道就绪状态（只读）：访问密钥有效性、实名认证、用户组等级、流量余额、隧道额度、frpc 客户端是否已下载。编排穿透前必调；未配置密钥时返回配置引导。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(CheckTunnelArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::ReadOnly
    }
    async fn run(&self, _args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if ctx.natfrp_token.trim().is_empty() {
            return Ok(token_missing());
        }
        let api = client(ctx);
        let user: UserInfo = match api.user_info().await {
            Ok(user) => user,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        if let Some(ban) = user.frozen() {
            return Ok(ToolOutcome::err(format!(
                "樱花frp 账号已被冻结：{}（原因：{}）。请引导用户到 https://www.natfrp.com/user/ 查看详情或申诉；冻结期间无法创建隧道",
                ban.title, ban.reason
            )));
        }

        let mut lines = vec![format!(
            "账号正常：{name}（UID {uid}）｜{}｜用户组 {group}（等级 {level}）｜限速 {speed}",
            if user.realnamed() {
                "已实名"
            } else {
                "未实名（创建隧道会被拒绝；请引导用户到 https://www.natfrp.com/user/ 完成实名认证）"
            },
            name = user.name,
            uid = user.id,
            group = if user.group.name.is_empty() {
                "默认"
            } else {
                &user.group.name
            },
            level = user.group.level,
            speed = if user.speed.is_empty() {
                "—"
            } else {
                &user.speed
            }
        )];
        if let Some(remaining) = user.traffic_remaining() {
            lines.push(format!(
                "总剩余流量：{remaining}",
                remaining = human_bytes(remaining)
            ));
        }
        match api.tunnels().await {
            Ok(tunnels) => {
                let mine = tunnels
                    .iter()
                    .filter(|t| t.name.starts_with(TUNNEL_NAME_PREFIX))
                    .count();
                lines.push(format!(
                    "隧道额度：当前 {total} 条（mcha 创建 {mine} 条）／上限 {limit}{limit_note}",
                    total = tunnels.len(),
                    limit = user.tunnels,
                    limit_note = if user.tunnels > 0 && tunnels.len() as i64 >= user.tunnels {
                        "（已达上限，需删除旧隧道腾出额度）"
                    } else {
                        ""
                    }
                ));
            }
            Err(reason) => lines.push(format!("隧道额度查询失败（不阻断）：{reason}")),
        }
        match api.data_plans().await {
            Ok(plans) if !plans.is_empty() => {
                let total: i64 = plans.iter().map(|p| p.remaining.max(0)).sum();
                lines.push(format!(
                    "可用流量包：{n} 个，合计剩余 {total}",
                    n = plans.len(),
                    total = human_bytes(total)
                ));
            }
            _ => {}
        }
        match find_installed_frpc(&ctx.data_dir) {
            Some((version, path)) => lines.push(format!(
                "frpc 客户端：{version} 已就绪（{path}）",
                path = path.display()
            )),
            None => lines.push("frpc 客户端：未下载（调用 ensure_frpc 安装）".to_string()),
        }
        if !user.realnamed() {
            return Ok(ToolOutcome::err(lines.join("\n")));
        }
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// ensure_frpc
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnsureFrpcArgs {
    /// 已有同版本时强制重新下载（默认 false）
    #[serde(default)]
    pub force: Option<bool>,
}

pub struct EnsureFrpcTool;

#[async_trait::async_trait]
impl Tool for EnsureFrpcTool {
    fn name(&self) -> &'static str {
        "ensure_frpc"
    }
    fn description(&self) -> String {
        "确保樱花frp 官方 frpc 客户端就绪：从官方分发接口获取当前版本与本机架构的下载地址，下载并做 MD5 校验后落受管目录（~/.mcha/runtime/frpc/<版本>/）。同版本已存在则幂等跳过。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(EnsureFrpcArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::Network
    }
    fn confirm_summary(&self, _args: &serde_json::Value) -> Vec<String> {
        vec![
            "下载樱花frp 官方 frpc 客户端到受管目录（MD5 校验；同版本已存在则跳过）".to_string(),
            format!("下载域：{}", natfrp::FRPC_CDN_HOST),
        ]
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: EnsureFrpcArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let force = args.force.unwrap_or(false);
        let api = client(ctx);
        let release = match api.frpc_release().await {
            Ok(release) => release,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let key = natfrp::frpc_arch_key();
        let Some(download) = release.archs.get(&key) else {
            let mut keys: Vec<&str> = release.archs.keys().map(String::as_str).collect();
            keys.sort();
            return Ok(ToolOutcome::err(format!(
                "官方分发没有本机架构 {key} 的 frpc 包（可用：{available}）",
                available = keys.join("、")
            )));
        };
        if !download.downloadable() {
            return Ok(ToolOutcome::err(format!(
                "本机架构 {key} 的 frpc 条目缺少直链或官方 MD5（可能为 docker 类条目）；已拒绝下载——哈希校验是下载安全底线（§12）"
            )));
        }

        let dest = installed_frpc_path(&ctx.data_dir, &release.ver);
        if dest.is_file() && !force {
            return Ok(ToolOutcome::ok(format!(
                "frpc {version} 已就绪（{dest}，幂等跳过）",
                version = release.ver,
                dest = dest.display()
            )));
        }

        // 下载域强校验（§12）
        let host = crate::knowledge::upstream::modrinth::url_host(&download.url)
            .ok_or_else(|| ToolError::Io(format!("下载 URL 非法：{}", download.url)))?;
        let allowed = allowed_download_hosts(ctx);
        if !allowed.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
            return Ok(ToolOutcome::err(format!(
                "下载 URL 域「{host}」不在白名单（仅允许 {allowed}）；已拒绝",
                allowed = allowed.join("、")
            )));
        }

        let dir = frpc_root(&ctx.data_dir).join(&release.ver);
        if let Err(err) = tokio::fs::create_dir_all(&dir).await {
            return Err(ToolError::Io(format!("创建目录失败：{err}")));
        }
        let tmp = dir.join(format!(".mcha-part-{}", frpc_binary_name()));
        if let Err(reason) = download_verified(
            ctx,
            &download.url,
            &tmp,
            &format!("下载 frpc {}", release.ver),
            &[ExpectedHash::Md5(download.hash.clone())],
        )
        .await
        {
            return Ok(ToolOutcome::err(reason));
        }
        if let Err(err) = tokio::fs::rename(&tmp, &dest).await {
            let _ = std::fs::remove_file(&tmp);
            return Err(ToolError::Io(format!(
                "落位失败（{}）：{err}",
                dest.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(err) =
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            {
                return Err(ToolError::Io(format!("设置执行权限失败：{err}")));
            }
        }
        Ok(ToolOutcome::ok(format!(
            "frpc {version} 就绪：{dest}（{size}，MD5 校验通过）",
            version = release.ver,
            dest = dest.display(),
            size = human_bytes(download.size as i64)
        )))
    }
}

// ---------------------------------------------------------------------------
// select_tunnel_node
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SelectNodeArgs {
    /// 返回条数上限（默认 5，最多 10）
    #[serde(default)]
    pub limit: Option<u32>,
}

pub struct SelectTunnelNodeTool;

#[async_trait::async_trait]
impl Tool for SelectTunnelNodeTool {
    fn name(&self) -> &'static str {
        "select_tunnel_node"
    }
    fn description(&self) -> String {
        "为隧道挑选樱花frp 节点（只读）：拉取节点列表与实时负载，按确定性规则硬过滤（满载/强制访问认证/私有/离线/等级不足）并打分排序（内地优先 → 负载低 → 非 BETA），返回 top N 供用户确认。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(SelectNodeArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: SelectNodeArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if ctx.natfrp_token.trim().is_empty() {
            return Ok(token_missing());
        }
        let api = client(ctx);
        let user = match api.user_info().await {
            Ok(user) => user,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let nodes = match api.nodes().await {
            Ok(nodes) => nodes,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let stats = match api.node_stats().await {
            Ok(stats) => stats,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let (kept, excluded) = score_nodes(nodes, &stats, user.group.level);
        if kept.is_empty() {
            let mut message = String::from("没有可用节点；剔除原因：\n");
            for reason in excluded.iter().take(8) {
                message.push_str(&format!("- {reason}\n"));
            }
            return Ok(ToolOutcome::err(message));
        }
        let limit = args.limit.unwrap_or(5).clamp(1, 10) as usize;
        let mut lines = vec![format!(
            "节点打分完成（可用 {total} 个，展示前 {n}；排序：内地优先 → 负载低 → 非 BETA）：",
            total = kept.len(),
            n = kept.len().min(limit)
        )];
        lines.extend(format_scored(&kept).into_iter().take(limit));
        lines.push(
            "请用 ask_user 请用户确认节点（默认推荐第一位），然后把所选节点 ID 传给 create_tunnel。"
                .to_string(),
        );
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// create_tunnel
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTunnelArgs {
    /// 节点 ID（select_tunnel_node 返回并经用户确认）
    pub node_id: i64,
    /// 本地 Minecraft 服务端端口（如 25565）
    pub local_port: u16,
    /// 指定远程端口（默认留空由平台自动分配）
    #[serde(default)]
    pub remote_port: Option<u16>,
}

pub struct CreateTunnelTool;

#[async_trait::async_trait]
impl Tool for CreateTunnelTool {
    fn name(&self) -> &'static str {
        "create_tunnel"
    }
    fn description(&self) -> String {
        "创建樱花frp TCP 隧道（把本地 Minecraft 端口开放到公网）：隧道名按约定 mcha-mc<本地端口> 自动生成，同名同端口的已有隧道直接复用（幂等），否则创建新隧道。返回隧道 ID 与公网端点。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(CreateTunnelArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::Network
    }
    fn confirm_summary(&self, args: &serde_json::Value) -> Vec<String> {
        let port = args
            .get("local_port")
            .and_then(|v| v.as_u64())
            .unwrap_or(25565);
        let node = args
            .get("node_id")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        vec![
            format!(
                "创建樱花frp TCP 隧道 {}（节点 {node}）：本地 127.0.0.1:{port} → 公网开放",
                tunnel_name(port as u16)
            ),
            "同名隧道已存在时自动复用，不重复创建；朋友将能通过公网地址直连你的服务器".to_string(),
        ]
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: CreateTunnelArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if ctx.natfrp_token.trim().is_empty() {
            return Ok(token_missing());
        }
        let api = client(ctx);
        let name = tunnel_name(args.local_port);

        // 复用优先（幂等自律）：同名且本地端口一致 → 直接返回
        let tunnels = match api.tunnels().await {
            Ok(tunnels) => tunnels,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        if let Some(existing) = tunnels.iter().find(|t| t.name == name) {
            if existing.local_port == i64::from(args.local_port) {
                let host = node_host(&api, existing.node).await.unwrap_or_default();
                let endpoint = if host.is_empty() || existing.remote.is_empty() {
                    "（端点待 frpc 连接后生效）".to_string()
                } else {
                    format!("{host}:{remote}", remote = existing.remote)
                };
                return Ok(ToolOutcome::ok(format!(
                    "复用现有隧道（幂等，不重复创建）：{name}（ID {id}）→ {endpoint}｜{}",
                    if existing.online {
                        "在线"
                    } else {
                        "离线（start_tunnel 拉起）"
                    },
                    id = existing.id
                )));
            }
            return Ok(ToolOutcome::err(format!(
                "已存在同名隧道 {name}（ID {id}），但本地端口为 {port}（请求 {want}）。请引导用户选择：删除旧隧道后重建（delete_tunnel），或沿用旧隧道对应的服务端端口",
                id = existing.id,
                port = existing.local_port,
                want = args.local_port
            )));
        }

        let created = match api
            .create_tcp_tunnel(
                &name,
                args.node_id,
                "127.0.0.1",
                args.local_port,
                args.remote_port,
                Some("mcha 自动创建"),
            )
            .await
        {
            Ok(created) => created,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let host = node_host(&api, args.node_id).await.unwrap_or_default();
        let endpoint = if host.is_empty() || created.remote.is_empty() {
            "（端点待 frpc 连接后生效）".to_string()
        } else {
            format!("{host}:{remote}", remote = created.remote)
        };
        Ok(ToolOutcome::ok(format!(
            "隧道已创建：{name}（ID {id}）→ {endpoint}\n下一步：确保服务器已启动（start_server），然后 start_tunnel 拉起 frpc 完成端到端验证。",
            id = created.id
        )))
    }
}

// ---------------------------------------------------------------------------
// start_tunnel
// ---------------------------------------------------------------------------

/// 窗口启动器：在独立终端窗口运行脚本（参数：脚本目录、脚本路径）。
/// 抽象为闭包供测试注入（CI / 无桌面环境无法弹窗）。
type Launcher = Arc<dyn Fn(&Path, &Path) -> Result<(), String> + Send + Sync>;

/// 真实启动器（D135：与 D134 同构——Windows cmd /k 保窗可读错误；Unix 探测
/// 终端模拟器，脚本退出后窗口停留展示信息）。
#[cfg_attr(test, allow(dead_code))]
fn real_launch(dir: &Path, script: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        let _ = dir;
        std::process::Command::new("cmd")
            .args([
                "/k",
                &script.file_name().unwrap_or_default().to_string_lossy(),
            ])
            .current_dir(dir)
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map(|_| ())
            .map_err(|err| format!("新开窗口启动 frpc 失败：{err}"))
    }
    #[cfg(unix)]
    {
        let candidates = [
            ("x-terminal-emulator", "-e"),
            ("gnome-terminal", "--"),
            ("konsole", "-e"),
            ("xfce4-terminal", "-x"),
            ("xterm", "-e"),
        ];
        for (term, flag) in candidates {
            if find_in_path(term).is_some() {
                return std::process::Command::new(term)
                    .args([flag])
                    .arg(script)
                    .current_dir(dir)
                    .spawn()
                    .map(|_| ())
                    .map_err(|err| format!("在 {term} 中启动 frpc 失败：{err}"));
            }
        }
        Err("未找到可用的图形终端（探测过 x-terminal-emulator / gnome-terminal / konsole / xfce4-terminal / xterm）；请在桌面环境运行，或手动执行 frpc-start.sh".into())
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (dir, script);
        Err("当前平台不支持自动弹窗启动 frpc；请手动执行脚本".into())
    }
}

#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// 写 frpc 启动脚本（token 固化在脚本内，与 start.bat 固化 Java 路径同理；
/// 脚本在数据目录受管 frpc 版本目录下，退出后窗口停留展示原因）。
fn write_frpc_script(
    dir: &Path,
    binary: &Path,
    token: &str,
    tunnel_id: i64,
) -> Result<PathBuf, String> {
    let (name, content) = if cfg!(windows) {
        (
            "frpc-start.cmd",
            format!(
                "@echo off\r\n\"{bin}\" -f {token}:{id}\r\necho.\r\necho frpc 已退出：窗口关闭或本进程退出后朋友将无法连接。\r\necho 如上方有报错，请把本窗口内容交给 mcha 排查。\r\n",
                bin = binary.display(),
                id = tunnel_id
            ),
        )
    } else {
        (
            "frpc-start.sh",
            format!(
                "#!/bin/sh\n'{bin}' -f '{token}:{id}'\necho\necho \"frpc 已退出：窗口关闭或本进程退出后朋友将无法连接。\"\necho \"如上方有报错，请把本窗口内容交给 mcha 排查。\"\n",
                bin = binary.display(),
                id = tunnel_id
            ),
        )
    };
    let path = dir.join(name);
    std::fs::write(&path, content).map_err(|err| format!("写入启动脚本失败：{err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(path)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartTunnelArgs {
    /// 隧道 ID（create_tunnel 返回）
    pub tunnel_id: i64,
    /// 在线等待上限秒（默认 60）
    #[serde(default)]
    pub ready_timeout_secs: Option<u64>,
}

pub struct StartTunnelTool {
    launcher: Launcher,
}

/// 构造 start_tunnel 工具：生产用真实弹窗启动器；测试环境（cfg(test)）
/// 注入无头启动器，与 process.rs 的启动器闭包注入同一模式（D134）。
pub(crate) fn start_tunnel_tool() -> StartTunnelTool {
    #[cfg(test)]
    {
        StartTunnelTool {
            launcher: Arc::new(|_dir, _script| Ok(())),
        }
    }
    #[cfg(not(test))]
    {
        StartTunnelTool {
            launcher: Arc::new(real_launch),
        }
    }
}

impl StartTunnelTool {
    /// 确认门内容：密钥以 `***` 呈现（D139）。
    fn confirmation_lines(args: &serde_json::Value) -> Vec<String> {
        let tunnel_id = args
            .get("tunnel_id")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        vec![
            format!("在独立终端窗口启动 frpc 连接隧道（frpc -f ***:{tunnel_id}，访问密钥不显示）"),
            "frpc 日志在该窗口滚动；关闭窗口或窗口内 Ctrl-C 即断开隧道。mcha 退出不影响隧道"
                .to_string(),
        ]
    }
}

#[async_trait::async_trait]
impl Tool for StartTunnelTool {
    fn name(&self) -> &'static str {
        "start_tunnel"
    }
    fn description(&self) -> String {
        "在独立终端窗口启动 frpc 让隧道上线（与手动运行一致；日志只在窗口滚动），轮询平台 online 状态等待隧道就绪，随后对公网端点做 TCP connect 与 MC SLP ping 端到端验证。隧道已在线时拒绝（防重复拉起）。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(StartTunnelArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::Execute
    }
    fn confirm_summary(&self, args: &serde_json::Value) -> Vec<String> {
        Self::confirmation_lines(args)
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: StartTunnelArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if ctx.natfrp_token.trim().is_empty() {
            return Ok(token_missing());
        }
        let api = client(ctx);
        let tunnels = match api.tunnels().await {
            Ok(tunnels) => tunnels,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let Some(tunnel) = tunnels.iter().find(|t| t.id == args.tunnel_id) else {
            return Ok(ToolOutcome::err(format!(
                "隧道 {id} 不存在；请用 create_tunnel 创建或 check_tunnel 查看现有隧道",
                id = args.tunnel_id
            )));
        };
        if tunnel.online {
            return Ok(ToolOutcome::err(format!(
                "隧道 {id} 已在线：疑似已有 frpc（或官方启动器）在运行。请先关闭对应窗口再重试，或直接用 tunnel_status 验证连通",
                id = args.tunnel_id
            )));
        }
        if tunnel.status == 2 {
            return Ok(ToolOutcome::err(format!(
                "隧道 {id} 已被封禁，无法启动；详情见 https://www.natfrp.com/tunnel/",
                id = args.tunnel_id
            )));
        }

        let Some((frpc_version, binary)) = find_installed_frpc(&ctx.data_dir) else {
            return Ok(ToolOutcome::err(
                "frpc 客户端未下载；请先调用 ensure_frpc".to_string(),
            ));
        };
        let dir = frpc_root(&ctx.data_dir).join(&frpc_version);
        let script = match write_frpc_script(&dir, &binary, ctx.natfrp_token.trim(), args.tunnel_id)
        {
            Ok(script) => script,
            Err(reason) => return Err(ToolError::Io(reason)),
        };
        let t0 = std::time::Instant::now();
        if let Err(reason) = (self.launcher)(&dir, &script) {
            return Ok(ToolOutcome::err(format!(
                "{reason}；逃生舱：手动在终端运行 {script}",
                script = script.display()
            )));
        }

        // 就绪轮询：API online=true（D135，免解析 frpc 日志；每 5s 一行进度，R4）
        let timeout = Duration::from_secs(args.ready_timeout_secs.unwrap_or(60).clamp(5, 600));
        let deadline = tokio::time::Instant::now() + timeout;
        let mut tick = tokio::time::interval(ONLINE_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut next_progress = PROGRESS_EVERY;
        let online = loop {
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => {
                    let _ = ctx.events.send(Event::OutputLine(
                        "│ 等待已打断；frpc 窗口仍在运行，隧道是否上线以窗口与平台状态为准".into(),
                    ));
                    return Err(ToolError::Cancelled);
                }
                _ = tokio::time::sleep_until(deadline) => break None,
                _ = tick.tick() => {
                    if let Ok(list) = api.tunnels().await
                        && let Some(t) = list.iter().find(|t| t.id == args.tunnel_id)
                        && t.online
                    {
                        break Some(t.clone());
                    }
                    let elapsed = t0.elapsed().as_secs();
                    if elapsed >= next_progress {
                        let _ = ctx.events.send(Event::OutputLine(format!(
                            "│ 已等待 {elapsed}s：等待 frpc 连接节点上线（日志在独立窗口滚动）"
                        )));
                        next_progress += PROGRESS_EVERY;
                    }
                }
            }
        };
        let Some(tunnel) = online else {
            return Ok(ToolOutcome::err(format!(
                "未在 {timeout} 秒内检测到隧道上线；frpc 窗口可能启动即报错（杀软拦截 / 密钥失效 / 节点故障），请查看窗口内容后重试或告知用户",
                timeout = timeout.as_secs()
            )));
        };

        // 端到端验证：TCP connect + MC SLP ping（尽力而为：服务器未启动时 TCP 仍应通）
        let host = node_host(&api, tunnel.node)
            .await
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "（未知节点）".to_string());
        let remote_port: Option<u16> = tunnel.remote.parse().ok();
        let mut lines = vec![format!(
            "frpc 已拉起（{version}），隧道在线（{elapsed:.1}s）：{tunnel_name} → {host}:{port}",
            version = frpc_version,
            elapsed = t0.elapsed().as_secs_f32(),
            tunnel_name = tunnel.name,
            port = tunnel.remote
        )];
        let endpoint = format!("{host}:{}", tunnel.remote);
        match remote_port {
            Some(port) if host != "（未知节点）" => {
                let addr = format!("{host}:{port}");
                // 显式绑定连接结果并在 ping 前释放：若让临时值活到整个 match
                // 结束，TCP 预检连接会一直占着，对端（MC 服务器）的握手处理
                // 与 ping 读回包可能互相等待（实测死锁）
                let tcp =
                    tokio::time::timeout(VERIFY_TIMEOUT, tokio::net::TcpStream::connect(&addr))
                        .await;
                match tcp {
                    Ok(Ok(stream)) => {
                        drop(stream);
                        lines.push(format!("TCP 连通 ✓（{addr}）"));
                        match tokio::time::timeout(VERIFY_TIMEOUT, probe::ping(&addr, &host, port))
                            .await
                        {
                            Ok(Ok(detail)) => lines.push(format!("MC SLP ping ✓（{detail}）")),
                            Ok(Err(reason)) => lines.push(format!(
                                "MC SLP ping ✗（{reason}）——若服务器尚未启动属正常现象；起服后可用 mc_ping 复验"
                            )),
                            Err(_) => lines.push(format!(
                                "MC SLP ping 超时（{addr}）——若服务器尚未启动属正常现象；起服后可用 mc_ping 复验"
                            )),
                        }
                    }
                    Ok(Err(err)) => lines.push(format!(
                        "TCP 连接失败（{addr}）：{err}（节点或隧道异常，可换节点重建）"
                    )),
                    Err(_) => lines.push(format!("TCP 连接超时（{addr}）")),
                }
            }
            _ => {
                lines.push("远程端口无法解析，跳过端到端验证；请用 tunnel_status 复查".to_string())
            }
        }
        lines.push(format!(
            "朋友连接地址：{endpoint}（启动隧道的服务窗口请保持开启；连接说明卡片在部署完成后给出）"
        ));
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// tunnel_status
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TunnelStatusArgs {
    /// 隧道 ID（默认检查全部 mcha- 前缀隧道）
    #[serde(default)]
    pub tunnel_id: Option<i64>,
}

pub struct TunnelStatusTool;

#[async_trait::async_trait]
impl Tool for TunnelStatusTool {
    fn name(&self) -> &'static str {
        "tunnel_status"
    }
    fn description(&self) -> String {
        "内网穿透诊断快照（只读）：隧道平台在线状态、本地服务端端口监听、公网端点 TCP 与 MC SLP ping、剩余流量。朋友说连不上时的第一步。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(TunnelStatusArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: TunnelStatusArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if ctx.natfrp_token.trim().is_empty() {
            return Ok(token_missing());
        }
        let api = client(ctx);
        let tunnels = match api.tunnels().await {
            Ok(tunnels) => tunnels,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let targets: Vec<_> = match args.tunnel_id {
            Some(id) => tunnels.into_iter().filter(|t| t.id == id).collect(),
            None => tunnels
                .into_iter()
                .filter(|t| t.name.starts_with(TUNNEL_NAME_PREFIX))
                .collect(),
        };
        if targets.is_empty() {
            return Ok(ToolOutcome::err(
                "没有找到相关隧道；用 check_tunnel 查看账号下的全部隧道，或 create_tunnel 创建",
            ));
        }
        let mut lines = Vec::new();
        for tunnel in &targets {
            let host = node_host(&api, tunnel.node).await.unwrap_or_default();
            lines.push(format!(
                "隧道 {name}（ID {id}）：平台状态 {}｜本地 127.0.0.1:{lport}｜公网 {host}:{remote}",
                if tunnel.online { "在线" } else { "离线" },
                name = tunnel.name,
                id = tunnel.id,
                lport = tunnel.local_port,
                host = if host.is_empty() { "?" } else { &host },
                remote = if tunnel.remote.is_empty() {
                    "?"
                } else {
                    &tunnel.remote
                }
            ));
            let local_ok = tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::TcpStream::connect(("127.0.0.1", tunnel.local_port as u16)),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            lines.push(format!(
                "  本地端口 {lport}：{state}",
                lport = tunnel.local_port,
                state = if local_ok {
                    "有服务监听 ✓"
                } else {
                    "无监听 ✗"
                }
            ));
            if tunnel.online
                && local_ok
                && !host.is_empty()
                && let Ok(port) = tunnel.remote.parse::<u16>()
            {
                let addr = format!("{host}:{port}");
                // 同 start_tunnel：先释放 TCP 预检连接再 ping（防临时值存活致互等）
                let tcp =
                    tokio::time::timeout(VERIFY_TIMEOUT, tokio::net::TcpStream::connect(&addr))
                        .await;
                match tcp {
                    Ok(Ok(stream)) => {
                        drop(stream);
                        match tokio::time::timeout(VERIFY_TIMEOUT, probe::ping(&addr, &host, port))
                            .await
                        {
                            Ok(Ok(_)) => lines.push(format!("  公网端点 {addr}：TCP ✓ / MC ping ✓（全链路正常）")),
                            Ok(Err(reason)) => lines.push(format!(
                                "  公网端点 {addr}：TCP ✓ / MC ping ✗（{reason}）——服务器可能仍在启动"
                            )),
                            Err(_) => lines.push(format!(
                                "  公网端点 {addr}：TCP ✓ / MC ping 超时——服务器可能仍在启动"
                            )),
                        }
                    }
                    Ok(Err(err)) => lines.push(format!("  公网端点 {addr}：TCP ✗（{err}）")),
                    Err(_) => lines.push(format!("  公网端点 {addr}：TCP 超时")),
                }
            }
            // 诊断结论
            if !tunnel.online {
                lines.push(
                    "  结论：隧道离线——frpc 窗口可能被关；用 start_tunnel 重新拉起".to_string(),
                );
            } else if !local_ok {
                lines.push(
                    "  结论：隧道在线但本地服务端无监听——服务器窗口可能被关；重新 start_server"
                        .to_string(),
                );
            }
        }
        if let Ok(user) = api.user_info().await
            && let Some(remaining) = user.traffic_remaining()
        {
            lines.push(format!(
                "剩余流量：{remaining}（隧道流量由樱花frp 计量，MCHA 仅展示）",
                remaining = human_bytes(remaining)
            ));
        }
        Ok(ToolOutcome::ok(lines.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// delete_tunnel
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteTunnelArgs {
    /// 隧道 ID
    pub tunnel_id: i64,
}

pub struct DeleteTunnelTool;

#[async_trait::async_trait]
impl Tool for DeleteTunnelTool {
    fn name(&self) -> &'static str {
        "delete_tunnel"
    }
    fn description(&self) -> String {
        "删除樱花frp 隧道（换节点重建或演示清理用）：公网入口立即失效，朋友将无法通过该地址连接。删除前须向用户确认。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(DeleteTunnelArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> Permission {
        Permission::Network
    }
    fn confirm_summary(&self, args: &serde_json::Value) -> Vec<String> {
        let tunnel_id = args
            .get("tunnel_id")
            .and_then(|v| v.as_i64())
            .unwrap_or_default();
        vec![
            format!("删除樱花frp 隧道 {tunnel_id}（公网入口立即失效，朋友将无法连接）"),
            "如需更换节点，删除后由 Agent 用 create_tunnel 重建".to_string(),
        ]
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: DeleteTunnelArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if ctx.natfrp_token.trim().is_empty() {
            return Ok(token_missing());
        }
        let api = client(ctx);
        match api.delete_tunnel(args.tunnel_id).await {
            Ok((deleted, failed)) => {
                if deleted.contains(&args.tunnel_id) {
                    Ok(ToolOutcome::ok(format!(
                        "隧道 {} 已删除（公网入口失效）",
                        args.tunnel_id
                    )))
                } else if failed.contains(&args.tunnel_id) {
                    Ok(ToolOutcome::err(format!(
                        "隧道 {} 已删除但未能踢下线（frpc 可能仍在连接）；已连接的窗口关闭后即彻底断开",
                        args.tunnel_id
                    )))
                } else {
                    Ok(ToolOutcome::err(format!(
                        "隧道 {} 未被删除（可能不存在或无权操作）；用 check_tunnel 查看现有隧道",
                        args.tunnel_id
                    )))
                }
            }
            Err(reason) => Ok(ToolOutcome::err(reason)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tools::general::tests::QuietInteraction;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    pub(crate) fn test_ctx(workspace: &Path, natfrp_api: &str, token: &str) -> ToolCtx {
        let (tx, _rx) = crate::events::event_channel();
        let mut network = crate::config::NetworkConfig::default();
        network.natfrp_api = natfrp_api.to_string();
        ToolCtx {
            workspace: workspace.to_path_buf(),
            data_dir: workspace.join(".data"),
            http: reqwest::Client::builder()
                .user_agent("mcha/0.2")
                .build()
                .unwrap(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: Arc::new(QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network,
            retrieval: Default::default(),
            curseforge_key: String::new(),
            natfrp_token: token.to_string(),
        }
    }

    #[test]
    fn tunnel_name_convention() {
        assert_eq!(tunnel_name(25565), "mcha-mc25565");
        assert_eq!(tunnel_name(8080), "mcha-mc8080");
    }

    fn node(id: i64, name: &str, vip: i64, flag: i64) -> NodeEntry {
        NodeEntry {
            id,
            name: name.to_string(),
            host: format!("{name}.natfrp.io"),
            description: String::new(),
            vip,
            flag,
        }
    }

    fn stat(id: i64, online: i64, uptime: i64, load: f64) -> (i64, NodeStat) {
        (
            id,
            NodeStat {
                id,
                online,
                uptime,
                load,
            },
        )
    }

    #[test]
    fn score_nodes_filters_and_ranks() {
        let creatable = natfrp::FLAG_CREATABLE;
        let mainland = natfrp::FLAG_MAINLAND;
        let nodes = vec![
            node(2, "海外低载", 0, creatable),
            node(1, "内地低载", 0, creatable | mainland),
            node(3, "满载", 0, mainland),
            node(
                4,
                "强制认证",
                0,
                creatable | mainland | natfrp::FLAG_FORCE_AUTH,
            ),
            node(5, "私有", 0, creatable | natfrp::FLAG_PRIVATE),
            node(6, "离线", 0, creatable | natfrp::FLAG_OFFLINE),
            node(7, "高门槛", 5, creatable | mainland),
            node(8, "内地BETA", 0, creatable | mainland | natfrp::FLAG_BETA),
            node(9, "内地高载", 0, creatable | mainland),
        ];
        let stats: HashMap<i64, NodeStat> = [
            stat(1, 0, 100_000, 19.0),
            stat(2, 0, 50_000, 5.0),
            stat(8, 0, 10_000, 1.0),
            stat(9, 0, 10_000, 90.0),
        ]
        .into_iter()
        .collect();
        let (kept, excluded) = score_nodes(nodes, &stats, 0);
        // 剔除：满载 / 强制认证 / 私有 / 离线 / VIP 门槛
        assert_eq!(excluded.len(), 5, "{excluded:?}");
        // 排序：内地优先（1、8、9）→ 负载（8:1% < 1:19% < 9:90%）→ 海外（2）
        let ids: Vec<i64> = kept.iter().map(|k| k.node.id).collect();
        assert_eq!(ids, vec![8, 1, 9, 2], "{ids:?}");
        assert!(kept[0].mainland);
        assert!(kept[3].beta == false);
        // 海外高负载排在海外低负载之后（此处仅一个海外节点，验证 uptime 稳定序）
        let (kept2, _) = score_nodes(
            vec![
                node(10, "海外A", 0, creatable),
                node(11, "海外B", 0, creatable),
            ],
            &[stat(10, 0, 1_000, 50.0), stat(11, 0, 999_999, 50.0)]
                .into_iter()
                .collect(),
            0,
        );
        assert_eq!(kept2[0].node.id, 11, "同负载应 uptime 高者优先");
    }

    #[test]
    fn frpc_script_contains_token_and_id_and_confirm_masks() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("frpc");
        let script = write_frpc_script(dir.path(), &binary, "secret-token-123", 114514).unwrap();
        let content = std::fs::read_to_string(&script).unwrap();
        assert!(content.contains("secret-token-123:114514"), "{content}");
        // 确认门永不展示明文密钥
        let lines =
            StartTunnelTool::confirmation_lines(&serde_json::json!({ "tunnel_id": 114514 }));
        let joined = lines.join("\n");
        assert!(joined.contains("***:114514"), "{joined}");
        assert!(!joined.contains("secret-token-123"));
    }

    // -- 联合 mock：natfrp API + frpc 二进制下载 + 状态翻转（供单元与集成测试）--

    /// GET /tunnels 的次数语义：创建后第 1 次返回离线（start 前置查重），
    /// 第 2 次起返回在线（模拟 frpc 已连上节点）。返回 (基址, 创建标记)。
    pub(crate) fn spawn_natfrp_mock(slp_port: u16) -> (String, Arc<AtomicBool>) {
        let created = Arc::new(AtomicBool::new(false));
        let gets = Arc::new(AtomicUsize::new(0));
        let created_clone = created.clone();
        let gets_clone = gets.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let binary: &[u8] = b"MCHA-FAKE-FRPC-BINARY";
            let binary_md5 = {
                use md5::Digest as _;
                hex(&md5::Md5::digest(binary))
            };
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf);
                let method = request.split_whitespace().next().unwrap_or("");
                let target = request.split_whitespace().nth(1).unwrap_or("");
                let authorized = request.contains("authorization: Bearer good-token");
                let created = created_clone.load(Ordering::SeqCst);
                let respond = |status: &str, ct: &str, body: String| {
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let response = match (method, target) {
                    ("GET", "/system/clients") => respond(
                        "200 OK",
                        "application/json",
                        format!(
                            r#"{{"frpc":{{"ver":"0.51.0-sakura-14","time":1784298660,"archs":{{"{}":{{"title":"测试","url":"http://{addr}/frpc_bin","hash":"{binary_md5}","size":{}}}}}}}}}"#,
                            natfrp::frpc_arch_key(),
                            binary.len()
                        ),
                    ),
                    ("GET", "/frpc_bin") => respond(
                        "200 OK",
                        "application/octet-stream",
                        String::from_utf8_lossy(binary).to_string(),
                    ),
                    (_, _) if !authorized => respond(
                        "401 Unauthorized",
                        "application/json",
                        r#"{"code":401,"msg":"访问密钥无效"}"#.to_string(),
                    ),
                    ("GET", "/user/info") => respond(
                        "200 OK",
                        "application/json",
                        r#"{"id":10,"name":"DemoUser","avatar":"","speed":"10 Mbps","tunnels":10,"realname":1,"group":{"name":"默认","level":0,"expires":0},"traffic":[1024,5368709120],"sign":{"config":[1,2],"signed":false,"last":"2026-01-01","days":0,"traffic":0.0}}"#.to_string(),
                    ),
                    ("GET", "/user/data_plans") => respond(
                        "200 OK",
                        "application/json",
                        r#"[{"id":1,"name":"普通流量包","type":10001,"type_extra":"{}","total":5368709120,"remaining":4210,"start_time":0,"end_time":0}]"#.to_string(),
                    ),
                    ("GET", "/nodes") => respond(
                        "200 OK",
                        "application/json",
                        format!(
                            r#"{{"1":{{"name":"内地测试节点","host":"127.0.0.1","description":"集成测试","vip":0,"flag":{}}}}}"#,
                            natfrp::FLAG_CREATABLE | natfrp::FLAG_MAINLAND
                        ),
                    ),
                    ("GET", "/node/stats") => respond(
                        "200 OK",
                        "application/json",
                        r#"{"time":1788000000,"nodes":[{"id":1,"online":0,"uptime":100000,"load":19.5}]}"#
                            .to_string(),
                    ),
                    ("GET", "/tunnels") => {
                        let old = format!(
                            r#"{{"id":114514,"name":"old-tunnel","type":"tcp","node":1,"online":true,"status":0,"status_reason":null,"note":"","extra":"","remote":"3721","local_ip":"127.0.0.1","local_port":1234,"export":"0|0|0","locks":{{"edit":false,"delete":false,"migrate":false}}}}"#
                        );
                        if !created {
                            // 创建前只有历史隧道：create_tunnel 查重不会误走复用路径
                            respond("200 OK", "application/json", format!("[{old}]"))
                        } else {
                            // 创建后：第 1 次（start 前置查重）离线，第 2 次起在线
                            //（模拟 frpc 连上节点的时延）
                            let n = gets_clone.fetch_add(1, Ordering::SeqCst);
                            let online = n >= 1;
                            let new_entry = format!(
                                r#"{{"id":114515,"name":"mcha-mc25565","type":"tcp","node":1,"online":{online},"status":0,"status_reason":null,"note":"mcha 自动创建","extra":"","remote":"{slp_port}","local_ip":"127.0.0.1","local_port":25565,"export":"0|0|0","locks":{{"edit":false,"delete":false,"migrate":false}}}}"#
                            );
                            respond(
                                "200 OK",
                                "application/json",
                                format!("[{old},{new_entry}]"),
                            )
                        }
                    }
                    ("POST", "/tunnels") => {
                        created_clone.store(true, Ordering::SeqCst);
                        gets_clone.store(0, Ordering::SeqCst);
                        respond(
                            "201 Created",
                            "application/json",
                            format!(
                                r#"{{"id":114515,"name":"mcha-mc25565","remote":"{slp_port}"}}"#
                            ),
                        )
                    }
                    ("POST", "/tunnel/delete") => respond(
                        "200 OK",
                        "application/json",
                        r#"{"deleted":[114515],"failed":[]}"#.to_string(),
                    ),
                    _ => respond(
                        "404 Not Found",
                        "application/json",
                        r#"{"code":404,"msg":"not found"}"#.to_string(),
                    ),
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), created)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[tokio::test]
    async fn check_tunnel_without_token_guides_setup() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path(), "", "");
        let outcome = CheckTunnelTool
            .run(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Err { error } = outcome else {
            panic!("应结构化报错");
        };
        assert!(error.contains("/token"), "{error}");
        assert!(error.contains("auth/login"), "{error}");
    }

    #[tokio::test]
    async fn ensure_frpc_downloads_and_verifies_md5() {
        let (base, _created) = spawn_natfrp_mock(1);
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path(), &base, "good-token");
        let outcome = EnsureFrpcTool
            .run(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
        let dest = installed_frpc_path(&ctx.data_dir, "0.51.0-sakura-14");
        assert!(dest.is_file(), "{dest:?}");
        let content = std::fs::read(&dest).unwrap();
        assert_eq!(content, b"MCHA-FAKE-FRPC-BINARY");
        // 幂等：再跑一次跳过下载
        let again = EnsureFrpcTool
            .run(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = again else {
            panic!("幂等复跑应成功");
        };
        assert!(content.contains("跳过"), "{content}");
    }

    #[tokio::test]
    async fn full_tunnel_flow_with_mock() {
        let (base, _created) = spawn_natfrp_mock(1);
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path(), &base, "good-token");

        let outcome = CheckTunnelTool
            .run(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");

        // frpc 就绪（start_tunnel 的前置）
        let outcome = EnsureFrpcTool
            .run(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");

        let outcome = SelectTunnelNodeTool
            .run(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = &outcome else {
            panic!("节点选择应成功：{outcome:?}");
        };
        assert!(content.contains("节点 ID 1"), "{content}");

        let outcome = CreateTunnelTool
            .run(
                serde_json::json!({ "node_id": 1, "local_port": 25565 }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = &outcome else {
            panic!("创建隧道应成功：{outcome:?}");
        };
        assert!(content.contains("114515"), "{content}");

        // start：无头启动器注入（poll 第 1 次离线 → 第 2 次在线）
        let tool = StartTunnelTool {
            launcher: Arc::new(|_dir, _script| Ok(())),
        };
        let outcome = tool
            .run(
                serde_json::json!({ "tunnel_id": 114515, "ready_timeout_secs": 10 }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = &outcome else {
            panic!("启动隧道应成功：{outcome:?}");
        };
        assert!(content.contains("在线"), "{content}");

        // 复用：同名同端口再建 → 复用现有（在线）
        let outcome = CreateTunnelTool
            .run(
                serde_json::json!({ "node_id": 1, "local_port": 25565 }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = &outcome else {
            panic!("复用应成功：{outcome:?}");
        };
        assert!(content.contains("复用"), "{content}");

        // 重复拉起守卫：已在线 → 拒绝
        let guard = tool
            .run(
                serde_json::json!({ "tunnel_id": 114515, "ready_timeout_secs": 10 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!guard.is_ok(), "已在线隧道应拒绝重复拉起：{guard:?}");

        // 状态快照
        let outcome = TunnelStatusTool
            .run(serde_json::json!({ "tunnel_id": 114515 }), &ctx)
            .await
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");

        // 删除
        let outcome = DeleteTunnelTool
            .run(serde_json::json!({ "tunnel_id": 114515 }), &ctx)
            .await
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
    }
}
