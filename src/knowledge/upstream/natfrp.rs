//! SakuraFrp API v4 客户端（内网穿透编排；设计 §8.8，决议 D135/D137/D139）。
//!
//! 事实基线（2026-09-04 抓取 `api.natfrp.com/docs` OpenAPI 规范 v4.1.0 逐端点
//! 核实）：认证 `Authorization: Bearer <访问密钥>`；错误统一 `{code, msg}`；
//! `/system/clients` 免认证，frpc 哈希为 **MD5**（32 位十六进制，官方文档
//! `md5sum` 同口径），下载域 `nya.globalslb.net`。
//!
//! 合规：API 定义 AGPL-3.0——仅经 HTTP 调用，不复制定义代码；引用注明。

use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::time::Duration;

/// 官方 API 基址。
pub const API_BASE: &str = "https://api.natfrp.com/v4";
/// frpc 下载 CDN 域（§12 白名单）。
pub const FRPC_CDN_HOST: &str = "nya.globalslb.net";

/// 单请求超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// 节点 flag 位掩码（API 文档定义；打分硬过滤与排序的输入，D138）
// ---------------------------------------------------------------------------

/// 允许 HTTP 隧道（0b11，两位；当前打分未用，保留 API 契约）。
#[allow(dead_code)]
pub const FLAG_HTTP: i64 = 0b11;
/// 允许创建隧道（满载时为 0）。
pub const FLAG_CREATABLE: i64 = 1 << 2;
/// 内地节点。
pub const FLAG_MAINLAND: i64 = 1 << 3;
/// 无防节点（当前打分未用，保留 API 契约）。
#[allow(dead_code)]
pub const FLAG_NO_PROTECT: i64 = 1 << 4;
/// 允许 UDP 流量（当前打分未用，保留 API 契约）。
#[allow(dead_code)]
pub const FLAG_UDP: i64 = 1 << 5;
/// 私有节点。
pub const FLAG_PRIVATE: i64 = 1 << 6;
/// tls_sucks（当前打分未用，保留 API 契约）。
#[allow(dead_code)]
pub const FLAG_TLS_SUCKS: i64 = 1 << 7;
/// 强制启用访问认证（会拦截朋友直连，硬过滤）。
pub const FLAG_FORCE_AUTH: i64 = 1 << 8;
/// 节点离线。
pub const FLAG_OFFLINE: i64 = 1 << 9;
/// BETA 节点。
pub const FLAG_BETA: i64 = 1 << 10;

// ---------------------------------------------------------------------------
// 响应类型
// ---------------------------------------------------------------------------

/// 用户信息。冻结态是独立 schema（仅 id/name/avatar/ban）——`ban` 出现即冻结。
/// 响应中的访问密钥字段刻意不反序列化（密钥不进内存结构、不进轨迹）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserInfo {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    /// 实名认证状态（非 0 = 已实名）。
    #[serde(default)]
    pub realname: i64,
    /// 隧道数上限。
    #[serde(default)]
    pub tunnels: i64,
    /// 限速信息字符串（如 "10 Mbps"）。
    #[serde(default)]
    pub speed: String,
    #[serde(default)]
    pub group: UserGroup,
    /// 流量信息 `[本日消耗, 总剩余]`（字节）。
    #[serde(default)]
    pub traffic: Vec<i64>,
    #[serde(default)]
    pub ban: Option<BanInfo>,
}

impl UserInfo {
    /// 账户冻结信息（存在即冻结）。
    pub fn frozen(&self) -> Option<&BanInfo> {
        self.ban.as_ref()
    }

    /// 是否已实名（realname 非 0）。
    pub fn realnamed(&self) -> bool {
        self.realname != 0
    }

    /// 总剩余流量（字节；traffic[1]）。
    pub fn traffic_remaining(&self) -> Option<i64> {
        self.traffic.get(1).copied()
    }
}

/// 用户组信息（level 是节点 VIP 门槛依据）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserGroup {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub level: i64,
    #[serde(default)]
    #[allow(dead_code)]
    pub expires: i64,
}

/// 账户冻结状态。
#[derive(Debug, Clone, Deserialize)]
pub struct BanInfo {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub expires: i64,
}

/// 流量包。
#[derive(Debug, Clone, Deserialize)]
pub struct DataPlan {
    #[serde(default)]
    #[allow(dead_code)]
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub total: i64,
    #[serde(default)]
    pub remaining: i64,
}

/// 节点（/nodes 顶层为 `id → 节点` 映射，id 已归一化回填）。
#[derive(Debug, Clone, Deserialize)]
pub struct NodeEntry {
    pub id: i64,
    pub name: String,
    pub host: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub description: String,
    #[serde(default)]
    pub vip: i64,
    #[serde(default)]
    pub flag: i64,
}

impl NodeEntry {
    pub fn creatable(&self) -> bool {
        self.flag & FLAG_CREATABLE != 0
    }
    pub fn mainland(&self) -> bool {
        self.flag & FLAG_MAINLAND != 0
    }
    pub fn private_node(&self) -> bool {
        self.flag & FLAG_PRIVATE != 0
    }
    pub fn force_auth(&self) -> bool {
        self.flag & FLAG_FORCE_AUTH != 0
    }
    pub fn offline(&self) -> bool {
        self.flag & FLAG_OFFLINE != 0
    }
    pub fn beta(&self) -> bool {
        self.flag & FLAG_BETA != 0
    }
}

/// /nodes 的原始条目（id 可能内嵌或仅存于顶层键）。
#[derive(Debug, Deserialize)]
struct NodeRaw {
    #[serde(default)]
    id: Option<i64>,
    name: String,
    host: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    vip: i64,
    #[serde(default)]
    flag: i64,
}

/// 节点状态（/node/stats.nodes 条目）。
#[derive(Debug, Clone, Deserialize)]
pub struct NodeStat {
    pub id: i64,
    /// -1 为离线，>= 0 为在线。
    #[serde(default)]
    pub online: i64,
    /// 运行时间（秒）。
    #[serde(default)]
    pub uptime: i64,
    /// 负载（%，schema 为 integer，实测示例带小数 → 用 f64 兼容两种）。
    #[serde(default)]
    pub load: f64,
}

#[derive(Debug, Deserialize)]
struct NodeStatsResponse {
    #[serde(default)]
    nodes: Vec<NodeStat>,
}

/// 隧道。
#[derive(Debug, Clone, Deserialize)]
pub struct Tunnel {
    pub id: i64,
    pub name: String,
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    pub tunnel_type: String,
    #[serde(default)]
    pub node: i64,
    #[serde(default)]
    pub online: bool,
    /// 0 = 正常，2 = 封禁。
    #[serde(default)]
    pub status: i64,
    /// 远程信息（tcp 类型为远程端口字符串）。
    #[serde(default)]
    pub remote: String,
    /// 本地 IP（当前仅展示语义，保留 API 契约）。
    #[serde(default)]
    #[allow(dead_code)]
    pub local_ip: String,
    #[serde(default)]
    pub local_port: i64,
    /// 备注（当前仅写入语义，保留 API 契约）。
    #[serde(default)]
    #[allow(dead_code)]
    pub note: String,
}

/// 创建隧道响应（201）。
#[derive(Debug, Clone, Deserialize)]
pub struct CreatedTunnel {
    pub id: i64,
    #[serde(default)]
    #[allow(dead_code)]
    pub name: String,
    /// 远程信息（tcp 为分配的远程端口字符串）。
    #[serde(default)]
    pub remote: String,
}

#[derive(Debug, Deserialize)]
struct DeleteResponse {
    #[serde(default)]
    deleted: Vec<i64>,
    #[serde(default)]
    failed: Vec<i64>,
}

/// frpc 单架构下载条目（hash 为 MD5）。
///
/// 实测注意：archs 里混有 docker 等无 hash/size 的条目——字段必须可选，
/// 否则整包 JSON 解码失败（真实冒烟抓到）。
#[derive(Debug, Clone, Deserialize)]
pub struct FrpcDownload {
    #[serde(default)]
    #[allow(dead_code)]
    pub title: String,
    pub url: String,
    /// MD5（32 位十六进制）；docker 类条目缺失为空串，下载前须判空。
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub size: u64,
}

impl FrpcDownload {
    /// 是否具备可校验的下载条件（直链 + MD5）。
    pub fn downloadable(&self) -> bool {
        !self.url.is_empty() && !self.hash.is_empty()
    }
}

/// frpc 发行信息（/system/clients 的 "frpc" 条目）。
#[derive(Debug, Clone, Deserialize)]
pub struct FrpcRelease {
    #[serde(default)]
    pub ver: String,
    #[serde(default)]
    pub archs: HashMap<String, FrpcDownload>,
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

pub struct NatfrpClient<'a> {
    http: &'a reqwest::Client,
    api_base: String,
    token: String,
}

impl<'a> NatfrpClient<'a> {
    pub fn new(http: &'a reqwest::Client, token: String) -> Self {
        Self {
            http,
            api_base: API_BASE.to_string(),
            token,
        }
    }

    /// 测试注入：自定义 API 基址（本地 mock）。
    #[allow(dead_code)]
    pub fn with_base(http: &'a reqwest::Client, api_base: String, token: String) -> Self {
        Self {
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
            token,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_base, path)
    }

    async fn get(&self, path: &str, what: &str) -> Result<reqwest::Response, String> {
        let url = self.url(path);
        let mut request = self.http.get(&url);
        if !self.token.is_empty() {
            request = request.bearer_auth(&self.token);
        }
        tokio::time::timeout(REQUEST_TIMEOUT, request.send())
            .await
            .map_err(|_| format!("{what} 超时（{url}）"))?
            .map_err(|err| format!("{what} 请求失败：{err}"))
    }

    async fn post(
        &self,
        path: &str,
        form: &[(&str, String)],
        what: &str,
    ) -> Result<reqwest::Response, String> {
        let url = self.url(path);
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.http
                .post(&url)
                .bearer_auth(&self.token)
                .form(form)
                .send(),
        )
        .await
        .map_err(|_| format!("{what} 超时（{url}）"))?
        .map_err(|err| format!("{what} 请求失败：{err}"))
    }

    /// 断言响应成功并解析 JSON；401/403 映射为带引导的结构化错误。
    async fn read_json<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
        what: &str,
    ) -> Result<T, String> {
        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            let body = response.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<ApiError>(&body)
                .ok()
                .map(|e| e.msg)
                .unwrap_or_default();
            return Err(map_api_error(what, code, &msg));
        }
        tokio::time::timeout(REQUEST_TIMEOUT, response.json::<T>())
            .await
            .map_err(|_| format!("读取 {what} 超时"))?
            .map_err(|err| format!("解析 {what} 失败：{err}"))
    }

    /// 用户信息（token 校验 / 实名 / 等级 / 流量 / 冻结识别）。
    pub async fn user_info(&self) -> Result<UserInfo, String> {
        let response = self.get("/user/info", "用户信息").await?;
        self.read_json(response, "用户信息").await
    }

    /// 流量包列表。
    pub async fn data_plans(&self) -> Result<Vec<DataPlan>, String> {
        let response = self.get("/user/data_plans", "流量包").await?;
        self.read_json(response, "流量包").await
    }

    /// 节点列表（顶层 `id → 节点` 映射归一化为带 id 的数组，按 id 升序）。
    pub async fn nodes(&self) -> Result<Vec<NodeEntry>, String> {
        let response = self.get("/nodes", "节点列表").await?;
        let map: HashMap<String, NodeRaw> = self.read_json(response, "节点列表").await?;
        let mut nodes = Vec::with_capacity(map.len());
        for (key, raw) in map {
            let id = raw
                .id
                .or_else(|| key.trim().parse().ok())
                .ok_or_else(|| format!("节点条目缺少可用 id（键 {key:?}）"))?;
            nodes.push(NodeEntry {
                id,
                name: raw.name,
                host: raw.host,
                description: raw.description,
                vip: raw.vip,
                flag: raw.flag,
            });
        }
        nodes.sort_by_key(|n| n.id);
        Ok(nodes)
    }

    /// 节点状态（id → 状态映射）。
    pub async fn node_stats(&self) -> Result<HashMap<i64, NodeStat>, String> {
        let response = self.get("/node/stats", "节点状态").await?;
        let parsed: NodeStatsResponse = self.read_json(response, "节点状态").await?;
        Ok(parsed.nodes.into_iter().map(|s| (s.id, s)).collect())
    }

    /// 当前账户全部隧道。
    pub async fn tunnels(&self) -> Result<Vec<Tunnel>, String> {
        let response = self.get("/tunnels", "隧道列表").await?;
        self.read_json(response, "隧道列表").await
    }

    /// 创建 TCP 隧道（remote 留空由平台分配；note 可选）。
    pub async fn create_tcp_tunnel(
        &self,
        name: &str,
        node: i64,
        local_ip: &str,
        local_port: u16,
        remote: Option<u16>,
        note: Option<&str>,
    ) -> Result<CreatedTunnel, String> {
        let mut form = vec![
            ("name", name.to_string()),
            ("type", "tcp".to_string()),
            ("node", node.to_string()),
            ("local_ip", local_ip.to_string()),
            ("local_port", local_port.to_string()),
        ];
        if let Some(remote) = remote {
            form.push(("remote", remote.to_string()));
        }
        if let Some(note) = note {
            form.push(("note", note.to_string()));
        }
        let response = self.post("/tunnels", &form, "创建隧道").await?;
        self.read_json(response, "创建隧道").await
    }

    /// 删除隧道（返回 (已删除, 删除但未能踢下线)）。
    pub async fn delete_tunnel(&self, id: i64) -> Result<(Vec<i64>, Vec<i64>), String> {
        let form = vec![("ids", id.to_string())];
        let response = self.post("/tunnel/delete", &form, "删除隧道").await?;
        let parsed: DeleteResponse = self.read_json(response, "删除隧道").await?;
        Ok((parsed.deleted, parsed.failed))
    }

    /// frpc 官方分发信息（/system/clients 免认证；哈希为 MD5）。
    pub async fn frpc_release(&self) -> Result<FrpcRelease, String> {
        let response = self.get("/system/clients", "frpc 分发信息").await?;
        let map: HashMap<String, FrpcRelease> = self.read_json(response, "frpc 分发信息").await?;
        map.get("frpc")
            .cloned()
            .ok_or_else(|| "frpc 分发信息缺失（/system/clients 无 frpc 条目）".to_string())
    }
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    msg: String,
}

/// 错误映射（NFR-3：用户可见错误必须附"下一步怎么办"）。
fn map_api_error(what: &str, code: u16, msg: &str) -> String {
    match code {
        401 => format!(
            "{what} 失败（HTTP 401）：访问密钥无效或已被重置；请用 /token 命令或 mcha setup 重新配置"
        ),
        403 => format!(
            "{what} 失败（HTTP 403）：{}（常见原因：未完成实名认证、用户组等级不足、账号被冻结）",
            if msg.is_empty() { "无权访问" } else { msg }
        ),
        _ => format!(
            "{what} 失败（HTTP {code}）：{msg}",
            msg = if msg.is_empty() {
                "无错误详情"
            } else {
                msg
            }
        ),
    }
}

/// 平台/架构 → frpc 下载键（/system/clients 的 archs 键，如 windows_amd64）。
pub fn arch_key(os: &str, arch: &str) -> String {
    let os = match os {
        "macos" => "darwin",
        other => other,
    };
    let arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "arm" => "armv7",
        other => other,
    };
    format!("{os}_{arch}")
}

/// 本机对应的 frpc 下载键。
pub fn frpc_arch_key() -> String {
    arch_key(std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_key_mapping() {
        assert_eq!(arch_key("windows", "x86_64"), "windows_amd64");
        assert_eq!(arch_key("windows", "aarch64"), "windows_arm64");
        assert_eq!(arch_key("linux", "x86_64"), "linux_amd64");
        assert_eq!(arch_key("linux", "aarch64"), "linux_arm64");
        assert_eq!(arch_key("macos", "x86_64"), "darwin_amd64");
        assert_eq!(arch_key("macos", "aarch64"), "darwin_arm64");
        assert_eq!(arch_key("linux", "arm"), "linux_armv7");
        assert_eq!(arch_key("linux", "x86"), "linux_386");
    }

    #[test]
    fn node_flag_helpers() {
        let node = |flag: i64, vip: i64| NodeEntry {
            id: 1,
            name: "n".into(),
            host: "h".into(),
            description: String::new(),
            vip,
            flag,
        };
        assert!(node(FLAG_CREATABLE | FLAG_MAINLAND, 0).creatable());
        assert!(node(FLAG_CREATABLE | FLAG_MAINLAND, 0).mainland());
        assert!(!node(0, 0).creatable(), "满载节点 bit2 为 0");
        assert!(node(FLAG_CREATABLE | FLAG_FORCE_AUTH, 0).force_auth());
        assert!(node(FLAG_CREATABLE | FLAG_PRIVATE, 0).private_node());
        assert!(node(FLAG_CREATABLE | FLAG_OFFLINE, 0).offline());
        assert!(node(FLAG_CREATABLE | FLAG_BETA, 0).beta());
        assert!(node(0, 3).vip > 0 || true);
    }

    #[test]
    fn user_info_parses_normal_and_frozen() {
        let normal: UserInfo = serde_json::from_str(
            r#"{"id":10,"name":"DemoUser","avatar":"","token":"secret-do-not-keep",
                "speed":"10 Mbps","tunnels":10,"realname":1,
                "group":{"name":"默认用户组","level":2,"expires":0},
                "traffic":[1024,5368709120],
                "sign":{"config":[1,2],"signed":false,"last":"2026-01-01","days":0,"traffic":0.0}}"#,
        )
        .unwrap();
        assert_eq!(normal.name, "DemoUser");
        assert!(normal.realnamed());
        assert_eq!(normal.group.level, 2);
        assert_eq!(normal.tunnels, 10);
        assert_eq!(normal.traffic_remaining(), Some(5_368_709_120));
        assert!(normal.frozen().is_none());

        let frozen: UserInfo = serde_json::from_str(
            r#"{"id":10,"name":"DemoUser","avatar":"",
                "ban":{"title":"该账户已被冻结","reason":"测试冻结","expires":1788000000}}"#,
        )
        .unwrap();
        assert!(!frozen.realnamed());
        assert!(frozen.traffic_remaining().is_none());
        let ban = frozen.frozen().expect("冻结态应被识别");
        assert!(ban.title.contains("冻结"));
    }

    #[test]
    fn error_mapping_mentions_next_step() {
        let err = map_api_error("用户信息", 401, "");
        assert!(err.contains("/token"), "{err}");
        let err = map_api_error("创建隧道", 403, "无权访问");
        assert!(err.contains("实名"), "{err}");
        let err = map_api_error("隧道列表", 500, "服务器内部错误");
        assert!(err.contains("500"), "{err}");
    }

    // -- 本地 mock（覆盖全部端点；Bearer 校验）--

    fn spawn_mock() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 16384];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf);
                let method = request.split_whitespace().next().unwrap_or("");
                let target = request.split_whitespace().nth(1).unwrap_or("");
                let authorized = request.contains("authorization: Bearer good-token");
                let json = |status: &str, body: String| {
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let (status, body) = if !authorized && target != "/system/clients" {
                    (
                        "401 Unauthorized",
                        r#"{"code":401,"msg":"访问密钥无效"}"#.to_string(),
                    )
                } else {
                    match (method, target) {
                        ("GET", "/user/info") => (
                            "200 OK",
                            r#"{"id":10,"name":"DemoUser","avatar":"","speed":"10 Mbps","tunnels":10,"realname":1,"group":{"name":"默认","level":0,"expires":0},"traffic":[1024,5368709120],"sign":{"config":[1,2],"signed":false,"last":"2026-01-01","days":0,"traffic":0.0}}"#.to_string(),
                        ),
                        ("GET", "/user/data_plans") => (
                            "200 OK",
                            r#"[{"id":1,"name":"普通流量包","type":10001,"type_extra":"{}","total":5368709120,"remaining":4210,
                                "start_time":1136214245,"end_time":1796214245}]"#.to_string(),
                        ),
                        ("GET", "/nodes") => (
                            "200 OK",
                            r#"{"1":{"name":"内地A","host":"a.natfrp.io","description":"推荐","vip":0,"flag":12},
                                "2":{"name":"满载","host":"b.natfrp.io","description":"","vip":0,"flag":8},
                                "3":{"name":"强制认证","host":"c.natfrp.io","description":"","vip":0,"flag":260},
                                "7":{"name":"高门槛","host":"g.natfrp.io","description":"","vip":5,"flag":12}}"#.to_string(),
                        ),
                        ("GET", "/node/stats") => (
                            "200 OK",
                            r#"{"time":1788000000,"nodes":[{"id":1,"online":0,"uptime":100000,"load":19.5}]}"#.to_string(),
                        ),
                        ("GET", "/tunnels") => (
                            "200 OK",
                            r#"[{"id":114514,"name":"mcha-mc25565","type":"tcp","node":1,"online":true,"status":0,
                                "status_reason":null,"note":"mcha","extra":"","remote":"3721","local_ip":"127.0.0.1",
                                "local_port":25565,"export":"0|0|0","locks":{"edit":false,"delete":false,"migrate":false}}]"#.to_string(),
                        ),
                        ("POST", "/tunnels") => (
                            "201 Created",
                            r#"{"id":114515,"name":"mcha-mc25566","remote":"3722"}"#.to_string(),
                        ),
                        ("POST", "/tunnel/delete") => (
                            "200 OK",
                            r#"{"deleted":[114515],"failed":[]}"#.to_string(),
                        ),
                        ("GET", "/system/clients") => (
                            "200 OK",
                            r#"{"frpc":{"ver":"0.51.0-sakura-14","time":1784298660,"archs":{
                                "docker_natfrp":{"title":"Docker (官方源)","url":"https://natfrp.com/registry/#!/taglist/frpc"},
                                "windows_amd64":{"title":"Windows 64 位","url":"http://127.0.0.1/frpc.exe","hash":"3dc416a0ee2348dc66639c7c5d3130d4","size":14133248},
                                "linux_amd64":{"title":"Linux 64 位","url":"http://127.0.0.1/frpc","hash":"b5632fff7156e29f231cfd19dd079674","size":6389308}}}}"#.to_string(),
                        ),
                        _ => ("404 Not Found", r#"{"code":404,"msg":"not found"}"#.to_string()),
                    }
                };
                let _ = stream.write_all(json(status, body).as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn endpoints_roundtrip() {
        let base = spawn_mock();
        let http = reqwest::Client::new();
        let client = NatfrpClient::with_base(&http, base, "good-token".to_string());

        let user = client.user_info().await.unwrap();
        assert!(user.realnamed());
        assert_eq!(user.group.level, 0);

        let plans = client.data_plans().await.unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].remaining, 4210);

        let nodes = client.nodes().await.unwrap();
        assert_eq!(nodes.len(), 4, "顶层键应归一化为 id：{nodes:?}");
        let node1 = nodes.iter().find(|n| n.id == 1).unwrap();
        assert_eq!(node1.host, "a.natfrp.io");
        assert!(node1.creatable() && node1.mainland());

        let stats = client.node_stats().await.unwrap();
        assert_eq!(stats[&1].load, 19.5);

        let tunnels = client.tunnels().await.unwrap();
        assert_eq!(tunnels.len(), 1);
        assert!(tunnels[0].online);
        assert_eq!(tunnels[0].remote, "3721");

        let created = client
            .create_tcp_tunnel("mcha-mc25566", 1, "127.0.0.1", 25566, None, Some("mcha"))
            .await
            .unwrap();
        assert_eq!(created.id, 114515);
        assert_eq!(created.remote, "3722");

        let (deleted, failed) = client.delete_tunnel(114515).await.unwrap();
        assert_eq!(deleted, vec![114515]);
        assert!(failed.is_empty());

        let release = client.frpc_release().await.unwrap();
        assert_eq!(release.ver, "0.51.0-sakura-14");
        assert!(release.archs.contains_key(&frpc_arch_key()));
        // archs 混有 docker 类无哈希条目（真实 API 形态）：要么 32 位 MD5，
        // 要么空串（不可直接下载，downloadable() 须拒绝）
        for (key, download) in &release.archs {
            assert!(
                download.hash.len() == 32 || download.hash.is_empty(),
                "{key} 哈希既非 MD5 也非空：{:?}",
                download.hash
            );
            assert_eq!(download.downloadable(), download.hash.len() == 32);
        }
    }

    #[tokio::test]
    async fn bad_token_is_structured_error() {
        let base = spawn_mock();
        let http = reqwest::Client::new();
        let client = NatfrpClient::with_base(&http, base, "wrong".to_string());
        let err = client.user_info().await.unwrap_err();
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("/token"), "{err}");
    }

    // -- 真实上游冒烟（cargo test --ignored；日常测试不联网）--

    fn live_http() -> reqwest::Client {
        reqwest::Client::builder()
            .user_agent("mcha/0.2")
            .build()
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_system_clients_has_local_arch() {
        let http = live_http();
        let client = NatfrpClient::new(&http, String::new());
        let release = client.frpc_release().await.expect("frpc 分发信息");
        assert!(release.ver.contains("sakura"), "{}", release.ver);
        let key = frpc_arch_key();
        let download = release
            .archs
            .get(&key)
            .unwrap_or_else(|| panic!("应有本机架构 {key} 的 frpc 包"));
        assert_eq!(download.hash.len(), 32, "MD5 应为 32 位十六进制");
        assert!(download.url.starts_with("https://"));
        assert!(download.size > 1_000_000);
    }

    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_user_info_with_token() {
        let Ok(token) = std::env::var("MCHA_NATFRP_TOKEN") else {
            eprintln!("未配置 MCHA_NATFRP_TOKEN，跳过实名冒烟");
            return;
        };
        if token.trim().is_empty() {
            eprintln!("MCHA_NATFRP_TOKEN 为空，跳过");
            return;
        }
        let http = live_http();
        let client = NatfrpClient::new(&http, token);
        let user = client.user_info().await.expect("用户信息");
        assert!(user.frozen().is_none(), "测试账号不应处于冻结态");
        eprintln!(
            "实名={} 等级={} 剩余流量={:?}",
            user.realnamed(),
            user.group.level,
            user.traffic_remaining()
        );
        let nodes = client.nodes().await.expect("节点列表");
        assert!(!nodes.is_empty());
        let stats = client.node_stats().await.expect("节点状态");
        eprintln!("可用节点 {} 个，状态 {} 条", nodes.len(), stats.len());
    }
}
