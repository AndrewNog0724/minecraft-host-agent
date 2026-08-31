//! 执行流水线（§8.5）：环境复检 → Java 供给 → 下载校验 → mod 安装 →
//! 配置生成 → 启动 → 就绪检测。每步为事务边界，逐任务发进度事件（R4）。

use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::events::{EventBus, ProgressEvent, TaskId, TraceEvent, TraceKind, TraceStep};
use crate::knowledge::KnowledgeBase;
use crate::knowledge::upstream::{
    DownloadItem, DownloadKind, FabricClient, HttpBase, ModrinthClient, MojangClient, PaperClient,
    SpigotClient,
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
) -> Result<DeployResult, DeployError> {
    let mut track = StepTrack::default();
    let result = deploy_inner(spec, ctx, task_id, &mut track).await;
    if let Err(e) = &result {
        track.finish_failed(ctx, task_id, &e.to_string());
    }
    result
}

/// 当前执行步骤追踪（决议 D19）：失败时补发该步骤的失败进度与轨迹，
/// 避免 `?` 早退路径留下"永远旋转的进度条 + trace 缺一步"。
#[derive(Default)]
struct StepTrack {
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
fn exec_trace(ctx: &DeployContext, task_id: &TaskId, summary: &str, step: &str) {
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
    step_begin(ctx, task_id, "java", "Java 供给", track);
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
    let java_detail = match &java_runtime {
        crate::spec::JavaRuntime::System { path, version } => {
            format!("使用系统 Java {version}（{path}）")
        }
        crate::spec::JavaRuntime::Managed { path, version, .. } => {
            // 决议 D19 ⑧：安装位置必须显式可见（实测用户问"Java 装哪了"）
            format!("受管 JRE {version} 已就绪：{path}")
        }
        crate::spec::JavaRuntime::Pending => "Java 运行时未就绪".into(),
    };
    spec.java.runtime = java_runtime;
    step_done(ctx, task_id, true, Some(java_detail), track);

    // 服务端主 jar 下载（官方渠道 + 哈希校验）
    step_begin(ctx, task_id, "download", "获取服务端", track);
    let jar_item = server_jar_item(spec, ctx).await?;
    let jar_path = download(ctx, task_id, "download", &jar_item, &server_dir).await?;
    // 决议 D19 ⑨：来源 URL 显式可见并入轨迹（实测用户问"服务端哪来的"）
    step_done(
        ctx,
        task_id,
        true,
        Some(format!("{}（来源：{}）", jar_item.file_name, jar_item.url)),
        track,
    );

    // mod 解析与下载（Fabric；依赖闭包在 knowledge 层展开）
    if let ServerSoftware::Fabric { .. } = &spec.software
        && !spec.mod_names.is_empty()
    {
        step_begin(ctx, task_id, "mods", "解析并安装 mod", track);
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
            true,
            Some(format!("共 {total} 个文件")),
            track,
        );
    }

    // 配置生成（eula / server.properties / 启动脚本 / whitelist.json）
    step_begin(ctx, task_id, "config", "生成配置", track);
    write_configs(spec, &server_dir, &jar_path)?;
    step_done(ctx, task_id, true, None, track);

    // 启动 + 就绪检测（决议 D19：超时可配置、日志落盘并直显、等待可感知）
    step_begin(ctx, task_id, "launch", "启动服务端", track);
    let java_path = managed_java_path(&spec.java.runtime)
        .ok_or(DeployError::Preflight("Java 运行时未就绪".into()))?;
    let jvm_args = vec![
        format!("-Xms{}M", spec.jvm_memory_mb / 2),
        format!("-Xmx{}M", spec.jvm_memory_mb),
    ];
    let process = ServerProcess::spawn(&java_path, &jvm_args, &jar_path, &server_dir).await?;
    let ready_timeout = ctx.cfg.deploy.ready_timeout_secs;
    let log_path = server_dir.join("mcha-launch.log");
    ctx.bus.publish(ProgressEvent::StepProgress {
        task_id: task_id.clone(),
        step: "launch".into(),
        current: 0,
        total: None,
        detail: Some(format!(
            "正在启动（就绪检测上限 {ready_timeout} 秒，日志实时滚动；完整日志 {}）",
            log_path.display()
        )),
    });
    let started_at = std::time::Instant::now();
    let readiness = process
        .wait_ready(
            ready_timeout,
            ctx.cancel.clone(),
            &log_path,
            {
                let task_id = task_id.clone();
                move |line| {
                    ctx.bus.publish(ProgressEvent::LogLine {
                        task_id: task_id.clone(),
                        step: "launch".into(),
                        line: line.to_string(),
                    });
                }
            },
            {
                let task_id = task_id.clone();
                move |elapsed| {
                    ctx.bus.publish(ProgressEvent::StepProgress {
                        task_id: task_id.clone(),
                        step: "launch".into(),
                        current: elapsed,
                        total: Some(ready_timeout),
                        detail: Some(format!("已等待 {elapsed}/{ready_timeout} 秒")),
                    });
                }
            },
        )
        .await;
    let (ready_ok, ready_detail) = match &readiness {
        Ok(()) => (
            true,
            Some(format!(
                "服务器就绪，用时 {:.0} 秒",
                started_at.elapsed().as_secs_f32()
            )),
        ),
        Err(e) => (false, Some(e.to_string())),
    };
    step_done(ctx, task_id, ready_ok, ready_detail, track);
    readiness?;

    spec.server_dir = Some(server_dir.to_string_lossy().to_string());
    Ok(DeployResult {
        server: process,
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
async fn server_jar_item(
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

/// 写 eula.txt / server.properties / 启动脚本 / whitelist.json
/// （FR-04 + 决议 D23；决策树节点落配置）。
fn write_configs(spec: &ServerSpec, server_dir: &Path, jar_path: &Path) -> Result<(), DeployError> {
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

fn step_begin(
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
fn step_done(
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

    /// Spigot 一次跑通验收（v0.11，用户场景：spigot + 26.2，本机 127.0.0.1
    /// 进服）：预检 → Java 25 供给（Program Files\Java 或受管目录）→
    /// getbukkit 镜像下载 → 配置 + start.bat → 启动 → 就绪后本机 TCP 连通
    /// 127.0.0.1:25565 → 干净停止。需联网，在实机运行：
    /// `cargo test -- --ignored 端到端_spigot`
    #[tokio::test]
    #[ignore = "真实端到端（下载约 50MB 流量，26.2 首次生成世界较慢）：cargo test -- --ignored"]
    async fn 端到端_spigot服务器部署() {
        // D24：adoptium_mirror 默认即 TUNA 镜像，这里不再手工配置，
        // 顺带验证"默认镜像开箱即用"
        let cfg = crate::config::AppConfig::default();
        let kb = crate::knowledge::KnowledgeBase::embedded().unwrap();
        let bus = crate::events::EventBus::new();
        let cancel = tokio_util::sync::CancellationToken::new();
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
        let result = deploy(&mut spec, &ctx, &task_id)
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
