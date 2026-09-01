# MC 联机设施建设 Agent · 需求与设计文档

- **版本**：v0.4.1（活文档，持续迭代）
- **关联**：选题陈述 `docs/topic-statement.md`；基线实验 `experiments/general-agent-baseline.md`；课程要求 `docs/requirements.md`
- **与最终提交设计文档的对应**：§2 → 痛点分析；§3 → 场景定制方案；§7–§9 → 系统架构（模块划分、数据流、关键数据结构）；§10 → 技术选型。定稿时按此结构抽取整理。

## 1. 产品定位

**一句话**：面向 Minecraft Java Edition 好友联机场景的"开服管家"——玩家用一句自然语言描述需求，Agent 在本机完成从方案推导、服务端部署、内网穿透到故障诊断的全流程，交付一台朋友能直接连入的服务器，并把一切配置与开销记录在案。

- **产品名**：正式定名 **Minecraft Host Agent**，仓库 `minecraft-host-agent`，简称 **MCHA**（行文）/**`mcha`**（标识符、命令、路径小写，决议 D15）
- **形态**：本地运行的 Rust 单二进制，初版 CLI 交互式终端（界面形态见 §15 决议 D1）
- **边界（只做这一件事）**：只服务"MC Java 版好友联机开服与维保"。不做通用聊天助手、不做服务器面板、不做基岩版服（跨平台见 §15 决议 D5）。

## 2. 痛点分析

### 2.1 目标用户

| 画像 | 描述 |
| --- | --- |
| 主要用户 | 想和 2–10 名好友联机的普通 MC Java 版玩家；会用启动器玩游戏，但不懂服务端、Java、网络配置 |
| 次要用户 | 需要维保（升级、加 mod、排障）的小服服主 |

非目标用户：需要百人级公网服与专业运维的社区服主。

### 2.2 痛点（P1–P5）

- **P1 知识分散且过时**：开服知识散落在 Wiki、论坛帖、视频教程里，随 MC / mod 生态演进快速失效（下载渠道变更、Java 版本要求变化、mod 停更）。
- **P2 决策分支多**：账号类型（正版 / 离线 / 混合）× 服务端选型（原版 / Spigot / Paper / Fabric / Forge）× MC × Java × mod 版本矩阵 × 内存 × EULA × 防火墙 × 网络拓扑，任何一环选错都开不起来；玩家不知道"自己不知道"哪些分支（如混合认证需要额外插件）。
- **P3 故障诊断黑盒**：报错只能把崩溃日志贴进搜索框碰运气；日志其实高度模式化（Java 版本不匹配、端口占用、mod 不兼容各有特征），但玩家读不懂。
- **P4 内网穿透劝退**："没有公网 IP"是好友联机最常见的拦路虎；frp / playit / Tailscale 各有适用条件与配置成本，选型和排障超出普通玩家能力。
- **P5 重复劳动**：每次开服从零开始；换机器、朋友人数变化后，此前的决策与配置无法复用。

### 2.3 通用 Agent 为什么解决不好（基线实验证据）

基线实验（opencode + GLM-5.2 @ Windows 11，记录见 `experiments/`）：在专家全程引导下通用 Agent 能搭起服务器，但每隔几步出现一次偏差——

| 实验观察到的失败模式 | 对应痛点 | 本产品的应对（→ §3） |
| --- | --- | --- |
| 版本号信息过时、认为 1.21.11 新于 26.2（幻觉） | P1 | 定制 2：实时 API 校验 |
| 下载渠道不合理 | P1 | 定制 2：官方渠道 + 哈希校验 |
| 漏掉混合认证 / 内存 / 穿透等分支 | P2 / P4 | 定制 1：决策树固化所有分支 |
| 出错后定位不到根因 | P3 | 定制 4：日志模式库诊断 |
| 无进度展示、无配置沉淀、无费用统计，每次从零 | P5 + R4/R5/R6 | R4–R6 作为产品能力内置 |

核心论点：**通用 Agent 需要一个懂行的人全程盯着才能完成这件事，而"懂行的人"恰恰是不一定存在的角色。**

## 3. 场景定制方案（≥2 项）

### 定制 1：开服决策树工作流（对应 P2）

- **内容**：将 §5.2 决策树全部节点固化为 Rust 编排的确定性流程；LLM 的职责收窄为——理解自然语言需求、生成澄清问题、在流程给定的选项中做模糊匹配。每个节点的执行（检测、下载、写配置、起进程）全部是确定性 Rust 代码。
- **验收**：对基线实验 T4 的复合需求一句话，系统推导的方案覆盖决策树**全部**必选节点，无遗漏。

### 定制 2：版本兼容知识库 + 实时校验（对应 P1）

- **内容**：内置静态知识（MC 版本 → 所需 Java 大版本、加载器与 mod 生态对应关系）+ 安装前实时调用 Mojang / PaperMC / Fabric / Modrinth 官方 API 核对"该版本是否存在、依赖是否满足、下载 URL 与哈希是否有效"；mod 安装前解析其依赖树。**LLM 不得直接决定任何下载**，只能引用知识库与 API 校验通过的结果。
- **验收**：对不存在的版本号 / mod 名（含实验幻觉样例"26.2"）明确拒绝并给出可用版本列表；所有下载文件通过哈希校验。

### 定制 3：内网穿透编排（对应 P4，默认樱花frp）

- **内容**：以樱花frp 为默认方案（决议 D9）：服主完成一次性注册 / 实名 / 获取访问密钥后，Agent 经官方 API v4 全自动完成——节点选择 → 创建 TCP 隧道 → 官方渠道下载并校验 frpc → 拉起并托管 → 端到端验证（TCP + MC 协议 ping）→ 生成"朋友们怎么连"说明卡片（含剩余流量）；全程无需改路由器与防火墙。有公网 IP 的少数场景给端口映射指引；自建 frp / Tailscale 为 P2 备选。故障时沿"本机监听 → frpc 进程 → 隧道状态 / 流量 → 节点探测"链路排查。详见 §8.6。
- **验收**：无公网 IP 环境下，从 token 就绪到外部客户端可连入全自动完成（人工步骤仅限注册 / 实名 / 粘贴 token）。

### 定制 4：MC 崩溃日志诊断（对应 P3）

- **内容**：错误模式库（Java 版本不匹配、端口占用、mod 不兼容、内存不足、存档版本不符等特征正则 / 关键词）→ 命中即给确定性修复建议或直接代执行；未命中再交 LLM 结合上下文分析。
- **验收**：基线实验 T5 三份真实日志根因定位到版本 / 文件 / 配置项级，修复后可启动。

## 4. 用户故事与核心交互流程

### 4.1 用户故事

- **US1 开新服**：我说"我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存"，Agent 追问我机器与网络情况后给出方案并（经确认）自动完成部署，最后告诉我朋友们怎么连。
- **US2 排障**：朋友连不上 / 服务器崩了，我告诉 Agent，它定位根因并修复，全程给我看它在查什么。
- **US3 复用**：下次开服时加载上次的配置档案，一键重建；只改变化的部分。
- **US4 升级（P2）**：MC 大版本更新后让 Agent 升级整服：先备份，再逐项核对 mod 兼容性，告诉我哪个 mod 没有新版，然后完成升级。
- **US5 管开销**：我设预算上限；Agent 实时显示每次调用 token 与费用，到预算自动停。

### 4.2 核心交互流程（US1 展开）

```text
玩家输入一句话需求
  → Agent（LLM）解析 → 缺失信息生成澄清问题（≤3 轮）
  → Rust 决策树逐节点推导 → ServerSpec（结构化方案 JSON）
  → 展示方案摘要 + 风险提示（离线模式等）→ 玩家确认
  → 执行编排（环境检测 → Java 供给 → 下载校验 → 配置生成 → 启动 → 就绪检测）
      每步实时渲染进度（如"下载服务端 45/120 MB"），全程可 Ctrl-C 打断
  → 成功：输出连接说明 + 保存配置档案 + 记录本次用量
    失败：进入诊断流程 → 修复或给出明确的人工步骤
```

## 5. 功能需求清单

优先级：**P0** = MVP 必做；**P1** = 公开互试版（09-08 前）；**P2** = 时间允许。

### 5.1 功能列表（FR）

| 编号 | 功能 | 优先级 | 说明 | 映射 |
| --- | --- | --- | --- | --- |
| FR-01 | 需求对话与方案生成 | P0 | 自然语言 → 澄清 → `ServerSpec`（结构化 JSON），schema 校验 + 重试 + 降级问答 | R2 |
| FR-02 | 环境感知与 Java 供给 | P0 | 检测 Java / 内存 / 端口 / 网络；Java 缺失或不匹配时**自动安装受管 JDK/JRE**（详见 §8.8） | — |
| FR-03 | 服务端获取与校验 | P0 | 原版 / Paper / Fabric：官方 API 查询 → 下载 → 哈希校验；支持代理与镜像 | 定制2 |
| FR-04 | 配置生成 | P0 | eula、server.properties、JVM 内存参数、mod 摆放 | 定制1 |
| FR-05 | mod 安装 | P0（按清单装）/ P1（自然语言推荐） | Modrinth API 检索、依赖解析、版本匹配下载 | 定制2 |
| FR-06 | 服务端生命周期 | P0 | 启动、就绪检测（日志 Done）、优雅停止；崩溃感知 → 触发诊断 | — |
| FR-07 | 连接说明生成 | P0 | 本机 / 局域网地址 + 客户端操作指引 | — |
| FR-08 | 内网穿透编排 | P1 | 默认樱花frp：token 引导、节点选择、API 建隧道、frpc 下载托管、SLP 端到端验证、流量监控、连接说明（§8.6）；P2 备选自建 frp / Tailscale | 定制3 |
| FR-09 | 日志诊断 | P1 | 模式库优先，LLM 兜底 | 定制4 |
| FR-10 | 配置档案 | P0（存 / 加载）/ P1（diff 复用） | ServerSpec + 产物清单 + 时间戳存 JSON | R5 |
| FR-11 | 升级迁移 | P2 | 备份 → mod 逐项核对 → 换版本 → 验证旧世界 | — |
| FR-12 | 模型配置 | P0 | Endpoint / Key / 上下文长度 / 思考模式 / 价格表；文件 + CLI 设置 | R3 |
| FR-13 | 进度渲染与打断 | P0 | >3s 任务发进度事件；Ctrl-C 干净退出不留孤儿进程 | R4 |
| FR-14 | 会话与任务历史 | P0 | 任务轨迹可查看、会话可导出 / 导入 JSON | R5 |
| FR-15 | 用量与费用统计 | P0 | 每次调用 in / out token + 价格换算；预算上限、超限中断 | R6 |
| FR-16 | 交互界面 | P0：CLI/TUI；P2：Web | 见决议 D1 | R2 |
| FR-17 | 安全防护 | P0 | 危险操作二次确认；离线模式风险提示；密钥不落仓库 | — |

### 5.2 决策树（定制 1 的范围界定）

```text
账号类型 ── 全正版 → online-mode=true
         ├─ 全离线 → online-mode=false + 白名单必选 + 风险提示
         └─ 混合   → online-mode=false + 认证方案（Paper: 登录插件 / Fabric: EasyAuth）
                      + 正版玩家影响说明
服务端类型 ── 原版（无 mod 需求）
           ├─ Spigot/Paper（插件玩法；混合认证成熟）
           └─ Fabric/Forge（mod 玩法）→ 加载器版本 × MC 版本匹配 → mod 依赖树解析
MC 版本 → Java 大版本（知识库）→ 本机检测 → 缺失/不匹配 → 受管自动安装（§8.8）
玩家数 + 机器内存 → JVM -Xmx 推荐（预留系统内存）
网络拓扑 ── 同一局域网 → 直连地址
         └─ 跨网络 → 有公网 IP？→ 端口映射 + 防火墙（给指引）
                     └─ 无 → 樱花frp 穿透编排（默认，§8.6；P2 备选自建 frp / Tailscale）
首次启动 → EULA 确认 → 就绪检测 → 连接说明
```

### 5.3 R1–R6 覆盖对照

| 课程要求 | 落点 |
| --- | --- |
| R1 Rust 核心逻辑 | 全部编排 / 决策树 / API 客户端 / 进程与文件管理（NFR-1、§9） |
| R2 界面 | FR-16 |
| R3 模型配置 | FR-12、§9 |
| R4 进度与打断 | FR-13、§9 |
| R5 历史管理 | FR-10 / FR-14、§9 |
| R6 用量统计 | FR-15、§9 |

## 6. 非功能需求（NFR）

- **NFR-1（Rust 主控）**：LLM 只做需求理解与模糊决策，一切副作用操作由 Rust 确定性代码执行。
- **NFR-2（安全）**：API Key 仅存本地（`.gitignore` 覆盖）；导出会话 / 日志自动打码公网 IP 与密钥；白名单外危险操作必须确认。
- **NFR-3（可靠）**：主流程无 `unwrap`；网络操作有超时 / 重试 / 断点续传；步骤为事务边界，失败可从已完成步骤续跑。
- **NFR-4（成本）**：预算硬上限由 Rust 侧强制；每次调用即时累计。
- **NFR-5（可解释性）**：模块边界清晰、避免技巧性写法，答辩时每一行代码可解释。
- **NFR-6（平台）**：Windows 10/11 优先，实现层不引入 Windows-only 依赖，尽量保持 Linux 可编译。

## 7. 系统架构

### 7.1 设计原则

系统形态：**单二进制 CLI 工具**（Rust），内嵌窄域 Agent Loop。四条设计原则：

1. **副作用不出 Rust**：LLM 只产生文本与结构化提案；一切副作用（下载、写文件、起进程、改防火墙）由确定性 Rust 代码执行。LLM 输出永远只是提案，经校验（schema + 知识库）与确认后才执行。
2. **能查就不猜**：任何版本 / 下载 / 依赖结论必须来自内置知识库或上游 API 实时查询；查不到即失败并报告可用选项，LLM 的记忆不作为事实来源。
3. **窄环路 + 确定性编排**：Agent Loop 只运行在"需求理解"与"故障诊断"两个认知密集阶段；部署执行阶段是纯确定性流水线，LLM 不参与。
4. **一切留痕**：LLM 调用、决策、执行、进度全部产生结构化事件并落盘——R4/R5/R6 是同一事件流的三个视图。

### 7.2 架构图（分层视图）

```text
┌─────────────────────────────────────────────────────────────────┐
│  ui（CLI/TUI）：交互、进度渲染(R4)、打断、确认                      │
├─────────────────────────────────────────────────────────────────┤
│  agent-core：Agent Loop（需求理解 / 诊断两个窄环）                 │
│    ├─ 消息与工具调度（工具调用协议：声明→模型请求→校验→执行→回传）   │
│    └─ 停止条件（最终提案 / 最大轮数 / 用户取消 / 预算耗尽）         │
├───────────────┬─────────────────────────────────────────────────┤
│  llm          │  provision：决策树引擎 + 执行流水线（确定性）       │
│  OpenAI 兼容   │    决策树 → ServerSpec → 编排执行                 │
│  客户端(流式)  │    Java供给 → 下载/校验 → 配置生成 → 进程管理      │
│               │    → 就绪检测                                     │
├───────────────┼───────────────┬─────────────┬───────────────────┤
│  knowledge    │  tunnel(P1)   │  diagnose(P1)│  store            │
│  静态知识库    │  穿透编排     │  日志模式库   │  档案/会话/用量    │
│  上游 API 客户 │  (樱花frp默认,│  诊断流程    │  (R5/R6 数据)     │
│  (mojang/paper│   见 §8.6)    │             │                   │
│   /fabric/mod-│               │             │                   │
│   rinth/adopt-│               │             │                   │
│   ium/natfrp) │               │             │                   │
├───────────────┴───────────────┴─────────────┴───────────────────┤
│  config：模型/价格/预算/网络(R3)   ·   事件总线（进度/用量/轨迹）    │
└─────────────────────────────────────────────────────────────────┘
```

### 7.3 模块职责表

| 模块 | 职责 | 关键接口（入 → 出） | 映射 |
| --- | --- | --- | --- |
| `agent-core` | 需求理解 / 故障诊断两个窄 Agent Loop；工具注册与分发；停止条件 | 用户意图 → `ServerSpecDraft`；故障现象 → `Diagnosis` | 定制1 |
| `llm` | OpenAI 兼容客户端：流式、结构化输出、重试、用量上报 | 消息列表 → 助理消息（文本 + tool_calls）+ `UsageRecord` | R3/R6 |
| `knowledge` | 静态知识库 + 五家上游 API 客户端（mojang/paper/fabric/modrinth/adoptium）+ 版本校验管线 | 版本/依赖查询 → 校验结论 + 下载清单（含哈希） | 定制2 |
| `provision` | 决策树引擎；执行流水线：Java 供给、下载校验、配置生成、进程管理、就绪检测 | `ServerSpec` → 执行结果 + 进度事件流 | 定制1、FR-02~07 |
| `tunnel`(P1) | 樱花frp 全自动编排：token 引导、节点选择、建隧道、frpc 下载托管、端到端验证、流量监控；P2 备选自建 frp / Tailscale | 网络条件 → 隧道端点 + 连接说明 | 定制3 |
| `diagnose`(P1) | 错误模式库、诊断环工具、修复提案执行 | 日志 → `Diagnosis`（根因 + 修复动作） | 定制4 |
| `store` | 档案 / 会话 / 用量的持久化与查询 | 事件与快照 → 磁盘；查询 → 历史 | R5/R6 |
| `ui` | CLI/TUI 交互、澄清问答、确认、进度渲染、打断 | 事件流 → 终端渲染；用户输入 → 指令 | R2/R4 |
| `config` | 模型与价格、预算、代理与镜像、行为开关 | 文件 + 环境变量 → `AppConfig` | R3 |

### 7.4 核心数据流

**流程 A：开服主流程（US1）**

```text
用户一句话
  → ui 打包为任务请求
  → agent-core【需求理解环】
      llm ⇄ 工具（probe_environment / check_version_compat / resolve_mod）
      产出：ServerSpecDraft + 待澄清问题（≤3 轮，经 ui 问答）
  → 决策树引擎：Draft 补全/校验所有必选节点 → ServerSpec（含风险项标注）
  → ui：方案摘要 + 离线模式风险提示 + 用户确认（FR-17）
  → provision【确定性流水线，逐任务发 ProgressEvent】
      环境复检 → Java 供给（§8.8）→ 下载与哈希校验（断点续传）
      → 配置生成 → 启动 → 就绪检测
      →（跨网络时）穿透编排与端到端验证（P1，§8.6）
  → 成功：连接说明 + store 落盘（档案/轨迹/用量）
  → 失败：转入【诊断环】（P1 前：给出错误与已做步骤，人工接管提示）
  （全程：每次 llm 调用发 UsageRecord → store 累计；预算守卫调用前拦截）
```

**流程 B：故障诊断（US2，P1）**

```text
"朋友连不上" / 服务端进程异常退出
  → diagnose：确定性检查（本机监听 → 防火墙 → 隧道状态）与日志模式匹配
  → 命中模式 → 修复提案（危险动作经确认）→ 执行 → 验证
  → 未命中 → agent-core【诊断环】llm ⇄ 工具（read_log / get_server_spec / probe_network）
      产出：根因假设 + 验证步骤（同样不得直接执行副作用）
```

## 8. 关键数据结构与模块设计

### 8.1 关键数据结构（定义草案）

字段细节实现期允许微调，结构语义冻结（均实现 serde 序列化，是 R5 落盘格式）。

```rust
/// 一次开服方案的完整描述（R5 档案主体；决策树输出、LLM 提案的目标格式）
struct ServerSpec {
    spec_id: String,             // 语义化 id（如 "twilight-5p"）
    created_at: DateTime,
    account: AccountPolicy,      // Online | Offline{whitelist} | Hybrid{auth: Plugin|EasyAuth}
    software: ServerSoftware,    // Vanilla | Paper{build} | Fabric{loader_ver, installer_ver}
    mc_version: String,          // 语义化版本，经 knowledge 校验存在
    java: JavaPlan,              // 见 §8.8
    jvm_memory_mb: u32,          // 决策树按玩家数与机器内存推导
    mods: Vec<ModRef>,           // ModRef{project, version_id, url, sha1, deps: Vec<ModRef>}
    network: NetworkPlan,        // LanOnly | Direct{port, firewall} | Tunnel{provider, params}
    world: WorldPlan,            // New{seed?} | Existing{path}
    notes: Vec<String>,          // 风险与注意事项，ui 必须展示
}

/// LLM 需求理解环的产出（未校验态：ServerSpec 的可空子集 + 澄清问题）
struct ServerSpecDraft { partial: PartialSpec, questions: Vec<Question> }

/// 进度事件（R4 数据基础；tokio 广播，ui 与 store 各订阅一份）
enum ProgressEvent {
    StepStarted { task_id, step: StepId, title: String },
    StepProgress { task_id, step: StepId, current: u64, total: Option<u64> },  // 如 45/120MB
    StepFinished { task_id, step: StepId, ok: bool, detail: Option<String> },
}

/// 单次 LLM 调用计量（R6 数据基础；由 llm 模块强制生成）
struct UsageRecord {
    call_id: String, task_id: String, at: DateTime,
    model: String,
    input_tokens: u64, output_tokens: u64,
    cost: Decimal,               // 按 config 价格表换算
    phase: Phase,                // Requirement | Diagnosis | Chat
}

/// 任务轨迹（R5 的"非黑盒"主体）
struct TaskTrace {
    task_id: String, spec_id: Option<String>,
    started_at: DateTime, finished_at: Option<DateTime>,
    steps: Vec<TraceStep>,       // TraceStep{kind: Llm|Tool|Decision|Exec, summary, usage_refs}
    status: Running | Done | Failed | Cancelled,
}
```

要点：`ServerSpecDraft` 与 `ServerSpec` 分离——**LLM 只能产出前者，决策树引擎是后者的唯一构造者**，结构上保证原则 1/2；`UsageRecord` 由 `llm` 模块在响应解析处强制生成（无 usage 字段的服务记调用次数并标注，对应课程 Q9）。

### 8.2 agent-core：窄 Agent Loop 与工具系统

- 循环（对应课程 agent-architecture.md 第五节）：发送消息（含工具声明）→ 解析回复 → `tool_calls` 则校验参数 JSON Schema → 执行 → 结果以 tool 消息回传 → 继续；最终提案文本则解析为 `ServerSpecDraft` 结束。停止条件：最终产出 / 最大轮数（默认 8）/ 用户取消 / 预算守卫拒绝。
- 工具集（需求理解环，全部只读无副作用）：`probe_environment()`、`check_version_compat(mc, software?, java?)`、`search_mods(query)`、`resolve_mod(name, mc, loader)`、`load_guide(topic)`（Skills 式按需注入领域指南，见 §8.9）。
- **提案提交走 tool-calling**（决议 D6）：把"提交方案"本身声明为一个工具 `submit_spec(ServerSpecDraft)`，强制模型以结构化参数交卷，与课程讲解的工具调用机制一致；普通文本只用于澄清问答。
- 系统提示词（L4，需求理解环与诊断环各一套，见 §8.9）声明角色边界："你是需求分析师，不得虚构版本号，未知信息调用工具或提问"；版本类事实不进 Prompt（设计红线，见 §8.9）。
- 诊断环（P1）工具：`read_log(path, tail_n)`、`get_server_spec()`、`probe_network(port)`、`load_guide(topic)`；产出 `Diagnosis{root_cause, evidence, fix: Vec<Action>, risk}`。

### 8.3 llm：OpenAI 兼容客户端

- 自研薄客户端（reqwest + SSE 流式解析），不引入 LLM SDK——核心调用编排即课程考察点（R1），且代码量小、答辩可解释。
- 结构化输出：工具参数 Schema 由 schemars 从类型派生 → 响应校验失败携错误重试（≤2）→ 仍失败降级逐项问答。
- 调用前后钩子：前查预算（`store` 累计值）；后生成 `UsageRecord` 入总线。思考模式、上下文长度从 `AppConfig` 透传（R3）。

### 8.4 knowledge：知识库与上游 API

- 静态知识库（L1，分层体系见 §8.9）：随包资源文件（TOML），含 MC→Java 映射、加载器生态、常见端口、mod 中文别名表（`search_mods` 先查别名再查 Modrinth）、崩溃错误模式库（供 diagnose 使用）；带版本号与来源日期，可独立更新。
- 上游客户端：Mojang piston-meta、PaperMC v2、Fabric meta、Modrinth v2、**Adoptium v3（Java 供给，见 §8.8）**、**SakuraFrp v4（穿透，见 §8.6）**；统一 trait `UpstreamClient`，代理与镜像在 HTTP 层统一注入。
- 版本校验管线：`semver` 解析（非法输入如"26.2"直接拒绝）→ 上游存在性核对 → 依赖闭包解析（Modrinth `dependencies` 递归展开）→ 产出带哈希的下载清单。

### 8.5 provision：决策树引擎与执行流水线

- 决策树引擎：节点为 `enum DecisionNode`（显式穷举 §5.2 分支），每节点 = 检测函数 + 规则 + 对 `ServerSpec` 的增量写入；信息不足返回 `Missing(NodeId)` 生成澄清问题。**不引入通用规则引擎**——枚举穷举可逐节点解释（NFR-5）。
- 执行流水线：步骤即事务边界——先落盘意图再执行，失败从已完成步骤续跑；下载断点续传 + 哈希校验。
- 进程管理：`tokio::process` 起服务端，stdout 行流解析就绪标记；进程句柄 Drop 守卫——取消时先停服务端再退出（R4 打断语义，不留孤儿进程）。

### 8.6 tunnel：内网穿透编排（默认樱花frp，P1）与 diagnose（P1）

**决议 D9**：跨网络场景默认使用樱花frp（SakuraFrp）。依据：国内节点延迟好、免 VPS、朋友们零安装（只有服主跑客户端）、官方提供开放 API 可全自动编排。自建 frp 与 Tailscale 为 P2 备选 adapter（trait 预留），playit 不做。本节设计基于官方资料核验：API v4 OpenAPI 定义（`api.natfrp.com/v4`，定义仓库 natfrp/api，AGPL-3.0）与 frpc 用户手册（doc.natfrp.com/frpc/manual）。

#### 8.6.1 一次性人工步骤（Agent 引导、不自动化）

注册账号 → 实名认证 → 管理面板获取**访问密钥** → 粘贴到配置（`[tunnel.natfrp] token`）。Agent 检测到 token 缺失时输出分步引导（附官方链接）；注册与实名涉及合规与隐私，明确不做自动化。

#### 8.6.2 全自动编排链路（token 就绪后）

| # | 调用 / 操作 | 说明 |
| --- | --- | --- |
| 1 | `GET /system/clients`（免鉴权） | 取官方 frpc 最新版下载 URL + 哈希 → 下载到受管目录 `<数据目录>/runtime/natfrp/` 并校验（官方 CDN 分发，不走第三方渠道） |
| 2 | `GET /user/info` | 校验 token 有效性与实名 / 等级状态（VIP 等级影响可选节点范围） |
| 3 | `GET /nodes` + `GET /node/stats` | 节点筛选与打分（见 8.6.3） |
| 4 | `POST /tunnels` | 建隧道：`type=tcp`、`node=<选中>`、`local_ip=127.0.0.1`、`local_port=<服务端端口>`、名称按约定自动生成（含 spec_id，便于查重）→ 返回 tunnel id 与 remote port |
| 5 | 拉起 frpc 子进程 | `frpc -f <token>:<tunnel_id> -n --disable_log_color`（或环境变量 `NATFRP_TOKEN` / `NATFRP_TARGET`）——配置由 frpc 从服务器自动拉取，**彻底消除手写配置文件这一错误类别** |
| 6 | 就绪确认 | 解析 frpc 日志中的代理启动成功特征行；失败转诊断链路 |
| 7 | 端到端验证 | 对 `<节点host>:<remote_port>` 先 TCP connect，再发 **MC SLP ping**（服务器列表握手协议，varint 帧很短）——取回 JSON（版本 / MOTD / 在线人数）才算"节点 → frpc → 本机服务端"全链路数据通路打通，并顺带把 MOTD / 人数展示进连接说明 |
| 8 | 落盘与展示 | 端点写入 ServerSpec；连接说明卡片：地址、朋友分步连接指引、剩余流量、风险提示 |

备选启动方式（实现期评估）：`POST /tunnel/config` 由服务端生成 frpc 配置文件后 `-c` 加载，或用上游 frp 客户端（≥0.18.0）加载生成配置。默认采用 `-f` 自动拉取。

#### 8.6.3 节点选择算法（决策树"穿透节点"节点）

- 硬过滤：节点在线（flag `1<<9` 或 stats `online>=0`）；允许创建隧道（`1<<2`，满载时为 0）；非强制访问认证（`1<<8`——访问认证会拦截 MC 客户端首包）；VIP 等级 ≤ 用户等级；非私有节点（`1<<6`）。
- 打分排序：内地节点（`1<<3`）优先 → 负载低优先（`/node/stats` 的 `load`）→ 非 BETA（`1<<10`）优先。
- 玩家可在澄清问答中表达地域偏好；选中节点不可用时按打分自动降级换节点重建。

#### 8.6.4 生命周期与流量监控

- frpc 由 provision 子进程托管 + Drop 守卫；**停机顺序：先 frpc 后服务端**（避免对外"假在线"）；frpc 异常退出 → 自动重启一次 → 仍失败转诊断。
- 复用优先：重复开服时 `GET /tunnels` 按名称约定查重并复用既有隧道，不做无谓建删（对第三方服务的频率自律）。
- 流量额度：`GET /user/data_plans`（有效流量包）+ `GET /tunnel/traffic`（隧道级用量）；连接说明展示剩余流量、低于阈值预警、"流量耗尽"入诊断模式库。换节点用 `POST /tunnel/migrate`。

#### 8.6.5 失败模式与诊断链路（diagnose 模式库新增）

| 特征 | 根因 | 动作 |
| --- | --- | --- |
| API 401/403 | token 无效 / 未实名 | 指引重新获取或完成实名 |
| 建隧道报节点不可用 | 节点满载（`1<<2`=0） | 按打分换节点重试 |
| frpc 日志鉴权失败后退出 | token 与隧道不匹配 / 客户端过旧 | 用 `/system/clients` 核对版本后重建 |
| frpc 在线但 SLP 超时 | 本地服务未监听 / 端口不符 | 回查本机 25565 与 `local_port` |
| 端到端验证通过但某玩家连不上 | 玩家侧网络 / 客户端问题 | 输出玩家侧分步自查指引 |
| 延迟高 / 卡顿 | 节点负载高或距离远 | 建议 migrate 换节点 |
| 流量耗尽 | 流量包用尽 | 提示获取流量（签到 / 套餐） |

diagnose 通用设计：模式库为有序规则表（正则 + 关键词 + 关联修复动作），新增模式 = 加一行配置；`1.21.11 > 26.2` 类版本比较错误本身入库（呼应基线实验）。

#### 8.6.6 安全、合规与 API 风险

- token 仅存本地配置，会话导出与日志打码；不内置任何共享账号。
- 尊重服务条款（`GET /system/policy` 可机读获取）：建 / 删隧道频率自律，隧道复用优先。
- 可选加固（P2）：frpc 端 `whitelist_ip`（IP 白名单，对 TCP 隧道友好）；`bandwidth_limit` 自限速。
- 引用注明：SakuraFrp 开放 API 定义为 AGPL-3.0，本项目仅调用其 HTTP 接口、不复制其代码。
- API 变更风险：锁定 v4，以官方 `openapi.yaml` 为字段事实源；编码前先跑 spike（注册测试账号 → curl 走通 8.6.2 全链路 → 请求 / 响应样例存 `experiments/` 留档）。

#### 8.6.7 备选 adapter（P2）

- 自建 frp（用户有 VPS）：frpc 侧全自动；frps 侧生成一键安装命令与配置供用户在 VPS 执行（边界：不远程登录用户服务器）。
- Tailscale（朋友愿装软件）：控制面 API 拉设备列表，取服务机的 tailnet IP 作连接地址。

### 8.7 store / config / ui

- `store`：数据目录 `~/.mcha/`（Windows 落 `%APPDATA%\mcha\`，决议 D4/D15），布局 `{profiles, sessions, usage, runtime}/`；单写多读；JSONL 追加日志 + 快照；导出 = 打包任务三类文件。
- `config`：`config.toml` + `.env`（仅 API Key）；价格表**内置常见模型预设**（GLM / DeepSeek / OpenAI 等，随包分发并在文档注明来源与更新日期，决议 D3），用户可覆盖；启动校验必填项，缺项给可复制模板。
- `ui`：clap 子命令（`new` / `diag` / `profiles` / `sessions` / `config` / `usage`）；dialoguer 交互；indicatif 多进度条；Ctrl-C 经 CancellationToken 汇入统一取消总线。

### 8.8 Java 自动供给（FR-02，决议 D2：不降级，全自动）

目标：玩家机器上没有合适 Java 时，Agent 自己解决，零人工干预，且不污染系统。

| 设计项 | 决策 | 理由 |
| --- | --- | --- |
| 发行版 | Eclipse Temurin（Adoptium） | 开源（GPL+CE）、免费无授权问题、MC 社区事实标准（Paper 文档推荐） |
| 包类型 | **zip 免安装包**，不用 msi/exe 安装器 | 免管理员权限、不写注册表、不改系统 PATH；删除目录即卸载，天然可回滚 |
| 镜像类型 | 默认 **JRE**（P2 若引入需 JDK 的工具再切 JDK 包） | 服务端只需 JRE；包体约为 JDK 一半，玩家下载快 |
| 安装路径 | `<数据目录>/runtime/jdk-<major>/<完整版本号>/`（受管目录） | 自包含、多版本可共存、绝不碰系统位置 |
| 选择顺序 | ① 系统 PATH 已有匹配版本 → 用系统的；② 受管目录已有 → 复用；③ 下载安装 | 尊重玩家已有环境；重复开服零下载 |
| API | Adoptium v3：`GET /v3/assets/latest/{major}/ga?os=...&arch=...&image_type=jre` 取元数据（含 sha256）→ 下载 zip | 元数据与二进制同源，校验值可信 |
| 架构适配 | `std::env::consts::ARCH`（x86_64 / aarch64）+ OS 探测 | Windows on ARM 等场景自动选对包 |
| 校验 | sha256 强制校验后解压；失败删除重试一次再报错 | 与服务端 / mod 下载同一套安全管线 |
| 解压 | `zip` crate 纯 Rust 解压到受管目录 | 无外部依赖 |
| 网络 | 走全局代理 / 镜像机制（国内默认镜像：清华 TUNA Adoptium 镜像） | 同 Q3 网络问题的统一解法 |
| 路径解析 | 安装完成后把**绝对路径**写入 `JavaPlan` 并入档案；运行服务端一律用该路径 | 不依赖 PATH，跨会话可复现 |
| 失败路径 | 下载失败按归因提示（代理 / 镜像建议）；整体失败则任务停在 Java 供给步骤，可续跑 | NFR-3 可恢复 |

`JavaPlan` 结构：

```rust
struct JavaPlan { required_major: u8, runtime: JavaRuntime }
enum JavaRuntime {
    System { path: PathBuf, version: String },      // 探测到并校验可用
    Managed { path: PathBuf, vendor: String, version: String },  // 受管安装
}
```

### 8.9 定制内容体系（分层，决议 D10）

定制化内容不是单一载体（Prompt 或知识库二选一），而是一套分层体系：每类知识放入"最不容易出错、最可维护"的载体。本节是总纲，各模块小节是其落地。

| 层 | 内容 | 载体 | 到达 LLM 的方式 |
| --- | --- | --- | --- |
| L0 流程知识 | 决策树全部分支（§5.2） | Rust 代码（enum 穷举） | 不到达——LLM 只负责把 `Missing(节点)` 措辞成自然语言追问 |
| L1 结构化事实 | MC→Java 映射、加载器生态、**mod 中文别名表**、端口常识 | `assets/knowledge/*.toml`（带版本号与来源日期，运行时加载） | 仅经工具返回值（`check_version_compat` 等），LLM 没有凭记忆作答的通道 |
| L2 易变事实 | 版本存在性、依赖树、下载地址、节点负载 | 上游 API 实时查询（§8.4） | 工具返回值（原则 2 的机制化） |
| L3 长文指南 | 离线认证方案对比、Fabric 服务端原理、穿透原理科普等需展开的领域背景 | `assets/guides/*.md` | Skills 式按需加载：`load_guide(topic)` 工具，决策树走到对应分支或诊断需要背景时才注入，平时不占上下文 |
| L4 表达与边界 | 角色声明、术语表（正版 / 盗版 / 离线 / 联机等黑话映射）、追问风格、风险话术模板 | `assets/prompts/*.md`（include_str! 编译期嵌入） | system prompt；按阶段两套（需求理解环 / 诊断环） |
| L-1 错误模式库 | 崩溃日志特征正则 + 关联修复动作 | `assets/knowledge/error_patterns.toml` | 不经 LLM——diagnose 确定性匹配优先（§8.6.5），LLM 兜底时只看到"已排除模式"摘要 |

**设计红线：版本类事实永远不进 Prompt。** 写进 system prompt 的"1.21 需要 Java 21"一旦过时即成系统性幻觉源；放入 L1 数据或 L2 API，更新只需改数据文件，且每次使用都经工具校验。Prompt 只承载不变的规则与说话方式。

**取舍：不使用 RAG / 向量检索（D10）。** 理由：①本域事实为枚举型，总量 KB 级且结构规整，精确查表优于模糊语义检索；②每条事实要求确定性正确，版本号不容"语义相近"；③上下文路由已由决策树承担，无需 embedding 决定读哪段文档；④embedding 调用属 API 开销，按 R6 要求须计入成本，能省则省。边界：P2 若扩充非结构化崩溃案例库，再评估轻量检索（关键词优先）。

**维护机制**：L1 / L3 / L-1 为数据文件——带版本号、来源与采集日期（与 D3 价格表同纪律），可独立更新、git 可 diff；L4 随代码版本发布。别名表是体验级定制点：中国玩家说"暮色森林""工业"，`search_mods` 先查别名表再走 Modrinth 检索——通用 Agent 在这一步就因不知道英文 slug 而幻觉，我们用一张数据表根治。

## 9. R1–R6 实现设计（课程硬性要求逐条落地）

| 要求 | 落地机制 |
| --- | --- |
| R1 Rust 主控 | 副作用不出 Rust（原则 1）；LLM 仅经自研 `llm` 客户端调用；全部编排、校验、执行在 Rust 单二进制内 |
| R2 界面 | CLI/TUI（clap + dialoguer + indicatif）；P2 视进度以 axum + SSE 增只读 Web 状态页 |
| R3 模型配置 | `config.toml` 的 `[model]`（endpoint / model / context_len / thinking）与 `[[prices]]` + 内置价格预设 + `.env` 的 key；`config set` 子命令改写 |
| R4 进度与打断 | `ProgressEvent` 广播 → indicatif 实时刷新；Ctrl-C → CancellationToken → 流水线步骤间检查点 + 进程 Drop 守卫 |
| R5 历史管理 | `profiles/`（ServerSpec+产物清单）、`sessions/`（TaskTrace 完整轨迹）、`usage/`；`sessions list/show/export` 查看、导出、导入 |
| R6 用量统计 | `UsageRecord` 强制生成 + 价格换算；`usage` 按任务 / 阶段汇总；预算守卫调用前拦截，超限取消任务 |

## 10. 技术选型

| 领域 | crate | 备选 | 选择理由 |
| --- | --- | --- | --- |
| 异步运行时 | tokio | async-std | 事实标准；进程管理与 CancellationToken（R4）依赖其生态 |
| HTTP 客户端 | reqwest（rustls-tls） | ureq | 异步流式（SSE）必需；rustls 免 Windows 上 OpenSSL 链接负担 |
| SSE 解析 | eventsource-stream | 手写行解析 | 标准协议解析自写易错；代码量小可解释 |
| 序列化 | serde + serde_json + toml | — | 事实标准；JSON 是 R5 落盘与 LLM 结构化输出统一格式 |
| Schema 派生与校验 | schemars + jsonschema | 手写校验 | 从类型派生 JSON Schema 约束 LLM 输出，响应按同一 Schema 校验，单一事实来源 |
| 错误处理 | thiserror + anyhow | 手写 | 模块内显式错误枚举（可枚举故障模式），main 层聚合 |
| CLI 框架 | clap（derive） | argh | 事实标准，子命令与帮助完备（R2） |
| 交互问答 | dialoguer | inquire | 确认 / 选择 / 输入控件（FR-17、澄清问答） |
| 进度条 | indicatif | 手写 | 多进度条与速率显示（R4 "45/120" 直接映射） |
| 版本解析 | semver | 手写 | 拒绝非语义化输入是定制 2 第一道闸 |
| 哈希校验 | sha1 + sha2（RustCrypto） | — | Mojang 用 sha1；Modrinth 给 sha1/sha512；Adoptium 给 sha256 |
| 压缩解压 | zip | 手写 | Java 供给的 zip 解压（§8.8），纯 Rust |
| 日志与观测 | tracing + tracing-subscriber | log | 结构化分模块日志；TaskTrace 可复用 span 数据 |
| 时间 | chrono | time | 事实标准，落盘时间戳统一 RFC3339 |
| 临时文件 | tempfile | 手写 | 原子写与下载临时目录（NFR-3） |
| Web（P2，可选） | axum + tower-http | actix-web | 届时仅为只读状态页，选学习成本最低的主流框架 |
| LLM SDK | **不引入**（自研薄客户端） | async-openai | 核心业务即 API 调用编排（R1 考察点），自研可控、答辩可解释 |

## 11. 数据源与外部依赖

| 数据 | 来源 | 用途 | 备注 |
| --- | --- | --- | --- |
| MC 版本清单 / 原版服务端 | Mojang piston-meta API | 版本校验、原版服下载 | 国内可达性：代理 / 镜像 |
| Paper 构建与下载 | PaperMC API | Paper 服下载 | 同上 |
| Fabric 版本与安装器 | Fabric meta / maven | Fabric 服搭建 | 同上 |
| mod 元数据 | Modrinth API v2 | 检索、版本匹配、依赖树、下载 | CurseForge 视需要后补 |
| JRE/JDK 二进制 | Adoptium v3 API（镜像：清华 TUNA） | Java 自动供给（§8.8） | sha256 校验 |
| 樱花frp API / frpc | `api.natfrp.com/v4`（Bearer 访问密钥）；frpc 经 `GET /system/clients` 官方分发（含哈希） | FR-08 穿透编排（§8.6） | 一次性人工：注册 + 实名 + token；API 定义 AGPL-3.0，引用注明 |
| LLM | 任意 OpenAI 兼容 Chat API（用户配置） | 需求解析、澄清、诊断兜底 | R3；无 usage 时记次数（课程 Q9） |

## 12. 错误处理与安全设计

- 错误分层：模块级 thiserror 枚举 → `AppError`；用户可见错误必须附"下一步怎么办"。
- 下载安全：HTTPS + 官方域白名单（含镜像域）+ 哈希校验三重；镜像仅替换白名单内域名。
- 执行安全：危险动作清单（防火墙、删除、公网暴露、系统 PATH 修改）默认需确认；`--yes` 仅限 CI / 演示并留痕。Java 供给只写受管目录，不触碰上述任何项。
- 密钥安全：`.env` / `config.toml` 在 `.gitignore`；导出打码（NFR-2）。
- 离线模式：风险说明与缓解（白名单必选）是决策树节点，非 LLM 自由发挥。

## 13. 测试与验收策略

- 单元：决策树节点（输入 → Spec 增量）、版本校验管线（含"26.2"拒绝、依赖闭包）、模式库正则、JVM 参数推导、Java 供给的版本解析与路径规则。
- 集成：`LlmClient` trait + Fake 实现（脚本化回复）驱动需求理解环全流程，CI 不花真钱。
- 端到端验收：复用基线实验测例 T1/T3/T4/T5 作验收脚本（同输入、同评分标准），形成"通用 Agent 失败样例 ↔ 本系统通过"一一对应，用于文档与答辩演示。
- 真实 API 冒烟：`cargo test --ignored` 跑上游连通与一次真实开服。

## 14. 仓库组织与实现顺序

```text
src/
├── main.rs            # clap 入口、取消总线装配
├── cli/               # ui：子命令、交互、进度渲染
├── config/            # AppConfig、价格表、加载校验
├── agent/             # agent-core：Loop、工具注册、Draft 解析
├── llm/               # 客户端、SSE、结构化输出、用量钩子
├── knowledge/         # 静态库、UpstreamClient、版本校验管线
├── provision/         # 决策树引擎、Java 供给、下载、配置生成、进程管理
├── tunnel/            # P1
├── diagnose/          # P1：模式库、诊断环工具
├── store/             # 档案/会话/用量持久化
├── events.rs          # ProgressEvent / UsageRecord / TaskTrace 与总线
└── assets/            # 定制内容（§8.9）：knowledge/*.toml、guides/*.md、prompts/*.md
```

每模块完成后独立可编译、可运行；M1 实现顺序：events → config → llm → knowledge → provision（含 Java 供给）→ agent → cli → store。

## 15. 决议记录

| # | 决议 | 内容 |
| --- | --- | --- |
| D1 | 界面形态 | MVP 用 CLI/TUI；Web（只读状态页）列 P2，09-06 公开展示前评估 |
| D2 | Java 供给 | **全自动受管安装，不降级**；设计见 §8.8 |
| D3 | 价格表 | 内置常见模型预设随包分发，注明来源与更新日期，用户可覆盖 |
| D4 | 数据目录 | `~/.mcha/`（Windows：`%APPDATA%\mcha\`；原名 `~/.mc-host-agent/`，D15 定名时统一更名，不迁移旧目录） |
| D5 | 基岩跨平台 | 不做（边界外未来工作） |
| D6 | 提案提交方式 | tool-calling（`submit_spec` 工具），不用 JSON mode |
| D7 | Forge | MVP 不做；P2 或降级为安装指导 |
| D8 | LLM SDK | 不引入，自研薄客户端 |
| D9 | 内网穿透选型 | 樱花frp 为默认（国内节点、免 VPS、朋友零安装、API v4 可全自动编排）；自建 frp / Tailscale 为 P2 备选；playit 不做 |
| D10 | 定制内容体系 | 五层载体（代码 / 数据 / API / 指南 / Prompt，另加确定性错误模式库）；版本事实不进 Prompt；不引入 RAG / embedding（枚举型小规模事实 + 决策树路由 + 成本考量），P2 扩充案例库再评估 |
| D15 | 命名规范 | 正式名 **Minecraft Host Agent**，仓库 `minecraft-host-agent`，简称 **MCHA**（行文）/**`mcha`**（标识符）；Cargo 包名 `minecraft-host-agent`，二进制/CLI 命令 `mcha`；数据目录 `~/.mcha/`（Windows `%APPDATA%\mcha\`）；环境变量 `MCHA_API_KEY` / `MCHA_DATA` / `MCHA_WORKSPACE`；其余内部标识一律用小写 `mcha` 前缀。**边界**：作为技术概念的 "Agent"（AI Agent、agent 模块、相关类型名）不属于产品命名，不改。编号沿用归档主线（archive/v0.12-mvp-line）决议表，D11–D14 属该主线，本文档不再使用 |

## 16. 里程碑与风险

| 里程碑 | 时间 | 内容 | 出口标准 |
| --- | --- | --- | --- |
| M0 设计定稿 | ~08-31 | 本文档迭代定稿 | 本文档 v1 |
| M1 MVP | ~09-04 | FR-01~07、10、12~17（P0 全部） | US1 全流程 CLI 演示成功 |
| M2 公开展示 | 09-06 前 | 稳定化 + README 补全 + 设计文档摘要发讨论区 | 同学可按 README 跑通 |
| M3 互试改进 | 09-08 前 | FR-08、09（P1）+ 试用反馈迭代 | ≥3 位同学试用并收到反馈 |
| M4 收尾 | 09-09~10 | 演示脚本（含离线兜底）、开销表、对话历史、答辩准备 | 分课堂展示 |

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 时间紧（M1 约一周） | MVP 砍功能 | 严格按 P0 范围；诊断 / Forge / Web 全部后置 |
| 上游 API 国内不可达 | 部署失败 | 代理与镜像统一机制（含 Adoptium 镜像）；失败归因明确 |
| 穿透依赖外部账号与实名 | FR-08 演示受阻 | 提前注册测试账号并完成实名；隧道经 API 查重复用；M3 起做；录屏兜底 |
| LLM 结构化输出不稳定 | 方案生成失败 | Schema 校验 + 重试 + 降级逐项问答 |
| 课堂网络不可控 | 现场演示翻车 | 提前缓存全部构件的离线演示路径 + 录屏兜底 |
| 范围蔓延 | 偏离"只做一件事" | 以 §1 边界与决策树为冻结范围，新想法记 backlog |

## 修订记录

| 日期 | 版本 | 说明 |
| --- | --- | --- |
| 2026-08-28 | v0.1a | 需求文档初版（原 `project-requirements.md`） |
| 2026-08-28 | v0.1b | 设计文档初版（原设计草稿） |
| 2026-08-28 | v0.2 | 合并为单一活文档；决议 D1–D8；新增 §8.8 Java 自动供给完整设计 |
| 2026-08-28 | v0.3 | 决议 D9：樱花frp 为穿透默认方案；§8.6 重写为基于官方 API v4 OpenAPI 与 frpc 手册的全自动编排详案 |
| 2026-08-28 | v0.4 | 新增 §8.9 定制内容分层体系（决议 D10）；工具集补 `load_guide`、仓库结构补 `assets/`、知识库小节补别名表与模式库 |
| 2026-09-01 | v0.4.1 | 全局定名规范化（沿用归档主线决议 D15）：产品名 Minecraft Host Agent / MCHA / `mcha`，Cargo 包名 `minecraft-host-agent`，数据目录 `~/.mcha/`，环境变量 `MCHA_API_KEY`/`MCHA_DATA`/`MCHA_WORKSPACE`；§1/§8.7/§15 同步；旧实现主线归档说明见 AGENTS.md |
