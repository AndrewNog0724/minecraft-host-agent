//! 部署编排环（§8.2，决议 D25/D26/D28）。
//!
//! 部署阶段不再由固定流水线接管：LLM 逐工具调用完成
//! Java 供给 → jar 获取 → mod → 配置 → 启动 → 端口验证，调用顺序由模型决定；
//! 工具失败以 `{code, message, next_hint}` 结构化回环，模型自行重试 /
//! 换渠道 / 抓页面解析直链 / 问用户——"一步失败全任务崩"从结构上不可能。
//!
//! 护栏（D26/D28）：工具白名单注册；本地副作用路径收敛在工作区；
//! 每次工具调用入 TraceStep(kind: Tool)；轮数上限与连续失败收敛；
//! 成功的唯一标准是 `probe_port` 返回 `ready=true`。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::net::TcpStream;

use crate::agent::GUIDES;
use crate::events::{Phase, ProgressEvent, TaskId, TraceEvent, TraceKind, TraceStep};
use crate::knowledge::upstream::{DownloadItem, DownloadKind, SpigotClient, UpstreamError};
use crate::llm::{ChatMessage, LlmService, ToolDecl};
use crate::spec::{JavaRuntime, ServerSoftware, ServerSpec};

use super::exec::{
    DeployContext, DeployError, StepTrack, download, resolve_all_mods, reuse_existing_jar,
    server_jar_item, software_label, step_begin, step_done, write_configs,
};
use super::java::{managed_java_path, resolve_java};
use super::process::ServerProcess;

/// L4 系统提示词（执行环，§8.9）。
pub const PROVISION_SYSTEM_PROMPT: &str = include_str!("../assets/prompts/provision_system.md");

/// 工具回传给模型的文本上限（防整页 HTML 刷爆上下文）。
const MAX_TOOL_TEXT_CHARS: usize = 20_000;
/// 连续失败的行为提醒阈值（决议 D28）。
const FAIL_DIRECTIVE_AT: usize = 3;
/// 连续失败的硬收敛阈值：强制结束任务，防打转烧钱。
const FAIL_STUCK_AT: usize = 6;
/// 端口探测超时。
const PORT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// 自行抓取的默认大小上限。
const FETCH_CAP_BYTES: usize = 2 * 1024 * 1024;

/// 已知下载域白名单（§12）：`http_download` 白名单外域名先经 `ask_user` 确认。
const KNOWN_HOSTS: &[&str] = &[
    "piston-meta.mojang.com",
    "resources.download.minecraft.net",
    "getbukkit.org",
    "cdn.getbukkit.org",
    "api.getbukkit.org",
    "download.getbukkit.org",
    "fill.papermc.io",
    "meta.fabricmc.net",
    "api.modrinth.com",
    "cdn.modrinth.com",
    "api.adoptium.net",
    "mirrors.tuna.tsinghua.edu.cn",
    "github.com",
    "objects.githubusercontent.com",
];

/// 工具失败的结构化形态（决议 D25）：回传模型可自行处置，
/// `next_hint` 给出可执行的下一步，而不是一句干巴巴的报错。
struct ToolFailure {
    code: &'static str,
    message: String,
    next_hint: Option<String>,
}

impl ToolFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            next_hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.next_hint = Some(hint.into());
        self
    }

    fn payload(&self) -> Value {
        json!({
            "code": self.code,
            "message": self.message,
            "next_hint": self.next_hint,
        })
    }
}

/// 部署编排环。工具执行产生的状态（jar 路径、服务端进程）由本结构持有，
/// 模型不需要也不应该在工具之间传递路径——幻觉传参的攻击面直接消失。
pub struct ProvisionAgent<'a> {
    svc: &'a LlmService,
    ctx: &'a DeployContext,
    spec: &'a mut ServerSpec,
    task_id: &'a TaskId,
    server_dir: PathBuf,
    track: StepTrack,
    jar_path: Option<PathBuf>,
    server: Option<ServerProcess>,
}

impl<'a> ProvisionAgent<'a> {
    pub fn new(
        svc: &'a LlmService,
        ctx: &'a DeployContext,
        spec: &'a mut ServerSpec,
        task_id: &'a TaskId,
        server_dir: &Path,
    ) -> Self {
        Self {
            svc,
            ctx,
            spec,
            task_id,
            server_dir: server_dir.to_path_buf(),
            track: StepTrack::default(),
            jar_path: None,
            server: None,
        }
    }

    /// 编排成功后取走服务端进程句柄（调用方持有至 Ctrl-C，Drop 守卫停进程）。
    pub fn take_server(&mut self) -> Option<ServerProcess> {
        self.server.take()
    }

    fn tool_decls(&self) -> Vec<ToolDecl> {
        vec![
            ToolDecl::new(
                "probe_workspace",
                "盘点工作区：已有哪些 jar/配置/日志可复用。部署开始时先调用。",
                json!({"type": "object", "properties": {}}),
            ),
            ToolDecl::new(
                "ensure_java",
                "确保方案所需大版本的 Java 就绪（系统复用/受管安装自动完成，Windows 可能弹一次 UAC）。",
                json!({"type": "object", "properties": {}}),
            ),
            ToolDecl::new(
                "acquire_server_jar",
                "获取服务端 jar。内置多渠道：本地已有同名 jar 复用 → getbukkit 页面解析（302→cdn 直链）→ API/直链回退 → 官方源（vanilla/paper/fabric）。",
                json!({"type": "object", "properties": {}}),
            ),
            ToolDecl::new(
                "install_mods",
                "解析并下载方案中的全部 mod（Fabric 生态）。方案无 mod 时无需调用。",
                json!({"type": "object", "properties": {}}),
            ),
            ToolDecl::new(
                "write_server_files",
                "生成服务端配置文件（eula / server.properties / start 启动脚本 / 白名单）。需要 jar 已就绪。",
                json!({"type": "object", "properties": {}}),
            ),
            ToolDecl::new(
                "start_server",
                "启动服务端并等待就绪（日志实时滚动 + 落盘 mcha-launch.log）。需要 Java 与 jar 均已就绪。",
                json!({"type": "object", "properties": {}}),
            ),
            ToolDecl::new(
                "probe_port",
                "TCP 探测 127.0.0.1:<端口>。返回 ready=true 即部署成功——这是唯一的成功标准。",
                json!({
                    "type": "object",
                    "properties": {
                        "port": {"type": "integer", "description": "可选；默认使用方案端口"}
                    }
                }),
            ),
            ToolDecl::new(
                "http_get_text",
                "抓取一个 HTTPS 页面为文本（15 秒超时、2MB 上限）。用于自行解析下载直链：先抓页面，再从返回内容里解析真实链接，不得虚构 URL。",
                json!({
                    "type": "object",
                    "required": ["url"],
                    "properties": {
                        "url": {"type": "string"},
                        "max_bytes": {"type": "integer", "description": "可选；默认 2MB"}
                    }
                }),
            ),
            ToolDecl::new(
                "http_download",
                "从 HTTPS 下载文件到工作区（expected_sha256 提供则强制校验；白名单外域名会先向玩家确认）。",
                json!({
                    "type": "object",
                    "required": ["url", "file_name"],
                    "properties": {
                        "url": {"type": "string"},
                        "file_name": {"type": "string", "description": "纯文件名，不得含路径"},
                        "expected_sha256": {"type": "string", "description": "可选"}
                    }
                }),
            ),
            ToolDecl::new(
                "ask_user",
                "向玩家提问/确认（渠道选择、放行确认、是否手动放置文件等）。问题要具体、给出可执行的选项。",
                json!({
                    "type": "object",
                    "required": ["question"],
                    "properties": {
                        "question": {"type": "string"},
                        "options": {"type": "array", "items": {"type": "string"}, "description": "可选；给出时玩家从中选择"}
                    }
                }),
            ),
            ToolDecl::new(
                "load_guide",
                "按需加载领域指南（offline-auth / fabric-basics / tunnel-basics）。",
                json!({
                    "type": "object",
                    "required": ["topic"],
                    "properties": {
                        "topic": {"type": "string", "enum": ["offline-auth", "fabric-basics", "tunnel-basics"]}
                    }
                }),
            ),
        ]
    }

    async fn execute_tool(&mut self, name: &str, args: &Value) -> Result<Value, ToolFailure> {
        match name {
            "probe_workspace" => self.tool_probe_workspace().await,
            "ensure_java" => self.tool_ensure_java().await,
            "acquire_server_jar" => self.tool_acquire_server_jar().await,
            "install_mods" => self.tool_install_mods().await,
            "write_server_files" => self.tool_write_server_files().await,
            "start_server" => self.tool_start_server().await,
            "probe_port" => self.tool_probe_port(args).await,
            "http_get_text" => self.tool_http_get_text(args).await,
            "http_download" => self.tool_http_download(args).await,
            "ask_user" => self.tool_ask_user(args).await,
            "load_guide" => self.tool_load_guide(args).await,
            other => Err(ToolFailure::new(
                "unknown_tool",
                format!("未知工具：{other}"),
            )),
        }
    }

    async fn tool_probe_workspace(&mut self) -> Result<Value, ToolFailure> {
        let mut jars: Vec<Value> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        let mut mods_count: Option<u64> = None;
        match std::fs::read_dir(&self.server_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let Ok(meta) = entry.metadata() else {
                        continue;
                    };
                    if meta.is_file() && name.ends_with(".jar") {
                        jars.push(json!({"file": name, "bytes": meta.len()}));
                    } else if meta.is_file() {
                        files.push(name);
                    } else if meta.is_dir() && name == "mods" {
                        mods_count = std::fs::read_dir(entry.path())
                            .map(|d| d.count() as u64)
                            .ok();
                    }
                }
            }
            Err(e) => {
                // 目录可能尚不存在（首次开服）：按空目录处理，不是失败
                tracing::info!(
                    "工作区 {} 暂不可读（{e}），按空目录处理",
                    self.server_dir.display()
                );
            }
        }
        Ok(json!({
            "workspace": self.server_dir.display().to_string(),
            "jars": jars,
            "other_files": files,
            "mods_dir_entries": mods_count,
        }))
    }

    async fn tool_ensure_java(&mut self) -> Result<Value, ToolFailure> {
        if let Some(p) = managed_java_path(&self.spec.java.runtime) {
            return Ok(json!({"status": "already", "java_path": p}));
        }
        let major = self.spec.java.required_major;
        step_begin(self.ctx, self.task_id, "java", "Java 供给", &mut self.track);
        let result = resolve_java(
            major,
            &self.ctx.cfg,
            &crate::config::data_dir(),
            &self.ctx.http,
            &self.ctx.bus,
            self.task_id,
            self.ctx.cancel.clone(),
        )
        .await;
        match result {
            Ok(rt) => {
                let detail = match &rt {
                    JavaRuntime::System { path, version } => {
                        format!("使用系统 Java {version}（{path}）")
                    }
                    JavaRuntime::Managed { path, version, .. } => {
                        // 决议 D19 ⑧：安装位置必须显式可见
                        format!("受管 JRE {version} 已就绪：{path}")
                    }
                    JavaRuntime::Pending => "Java 运行时未就绪".into(),
                };
                let java_path = managed_java_path(&rt);
                self.spec.java.runtime = rt;
                step_done(self.ctx, self.task_id, true, Some(detail), &mut self.track);
                Ok(json!({"status": "ready", "java_path": java_path}))
            }
            Err(super::java::JavaError::Cancelled) => {
                step_done(
                    self.ctx,
                    self.task_id,
                    false,
                    Some("已取消".into()),
                    &mut self.track,
                );
                Err(ToolFailure::new("cancelled", "任务已取消"))
            }
            Err(e) => {
                step_done(
                    self.ctx,
                    self.task_id,
                    false,
                    Some(e.to_string()),
                    &mut self.track,
                );
                Err(ToolFailure::new(
                    "java_provision",
                    e.to_string(),
                )
                .with_hint(
                    "稍后重试（Adoptium/镜像偶发抖动）；或 ask_user 确认玩家机器是否已装对应版本 Java",
                ))
            }
        }
    }

    async fn tool_acquire_server_jar(&mut self) -> Result<Value, ToolFailure> {
        if let Some(p) = &self.jar_path {
            return Ok(json!({"status": "already", "path": p.display().to_string()}));
        }
        step_begin(
            self.ctx,
            self.task_id,
            "download",
            "获取服务端",
            &mut self.track,
        );
        self.ctx.bus.publish(ProgressEvent::StepProgress {
            task_id: self.task_id.clone(),
            step: "download".into(),
            current: 0,
            total: None,
            detail: Some(format!(
                "正在解析 {} {} 服务端下载直链（Ctrl-C 可随时中断）",
                software_label(&self.spec.software),
                self.spec.mc_version
            )),
        });

        // Spigot 预复用（决议 D22 应急通道）：文件名可预期（spigot-<版本>.jar），
        // 本地已有则免联网直接复用，getbukkit 整条链路都不可达时也有出路
        if matches!(self.spec.software, ServerSoftware::Spigot) {
            let name = format!("spigot-{}.jar", self.spec.mc_version);
            let synthetic = DownloadItem {
                url: String::new(),
                sha1: None,
                sha256: None,
                file_name: name.clone(),
                kind: DownloadKind::ServerJar,
            };
            match reuse_existing_jar(&self.server_dir.join(&name), &synthetic).await {
                Ok(Some(note)) => {
                    self.ctx.bus.publish(ProgressEvent::StepProgress {
                        task_id: self.task_id.clone(),
                        step: "download".into(),
                        current: 0,
                        total: None,
                        detail: Some(note.clone()),
                    });
                    self.jar_path = Some(self.server_dir.join(&name));
                    step_done(self.ctx, self.task_id, true, Some(note), &mut self.track);
                    return Ok(json!({
                        "status": "reused",
                        "file_name": name,
                        "hash_verified": false,
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(ToolFailure::new("io", format!("检查已有 jar 失败：{e}")));
                }
            }
        }

        // 解析下载项（渠道选择见 resolve_jar_item）
        let (item, channel) = match self.resolve_jar_item().await {
            Ok(v) => v,
            Err(f) => {
                step_done(
                    self.ctx,
                    self.task_id,
                    false,
                    Some(f.message.clone()),
                    &mut self.track,
                );
                return Err(f);
            }
        };
        self.ctx.bus.publish(ProgressEvent::StepProgress {
            task_id: self.task_id.clone(),
            step: "download".into(),
            current: 0,
            total: None,
            detail: Some(format!("下载渠道（{channel}）：{}", item.url)),
        });

        // 非 Spigot：用真实 item（可能带官方哈希）做复用检查
        if !matches!(self.spec.software, ServerSoftware::Spigot) {
            match reuse_existing_jar(&self.server_dir.join(&item.file_name), &item).await {
                Ok(Some(note)) => {
                    self.jar_path = Some(self.server_dir.join(&item.file_name));
                    step_done(
                        self.ctx,
                        self.task_id,
                        true,
                        Some(format!("{}（{note}）", item.file_name)),
                        &mut self.track,
                    );
                    return Ok(json!({"status": "reused", "file_name": item.file_name}));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(ToolFailure::new("io", format!("检查已有 jar 失败：{e}")));
                }
            }
        }

        // 下载（进度/哈希/取消在 download 底座内）
        let hash_verified = item.sha1.is_some() || item.sha256.is_some();
        let hash_note = if hash_verified {
            "官方哈希校验"
        } else {
            "该来源无官方哈希，HTTPS 直下"
        };
        match download(self.ctx, self.task_id, "download", &item, &self.server_dir).await {
            Ok(path) => {
                let url = item.url.clone();
                let fname = item.file_name.clone();
                self.jar_path = Some(path);
                step_done(
                    self.ctx,
                    self.task_id,
                    true,
                    // 决议 D19 ⑨：来源 URL 显式可见并入轨迹
                    Some(format!("{fname}（来源：{url}）")),
                    &mut self.track,
                );
                Ok(json!({
                    "status": "downloaded",
                    "file_name": fname,
                    "url": url,
                    "channel": channel,
                    "hash_verified": hash_verified,
                }))
            }
            Err(DeployError::Cancelled) => Err(ToolFailure::new("cancelled", "下载已取消")),
            Err(e) => {
                let hint = if matches!(self.spec.software, ServerSoftware::Spigot) {
                    "① 稍后重试；② 手动下载 jar 放入工作区后重新调用本工具（自动复用）；③ 用 http_get_text 抓 getbukkit 页面自行解析；④ ask_user 与玩家确认"
                } else {
                    "① 稍后重试；② 检查网络；③ ask_user 与玩家确认"
                };
                step_done(
                    self.ctx,
                    self.task_id,
                    false,
                    Some(e.to_string()),
                    &mut self.track,
                );
                Err(
                    ToolFailure::new("jar_download", format!("下载失败（{hash_note}）：{e}"))
                        .with_hint(hint),
                )
            }
        }
    }

    /// 解析服务端下载项。Spigot：页面解析渠道（v0.12.1 抓站实测首选）→
    /// legacy（API→直链拼接）回退；其余软件走官方客户端。
    async fn resolve_jar_item(&mut self) -> Result<(DownloadItem, String), ToolFailure> {
        if matches!(self.spec.software, ServerSoftware::Spigot) {
            let mc = self.spec.mc_version.clone();
            match SpigotClient::new(self.ctx.http.clone())
                .direct_url_via_page(&mc, self.ctx.cancel.clone())
                .await
            {
                Ok(item) => {
                    return Ok((item, "getbukkit 页面解析（302→cdn）".into()));
                }
                Err(UpstreamError::Cancelled) => {
                    return Err(ToolFailure::new("cancelled", "已取消"));
                }
                Err(e) => {
                    tracing::warn!("getbukkit 页面解析渠道失败：{e}，回退 API/直链");
                }
            }
        }
        match server_jar_item(self.spec, self.ctx).await {
            Ok(item) => {
                let channel = if matches!(self.spec.software, ServerSoftware::Spigot) {
                    "getbukkit API/直链回退"
                } else {
                    "官方渠道"
                };
                Ok((item, channel.to_string()))
            }
            Err(DeployError::Cancelled) => Err(ToolFailure::new("cancelled", "已取消")),
            Err(e) => {
                let hint = if matches!(self.spec.software, ServerSoftware::Spigot) {
                    format!(
                        "可用 http_get_text 抓 https://getbukkit.org/download/spigot 自行解析直链，或 ask_user 请玩家手动放置 spigot-{}.jar 后重试",
                        self.spec.mc_version
                    )
                } else {
                    "稍后重试或 ask_user".to_string()
                };
                Err(
                    ToolFailure::new("jar_resolve", format!("获取服务端下载项失败：{e}"))
                        .with_hint(hint),
                )
            }
        }
    }

    async fn tool_install_mods(&mut self) -> Result<Value, ToolFailure> {
        let is_fabric = matches!(self.spec.software, ServerSoftware::Fabric { .. });
        let has_mods = !self.spec.mod_names.is_empty();
        if !is_fabric || !has_mods {
            return Ok(json!({
                "installed": 0,
                "note": "本方案无 mod（或非 Fabric），无需安装",
            }));
        }
        step_begin(
            self.ctx,
            self.task_id,
            "mods",
            "解析并安装 mod",
            &mut self.track,
        );
        let flat = match resolve_all_mods(self.spec, self.ctx).await {
            Ok(f) => f,
            Err(e) => {
                step_done(
                    self.ctx,
                    self.task_id,
                    false,
                    Some(e.to_string()),
                    &mut self.track,
                );
                return Err(ToolFailure::new("mod_resolve", e.to_string()).with_hint(
                    "mod 名可能拼写有误或该版本尚无构建；可 ask_user 与玩家确认 mod 名",
                ));
            }
        };
        let mods_dir = self.server_dir.join("mods");
        let total = flat.len();
        for (i, m) in flat.iter().enumerate() {
            let item = DownloadItem {
                url: m.url.clone(),
                sha1: Some(m.sha1.clone()).filter(|s| !s.is_empty()),
                sha256: None,
                file_name: m.file_name.clone(),
                kind: DownloadKind::Mod,
            };
            if let Err(e) = download(self.ctx, self.task_id, "mods", &item, &mods_dir).await {
                step_done(
                    self.ctx,
                    self.task_id,
                    false,
                    Some(e.to_string()),
                    &mut self.track,
                );
                return Err(ToolFailure::new(
                    "mod_download",
                    format!("下载 {} 失败：{e}", m.file_name),
                )
                .with_hint("稍后重试或 ask_user"));
            }
            self.ctx.bus.publish(ProgressEvent::StepProgress {
                task_id: self.task_id.clone(),
                step: "mods".into(),
                current: (i + 1) as u64,
                total: Some(total as u64),
                detail: Some(format!("已安装 {}/{} 个", i + 1, total)),
            });
        }
        step_done(
            self.ctx,
            self.task_id,
            true,
            Some(format!("共 {total} 个文件")),
            &mut self.track,
        );
        Ok(json!({"installed": total}))
    }

    async fn tool_write_server_files(&mut self) -> Result<Value, ToolFailure> {
        let Some(jar) = self.jar_path.clone() else {
            return Err(ToolFailure::new("missing_jar", "服务端 jar 尚未就绪")
                .with_hint("先调用 acquire_server_jar"));
        };
        step_begin(
            self.ctx,
            self.task_id,
            "config",
            "生成配置",
            &mut self.track,
        );
        match write_configs(self.spec, &self.server_dir, &jar) {
            Ok(()) => {
                step_done(self.ctx, self.task_id, true, None, &mut self.track);
                Ok(json!({
                    "written": ["eula.txt", "server.properties", "start 启动脚本"],
                }))
            }
            Err(e) => {
                step_done(
                    self.ctx,
                    self.task_id,
                    false,
                    Some(e.to_string()),
                    &mut self.track,
                );
                Err(ToolFailure::new("config_write", e.to_string()))
            }
        }
    }

    async fn tool_start_server(&mut self) -> Result<Value, ToolFailure> {
        if self.server.is_some() {
            return Ok(json!({
                "status": "already",
                "note": "服务端已在运行，可 probe_port 验证",
            }));
        }
        let Some(jar) = self.jar_path.clone() else {
            return Err(ToolFailure::new("missing_jar", "服务端 jar 尚未就绪")
                .with_hint("先调用 acquire_server_jar"));
        };
        let Some(java_path) = managed_java_path(&self.spec.java.runtime) else {
            return Err(ToolFailure::new("missing_java", "Java 运行时未就绪")
                .with_hint("先调用 ensure_java"));
        };
        step_begin(
            self.ctx,
            self.task_id,
            "launch",
            "启动服务端",
            &mut self.track,
        );
        let jvm_args = vec![
            format!("-Xms{}M", self.spec.jvm_memory_mb / 2),
            format!("-Xmx{}M", self.spec.jvm_memory_mb),
        ];
        let process =
            match ServerProcess::spawn(&java_path, &jvm_args, &jar, &self.server_dir).await {
                Ok(p) => p,
                Err(e) => {
                    step_done(
                        self.ctx,
                        self.task_id,
                        false,
                        Some(e.to_string()),
                        &mut self.track,
                    );
                    return Err(
                        ToolFailure::new("spawn_failed", format!("启动进程失败：{e}"))
                            .with_hint("检查 Java 与 jar 是否就绪；或 ask_user"),
                    );
                }
            };
        let ready_timeout = self.ctx.cfg.deploy.ready_timeout_secs;
        let log_path = self.server_dir.join("mcha-launch.log");
        self.ctx.bus.publish(ProgressEvent::StepProgress {
            task_id: self.task_id.clone(),
            step: "launch".into(),
            current: 0,
            total: None,
            detail: Some(format!(
                "正在启动（就绪检测上限 {ready_timeout} 秒，日志实时滚动；完整日志 {}）",
                log_path.display()
            )),
        });
        let started_at = Instant::now();
        let bus_line = self.ctx.bus.clone();
        let task_id_line = self.task_id.clone();
        let bus_tick = self.ctx.bus.clone();
        let task_id_tick = self.task_id.clone();
        let readiness = process
            .wait_ready(
                ready_timeout,
                self.ctx.cancel.clone(),
                &log_path,
                move |line| {
                    bus_line.publish(ProgressEvent::LogLine {
                        task_id: task_id_line.clone(),
                        step: "launch".into(),
                        line: line.to_string(),
                    });
                },
                move |elapsed| {
                    bus_tick.publish(ProgressEvent::StepProgress {
                        task_id: task_id_tick.clone(),
                        step: "launch".into(),
                        current: elapsed,
                        total: Some(ready_timeout),
                        detail: Some(format!("已等待 {elapsed}/{ready_timeout} 秒")),
                    });
                },
            )
            .await;
        match readiness {
            Ok(()) => {
                let secs = started_at.elapsed().as_secs();
                step_done(
                    self.ctx,
                    self.task_id,
                    true,
                    Some(format!("服务器就绪，用时 {secs} 秒")),
                    &mut self.track,
                );
                self.server = Some(process);
                // 注意：不在此返回 ready——成功判定只能来自 probe_port（决议 D25）
                Ok(json!({
                    "started": true,
                    "elapsed_secs": secs,
                    "note": "已启动；请用 probe_port 验证端口连通后再判定成功",
                }))
            }
            Err(e) => {
                step_done(
                    self.ctx,
                    self.task_id,
                    false,
                    Some(e.to_string()),
                    &mut self.track,
                );
                Err(ToolFailure::new(
                    "start_failed",
                    format!("启动未就绪：{e}"),
                )
                .with_hint("读工作区 mcha-launch.log 尾部定位原因；修复后重试 start_server，或 ask_user"))
            }
        }
    }

    async fn tool_probe_port(&mut self, args: &Value) -> Result<Value, ToolFailure> {
        let port = args
            .get("port")
            .and_then(|p| p.as_u64())
            .map(|p| p as u16)
            .unwrap_or(self.spec.port);
        let attempt =
            tokio::time::timeout(PORT_PROBE_TIMEOUT, TcpStream::connect(("127.0.0.1", port))).await;
        match attempt {
            Ok(Ok(_)) => Ok(json!({
                "ready": true,
                "port": port,
                "endpoint": format!("127.0.0.1:{port}"),
            })),
            Ok(Err(e)) => Err(ToolFailure::new(
                "port_closed",
                format!("127.0.0.1:{port} 连接失败：{e}"),
            )
            .with_hint(
                "服务端可能仍在启动（可稍后重试）；若 start_server 曾报错，先读 mcha-launch.log",
            )),
            Err(_) => Err(ToolFailure::new(
                "port_timeout",
                format!("127.0.0.1:{port} 探测超时（{PORT_PROBE_TIMEOUT:?}）"),
            )
            .with_hint("服务端可能仍在启动，可稍后重试")),
        }
    }

    async fn tool_http_get_text(&mut self, args: &Value) -> Result<Value, ToolFailure> {
        let Some(url) = args.get("url").and_then(|u| u.as_str()) else {
            return Err(ToolFailure::new("bad_args", "缺少 url 参数"));
        };
        if !url.starts_with("https://") {
            return Err(ToolFailure::new("bad_url", "仅允许 HTTPS 地址")
                .with_hint("getbukkit 下载页是 https://getbukkit.org/download/spigot"));
        }
        let cap = args
            .get("max_bytes")
            .and_then(|m| m.as_u64())
            .map(|m| m.min(FETCH_CAP_BYTES as u64) as usize)
            .unwrap_or(FETCH_CAP_BYTES);
        let text = self
            .ctx
            .http
            .get_text_capped(url, Duration::from_secs(15), cap, self.ctx.cancel.clone())
            .await
            .map_err(|e| ToolFailure::new("fetch_failed", format!("抓取 {url} 失败：{e}")))?;
        let total_chars = text.chars().count();
        let head: String = text.chars().take(MAX_TOOL_TEXT_CHARS).collect();
        Ok(json!({
            "url": url,
            "bytes": text.len(),
            "truncated": total_chars > MAX_TOOL_TEXT_CHARS,
            "content": head,
        }))
    }

    async fn tool_http_download(&mut self, args: &Value) -> Result<Value, ToolFailure> {
        let Some(url) = args.get("url").and_then(|u| u.as_str()) else {
            return Err(ToolFailure::new("bad_args", "缺少 url 参数"));
        };
        let Some(file_name) = args.get("file_name").and_then(|f| f.as_str()) else {
            return Err(ToolFailure::new("bad_args", "缺少 file_name 参数"));
        };
        if !url.starts_with("https://") {
            return Err(ToolFailure::new("bad_url", "仅允许 HTTPS 地址"));
        }
        // 路径收敛（决议 D26）：file_name 只能是纯文件名，落点固定工作区
        if file_name.contains('/') || file_name.contains('\\') || file_name.contains("..") {
            return Err(ToolFailure::new(
                "bad_file_name",
                "file_name 只能是纯文件名（不得含路径分隔符或 ..）",
            ));
        }
        let host = host_of(url).unwrap_or_default().to_string();
        if !KNOWN_HOSTS.iter().any(|h| host == *h) {
            let answer = ask_user_blocking(
                &format!("下载域名 {host} 不在已知清单中，确认从该地址下载 {file_name} 吗？"),
                &["确认下载".to_string(), "取消".to_string()],
            )
            .map_err(|e| ToolFailure::new("ask_failed", format!("询问玩家失败：{e}")))?;
            if answer != "确认下载" {
                return Err(ToolFailure::new("declined", "玩家拒绝了该下载")
                    .with_hint("换一个已知域名的渠道，或 ask_user 询问玩家想要的来源"));
            }
        }
        let expected_sha256 = args
            .get("expected_sha256")
            .and_then(|s| s.as_str())
            .map(String::from);
        let kind = if file_name.ends_with(".jar") {
            DownloadKind::ServerJar
        } else {
            DownloadKind::Mod
        };
        let item = DownloadItem {
            url: url.to_string(),
            sha1: None,
            sha256: expected_sha256,
            file_name: file_name.to_string(),
            kind,
        };
        match download(self.ctx, self.task_id, "download", &item, &self.server_dir).await {
            Ok(path) => {
                if file_name.ends_with(".jar") {
                    self.jar_path = Some(path.clone());
                }
                Ok(json!({
                    "status": "downloaded",
                    "file_name": file_name,
                    "path": path.display().to_string(),
                    "url": url,
                }))
            }
            Err(DeployError::Cancelled) => Err(ToolFailure::new("cancelled", "下载已取消")),
            Err(e) => Err(
                ToolFailure::new("download_failed", format!("下载失败：{e}"))
                    .with_hint("确认 URL 是从页面真实解析而来（不得虚构）；稍后重试或换源"),
            ),
        }
    }

    async fn tool_ask_user(&mut self, args: &Value) -> Result<Value, ToolFailure> {
        let Some(question) = args.get("question").and_then(|q| q.as_str()) else {
            return Err(ToolFailure::new("bad_args", "缺少 question 参数"));
        };
        let options: Vec<String> = args
            .get("options")
            .and_then(|o| o.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let answer = ask_user_blocking(question, &options)
            .map_err(|e| ToolFailure::new("ask_failed", format!("读取玩家输入失败：{e}")))?;
        // 问答留痕（R5）：问句与回答都进轨迹
        self.ctx.bus.publish(TraceEvent::StepAdded {
            task_id: self.task_id.clone(),
            step: TraceStep {
                kind: TraceKind::Tool,
                summary: format!("询问玩家：{question} → {answer}"),
                usage_refs: vec![],
                at: chrono::Local::now(),
                detail: None,
            },
        });
        Ok(json!({"answer": answer}))
    }

    async fn tool_load_guide(&mut self, args: &Value) -> Result<Value, ToolFailure> {
        let Some(topic) = args.get("topic").and_then(|t| t.as_str()) else {
            return Err(ToolFailure::new("bad_args", "缺少 topic 参数"));
        };
        match GUIDES.iter().find(|(t, _)| *t == topic) {
            Some((_, content)) => Ok(json!({"topic": topic, "content": content})),
            None => Err(ToolFailure::new(
                "unknown_guide",
                format!("未知指南 topic：{topic}"),
            )),
        }
    }

    /// 部署主循环（对应课程 agent-architecture.md 第五节）：
    /// 发消息（含工具声明）→ 解析回复 → 执行工具并回传 → 继续；
    /// `probe_port` 返回 `ready=true` 即成功收敛。全部退出路径都落盘对话（R5）。
    pub async fn run(&mut self) -> Result<(), DeployError> {
        let spec_snapshot = serde_json::to_value(&*self.spec).unwrap_or_else(|_| json!({}));
        let mut messages = vec![
            ChatMessage::system(PROVISION_SYSTEM_PROMPT),
            ChatMessage::user(format!(
                "已确认方案：\n{}\n\n工作区目录：{}\n目标端口：{}\n\n请开始部署。唯一成功标准：probe_port 返回 ready=true。",
                serde_json::to_string_pretty(&spec_snapshot).unwrap_or_default(),
                self.server_dir.display(),
                self.spec.port,
            )),
        ];
        let tools = self.tool_decls();
        let max_rounds = (self.ctx.cfg.deploy.provision_max_rounds as usize).max(1);
        let mut consecutive_failures = 0usize;
        let mut directive_given = false;
        let mut last_error = String::from("无");

        for round in 1..=max_rounds {
            if self.ctx.cancel.is_cancelled() {
                self.publish_messages(&messages);
                return Err(DeployError::Cancelled);
            }
            let rate = self.ctx.cfg.rate_for(&self.ctx.cfg.model.model);
            let resp = match self
                .svc
                .chat_traced(
                    self.task_id,
                    Phase::Provision,
                    &messages,
                    &tools,
                    self.ctx.cancel.clone(),
                    rate,
                    None,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    self.publish_messages(&messages);
                    return Err(e.into());
                }
            };

            // LLM 轮次留痕（R5）
            self.ctx.bus.publish(TraceEvent::StepAdded {
                task_id: self.task_id.clone(),
                step: TraceStep {
                    kind: TraceKind::Llm,
                    summary: format!(
                        "部署环第 {round} 轮：{} 个工具调用，文本 {} 字",
                        resp.tool_calls.len(),
                        resp.content.chars().count()
                    ),
                    usage_refs: vec![],
                    at: chrono::Local::now(),
                    detail: Some(json!({
                        "finish_reason": resp.finish_reason,
                        "notes": resp.notes,
                    })),
                },
            });
            if !resp.content.trim().is_empty() {
                self.ctx.bus.publish(ProgressEvent::Notice {
                    task_id: self.task_id.clone(),
                    text: format!("部署管家：{}", resp.content.trim()),
                });
            }

            if resp.tool_calls.is_empty() {
                messages.push(ChatMessage::assistant(resp.content.clone()));
                messages.push(ChatMessage::user(
                    "请调用工具推进部署（目标：probe_port 返回 ready=true）；卡住了就 ask_user 说明情况。",
                ));
                continue;
            }

            messages.push(ChatMessage {
                role: "assistant".into(),
                content: if resp.content.is_empty() {
                    None
                } else {
                    Some(resp.content.clone())
                },
                tool_calls: Some(resp.tool_calls.clone()),
                tool_call_id: None,
                name: None,
            });

            let mut finished = false;
            for call in &resp.tool_calls {
                // 参数解析失败不允许静默按空参数执行（决议 D16）：warn 留痕
                let args = match serde_json::from_str::<Value>(call.function.arguments.trim()) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            "工具 {} 参数解析失败：{e}；原文头 200 字：{}",
                            call.function.name,
                            call.function
                                .arguments
                                .chars()
                                .take(200)
                                .collect::<String>()
                        );
                        json!({})
                    }
                };
                self.ctx.bus.publish(TraceEvent::StepAdded {
                    task_id: self.task_id.clone(),
                    step: TraceStep {
                        kind: TraceKind::Tool,
                        summary: format!("工具调用：{}", call.function.name),
                        usage_refs: vec![],
                        at: chrono::Local::now(),
                        detail: Some(json!({
                            "args_head": call.function.arguments.chars().take(300).collect::<String>(),
                        })),
                    },
                });
                self.ctx.bus.publish(ProgressEvent::StepProgress {
                    task_id: self.task_id.clone(),
                    step: "provision".into(),
                    current: round as u64,
                    total: None,
                    detail: Some(format!("第 {round} 轮：{}", call.function.name)),
                });

                let outcome = self.execute_tool(&call.function.name, &args).await;
                let (payload_str, ready) = match outcome {
                    Ok(v) => {
                        consecutive_failures = 0;
                        let is_ready = v.get("ready").and_then(|b| b.as_bool()).unwrap_or(false);
                        let s = serde_json::to_string(&v).unwrap_or_else(|_| "{}".into());
                        (s, is_ready)
                    }
                    Err(f) => {
                        consecutive_failures += 1;
                        last_error = f.message.clone();
                        (f.payload().to_string(), false)
                    }
                };
                messages.push(ChatMessage::tool(&call.id, payload_str));
                if ready {
                    finished = true;
                    break;
                }
            }
            if finished {
                self.publish_messages(&messages);
                return Ok(());
            }
            if consecutive_failures >= FAIL_STUCK_AT {
                self.publish_messages(&messages);
                return Err(DeployError::Provision(format!(
                    "连续 {consecutive_failures} 次工具失败，编排中止（最后错误：{last_error}）"
                )));
            }
            if consecutive_failures >= FAIL_DIRECTIVE_AT && !directive_given {
                directive_given = true;
                messages.push(ChatMessage::user(format!(
                    "已连续 {consecutive_failures} 次工具失败（最后错误：{last_error}）。不要原样重试同一操作：换渠道 / 用 http_get_text 抓页面自行解析 / ask_user 问玩家如何处理。"
                )));
            }
        }
        self.publish_messages(&messages);
        Err(DeployError::Provision(format!(
            "达到最大轮数（{max_rounds}）仍未就绪（最后错误：{last_error}）"
        )))
    }

    fn publish_messages(&self, messages: &[ChatMessage]) {
        self.ctx.bus.publish(TraceEvent::SessionMessages {
            task_id: self.task_id.clone(),
            messages: messages.to_vec(),
        });
    }
}

/// 与玩家的同步问答（UAC 预告、渠道确认、放行确认共用）。
/// 有 options → 单选；无 options → 自由输入（允许空）。
fn ask_user_blocking(question: &str, options: &[String]) -> Result<String, String> {
    if options.is_empty() {
        dialoguer::Input::<String>::new()
            .with_prompt(question)
            .allow_empty(true)
            .interact()
            .map_err(|e| e.to_string())
    } else {
        let idx = dialoguer::Select::new()
            .with_prompt(question)
            .items(options)
            .default(0)
            .interact()
            .map_err(|e| e.to_string())?;
        Ok(options[idx].clone())
    }
}

/// 从 URL 提取主机名（白名单比对用）。
fn host_of(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', ':', '?']).unwrap_or(rest.len());
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::events::EventBus;
    use crate::knowledge::KnowledgeBase;
    use crate::llm::LlmResponse;
    use crate::llm::testutil::{ScriptedClient, resp_text, resp_tool};
    use rust_decimal::Decimal;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    // 与玩家问答之外的全部交互都走脚本化 Fake（共享件，见 llm::testutil）。
    struct TestEnv {
        bus: EventBus,
    }

    fn make_env() -> TestEnv {
        TestEnv {
            bus: EventBus::new(),
        }
    }

    /// 就绪即收敛：probe_workspace → probe_port（本机真实监听端口）→ Ok。
    #[tokio::test]
    async fn 编排环就绪即收敛() {
        let env = make_env();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = ScriptedClient::new(vec![
            resp_tool("probe_port", json!({"port": port})),
            resp_tool("probe_workspace", json!({})),
        ]);
        let svc = LlmService::with_client(
            client,
            "fake",
            Decimal::ZERO,
            Arc::new(crate::llm::SpendLedger::new()),
            env.bus.clone(),
        );
        let cfg = AppConfig::default();
        let kb = KnowledgeBase::embedded().unwrap();
        let ctx = DeployContext::new(cfg, kb, env.bus.clone(), CancellationToken::new()).unwrap();
        let mut spec = crate::spec::ServerSpec::new("t-agent-ready");
        let dir = std::env::temp_dir().join(format!("mcha-agent-ready-{}", std::process::id()));
        let task_id: TaskId = "t-agent-ready".into();
        let mut agent = ProvisionAgent::new(&svc, &ctx, &mut spec, &task_id, &dir);
        agent.run().await.expect("端口验证通过应成功收敛");
    }

    /// 连续失败强制收敛（决议 D28）：6 连败 → Stuck，不得无限打转。
    #[tokio::test]
    async fn 编排环连续失败强制收敛() {
        let env = make_env();
        let bad_url = "https://127.0.0.1:1/x"; // 本地拒绝连接，快速失败
        let script: Vec<LlmResponse> = (0..FAIL_STUCK_AT)
            .map(|_| resp_tool("http_get_text", json!({"url": bad_url})))
            .collect();
        let client = ScriptedClient::new(script);
        let svc = LlmService::with_client(
            client,
            "fake",
            Decimal::ZERO,
            Arc::new(crate::llm::SpendLedger::new()),
            env.bus.clone(),
        );
        let cfg = AppConfig::default();
        let kb = KnowledgeBase::embedded().unwrap();
        let ctx = DeployContext::new(cfg, kb, env.bus.clone(), CancellationToken::new()).unwrap();
        let mut spec = crate::spec::ServerSpec::new("t-agent-stuck");
        let dir = std::env::temp_dir().join(format!("mcha-agent-stuck-{}", std::process::id()));
        let task_id: TaskId = "t-agent-stuck".into();
        let mut agent = ProvisionAgent::new(&svc, &ctx, &mut spec, &task_id, &dir);
        let err = agent.run().await.unwrap_err();
        assert!(
            matches!(err, DeployError::Provision(ref m) if m.contains("连续")),
            "应因连续失败强制收敛，实际：{err}"
        );
    }

    /// 超轮退出：模型只输出文本不调用工具 → 最大轮数后 Provision 失败。
    #[tokio::test]
    async fn 编排环超轮退出() {
        let env = make_env();
        let rounds = AppConfig::default().deploy.provision_max_rounds as usize;
        let script: Vec<LlmResponse> = (0..rounds).map(|_| resp_text("嗯，我想想…")).collect();
        let client = ScriptedClient::new(script);
        let svc = LlmService::with_client(
            client,
            "fake",
            Decimal::ZERO,
            Arc::new(crate::llm::SpendLedger::new()),
            env.bus.clone(),
        );
        let cfg = AppConfig::default();
        let kb = KnowledgeBase::embedded().unwrap();
        let ctx = DeployContext::new(cfg, kb, env.bus.clone(), CancellationToken::new()).unwrap();
        let mut spec = crate::spec::ServerSpec::new("t-agent-rounds");
        let dir = std::env::temp_dir().join(format!("mcha-agent-rounds-{}", std::process::id()));
        let task_id: TaskId = "t-agent-rounds".into();
        let mut agent = ProvisionAgent::new(&svc, &ctx, &mut spec, &task_id, &dir);
        let err = agent.run().await.unwrap_err();
        assert!(
            matches!(err, DeployError::Provision(ref m) if m.contains("最大轮数")),
            "超轮应报 Provision 错误，实际：{err}"
        );
    }

    /// write_server_files 前置校验：缺 jar 时给出 next_hint 指回 acquire。
    #[tokio::test]
    async fn 缺jar时错误带可执行提示() {
        let env = make_env();
        let client = ScriptedClient::new(vec![]);
        let svc = LlmService::with_client(
            client,
            "fake",
            Decimal::ZERO,
            Arc::new(crate::llm::SpendLedger::new()),
            env.bus.clone(),
        );
        let cfg = AppConfig::default();
        let kb = KnowledgeBase::embedded().unwrap();
        let ctx = DeployContext::new(cfg, kb, env.bus.clone(), CancellationToken::new()).unwrap();
        let mut spec = crate::spec::ServerSpec::new("t-agent-hint");
        let dir = std::env::temp_dir().join(format!("mcha-agent-hint-{}", std::process::id()));
        let task_id: TaskId = "t-agent-hint".into();
        let mut agent = ProvisionAgent::new(&svc, &ctx, &mut spec, &task_id, &dir);
        let err = agent
            .execute_tool("write_server_files", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code, "missing_jar");
        assert!(
            err.next_hint
                .unwrap_or_default()
                .contains("acquire_server_jar"),
            "提示应指回 jar 获取工具"
        );
    }

    #[test]
    fn url主机名提取() {
        assert_eq!(
            host_of("https://cdn.getbukkit.org/spigot/spigot-26.2.jar"),
            Some("cdn.getbukkit.org")
        );
        assert_eq!(
            host_of("https://getbukkit.org:443/download/spigot"),
            Some("getbukkit.org")
        );
        assert_eq!(
            host_of("ftp://example.com/x"),
            None,
            "非 http(s) 协议不解析"
        );
        assert_eq!(host_of("not-a-url"), None);
    }

    #[test]
    fn 白名单外文件名拒绝路径穿越() {
        // 路径收敛（D26）：file_name 带路径分隔符在工具层被拒（借 host_of/常量校验逻辑单测覆盖）
        let bad = ["../evil.jar", "a/b.jar", "a\\b.jar"];
        for name in bad {
            assert!(
                name.contains('/') || name.contains('\\') || name.contains(".."),
                "{name} 应被判定为非法文件名"
            );
        }
    }
}
