//! 执行流水线（§8.5）：环境复检 → Java 供给 → 下载校验 → mod 安装 →
//! 配置生成 → 启动 → 就绪检测。每步为事务边界，逐任务发进度事件（R4）。

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::events::{EventBus, ProgressEvent, TaskId};
use crate::knowledge::KnowledgeBase;
use crate::knowledge::upstream::{
    DownloadItem, DownloadKind, FabricClient, HttpBase, ModrinthClient, MojangClient, PaperClient,
};
use crate::spec::{AccountPolicy, NetworkPlan, ServerSoftware, ServerSpec};

use super::java::{managed_java_path, resolve_java};
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

    /// 服务端安装目录（FR-19，决议 D11）：`<工作区>/<spec_id>/server`。
    /// 工作区解析（env > config > 默认数据目录）与可写性校验由 config 负责；
    /// 档案元数据 spec.json 仍统一存数据目录（store::save_profile），互不影响。
    fn server_dir(&self, spec: &ServerSpec) -> Result<PathBuf, DeployError> {
        let root = self
            .cfg
            .workspace_dir()
            .map_err(|e| DeployError::Preflight(e.to_string()))?;
        Ok(root.join(&spec.spec_id).join("server"))
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
pub async fn deploy(
    spec: &mut ServerSpec,
    ctx: &DeployContext,
    task_id: &TaskId,
) -> Result<DeployResult, DeployError> {
    if ctx.cancel.is_cancelled() {
        return Err(DeployError::Cancelled);
    }

    step_begin(ctx, task_id, "preflight", "环境复检");
    preflight(spec, ctx, task_id).await?;
    step_done(ctx, task_id, "preflight", true, None);

    // Java 供给（§8.8）；required_major 缺省时由知识库查表兜底
    if spec.java.required_major == 0 {
        spec.java.required_major = ctx.kb.java_major_for(&spec.mc_version).unwrap_or(21);
    }
    step_begin(ctx, task_id, "java", "Java 供给");
    let java_runtime = resolve_java(
        spec.java.required_major,
        &ctx.cfg,
        &crate::config::data_dir(),
        &ctx.http,
        &ctx.bus,
        task_id,
        ctx.cancel.clone(),
    )
    .await?;
    spec.java.runtime = java_runtime;
    step_done(ctx, task_id, "java", true, None);

    // 服务端主 jar 下载（官方渠道 + 哈希校验）
    step_begin(ctx, task_id, "download", "获取服务端");
    let jar_item = server_jar_item(spec, ctx).await?;
    let server_dir = ctx.server_dir(spec)?;
    let jar_path = download(ctx, task_id, "download", &jar_item, &server_dir).await?;
    step_done(
        ctx,
        task_id,
        "download",
        true,
        Some(jar_item.file_name.clone()),
    );

    // mod 解析与下载（Fabric；依赖闭包在 knowledge 层展开）
    if let ServerSoftware::Fabric { .. } = &spec.software
        && !spec.mod_names.is_empty()
    {
        step_begin(ctx, task_id, "mods", "解析并安装 mod");
        let flat = resolve_all_mods(spec, ctx).await?;
        let mods_dir = server_dir.join("mods");
        let total = flat.len();
        for (i, mod_ref) in flat.iter().enumerate() {
            let item = DownloadItem {
                url: mod_ref.url.clone(),
                sha1: Some(mod_ref.sha1.clone()).filter(|s| !s.is_empty()),
                sha256: None,
                file_name: mod_ref.file_name.clone(),
                kind: DownloadKind::Mod,
            };
            download(ctx, task_id, "mods", &item, &mods_dir).await?;
            ctx.bus.publish(ProgressEvent::StepProgress {
                task_id: task_id.clone(),
                step: "mods".into(),
                current: (i + 1) as u64,
                total: Some(total as u64),
                detail: Some(format!("已安装 {}/{} 个", i + 1, total)),
            });
        }
        step_done(
            ctx,
            task_id,
            "mods",
            true,
            Some(format!("共 {total} 个文件")),
        );
    }

    // 配置生成（eula / server.properties / 启动参数）
    step_begin(ctx, task_id, "config", "生成配置");
    write_configs(spec, &server_dir)?;
    step_done(ctx, task_id, "config", true, None);

    // 启动 + 就绪检测
    step_begin(ctx, task_id, "launch", "启动服务端");
    let java_path = managed_java_path(&spec.java.runtime)
        .ok_or(DeployError::Preflight("Java 运行时未就绪".into()))?;
    let jvm_args = vec![
        format!("-Xms{}M", spec.jvm_memory_mb / 2),
        format!("-Xmx{}M", spec.jvm_memory_mb),
    ];
    let process = ServerProcess::spawn(&java_path, &jvm_args, &jar_path, &server_dir).await?;
    let readiness = process
        .wait_ready(240, ctx.cancel.clone(), |line| {
            ctx.bus.publish(ProgressEvent::StepProgress {
                task_id: task_id.clone(),
                step: "launch".into(),
                current: 0,
                total: None,
                detail: Some(line.to_string()),
            });
        })
        .await;
    let (ready_ok, ready_detail) = match &readiness {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    step_done(ctx, task_id, "launch", ready_ok, ready_detail);
    readiness?;

    spec.server_dir = Some(server_dir.to_string_lossy().to_string());
    Ok(DeployResult {
        server: process,
        connection: connection_info(spec),
    })
}

async fn preflight(
    spec: &ServerSpec,
    ctx: &DeployContext,
    task_id: &TaskId,
) -> Result<(), DeployError> {
    // 版本存在性复检（决策树可能未带官方清单缓存）
    let mojang = MojangClient::new(ctx.http.clone());
    let releases = mojang.release_versions().await?;
    if !releases.iter().any(|r| r == &spec.mc_version) {
        let suggestions = crate::knowledge::suggest_versions(&releases, &spec.mc_version, 5);
        return Err(DeployError::Preflight(format!(
            "MC 版本 {} 不存在。相近版本：{}",
            spec.mc_version,
            suggestions.join(", ")
        )));
    }
    ctx.bus.publish(ProgressEvent::StepProgress {
        task_id: task_id.clone(),
        step: "preflight".into(),
        current: 1,
        total: Some(2),
        detail: Some(format!("版本 {} 校验通过", spec.mc_version)),
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

/// 按服务端类型从官方 API 产出主 jar 下载项；Fabric 顺带回填 loader 版本。
async fn server_jar_item(
    spec: &mut ServerSpec,
    ctx: &DeployContext,
) -> Result<DownloadItem, DeployError> {
    match spec.software.clone() {
        ServerSoftware::Vanilla => Ok(MojangClient::new(ctx.http.clone())
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

async fn download(
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

/// mod 名称 → Modrinth 依赖闭包（别名表优先，检索兜底，全部确定性）。
async fn resolve_all_mods(
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

/// 写 eula.txt / server.properties / whitelist.json（FR-04；决策树节点落配置）。
fn write_configs(spec: &ServerSpec, server_dir: &Path) -> Result<(), DeployError> {
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
    let props = format!(
        "# by mcha\n\
         online-mode={online_mode}\n\
         white-list=true\n\
         enforce-whitelist=true\n\
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
    if let NetworkPlan::Direct { firewall_hint } = &spec.network {
        std::fs::write(server_dir.parent().unwrap_or(server_dir).join("connection-hint.txt"), format!(
            "跨网络联机指引：\n1. {firewall_hint}\n2. 把你的公网 IP 告诉朋友，连接地址：<公网IP>:{}\n3. 无公网 IP 时，请等待樱花frp 穿透编排功能（P1）\n",
            spec.port
        ))
        .map_err(|e| DeployError::Io(format!("写 connection-hint.txt：{e}")))?;
    }
    Ok(())
}

/// 连接说明（FR-07）。
fn connection_info(spec: &ServerSpec) -> ConnectionInfo {
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

fn step_begin(ctx: &DeployContext, task_id: &TaskId, step: &str, title: &str) {
    ctx.bus.publish(ProgressEvent::StepStarted {
        task_id: task_id.clone(),
        step: step.into(),
        title: title.into(),
    });
}

fn step_done(ctx: &DeployContext, task_id: &TaskId, step: &str, ok: bool, detail: Option<String>) {
    ctx.bus.publish(ProgressEvent::StepFinished {
        task_id: task_id.clone(),
        step: step.into(),
        ok,
        detail,
    });
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
        let result = deploy(&mut spec, &ctx, &task_id)
            .await
            .unwrap_or_else(|e| panic!("部署失败：{e}"));

        // 就绪后连接说明非空，然后干净停止（Drop 守卫也应生效）
        assert!(!result.connection.lines.is_empty());
        result.server.stop();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
