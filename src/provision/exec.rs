//! 执行流水线（§8.5）：环境复检 → Java 供给 → 下载校验 → mod 安装 →
//! 配置生成 → 启动 → 就绪检测。每步为事务边界，逐任务发进度事件（R4）。

use std::path::{Path, PathBuf};

use sha1::Digest as _;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::events::{EventBus, ProgressEvent, TaskId, TraceEvent, TraceKind, TraceStep};
use crate::knowledge::KnowledgeBase;
use crate::knowledge::upstream::{
    DownloadItem, FabricClient, HttpBase, ModrinthClient, MojangClient, PaperClient, SpigotClient,
};
use crate::spec::{AccountPolicy, NetworkPlan, ServerSoftware, ServerSpec};

use super::process::{ProcessError, ServerProcess};

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("预检失败：{0}")]
    Preflight(String),
    #[error("{0}")]
    Java(#[from] super::java::JavaError),
    #[error("{0}")]
    Upstream(#[from] crate::knowledge::upstream::UpstreamError),
    #[error("{0}")]
    Knowledge(#[from] crate::knowledge::KnowledgeError),
    #[error("{0}")]
    Process(#[from] ProcessError),
    #[error("写配置失败：{0}")]
    Io(String),
    #[error("任务已取消")]
    Cancelled,
    #[error("LLM 调用失败：{0}")]
    Llm(#[from] crate::llm::LlmError),
    #[error("部署编排失败：{0}")]
    Provision(String),
}

/// 部署上下文：全部依赖显式注入（可测试、可解释）。
/// `cfg` 当前用于网络底座构建；P1 穿透编排（token、镜像策略）将直接读取。
pub struct DeployContext {
    #[allow(dead_code)]
    pub cfg: AppConfig,
    pub kb: KnowledgeBase,
    pub http: HttpBase,
    pub bus: EventBus,
    pub cancel: CancellationToken,
}

impl DeployContext {
    pub fn new(
        cfg: AppConfig,
        kb: KnowledgeBase,
        bus: EventBus,
        cancel: CancellationToken,
    ) -> Result<Self, DeployError> {
        let http = HttpBase::new(&cfg)?;
        Ok(Self {
            cfg,
            kb,
            http,
            bus,
            cancel,
        })
    }

    /// 服务端安装目录（FR-19，决议 D11/D18，v0.10.2 彻底拍平）：
    /// 就是你指定的目录本身——工作区根目录，不再有 `<spec_id>/` 子目录
    /// （spec_id 仅用于档案命名与 motd）。同一目录跑第二个服前会有
    /// 已有文件确认拦截（cli 层），多服靠选不同目录隔离。
    /// 工作区解析（env > config > 默认当前目录）与可写性校验由 config 负责；
    /// 档案元数据 spec.json 仍统一存数据目录（store::save_profile），互不影响。
    fn server_dir(&self) -> Result<PathBuf, DeployError> {
        self.cfg
            .workspace_dir()
            .map_err(|e| DeployError::Preflight(e.to_string()))
    }
}

/// 部署结果：托管中的服务端 + 连接说明（FR-07）。
pub struct DeployResult {
    pub server: ServerProcess,
    pub connection: ConnectionInfo,
}

/// 朋友们怎么连（连接说明卡片）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConnectionInfo {
    pub lines: Vec<String>,
}

/// 主入口：把一份已确认的 ServerSpec 部署为运行中的服务器。
/// 全程确定性执行，LLM 不参与（设计原则 3）。
/// 外层捕获失败：中断的步骤以失败摘要收尾（进度 + Exec 轨迹，决议 D19）。
pub async fn deploy(
    spec: &mut ServerSpec,
    ctx: &DeployContext,
    task_id: &TaskId,
    svc: &crate::llm::LlmService,
) -> Result<DeployResult, DeployError> {
    let mut track = StepTrack::default();
    let result = deploy_inner(spec, ctx, task_id, svc, &mut track).await;
    if let Err(e) = &result {
        track.finish_failed(ctx, task_id, &e.to_string());
    }
    result
}

/// 当前执行步骤追踪（决议 D19）：失败时补发该步骤的失败进度与轨迹，
/// 避免 `?` 早退路径留下"永远旋转的进度条 + trace 缺一步"。
#[derive(Default)]
pub(crate) struct StepTrack {
    /// (step id, 标题)；step_done 消费，finish_failed 兜底消费
    current: Option<(&'static str, String)>,
}

impl StepTrack {
    fn finish_failed(&mut self, ctx: &DeployContext, task_id: &TaskId, error: &str) {
        if let Some((step, title)) = self.current.take() {
            ctx.bus.publish(ProgressEvent::StepFinished {
                task_id: task_id.clone(),
                step: step.into(),
                ok: false,
                detail: Some(error.to_string()),
            });
            exec_trace(ctx, task_id, &format!("{title}：✘ {error}"), step);
        }
    }
}

/// 发布一条 Exec 轨迹步骤（决议 D19：执行流水线全程留痕，R5）。
pub(crate) fn exec_trace(ctx: &DeployContext, task_id: &TaskId, summary: &str, step: &str) {
    ctx.bus.publish(TraceEvent::StepAdded {
        task_id: task_id.clone(),
        step: TraceStep {
            kind: TraceKind::Exec,
            summary: summary.into(),
            usage_refs: vec![],
            at: chrono::Local::now(),
            detail: Some(serde_json::json!({ "step": step })),
        },
    });
}

async fn deploy_inner(
    spec: &mut ServerSpec,
    ctx: &DeployContext,
    task_id: &TaskId,
    svc: &crate::llm::LlmService,
    track: &mut StepTrack,
) -> Result<DeployResult, DeployError> {
    if ctx.cancel.is_cancelled() {
        return Err(DeployError::Cancelled);
    }

    // 服务端目录提前解析（FR-19/D18）：工作区不可写应在下载前失败，
    // 且安装位置必须显式可见（用户不再需要猜文件装到哪了）
    let server_dir = ctx.server_dir()?;

    step_begin(ctx, task_id, "preflight", "环境复检", track);
    preflight(spec, ctx, task_id, &server_dir).await?;
    step_done(ctx, task_id, true, None, track);

    // Java 供给（§8.8）：required_major 以官方动态值为准（v0.9 与 check_version_compat
    // 同口径，"能查就不猜"），上游不可达时回落知识库静态表；决策树此前的静态外推
    // 若与官方口径不一致（如版本制式切换），以此处为准并留痕
    let manifest_major = match MojangClient::new(ctx.http.clone())
        .version_java_major(&spec.mc_version)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "获取 {} 的官方 Java 需求失败，回落静态表：{e}",
                spec.mc_version
            );
            None
        }
    };
    let (official_major, source) =
        crate::knowledge::resolve_java_major(manifest_major, &ctx.kb, &spec.mc_version);
    let official_major = official_major.unwrap_or(21);
    if spec.java.required_major != official_major {
        tracing::info!(
            "Java 需求校准：{} 官方要求 Java {official_major}（口径 {source:?}），原值 {}",
            spec.mc_version,
            spec.java.required_major
        );
        spec.java.required_major = official_major;
    }

    // ── 部署编排环（决议 D25，v0.12）────────────────────────────
    // Java 供给 / jar 获取 / mod / 配置 / 启动 / 验证不再硬编码顺序，
    // 由 LLM 逐工具调用编排；失败结构化回环（重试 / 换渠道 / 问用户），
    // 不再"一步失败全任务崩"。确定性只保留 preflight、上面的需求校准、
    // 以及最终的 probe_port 就绪验证（在编排环内完成）。
    let mut agent = super::agent::ProvisionAgent::new(svc, ctx, spec, task_id, &server_dir);
    agent.run().await?;
    let server = agent.take_server().ok_or(DeployError::Preflight(
        "编排环结束但服务端进程未启动".into(),
    ))?;

    spec.server_dir = Some(server_dir.to_string_lossy().to_string());
    Ok(DeployResult {
        server,
        connection: connection_info(spec),
    })
}

async fn preflight(
    spec: &mut ServerSpec,
    ctx: &DeployContext,
    task_id: &TaskId,
    server_dir: &Path,
) -> Result<(), DeployError> {
    // 版本存在性复检（决策树可能未带官方清单缓存）。
    // 规范 id 原则（§8.4 v0.9.6）：精确匹配失败先做语义比对并自愈——
    // 存量 spec 里可能是归一化串（如 26.2.0），语义上就是清单里的 26.2，
    // 应校正后继续，而不是自相矛盾地拒绝用户要的版本。
    let mojang = MojangClient::new(ctx.http.clone());
    let releases = mojang.release_versions().await?;
    if !releases.iter().any(|r| r == &spec.mc_version) {
        match crate::knowledge::canonicalize_version(&releases, &spec.mc_version) {
            Some(canonical) => {
                tracing::info!(
                    "版本号校正：{} → {canonical}（官方清单原文，语义相等）",
                    spec.mc_version
                );
                spec.mc_version = canonical;
            }
            None => {
                let suggestions =
                    crate::knowledge::suggest_versions(&releases, &spec.mc_version, 5);
                return Err(DeployError::Preflight(format!(
                    "MC 版本 {} 不存在。相近版本：{}",
                    spec.mc_version,
                    suggestions.join(", ")
                )));
            }
        }
    }
    ctx.bus.publish(ProgressEvent::StepProgress {
        task_id: task_id.clone(),
        step: "preflight".into(),
        current: 1,
        total: Some(2),
        detail: Some(format!(
            "版本 {} 校验通过，安装目录：{}",
            spec.mc_version,
            server_dir.display()
        )),
    });

    // 端口占用检查：绑定成功后立即释放
    let port = spec.port;
    let bind = tokio::net::TcpListener::bind(("0.0.0.0", port)).await;
    match bind {
        Ok(listener) => drop(listener),
        Err(e) => {
            return Err(DeployError::Preflight(format!(
                "端口 {port} 已被占用（{e}）。请释放端口或在方案中改用其它端口"
            )));
        }
    }
    ctx.bus.publish(ProgressEvent::StepProgress {
        task_id: task_id.clone(),
        step: "preflight".into(),
        current: 2,
        total: Some(2),
        detail: Some(format!("端口 {port} 空闲")),
    });
    Ok(())
}

/// 按服务端类型从官方 API / 约定镜像产出主 jar 下载项；Fabric 顺带回填 loader 版本。
pub(crate) async fn server_jar_item(
    spec: &mut ServerSpec,
    ctx: &DeployContext,
) -> Result<DownloadItem, DeployError> {
    match spec.software.clone() {
        ServerSoftware::Vanilla => Ok(MojangClient::new(ctx.http.clone())
            .server_jar(&spec.mc_version)
            .await?),
        // 决议 D22：用户点名 spigot 就用 spigot（getbukkit 镜像渠道）
        ServerSoftware::Spigot => Ok(SpigotClient::new(ctx.http.clone())
            .server_jar(&spec.mc_version)
            .await?),
        ServerSoftware::Paper { .. } => Ok(PaperClient::new(ctx.http.clone())
            .server_jar(&spec.mc_version)
            .await?),
        ServerSoftware::Fabric { .. } => {
            let resolved = FabricClient::new(ctx.http.clone())
                .resolve_server(&spec.mc_version)
                .await?;
            // 把 L2 实时查询结果回填进 spec（决策树的 L2 补全点）
            spec.software = ServerSoftware::Fabric {
                loader_version: resolved.loader_version.clone(),
                installer_version: resolved.installer_version.clone(),
            };
            Ok(resolved.item)
        }
    }
}

pub(crate) async fn download(
    ctx: &DeployContext,
    task_id: &TaskId,
    step: &str,
    item: &DownloadItem,
    dest_dir: &Path,
) -> Result<PathBuf, DeployError> {
    let bus_step = step.to_string();
    let path = ctx
        .http
        .download(item, dest_dir, ctx.cancel.clone(), &|current, total| {
            ctx.bus.publish(ProgressEvent::StepProgress {
                task_id: task_id.clone(),
                step: bus_step.clone(),
                current,
                total,
                detail: Some(format!(
                    "下载 {} {}/{} 字节",
                    item.file_name,
                    current,
                    total.map(|t| t.to_string()).unwrap_or_else(|| "?".into())
                )),
            });
        })
        .await?;
    Ok(path)
}

/// 软件类型的中文标签（进度与轨迹展示用）。
pub(crate) fn software_label(sw: &ServerSoftware) -> &'static str {
    match sw {
        ServerSoftware::Vanilla => "原版",
        ServerSoftware::Spigot => "Spigot",
        ServerSoftware::Paper { .. } => "Paper",
        ServerSoftware::Fabric { .. } => "Fabric",
    }
}

/// 应急通道判定（决议 D22 v0.11.1）：安装目录已有同名 jar 时是否复用。
/// 返回 Ok(Some(说明)) = 复用（说明入轨迹）；Ok(None) = 走正常下载。
/// 规则：有哈希 → 实算比对，匹配才复用，不符重下覆盖；
/// 无哈希（getbukkit 镜像常态）→ 文件非空且 ≥1MB 即复用，轨迹明示
/// "第三方来源未校验"；过小文件视为占位/残件，直接重下。
pub(crate) async fn reuse_existing_jar(
    path: &Path,
    item: &DownloadItem,
) -> Result<Option<String>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() < 1024 * 1024 {
        return Ok(None);
    }
    let has_hash = item.sha1.is_some() || item.sha256.is_some();
    if !has_hash {
        return Ok(Some(format!(
            "已有同名文件（{} 字节），该来源无官方哈希可校验，按应急通道复用（第三方来源未校验）",
            meta.len()
        )));
    }
    let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    if let Some(expected) = &item.sha256 {
        let actual = hex::encode(sha2::Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(expected) {
            return Ok(None);
        }
    }
    if let Some(expected) = &item.sha1 {
        let actual = hex::encode(sha1::Sha1::digest(&bytes));
        if !actual.eq_ignore_ascii_case(expected) {
            return Ok(None);
        }
    }
    Ok(Some(format!(
        "已有同名文件（{} 字节），哈希校验通过，复用本地文件",
        meta.len()
    )))
}

/// mod 名称 → Modrinth 依赖闭包（别名表优先，检索兜底，全部确定性）。
pub(crate) async fn resolve_all_mods(
    spec: &ServerSpec,
    ctx: &DeployContext,
) -> Result<Vec<crate::spec::ModRef>, DeployError> {
    let modrinth = ModrinthClient::new(ctx.http.clone());
    let loader = "fabric";
    let mut resolved: Vec<crate::spec::ModRef> = Vec::new();
    for name in &spec.mod_names {
        let project = ctx
            .kb
            .alias_lookup(name)
            .ok_or_else(|| crate::knowledge::KnowledgeError::ModNotFound(name.clone()))?;
        let mod_ref = modrinth
            .resolve_mod(&project, &spec.mc_version, loader)
            .await?;
        resolved.push(mod_ref);
    }
    Ok(crate::knowledge::flatten_mods(&resolved))
}

/// 写 eula.txt / server.properties / 启动脚本 / whitelist.json
/// （FR-04 + 决议 D23；决策树节点落配置）。
pub(crate) fn write_configs(
    spec: &ServerSpec,
    server_dir: &Path,
    jar_path: &Path,
) -> Result<(), DeployError> {
    std::fs::create_dir_all(server_dir)
        .map_err(|e| DeployError::Io(format!("创建服务端目录：{e}")))?;

    // EULA：用户在方案确认环节已同意（FR-17 二次确认在 CLI）
    std::fs::write(server_dir.join("eula.txt"), "eula=true\n")
        .map_err(|e| DeployError::Io(format!("写 eula.txt：{e}")))?;

    let online_mode = matches!(spec.account, AccountPolicy::Online);
    let whitelist = match &spec.account {
        AccountPolicy::Offline { whitelist } | AccountPolicy::Hybrid { whitelist, .. } => {
            whitelist.clone()
        }
        AccountPolicy::Online => vec![],
    };
    // 白名单开关必须与名单是否非空一致（v0.9.5）：
    // 空名单 + white-list=true 会拒绝所有玩家进入
    let whitelist_enabled = !whitelist.is_empty();
    let props = format!(
        "# by mcha\n\
         online-mode={online_mode}\n\
         white-list={whitelist_enabled}\n\
         enforce-whitelist={whitelist_enabled}\n\
         server-port={port}\n\
         max-players={players}\n\
         motd={spec_id}\n\
         enable-command-block=false\n\
         view-distance=8\n",
        online_mode = online_mode,
        port = spec.port,
        players = spec.max_players,
        spec_id = spec.spec_id,
    );
    std::fs::write(server_dir.join("server.properties"), props)
        .map_err(|e| DeployError::Io(format!("写 server.properties：{e}")))?;

    // 启动脚本落盘（决议 D23）：与 mcha 托管启动完全同参数，
    // 用户以后可双击脚本自行开服，不依赖 mcha 在场
    if let Some(java_path) = super::java::managed_java_path(&spec.java.runtime) {
        let jar_name = jar_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let script = start_script_content(
            cfg!(windows),
            &java_path,
            &jar_name,
            spec.jvm_memory_mb,
            &spec.spec_id,
        );
        let script_path = if cfg!(windows) {
            server_dir.join("start.bat")
        } else {
            server_dir.join("start.sh")
        };
        std::fs::write(&script_path, script)
            .map_err(|e| DeployError::Io(format!("写启动脚本：{e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755));
        }
    }

    if !whitelist.is_empty() {
        // whitelist.json 每行一个对象（Minecraft 接受 JSON 数组）
        let entries: Vec<String> = whitelist
            .iter()
            .map(|n| format!(r#"{{"name": "{n}"}}"#))
            .collect();
        std::fs::write(
            server_dir.join("whitelist.json"),
            format!("[{}]\n", entries.join(",\n")),
        )
        .map_err(|e| DeployError::Io(format!("写 whitelist.json：{e}")))?;
    }

    // 跨网络指引落盘（P1 穿透编排交付前的可用指引，FR-07/FR-08 过渡）
    // v0.10.1 目录拍平后与服务器文件同层
    if let NetworkPlan::Direct { firewall_hint } = &spec.network {
        std::fs::write(
            server_dir.join("connection-hint.txt"),
            format!(
                "跨网络联机指引：\n1. {firewall_hint}\n2. 把你的公网 IP 告诉朋友，连接地址：<公网IP>:{}\n3. 无公网 IP 时，请等待樱花frp 穿透编排功能（P1）\n",
                spec.port
            ),
        )
        .map_err(|e| DeployError::Io(format!("写 connection-hint.txt：{e}")))?;
    }
    Ok(())
}

/// 生成启动脚本内容（决议 D23）。`windows` 显式入参便于双形态单测：
/// Windows 产 start.bat（CRLF、`cd /d %~dp0`、java 绝对路径含空格必须加引号）；
/// 其余平台产 start.sh。
fn start_script_content(
    windows: bool,
    java_path: &str,
    jar_name: &str,
    mem_mb: u32,
    spec_id: &str,
) -> String {
    let xms = mem_mb / 2;
    if windows {
        format!(
            "@echo off\r\n\
             title MCHA - {spec_id}\r\n\
             cd /d %~dp0\r\n\
             \"{java}\" -Xms{xms}M -Xmx{mem}M -jar {jar} nogui\r\n\
             pause\r\n",
            java = java_path,
            jar = jar_name,
            mem = mem_mb,
        )
    } else {
        format!(
            "#!/bin/sh\n\
             cd \"$(dirname \"$0\")\"\n\
             exec \"{java}\" -Xms{xms}M -Xmx{mem}M -jar {jar} nogui\n",
            java = java_path,
            jar = jar_name,
            mem = mem_mb,
        )
    }
}

/// 连接说明（FR-07）。
pub(crate) fn connection_info(spec: &ServerSpec) -> ConnectionInfo {
    let mut lines = vec![
        format!("本机连接：localhost:{}", spec.port),
        "局域网朋友：连接 <你的内网 IP>（ipconfig / ifconfig 查看）".to_string(),
    ];
    match &spec.network {
        NetworkPlan::LanOnly => lines.push("仅在局域网内游玩".into()),
        NetworkPlan::Direct { firewall_hint } => {
            lines.push(format!("跨网络：{firewall_hint}"));
            lines.push(format!("朋友连接地址：<你的公网 IP>:{}", spec.port));
        }
        NetworkPlan::Tunnel { .. } => {
            lines.push("内网穿透端点已写入档案（P1）".into());
        }
    }
    for note in &spec.notes {
        lines.push(format!("提示：{note}"));
    }
    ConnectionInfo { lines }
}

pub(crate) fn step_begin(
    ctx: &DeployContext,
    task_id: &TaskId,
    step: &'static str,
    title: &str,
    track: &mut StepTrack,
) {
    track.current = Some((step, title.to_string()));
    ctx.bus.publish(ProgressEvent::StepStarted {
        task_id: task_id.clone(),
        step: step.into(),
        title: title.into(),
    });
}

/// 步骤收尾：进度事件 + Exec 轨迹一并发布（决议 D19）。
pub(crate) fn step_done(
    ctx: &DeployContext,
    task_id: &TaskId,
    ok: bool,
    detail: Option<String>,
    track: &mut StepTrack,
) {
    let Some((step, title)) = track.current.take() else {
        return;
    };
    ctx.bus.publish(ProgressEvent::StepFinished {
        task_id: task_id.clone(),
        step: step.into(),
        ok,
        detail: detail.clone(),
    });
    let mark = if ok { "✔" } else { "✘" };
    let summary = match detail {
        Some(d) if !d.is_empty() => format!("{title}：{mark} {d}"),
        _ => format!("{title}：{mark}"),
    };
    exec_trace(ctx, task_id, &summary, step);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::upstream::DownloadKind;

    /// 决议 D23：start.bat 形态——java 绝对路径含空格必须加引号，
    /// 内存与 jar 参数与托管启动一致，CRLF 行尾。
    #[test]
    fn 启动脚本_bat形态() {
        let s = start_script_content(
            true,
            r"C:\Program Files\Java\jdk-25.0.1+12-jre\bin\java.exe",
            "spigot-26.2.jar",
            2048,
            "e2e-spigot",
        );
        assert!(s.contains("cd /d %~dp0"));
        assert!(
            s.contains(r#""C:\Program Files\Java\jdk-25.0.1+12-jre\bin\java.exe""#),
            "含空格路径必须加引号：{s}"
        );
        assert!(s.contains("-Xms1024M -Xmx2048M -jar spigot-26.2.jar nogui"));
        assert!(s.ends_with("pause\r\n"));
        // 每个 \n 都必须是 \r\n（bat 规范）
        assert_eq!(
            s.matches('\n').count(),
            s.matches("\r\n").count(),
            "bat 必须 CRLF 行尾"
        );
    }

    /// 决议 D23：start.sh 形态——可执行脚本，exec 承接，LF 行尾。
    #[test]
    fn 启动脚本_sh形态() {
        let s = start_script_content(
            false,
            "/opt/java/jdk-25/bin/java",
            "spigot-26.2.jar",
            4096,
            "e2e-spigot",
        );
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("cd \"$(dirname \"$0\")\""));
        assert!(s.contains("-Xms2048M -Xmx4096M -jar spigot-26.2.jar nogui"));
        assert!(!s.contains('\r'), "sh 必须 LF 行尾");
    }

    /// 决议 D22 v0.11.1：应急通道——同名 jar 无哈希复用 / 哈希校验复用。
    #[tokio::test]
    async fn 应急通道_无哈希复用与哈希校验() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("spigot-26.2.jar");
        let mk = |sha256: Option<String>| DownloadItem {
            url: "https://download.getbukkit.org/spigot/spigot-26.2.jar".into(),
            sha1: None,
            sha256,
            file_name: "spigot-26.2.jar".into(),
            kind: DownloadKind::ServerJar,
        };

        // 文件不存在 → 走下载
        assert!(reuse_existing_jar(&p, &mk(None)).await.unwrap().is_none());
        // 过小文件（残件/占位）→ 不复用，重下覆盖
        std::fs::write(&p, b"garbage").unwrap();
        assert!(reuse_existing_jar(&p, &mk(None)).await.unwrap().is_none());
        // 无哈希 + 足够大 → 复用并明示"未校验"
        let blob = vec![0u8; 2 * 1024 * 1024];
        std::fs::write(&p, &blob).unwrap();
        let note = reuse_existing_jar(&p, &mk(None))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("无哈希大文件应复用"));
        assert!(note.contains("未校验"), "{note}");
        // sha256 匹配 → 复用
        let hash = hex::encode(sha2::Sha256::digest(&blob));
        let note = reuse_existing_jar(&p, &mk(Some(hash)))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("哈希匹配应复用"));
        assert!(note.contains("哈希校验通过"), "{note}");
        // sha256 不符 → 重下
        assert!(
            reuse_existing_jar(&p, &mk(Some("deadbeef".into())))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod integration {
    use super::*;
    use crate::spec::{AccountPolicy, WorldPlan};
    use std::time::Duration;

    /// M1 端到端验收（US1 确定性部分）：预检 → 受管 Java 供给 → 官方下载 →
    /// 配置生成 → 启动 → 就绪检测 → 干净停止。
    #[tokio::test]
    #[ignore = "真实端到端（下载约 110MB 流量，服务器启动需数分钟）：cargo test -- --ignored"]
    async fn 端到端_原版服务器部署() {
        let mut cfg = crate::config::AppConfig::default();
        cfg.network.adoptium_mirror = "https://mirrors.tuna.tsinghua.edu.cn/Adoptium".into();
        let kb = crate::knowledge::KnowledgeBase::embedded().unwrap();
        let bus = crate::events::EventBus::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        // 部署已 Agent 化（决议 D25）：e2e 用脚本化 LLM 按标准顺序驱动工具，
        // 端到端的确定性不依赖真实模型
        let script = vec![
            crate::llm::testutil::resp_tool("probe_workspace", serde_json::json!({})),
            crate::llm::testutil::resp_tool("ensure_java", serde_json::json!({})),
            crate::llm::testutil::resp_tool("acquire_server_jar", serde_json::json!({})),
            crate::llm::testutil::resp_tool("write_server_files", serde_json::json!({})),
            crate::llm::testutil::resp_tool("start_server", serde_json::json!({})),
            crate::llm::testutil::resp_tool("probe_port", serde_json::json!({"port": 25599})),
        ];
        let svc = crate::llm::LlmService::with_client(
            crate::llm::testutil::ScriptedClient::new(script),
            "fake",
            rust_decimal::Decimal::ZERO,
            std::sync::Arc::new(crate::llm::SpendLedger::new()),
            bus.clone(),
        );
        let ctx = DeployContext::new(cfg, kb, bus, cancel).unwrap();

        let mut spec = ServerSpec::new("e2e-vanilla");
        spec.mc_version = "1.21.1".into();
        spec.software = ServerSoftware::Vanilla;
        spec.account = AccountPolicy::Online;
        spec.network = NetworkPlan::LanOnly;
        spec.world = WorldPlan::New { seed: None };
        spec.port = 25599;
        spec.max_players = 5;
        spec.jvm_memory_mb = 2048;

        let task_id = "t-e2e".to_string();
        let result = deploy(&mut spec, &ctx, &task_id, &svc)
            .await
            .unwrap_or_else(|e| panic!("部署失败：{e}"));

        // 就绪后连接说明非空，然后干净停止（Drop 守卫也应生效）
        assert!(!result.connection.lines.is_empty());
        result.server.stop();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    /// Spigot 一次跑通验收（v0.11，用户场景：spigot + 26.2，本机 127.0.0.1
    /// 进服）：预检 → Java 25 供给（Program Files\Java 或受管目录）→
    /// getbukkit 镜像下载 → 配置 + start.bat → 启动 → 就绪后本机 TCP 连通
    /// 127.0.0.1:25565 → 干净停止。需联网，在实机运行：
    /// `cargo test -- --ignored 端到端_spigot`
    #[tokio::test]
    #[ignore = "真实端到端（下载约 50MB 流量，26.2 首次生成世界较慢）：cargo test -- --ignored"]
    async fn 端到端_spigot服务器部署() {
        let cfg = crate::config::AppConfig::default();
        let kb = crate::knowledge::KnowledgeBase::embedded().unwrap();
        let bus = crate::events::EventBus::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        // 部署已 Agent 化（决议 D25）：脚本化 LLM 按标准顺序驱动工具；
        // Spigot 渠道链（页面解析→API→复用）在工具内自动完成
        let script = vec![
            crate::llm::testutil::resp_tool("probe_workspace", serde_json::json!({})),
            crate::llm::testutil::resp_tool("ensure_java", serde_json::json!({})),
            crate::llm::testutil::resp_tool("acquire_server_jar", serde_json::json!({})),
            crate::llm::testutil::resp_tool("write_server_files", serde_json::json!({})),
            crate::llm::testutil::resp_tool("start_server", serde_json::json!({})),
            crate::llm::testutil::resp_tool("probe_port", serde_json::json!({"port": 25565})),
        ];
        let svc = crate::llm::LlmService::with_client(
            crate::llm::testutil::ScriptedClient::new(script),
            "fake",
            rust_decimal::Decimal::ZERO,
            std::sync::Arc::new(crate::llm::SpendLedger::new()),
            bus.clone(),
        );
        let ctx = DeployContext::new(cfg, kb, bus, cancel).unwrap();

        let mut spec = ServerSpec::new("e2e-spigot");
        spec.mc_version = "26.2".into();
        spec.software = ServerSoftware::Spigot;
        spec.account = AccountPolicy::Online;
        spec.network = NetworkPlan::LanOnly;
        spec.world = WorldPlan::New { seed: None };
        spec.port = 25565;
        spec.max_players = 5;
        spec.jvm_memory_mb = 2048;

        let task_id = "t-e2e-spigot".to_string();
        let result = deploy(&mut spec, &ctx, &task_id, &svc)
            .await
            .unwrap_or_else(|e| panic!("部署失败：{e}"));

        // 验收核心：就绪后本机必须能建立 TCP 连接（进服前的连通性底线）
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", spec.port))
            .await
            .unwrap_or_else(|e| panic!("127.0.0.1:{} 连接失败：{e}", spec.port));
        drop(stream);

        // start.bat（Windows）/start.sh 必须已落盘且引用下载到的 jar
        let script = if cfg!(windows) {
            std::fs::read_to_string(
                std::path::Path::new(&spec.server_dir.clone().unwrap()).join("start.bat"),
            )
        } else {
            std::fs::read_to_string(
                std::path::Path::new(&spec.server_dir.clone().unwrap()).join("start.sh"),
            )
        }
        .unwrap_or_else(|e| panic!("启动脚本未落盘：{e}"));
        assert!(
            script.contains("nogui"),
            "启动脚本应含完整启动命令：{script}"
        );

        result.server.stop();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
