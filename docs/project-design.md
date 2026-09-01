# Minecraft Host Agent（MCHA）· 需求与设计文档

- **关联**：选题陈述 `docs/topic-statement.md`；基线实验 `experiments/general-agent-baseline.md`；课程要求 `docs/requirements.md`；Agent 技术参考 `docs/agent-architecture.md`；设计决策记录 `docs/decisions.md`

## 0. 设计立场：为什么 MCHA 首先是一个 Agent

MCHA 的设计出发点：**先是一个完整的 AI Agent，其次才是开服领域专家**。Agent 的本质是"模型提议、框架执行、结果回传"的循环——模型在循环中自主决策下一步做什么，框架通过工具调用把决策落成真实动作，并把每一步的结果交还模型继续思考。缺少这个循环、把流程写死在代码里的产品，无论交互包装得多自然，都只是带语言界面的自动化脚本。

由此确立 Agent-First 的设计总纲：

1. **Agent Loop 是整个产品的骨架**——唯一执行引擎，全阶段运行。需求澄清、方案决策、部署执行、故障诊断、日常维保，全部是同一个会话内 Agent Loop 的连续回合，不存在"确定性流水线阶段"。
2. **工具调用是一切副作用的唯一通道**——LLM 发起工具调用，Rust 实现工具后端并执行。一切具体操作（跑命令、写文件、下载、起服务）都由 Agent 发出并完成执行。
3. **先框架、后场景**——先实现一个真正的通用 Agent 框架（Agent Loop + 通用工具 + 多轮会话），再叠加开服场景适配（领域工具 + 知识库 + Skills + 提示词）。专用性建立在通用性之上。
4. **Rust 的职责重新定义**——不替代 Agent 做编排决策，而是：实现 Agent Loop 与工具后端、守护安全边界（路径收敛 / 危险确认 / 预算守卫）、提供确定性算法（校验、打分、模式匹配下沉为工具内部逻辑）。

Java 自动供给、内网穿透编排、上游 API 客户端、价格表与预算机制等开服领域的工程方案，均以"工具后端"的形态集成进 Agent 框架，由 Agent 在循环中按需调用。

## 1. 产品定位

面向 Minecraft Java Edition 好友联机场景的开服管家 Agent——它首先是一个完整的 AI Agent（多轮交互 + 工具调用），其次才是开服领域专家（领域工具 + 知识库 + Skills 使它在该场景显著优于通用 Agent）。玩家用自然语言对话，Agent 在本机完成从方案推导、服务端部署、内网穿透到故障诊断的全流程，交付一台朋友能直接连入的服务器，并把一切过程与开销记录在案。

- **产品名**：正式定名 **Minecraft Host Agent**，仓库 `minecraft-host-agent`，简称 **MCHA**（行文）/**`mcha`**（标识符、命令、路径小写）
- **形态**：本地运行的 Rust 单二进制；交互形态为 **CLI 多轮会话（REPL）**（§4）
- **边界（只做这一件事）**：只服务"MC Java 版好友联机开服与维保"。不做通用聊天助手、不做服务器面板、不做基岩版服。

## 2. 痛点分析

### 2.1 目标用户

| 画像     | 描述                                                                                      |
| -------- | ----------------------------------------------------------------------------------------- |
| 主要用户 | 想和 2–10 名好友联机的普通 MC Java 版玩家；会用启动器玩游戏，但不懂服务端、Java、网络配置 |
| 次要用户 | 需要维保（升级、加 mod、排障）的小服服主                                                  |

非目标用户：需要百人级公网服与专业运维的社区服主。

### 2.2 痛点（P1–P5）

- **P1 知识分散且过时**：开服知识散落在 Wiki、论坛帖、视频教程里，随 MC / mod 生态演进快速失效（下载渠道变更、Java 版本要求变化、mod 停更）。
- **P2 决策分支多**：账号类型（正版 / 离线 / 混合）× 服务端选型（原版 / Spigot / Paper / Fabric / Forge）× MC × Java × mod 版本矩阵 × 内存 × EULA × 防火墙 × 网络拓扑，任何一环选错都开不起来；玩家不知道"自己不知道"哪些分支（如混合认证需要额外插件）。
- **P3 故障诊断黑盒**：报错只能把崩溃日志贴进搜索框碰运气；日志其实高度模式化（Java 版本不匹配、端口占用、mod 不兼容各有特征），但玩家读不懂。
- **P4 内网穿透劝退**："没有公网 IP"是好友联机最常见的拦路虎；frp / playit / Tailscale 各有适用条件与配置成本，选型和排障超出普通玩家能力。
- **P5 重复劳动**：每次开服从零开始；换机器、朋友人数变化后，此前的决策与配置无法复用。

### 2.3 通用 Agent 为什么解决不好（基线实验证据）

基线实验（opencode + GLM-5.2 @ Windows 11，记录见 `experiments/`）：在专家全程引导下通用 Agent 能搭起服务器，但每隔几步出现一次偏差——

| 实验观察到的失败模式                           | 对应痛点      | 本产品的应对（→ §3）                      |
| ---------------------------------------------- | ------------- | ----------------------------------------- |
| 版本号信息过时、认为 1.21.11 新于 26.2（幻觉） | P1            | 定制 2：知识库 + 实时校验工具             |
| 下载渠道不合理                                 | P1            | 定制 2：官方渠道工具 + 哈希校验           |
| 漏掉混合认证 / 内存 / 穿透等分支               | P2 / P4       | 定制 1：开服决策指南（Skill）驱动分支覆盖 |
| 出错后定位不到根因                             | P3            | 定制 4：错误模式库 + 会话式诊断           |
| 无进度展示、无配置沉淀、无费用统计，每次从零   | P5 + R4/R5/R6 | R4–R6 作为 Agent 框架能力内置             |

核心论点：**通用 Agent 需要一个懂行的人全程盯着才能完成这件事，而"懂行的人"恰恰是不一定存在的角色。**

## 3. 场景定制方案（≥2 项）

定制不是"把流程写死在代码里"，而是：**给一个真正的 Agent 装上开服领域的工具、知识和操作规程**（课程 Q8 的路径：针对数据源的工具调用、场景 Skills / 知识库、结构化校验、定制提示词）。

### 定制 1：开服决策指南（Skill）+ 全链路领域工具（对应 P2）

- **内容**：将开服的全部决策分支写成 **Agent 的决策指南**（`assets/skills/server-setup/`，Skills 式按需加载，见 §8.5）：指南以操作规程形式列清"哪些决策必须做、按什么规则做、用哪个工具查证、漏了会怎样"——账号类型 × 服务端选型 × MC/Java 版本 × 内存 × EULA × 网络拓扑全部分支。Agent 在会话中按指南推进：信息缺口用 `ask_user` 选项式提问补齐，事实用 `check_version_compat` 等工具查证，执行调用领域工具（`ensure_java` / `fetch_server_jar` / `write_server_files` / `start_server`…）。**用户点名 Spigot 就用 Spigot**（指南中的硬规则：以用户明确点名的选择为准），不得改判。
- **为什么优于通用 Agent**：通用 Agent 不知道开服有哪些必查分支、没有领域工具，只能科普和让玩家手敲命令；本 Agent 的指南保证分支覆盖（混合认证、内存、EULA 这些玩家没主动提的分支被主动处理），领域工具保证执行质量（校验、受管安装、进程托管）。
- **验收**：基线 T4 复合需求一句话，会话轨迹显示 Agent 自主覆盖全部必选分支；部署前 `check_plan` 确定性校验通过——"不漏分支"由决策指南约束与确定性验收双保险保证。

### 定制 2：版本兼容知识库 + 实时校验工具（对应 P1）

- **内容**：内置静态知识（MC 版本 → 所需 Java 大版本、加载器与 mod 生态、mod 中文别名表）+ 实时调用 Mojang / PaperMC / Fabric / Modrinth 官方 API，以**查询工具**形态暴露（`check_version_compat` / `search_mods` / `resolve_mod`）。**设计红线保留：版本类事实永远不进 Prompt**——LLM 的记忆不作事实来源，答什么必须先查什么；下载类操作经带哈希校验的工具完成（`fetch_server_jar` 内置 sha1/sha256 校验，或 Agent 用 `http_download` 时传入校验参数），校验不过即失败回环。
- **验收**：对不存在的版本号 / mod 名（含实验幻觉样例"26.2"）明确拒绝并给出可用版本列表；所有下载文件通过哈希校验。

### 定制 3：内网穿透编排工具包（对应 P4，默认樱花frp）

- **内容**：以樱花frp 为默认：服主完成一次性注册 / 实名 / 获取访问密钥后，Agent 经官方 API v4 工具（`tunnel_*`，见 §8.8）全自动完成——节点选择 → 创建 TCP 隧道 → 官方渠道下载并校验 frpc → 拉起托管 → 端到端验证（TCP + MC SLP ping）→ 生成"朋友们怎么连"说明卡片。选节点、建隧道、排故障由 Agent 编排；节点打分等确定性算法下沉为工具内部逻辑。有公网 IP 的场景给端口映射指引；自建 frp / Tailscale 为备选方案。
- **验收**：无公网 IP 环境下，从 token 就绪到外部客户端可连入，Agent 在会话中自主完成（人工步骤仅限注册 / 实名 / 粘贴 token）。

### 定制 4：MC 崩溃日志诊断（对应 P3，选做）

- **内容**：错误模式库（Java 版本不匹配、端口占用、mod 不兼容、内存不足等特征）作为知识资产；诊断是会话中的 Agent 行为：`read_server_log` 读日志 → `analyze_log` 工具内确定性模式匹配返回候选根因与修复动作 → LLM 结合会话上下文定夺并经工具执行修复（危险动作确认）。确定性知识给弹药，编排决策在 Agent。
- **验收**：基线 T5 三份真实日志根因定位到版本 / 文件 / 配置项级，修复后可启动。

## 4. 用户故事与核心交互流程

### 4.1 用户故事

- **US0 持续会话**：Agent 是一个可以一直对话的对象，不是"一次输入、一次输出"的工具。开完服说"把白名单加上 XXX""内存调大一点""朋友连不上了"——同一会话上下文里继续办。
- **US1 开新服**：我说"我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存"，Agent 按决策指南查证、追问、给方案，确认后自主完成部署，告诉我朋友们怎么连。
- **US2 排障**：朋友连不上 / 服务器崩了，我告诉 Agent，它读日志、探测、定位根因并修复，全程给我看它在查什么。
- **US3 复用**：下次开服时让 Agent 加载上次的配置档案，只改变化的部分。
- **US4 升级**：MC 大版本更新后让 Agent 升级整服：先备份，再逐项核对 mod 兼容性，告诉我哪个 mod 没有新版。
- **US5 管开销**：我设预算上限；Agent 实时显示每次调用 token 与费用，到预算自动停。

### 4.2 核心交互流程（US1 展开，会话式）

会话界面为 inline 滚动流（§8.6）：每类内容以独立视觉块区分——思考（暗灰）、工具调用（`⏺` 行 + 进度）、结果（`⎿` 缩进挂载）、确认（高亮块）、用量（暗色统计行）。一轮完整交互的显示效果示意：

```text
$ mcha                          # 进入交互会话（mcha new "…" 为预填首条消息的快捷方式）

> 我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存          ← 用户输入

✻ 思考中…（暗灰斜体，流式；结束后收起为"已思考 Ns"）
⏺ load_skill("server-setup")                                   ← 工具调用（青色）
  ⎿ ✓ 已加载开服决策指南（214 行）                              ← 结果挂载（缩进）
⏺ check_version_compat(mc="1.21.1", software="fabric", mod="twilight-forest")
  ⎿ ✓ 兼容：1.21.1 ← Fabric 0.16，需 Java 21；暮色森林 4.7 可用
⏺ ask_user("服务器要给谁连？", [同一局域网 | 跨网络·有公网IP | 跨网络·无公网IP])
  ⎿ ← 跨网络·无公网IP                                           ← 用户回答回显

  方案摘要（Markdown 渲染的静态块）…… + 离线模式风险提示
  确认开始部署？[y/N]                                            ← 确认门（高亮块）

⏺ ensure_java(major=21)
  ⎿ ✓ 已有 Java 21（系统 PATH，跳过下载）
⏺ fetch_server_jar(software="fabric", mc="1.21.1")
  ⎿ ✓ fabric-server.jar  38.2/38.2 MB  sha256 校验通过          ← 下载进度
⏺ write_server_files(...)   ⏺ start_server   ⏺ probe_port
  ⎿ ✓ 服务器就绪（12.4s，日志 "Done (3.2s)!"）

  服务器已就绪！本机地址 127.0.0.1:25565。离线模式说明：…        ← 助理回复

> 朋友说连不上                                                   ← 下一轮，同一会话
⏺ read_server_log(tail=50)   ⏺ probe_port(25565)
  ⎿ 端口在监听；日志无异常 → 初步判断朋友侧网络/客户端问题
  …（给出排查指引，或经确认代执行防火墙放行）

> /exit                                                          ← 退出会话（Ctrl-D 同效）
  ── 会话结束 ──                                                  ← 结束汇总（暗色块）
  输入 82.4K tokens · 输出 15.1K tokens · 费用 ¥1.86 · 用时 23 分钟
  轨迹已保存：~/.mcha/sessions/2026-0901-1430.jsonl
```

澄清、执行、诊断、维保都是同一会话的连续回合；**没有澄清轮次上限、没有一次性任务边界**，会话持续到用户满意为止。

## 5. 功能需求清单

### 5.1 功能列表（FR）

**Agent 框架层（M1，与场景无关）**

| 编号 | 功能 | 说明 | 映射 |
| --- | --- | --- | --- |
| FR-01 | Agent Loop | 消息循环、工具调度、停止条件、取消（§8.1） | R1 |
| FR-02 | 多轮会话（REPL） | CLI 交互式会话；任务无轮次边界；历史注入（§8.6） | R2 |
| FR-03 | 通用工具集 | run_command / fs 四件 / http 两件 / web_search / ask_user / load_skill（§8.2） | R1 |
| FR-04 | 工具安全边界 | 工作区路径收敛、危险操作确认、超时与取消（§12） | — |
| FR-05 | 模型配置 | 首次启动 `setup` 向导（必填 3 项 + 连接测试）；Endpoint / Key / 上下文长度 / 思考模式 / 价格表；`config set/list/test` + 手编 TOML | R3 |
| FR-06 | 进度渲染与打断 | 语义化渲染分块（思考 / 工具调用 / 结果 / 确认 / 用量，§8.6）；下载与命令输出实时进度；Ctrl-C 打断当前回合 | R4 |
| FR-07 | 会话历史管理 | 会话 = 完整消息流（含工具调用与结果）落盘；查看 / 导出 / 恢复 | R5 |
| FR-08 | 用量与费用统计 | 每次调用 in / out token 计量与价格换算（明细经 `sessions show` 查看）；会话结束汇总展示、`usage` 全局账本；预算上限、超限中断 | R6 |

**开服场景层（M2+）**

| 编号 | 功能 | 说明 | 映射 |
| --- | --- | --- | --- |
| FR-09 | 环境感知与 Java 供给 | `ensure_java`：检测 / 受管自动安装（§8.7） | 定制1 |
| FR-10 | 服务端获取与校验 | `fetch_server_jar`：原版 / Spigot / Paper / Fabric，官方渠道 + 哈希校验 | 定制2 |
| FR-11 | 配置生成 | `write_server_files`：eula / server.properties / 启动脚本 / 白名单 | 定制1 |
| FR-12 | mod 清单安装 | 玩家给出明确 mod 清单（"装暮色森林和 JEI"）：`search_mods` / `resolve_mod` 检索、中文别名表、依赖闭包解析、版本匹配下载 | 定制2 |
| FR-13 | mod 自然语言推荐 | 玩家描述玩法偏好（"想要一些生存向的 mod"）：Agent 检索筛选、给出候选清单，经用户确认后安装 | 定制2 |
| FR-14 | 服务端生命周期 | `start_server` / `stop_server` / 就绪检测 / 崩溃感知 | 定制1 |
| FR-15 | 连接说明生成 | 本机 / 局域网地址 + 客户端操作指引 | — |
| FR-16 | 配置档案 | `save_profile` / `load_profile`：方案快照 + 产物清单存取 | R5 |
| FR-17 | 内网穿透编排 | `tunnel_*`：樱花frp 全自动编排（§8.8） | 定制3 |
| FR-18 | 日志诊断（选做） | `read_server_log` / `analyze_log`：模式库 + 会话式排障 | 定制4 |
| FR-19 | 升级迁移 | 备份 → mod 逐项核对 → 换版本 → 验证旧世界 | — |

### 5.2 开服决策指南（定制 1 的内容大纲）

开服决策的全部分支如下——它们是**决策指南（Skill）的内容大纲**（指南文档按此编写，Agent 经 `load_skill` 加载执行）：

```text
账号类型 ── 全正版 → online-mode=true
         ├─ 全离线 → online-mode=false + 白名单 + 风险提示
         └─ 混合   → online-mode=false + 认证方案（Paper: 登录插件 / Fabric: EasyAuth）
服务端类型 ── 原版（无 mod 需求）
           ├─ Spigot/Paper（插件玩法；混合认证成熟；点名 Spigot 即 Spigot）
           └─ Fabric/Forge（mod 玩法）→ 加载器 × MC 版本匹配 → mod 依赖解析
MC 版本 → Java 大版本（check_version_compat 查证）→ ensure_java
玩家数 + 机器内存 → JVM -Xmx 推荐（预留系统内存）
网络拓扑 ── 同一局域网 → 直连地址
         └─ 跨网络 → 有公网 IP？→ 端口映射 + 防火墙（给指引）
                     └─ 无 → tunnel_* 穿透编排（默认樱花frp）
首次启动 → EULA 确认（ask_user）→ 就绪检测 → 连接说明
```

### 5.3 R1–R6 覆盖对照

| 课程要求         | 落点                                                                                       |
| ---------------- | ------------------------------------------------------------------------------------------ |
| R1 Rust 核心逻辑 | Agent Loop、工具系统与全部工具后端、LLM 客户端、API 编排（Agent 的"身体"全在 Rust，§7/§9） |
| R2 界面          | FR-02（CLI 交互式会话）                                                                    |
| R3 模型配置      | FR-05、§8.6                                                                                |
| R4 进度与打断    | FR-06、§8.1/§8.6                                                                           |
| R5 历史管理      | FR-07 / FR-16、§8.6                                                                        |
| R6 用量统计      | FR-08、§8.3                                                                                |

## 6. 非功能需求（NFR）

- **NFR-1（Agent 主控）**：一切副作用操作由 LLM 经工具调用发起、由 Rust 工具后端执行；Rust 确定性代码的职责是 Agent Loop、工具实现、事实校验与安全边界，**不替代 Agent 做任务编排与决策**（立场阐述见 §0）。
- **NFR-2（安全）**：API Key 仅存本地（`.gitignore` 覆盖）；导出会话 / 日志自动打码公网 IP 与密钥；危险操作（执行命令、覆盖文件、起停进程、公网暴露）经确认门（§12）。
- **NFR-3（可靠）**：主流程无 `unwrap`；网络操作有超时 / 重试 / 断点续传；工具失败以结构化错误回传给 Agent 自行恢复（重试 / 换渠道 / 问用户），不单点崩溃。
- **NFR-4（成本）**：预算硬上限由 Rust 侧在每轮 LLM 调用前强制；每次调用即时累计。
- **NFR-5（可解释性）**：模块边界清晰、避免技巧性写法；Agent 每一步（工具调用、参数、结果）全程留痕可回放，答辩时每一行代码可解释。
- **NFR-6（平台）**：Windows 10/11 优先，实现层不引入 Windows-only 依赖，尽量保持 Linux 可编译。

## 7. 系统架构

### 7.1 设计原则

1. **决策权在 Agent**：做什么、何时做、失败怎么办，由 LLM 在 Loop 中决定；代码不替它编排任务流程（NFR-1）。
2. **工具是唯一副作用通道**：每个工具单一职责、语义完整、参数 Schema 校验、失败结构化回传；LLM 永远不直接触碰系统，它只能"请求"。
3. **确定性下沉，决策权上移**：哈希校验、版本比对、节点打分、模式匹配等确定性算法封装为工具内部逻辑或独立查询工具——LLM 不手工复刻算法，但决定何时用、怎么用结果。
4. **能查就不猜**：版本 / 依赖 / 下载事实必须来自工具（知识库 / 上游 API / 搜索）；版本类事实不进 Prompt（红线，§8.5）。
5. **安全边界在 Rust**：路径收敛、危险确认、预算守卫、密钥打码在框架层强制；Agent 的自由度在边界之内。
6. **一切留痕**：会话消息流（含全部工具调用与结果）即轨迹——R4/R5/R6 是同一数据流的三个视图。

### 7.2 架构图（分层视图）

```text
┌───────────────────────────────────────────────────────────────────┐
│  cli：多轮会话（REPL）、流式输出、进度渲染(R4)、确认交互、Ctrl-C 打断  │
├───────────────────────────────────────────────────────────────────┤
│  agent：Agent Loop（唯一执行引擎，全阶段）                           │
│    ├─ 消息历史管理 / 上下文窗口裁剪（context_len, R3）                │
│    ├─ 工具注册表 · 参数 Schema 校验 · 确认门 · 执行分发               │
│    └─ 停止条件：模型回合结束（交还用户）/ 用户打断 / 预算耗尽 / 轮数保险  │
├──────────────────┬────────────────────────────┬───────────────────┤
│  llm             │  tools（工具系统）           │  knowledge         │
│  OpenAI 兼容客户端│   通用：run_command /        │  静态知识库(TOML)   │
│  SSE 流式 +       │   read_file/write_file/     │  上游 API 客户端    │
│  tool_calls 解析  │   edit_file/list_dir/       │  (mojang/paper/    │
│  用量钩子(R6)     │   http_get_text/            │   fabric/modrinth/ │
│  预算守卫         │   http_download/            │   adoptium/natfrp) │
│                  │   web_search/ask_user/      │  版本校验管线       │
│                  │   load_skill                │                   │
│                  │   场景(M2)：ensure_java/     │  skills（指南）    │
│                  │   fetch_server_jar/...      │  指南资产(Skills)   │
│                  │   （统一注册，§8.2）          │                   │
├──────────────────┴───────────────┬────────────┴───────────────────┤
│  store：会话/档案/用量（R5/R6）     │  config：模型/价格/预算/安全(R3)  │
│  events：事件总线（进度/用量/轨迹） │  assets：prompts/ skills/ knowledge/ │
└──────────────────────────────────┴────────────────────────────────┘
```

### 7.3 模块职责表

| 模块        | 职责                                                                            | 关键接口（入 → 出）                             | 映射     |
| ----------- | ------------------------------------------------------------------------------- | ----------------------------------------------- | -------- |
| `agent`     | Agent Loop：消息组装、上下文裁剪、工具调度与确认门、停止条件、取消              | 用户消息 → 助理回合（文本 + 工具调用）          | FR-01    |
| `llm`       | OpenAI 兼容自研客户端：SSE 流式、tool_calls 解析、重试、预算守卫、用量上报      | 消息列表 + 工具声明 → 助理消息 + `UsageRecord`  | R3/R6    |
| `tools`     | 工具系统：注册表、Schema 校验、确认门、执行与取消；通用工具实现                 | `ToolCall` → `ToolOutcome`                      | FR-03/04 |
| `mc`        | 开服领域工具后端：Java 供给、服务端获取、配置生成、进程托管、穿透、诊断（选做） | 领域工具调用 → 结构化结果 + 进度事件            | 定制1–4  |
| `knowledge` | 静态知识库（TOML）+ 五家上游 API 客户端 + 版本校验管线；查询工具的后端          | 版本 / 依赖查询 → 校验结论 + 下载清单（含哈希） | 定制2    |
| `store`     | 会话 / 档案 / 用量的持久化与查询                                                | 消息流 → 磁盘；查询 → 历史                      | R5/R6    |
| `config`    | 模型与价格、预算、确认策略、代理与镜像、搜索后端                                | 文件 + 环境变量 → `AppConfig`                   | R3       |
| `cli`       | REPL、流式渲染、进度条、确认交互、打断                                          | 事件流 → 终端；用户输入 → 会话                  | R2/R4    |

### 7.4 核心数据流（一次会话回合）

```text
用户输入（REPL）
  → agent：追加 user 消息 → Loop 开始
      llm.chat(消息历史 + 工具声明)          ← 调用前：预算守卫(R6)；调用后：UsageRecord
      ├─ 返回 tool_calls → 逐个：Schema 校验 → 确认门 → 执行（可取消、发进度事件）
      │     → 结果以 tool 消息回传 → 继续循环
      └─ 返回纯文本（无工具调用）→ 本回合结束，渲染给用户，交还输入提示符
  → 全程：消息流逐条落盘（R5）、事件总线驱动渲染（R4）、用量累计（R6）
```

## 8. 关键设计

### 8.1 agent：Agent Loop 细节

```rust
/// 会话中的一条消息（与 OpenAI Chat 消息同构；R5 落盘主体）
enum Message {
    System { content: String },
    User { content: String },
    Assistant { content: Option<String>, tool_calls: Vec<ToolCall> },
    Tool { call_id: String, name: String, outcome: ToolOutcome },
}

struct ToolCall { id: String, name: String, arguments: serde_json::Value }

/// 工具执行结果：失败也结构化回传，由 Agent 决定下一步（重试/换路/问用户）
enum ToolOutcome { Ok { content: String }, Err { error: String } }
```

- **循环**：发送消息（system + 历史 + 工具声明）→ 解析回复 → 有 `tool_calls` 则逐个执行并回传 → 纯文本且无工具调用则本回合结束。与课程 `agent-architecture.md` 第五节的标准 Agent Loop 一致。
- **回合原子性（消息流合法性）**：裁剪与打断都不破坏消息配对关系——上下文裁剪以**完整回合**（user + assistant + 全部 tool 消息）为单位；用户打断时，该回合未完成的 tool_calls 逐一回填 `ToolOutcome::Err("用户中断")` 后照常入史，半截 assistant 文本丢弃。任何时刻会话中的消息流都结构合法，可直接导出 / 恢复（R5）。
- **停止条件**：① 模型回合自然结束（交还用户输入，多轮继续）；② Ctrl-C（取消当前 LLM 流式或工具执行，按回合原子性规则收尾，会话保留）；③ 预算耗尽（R6，强制收尾并报告）；④ 轮数保险丝（单回合工具调用次数上限，config 可调，默认 40——防失控循环，不是业务限制）。
- **上下文窗口管理（R3 的 context_len 落点）**：发送前按保守字符近似估算 token（CJK ≈ 1 token/字，ASCII ≈ 4 字符/token；M1 不引入 tokenizer）；超限时从最老的完整回合开始裁剪，system prompt 永远保留（被裁内容全量仍落盘，R5 不受影响；工具大输出先经"占位摘要"替换再参与裁剪）。更精细的摘要压缩留作后续迭代。
- **大输出落盘**：工具结果超过阈值（如 8 KB）时写入会话附件目录、回传"路径 + 首尾摘要"，避免撑爆上下文。

### 8.2 tools：工具系统与工具集

```rust
/// 工具统一抽象（课程 agent-architecture.md 第三节的落地）
trait Tool {
    fn name(&self) -> &'static str;
    fn description(&self) -> String;                    // 写清职责，模型选错工具多因描述不清
    fn parameters_schema(&self) -> serde_json::Value;   // schemars 从类型派生
    fn permission(&self) -> Permission;                 // 确认门依据
    async fn run(&self, args: serde_json::Value, ctx: ToolCtx) -> Result<ToolOutcome, ToolError>;
}

enum Permission { ReadOnly, Write, Execute, Network }   // 确认策略见 §12
```

**通用工具集（M1，框架自带，与场景无关）**

| 工具                                                  | 职责                                                             | 边界约束                                        |
| ----------------------------------------------------- | ---------------------------------------------------------------- | ----------------------------------------------- |
| `run_command` | 执行 shell 命令（默认 cwd=工作区），回传 stdout / stderr / exit code；Windows 经 `powershell.exe -NoProfile -Command`，Unix 经 `sh -c` | 超时默认 120s（可配）；输出截断 200 行 / 8 KB（保头尾）；进程树可杀；确认门 y/a/n（§12） |
| `read_file` / `write_file` / `edit_file` / `list_dir` | 工作区文件读 / 写 / 精确替换编辑 / 列目录；edit_file 的 old_string 必须唯一匹配，多处命中报错并要求补足上下文 | 路径收敛：仅工作区与数据目录内；write/edit 确认门 y/a/n（§12） |
| `http_get_text` | 抓取网页 / API 文本（查文档、解析直链） | 超时；大响应截断 |
| `http_download` | 下载文件到工作区（断点续传，不支持 Range 时整段下载；可选 sha256 参数） | 进度事件；哈希不符即失败 |
| `web_search` | 关键词搜索，返回标题 / 链接 / 摘要列表 | 后端可配；未配置时返回结构化错误（提示改用 http_get_text 或配置 `[search]`），Agent 自行转向 |
| `ask_user` | 向用户提问：单选项列表或自由文本（默认允许选项外自由输入） | Agent 获取用户输入的唯一通道；用户 Ctrl-C = 打断当前回合（同全局语义） |
| `load_skill`                                          | 按需加载领域指南（Skills，§8.5）                                 | 只读注入                                        |

**开服领域工具（M2 起注册，`mc` 模块）**

| 工具                                             | 职责（内部细节见 §8.7/§8.8）                                                 |
| ------------------------------------------------ | ---------------------------------------------------------------------------- |
| `check_version_compat`                           | MC × 服务端 × 加载器 × Java × mod 兼容性查证（知识库 + 上游 API）            |
| `search_mods` / `resolve_mod`                    | Modrinth 检索（含中文别名表）、依赖闭包解析、带哈希下载清单                  |
| `ensure_java`                                    | Java 探测 / 受管自动安装（§8.7 整体作为其后端）                              |
| `fetch_server_jar`                               | 原版 / Spigot(getbukkit 抓页解析) / Paper / Fabric 官方渠道下载 + 哈希校验   |
| `write_server_files` | eula / server.properties / whitelist / 启动脚本生成（eula 经 ask_user 确认）；whitelist 条目须含**离线 UUID**（Mojang v3 口径——缺 uuid 字段会被服务端静默丢弃，实测教训） |
| `check_plan` | 部署前确定性校验：online-mode 与账号类型一致、Java 版本匹配、内存 > 0、白名单与离线模式配套、端口未占用等 checklist 核对，缺项返回结构化清单（定制 1 的"不漏分支"第二重保险） |
| `start_server` / `stop_server` / `server_status` | 进程托管、日志行流、就绪检测、Drop 守卫防孤儿进程                            |
| `probe_port` / `mc_ping`                         | 本机端口验证 / SLP ping（取回 MOTD / 版本 / 人数）                           |
| `save_profile` / `load_profile`                  | 部署方案快照与产物清单存取（§8.6）                                           |
| `tunnel_*`                                       | 樱花frp 编排：token 校验 / 节点打分 / 建隧道 / frpc 托管 / 端到端验证        |
| `read_server_log` / `analyze_log`（选做）        | 日志尾部读取；错误模式库确定性匹配 → 候选根因 + 修复动作                     |

分层理由：领域工具是"高频复合操作的可靠封装"（一次调用 = 一个完整语义动作，内建校验与进度），降低 LLM 出错面与轮数；通用工具是逃生舱——领域工具未覆盖或失败时，Agent 可用 `run_command` + `http_download` 等手工完成任务，保证任何真实世界的意外都有出路。

### 8.3 llm：OpenAI 兼容客户端

- 自研薄客户端（reqwest + SSE 流式解析），不引入 LLM SDK——核心调用编排即课程考察点（R1），代码量小、答辩可解释。
- tool_calls 的流式增量拼装；参数 JSON Schema 校验失败携错误重试（≤2）→ 仍失败作为 `ToolOutcome::Err` 回传模型自纠。
- 调用前钩子：预算守卫（`store` 累计值，超限拒绝并触发收尾）；调用后钩子：强制生成 `UsageRecord`（无 usage 字段的服务记调用次数并标注，对应课程 Q9）。思考模式、上下文长度从 `AppConfig` 透传（R3）。
- 重试与超时：连接 15s、单次调用整体 300s（可配）；429 / 5xx 指数退避重试 ≤ 2 次，重试同样计入调用次数（R6 诚实计量）。
- 思考模式（thinking）：上游返回的 reasoning 增量流式渲染（暗灰块）但不持久化全文——会话中仅存"已思考 Ns"占位，避免撑爆上下文；其 token 计量照常计入输出（usage 返回即记录）。
- 价格表缺失的模型：费用记 0 并在 `usage` 报表标注"无价格预设，仅 token 数"。

### 8.4 knowledge：知识库与上游 API

- 静态知识库（L1）：随包 TOML 资源——MC→Java 映射、加载器生态、mod 中文别名表、崩溃错误模式库；带版本号与来源日期，可独立更新。
- 上游客户端：Mojang piston-meta、PaperMC v2、Fabric meta、Modrinth v2、Adoptium v3、SakuraFrp v4；统一 trait，代理与镜像在 HTTP 层统一注入。
- 版本校验管线：`semver` 解析（非法输入如"26.2"拒绝并给就近建议）→ 上游存在性核对 → 依赖闭包解析（Modrinth `dependencies` 递归）→ 产出带哈希的下载清单。以上均作为 `check_version_compat` 等查询工具的后端。

### 8.5 定制内容体系（Skills / 知识 / 提示词）

定制内容按"载体 + 到达 Agent 的方式"分层——每类知识放进最不容易出错、最可维护的载体：

| 层              | 内容                                                     | 载体                                             | 到达 Agent 的方式                                                                                      |
| --------------- | -------------------------------------------------------- | ------------------------------------------------ | ------------------------------------------------------------------------------------------------------ |
| Skills 流程知识 | 开服决策指南、诊断规程、穿透操作手册、离线认证方案对比等 | `assets/skills/<name>/SKILL.md`（+ 参考文档）    | `load_skill` 按需加载：平时只在 system prompt 放一句话清单，Agent 判断需要时加载全文（上下文管理友好） |
| L1 结构化事实   | MC→Java 映射、别名表、端口常识、错误模式库               | `assets/knowledge/*.toml`                        | 仅经查询工具返回值                                                                                     |
| L2 易变事实     | 版本存在性、依赖树、下载地址、节点负载                   | 上游 API 实时查询                                | 工具返回值                                                                                             |
| L4 表达与边界 | 框架级 system prompt（M1：通用助手角色、工具使用纪律、中文交互、安全规则——"不得尝试绕过确认门与路径收敛"）；场景提示词（M2：开服管家角色、领域规则、Skills 清单） | `assets/prompts/*.md`（include_str! 编译期嵌入）；最终 system prompt = 框架基础段（固定）+ 场景段（M2 注入） | system prompt |

**设计红线保留：版本类事实永远不进 Prompt。** 写进 system prompt 的"1.21 需要 Java 21"一旦过时即成系统性幻觉源；放入 L1 / L2，每次使用都经工具查证。Prompt 只承载不变的规则与说话方式。Skills 指南与知识库注释以**中文**撰写——它们既是给模型的操作规程（目标模型中文理解强），也是答辩时可展示的领域资产。

**取舍：不使用 RAG / 向量检索。** 本域事实为枚举型、KB 级、结构规整，精确查表优于语义检索；每条事实要求确定性正确；embedding 调用成本按 R6 计入，能省则省。Skills 的"按需加载"已解决长文档的上下文路由问题。

### 8.6 store / config / cli

- `store`：数据目录 `~/.mcha/`（Windows `%APPDATA%\mcha\`），布局 `{sessions, profiles, usage, runtime}/`。会话 = 消息流 JSONL 逐条追加（崩溃可恢复）+ 元数据快照；`sessions list/show/export` 查看、回放、导出 JSON（自动打码）；档案 `profiles/`。
- **Profile（部署档案）**：部署方案的结构化快照（方案 + 产物清单 + 时间戳），由 Agent 经 `save_profile` 工具落盘、`load_profile` 读回会话上下文；它是记录产物与复用载体（US3），不是流程关卡。字段草案（M2 实现期可微调）：`profile_id`、`created_at`、`account`（Online / Offline{whitelist} / Hybrid{auth}）、`software`（Vanilla / Spigot / Paper{build} / Fabric{loader}）、`mc_version`、`java`（required_major + runtime 路径）、`jvm_memory_mb`、`mods`（project / version / url / sha1）、`network`（Lan / Direct{port} / Tunnel{provider, endpoint}）、`world`（New / Existing{path}）、`artifacts`（实际产物清单：jar / 脚本 / 日志路径）、`notes`（风险提示）。
- `config`（R3，配置与首次启动）：
  - **存储**：`~/.mcha/config.toml`（除 Key 外的一切）+ `~/.mcha/.env`（仅 API Key，环境变量 `MCHA_API_KEY`，可用 `model.api_key_env` 改用其他变量名）；Key 永不写入 config.toml 与仓库。
  - **首次启动向导**：`mcha` 检测到无配置时自动进入 `setup`——必填仅 3 项（endpoint / 模型名 / API Key，endpoint 提供智谱 GLM、DeepSeek 等预设快捷项），其余全部有默认值归入"高级选项"回车即过；完成后自动执行一次**连接测试**（发最小对话请求，显示延迟与模型应答）再进入会话，配置错误在第一步就被发现。
  - **配置文件全景**：

    ```toml
    [model]
    endpoint = "https://open.bigmodel.cn/api/paas/v4"
    model = "glm-5.2"
    context_len = 128000        # 上下文长度（token），裁剪依据
    thinking = false            # 思考模式开关
    # api_key_env = "MCHA_API_KEY"

    [[prices]]                  # 内置常见模型预设随包分发（注明来源与采集日期），用户可覆盖
    model = "glm-5.2"
    input_per_m = ...           # 元 / 百万输入 token
    output_per_m = ...          # 元 / 百万输出 token；无预设的模型费用记 0 并标注

    [budget]
    limit_cny = 10.0            # 费用硬上限，超限自动中断（R6）

    [safety]
    confirm_level = "standard"  # paranoid | standard | auto（§12）

    [search]
    backend = ""                # 空 = 无搜索后端（web_search 返回结构化错误）
    ```

  - **修改途径**：`mcha config set <key> <value>`（toml_edit 写回、保留注释）、`mcha config list`、`mcha setup` 重跑向导、`mcha config test` 单独连接测试；REPL 内 `/usage` 看本会话累计。
  - **启动校验**：必填项缺失时不进入会话，打印可复制的配置模板与缺失项说明。
- `cli`：`mcha`（无子命令）进入交互会话；`mcha new "…"` 预填首条消息的快捷方式；`mcha --continue` 接续最近会话、`mcha --resume` 列出历史会话选择恢复；管理子命令 `config` / `usage` / `sessions` / `profiles`。

**会话界面设计（FR-02 / FR-06 的体验层，M1 交付；风格对标 Claude Code / Codex 类产品）**

- **布局：inline 滚动流，不接管整屏**（不采用 ratatui 式全屏 TUI）。理由：与终端滚动缓冲共存，长会话可回看、可复制；屏幕渲染流与会话落盘的消息流同构，R5 的"非黑盒"直接可见；避免全屏 TUI 的滚动区 / 焦点管理复杂度，答辩可解释。输入为普通行输入（`> ` 前缀）；回合执行中显示活动状态与"Ctrl-C 打断"提示。
- **斜杠命令（REPL 内置）**：M1 最小集 `/exit`（退出并显示会话汇总）、`/usage`（显示本会话累计用量）、`/help`；`/sessions`（列出 / 恢复历史会话）随 R5 完善。
- **终端兼容**：启动时检测 Unicode 渲染能力，不支持 `⏺ ✻ ⎿ ✓` 的环境（老 conhost / GBK 代码页）自动降级 ASCII 符号集。
- **语义化渲染块规范**——每种事件一个视觉样式，符号 + 颜色 + 缩进三层区分（效果示意见 §4.2）：

| 块 | 触发事件 | 视觉规范 |
| --- | --- | --- |
| 用户输入 | user 消息 | `> ` 前缀回显 |
| 思考 | reasoning 流式增量 | 暗灰斜体流式；结束后收起为一行"已思考 Ns" |
| 工具调用 | ToolStarted | `⏺ 工具名(关键参数摘要)`（青色）+ spinner |
| 工具结果 | ToolFinished | `⎿` 缩进挂载 + ✓/✗ + 耗时 + 结果摘要（超长折叠为行数提示，全文可经 `sessions show` 回看） |
| 下载 / 命令进度 | ProgressEvent | indicatif 进度条就地更新；命令输出行原样滚动 |
| 确认门 | ConfirmationRequest | 高亮块：完整命令 / 写入内容（diff 式）+ y/n/本会话允许 |
| ask_user | AskUser 事件 | dialoguer 选项列表；用户选择以 `⎿ ←` 回显 |
| 助理文本 | 助理消息 | 正常色流式直显；方案摘要等静态块以 Markdown 渲染（termimad） |
| 错误 | 错误事件 | 红色块 + "下一步怎么办"指引 |
| 用量 | 会话结束 | **不在会话过程中刷屏**；退出时打印暗色汇总块：总输入 / 输出 token、总费用、会话时长（R6 的"清晰展示"由三层满足：退出汇总、`mcha usage` 全局账本、`sessions show` 每次调用明细）。预算告警 / 超限中断时即时提示例外 |
| 退出 | `/exit`、提示符处 Ctrl-D 或 Ctrl-C | 打印会话结束汇总后退出；消息流已逐条落盘，随时可恢复（R5） |
| 打断 | 取消 | `⏹ 已打断`，会话保留、回到提示符 |

- **流式策略**：LLM 文本与思考增量按原样流式直显（等宽）；Markdown 结构化渲染只用于回合结束的静态块与方案摘要——全流式 Markdown 渲染复杂度高，留作后续迭代，不影响 M1 体验主干。
- **实现组件**：crossterm（颜色 / 样式 / 光标）+ indicatif（spinner、多进度条）+ dialoguer（选项问答）+ termimad（Markdown 静态块）。**渲染器是事件总线的一个订阅者**（与 store 落盘并列）——R4/R5 是同一数据流的两个视图，界面不维护独立状态。

### 8.7 Java 自动供给（`ensure_java` 后端，FR-09）

以下设计全部封装在 `ensure_java` 工具内部，对 Agent 而言是单次调用：

| 设计项   | 决策                                                                | 理由                                     |
| -------- | ------------------------------------------------------------------- | ---------------------------------------- |
| 发行版   | Eclipse Temurin（Adoptium）                                         | 开源免费、MC 社区事实标准                |
| 包类型   | **zip 免安装包**，不用 msi/exe                                      | 免管理员权限、不写注册表、删除目录即卸载 |
| 镜像类型 | 默认 JRE                                                            | 服务端只需 JRE；包体约 JDK 一半          |
| 安装路径 | `<数据目录>/runtime/jdk-<major>/<版本>/`（受管目录）                | 自包含、多版本共存、不污染系统           |
| 选择顺序 | ① 系统 PATH 已有匹配版本 → 用；② 受管目录已有 → 复用；③ 下载安装    | 尊重已有环境；重复开服零下载             |
| API      | Adoptium v3 元数据（含 sha256）→ 下载 zip → 强制校验 → 纯 Rust 解压 | 元数据与二进制同源，校验可信             |
| 架构适配 | `std::env::consts::ARCH` + OS 探测                                  | Windows on ARM 自动选对包                |
| 网络     | 走全局代理 / 镜像（国内默认清华 TUNA Adoptium 镜像）                | 国内可达性统一解法                       |
| 结果     | 绝对路径写入档案；起服一律用该路径                                  | 不依赖 PATH，跨会话可复现                |

### 8.8 内网穿透编排（`tunnel_*` 后端）

一次性人工步骤：注册 → 实名 → 获取访问密钥 → 粘贴配置（Agent 检测缺失时给出分步引导；注册实名涉及合规与隐私，不做自动化）。token 就绪后的全自动编排链路（工具后端实现规格）：

1. `GET /system/clients` 取官方 frpc 下载 URL + 哈希 → `http_download` 校验落受管目录；
2. `GET /user/info` 校验 token / 实名 / 等级；
3. `GET /nodes` + `/node/stats` → 节点打分（硬过滤：在线、可建隧道、非强制访问认证、VIP 等级、非私有；排序：内地优先 → 负载低 → 非 BETA）——**打分算法为工具内部确定性逻辑**，Agent 传入用户偏好即可；
4. `POST /tunnels` 建隧道（tcp / 127.0.0.1 / 服务端端口，名称按约定自动生成便于查重复用）；
5. 拉起 frpc 子进程（`-f <token>:<tunnel_id>` 配置自动拉取，消除手写配置错误类别）；
6. 解析 frpc 日志就绪特征 → 对 `<节点>:<remote_port>` 先 TCP connect 再 **MC SLP ping**，端到端验证全链路；
7. 连接说明卡片：地址、朋友连接分步指引、剩余流量、风险提示。

生命周期：frpc 由进程托管 + Drop 守卫；停机顺序先 frpc 后服务端；复用优先（`GET /tunnels` 按名称查重）；流量查询 `/user/data_plans`、`/tunnel/traffic`。失败模式与诊断要点：API 401/403 → token/实名问题；节点满载 → 换节点重试；frpc 在线但 SLP 超时 → 回查本地监听；安全合规：token 仅存本地并打码；建删隧道频率自律；API 定义 AGPL-3.0 仅调用不复制。

### 8.9 故障诊断（定制 4，选做）

诊断是会话中的 Agent 行为（无独立流水线）：`read_server_log(path, tail)` 读日志 → `analyze_log` 内部跑错误模式库（正则 / 关键词 → 候选根因 + 修复动作 + 置信度）→ LLM 结合会话上下文定夺 → 修复经工具执行（危险动作确认）→ 验证。模式库新增 = 加一行 TOML；`1.21.11 > 26.2` 类版本比较错误本身入库（呼应基线实验）。

## 9. R1–R6 实现设计（课程硬性要求逐条落地）

| 要求             | 落地机制                                                                                                                                                                                              |
| ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R1 Rust 核心逻辑 | **Agent Loop 本身就是 Rust 主控流程**：消息管理、工具调度、确认门、停止条件、全部工具后端、LLM 客户端、校验算法在 Rust 单二进制内；LLM 产出的是意图（tool_calls 数据），执行、校验、状态管理全在 Rust |
| R2 界面          | CLI 交互式会话（clap + dialoguer + indicatif）；只读 Web 状态页可作后续扩展                                                                                                                           |
| R3 模型配置      | `config.toml`（`[model]` / `[[prices]]` / `[budget]` / `[safety]` / `[search]`）+ `.env` 的 key；首次启动 `setup` 向导 + 连接测试；`config set/list/test` 子命令                                                                                             |
| R4 进度与打断    | 工具开始 / 结束、下载字节、命令输出行、LLM 流式字符全部为事件实时渲染；Ctrl-C → CancellationToken → 当前调用取消 + 进程 Drop 守卫，会话保留                                                           |
| R5 历史管理      | 会话 = 完整消息流（含工具调用与结果）JSONL 落盘；`sessions list/show/export`；`save_profile` / `load_profile` 档案复用                                                                                |
| R6 用量统计      | `UsageRecord` 强制生成 + 价格换算；`usage` 按会话 / 全局汇总；预算守卫每轮调用前拦截，超限强制收尾                                                                                                    |

## 10. 技术选型

| 领域              | crate                        | 备选         | 选择理由                                               |
| ----------------- | ---------------------------- | ------------ | ------------------------------------------------------ |
| 异步运行时        | tokio                        | async-std    | 事实标准；CancellationToken 与进程管理（R4）依赖其生态 |
| HTTP 客户端       | reqwest（rustls-tls）        | ureq         | 异步流式（SSE）必需；rustls 免 Windows OpenSSL 负担    |
| SSE 解析          | eventsource-stream           | 手写行解析   | 标准协议解析自写易错                                   |
| 序列化            | serde + serde_json + toml    | —            | 事实标准；JSON 是 R5 落盘与工具参数统一格式            |
| Schema 派生与校验 | schemars + jsonschema        | 手写校验     | 工具参数 Schema 从类型派生，响应按同一 Schema 校验     |
| 错误处理          | thiserror + anyhow           | 手写         | 模块内显式错误枚举，main 层聚合                        |
| CLI 框架          | clap（derive）               | argh         | 子命令与帮助完备（R2）                                 |
| 交互问答 | dialoguer | inquire | ask_user 的选项 / 输入 / 确认控件 |
| 终端控制 | crossterm | — | 颜色、样式与光标控制；会话界面渲染基底 |
| Markdown 渲染 | termimad | 手写 | 方案摘要等静态块的终端 Markdown 渲染 |
| 进度条            | indicatif                    | 手写         | 多进度条与速率显示（R4）                               |
| 版本解析          | semver                       | 手写         | 拒绝非语义化输入是定制 2 第一道闸                      |
| 哈希校验          | sha1 + sha2（RustCrypto）    | —            | Mojang sha1 / Modrinth sha1+sha512 / Adoptium sha256   |
| 压缩解压          | zip                          | 手写         | Java 供给解压，纯 Rust                                 |
| 正则              | regex                        | —            | 错误模式库匹配（§8.9）                                 |
| 日志与观测        | tracing + tracing-subscriber | log          | 结构化分模块日志                                       |
| 时间              | chrono                       | time         | 落盘时间戳统一 RFC3339                                 |
| 临时文件          | tempfile                     | 手写         | 原子写与下载临时目录                                   |
| LLM SDK           | **不引入**（自研薄客户端）   | async-openai | 核心业务即 API 调用编排（R1 考察点），答辩可解释 |

## 11. 数据源与外部依赖

| 数据                     | 来源                                                                                       | 用途                         | 备注                                                                          |
| ------------------------ | ------------------------------------------------------------------------------------------ | ---------------------------- | ----------------------------------------------------------------------------- |
| MC 版本清单 / 原版服务端 | Mojang piston-meta API                                                                     | 版本校验、原版服下载         | 国内可达性：代理 / 镜像机制                                                   |
| Paper 构建与下载         | PaperMC API                                                                                | Paper 服下载                 | 同上                                                                          |
| Fabric 版本与安装器      | Fabric meta / maven                                                                        | Fabric 服搭建                | 同上                                                                          |
| Spigot jar | getbukkit 下载页抓取解析（下载页令牌 → 302 → CDN 直链） | Spigot 服下载 | 无官方哈希，轨迹明示第三方来源 |
| mod 元数据               | Modrinth API v2                                                                            | 检索、版本匹配、依赖树、下载 | CurseForge 视需要后补                                                         |
| JRE 二进制               | Adoptium v3 API（镜像：清华 TUNA）                                                         | Java 自动供给（§8.7）        | sha256 校验                                                                   |
| 樱花frp API / frpc       | `api.natfrp.com/v4`；frpc 经官方分发（含哈希）                                             | 穿透编排（§8.8）             | 一次性人工：注册 + 实名 + token；API 定义 AGPL-3.0，引用注明                  |
| 网络搜索 | 可配置后端：默认无；可选 DuckDuckGo HTML 抓取（免 key）或 Serper 等（配 key） | `web_search` 工具 | 国内可达性与反爬脆弱性诚实标注；领域事实主通道是知识库 + 上游 API，搜索是兜底 |
| LLM                      | 任意 OpenAI 兼容 Chat API（用户配置）                                                      | Agent 大脑                   | R3；无 usage 时记次数（课程 Q9）                                              |

## 12. 错误处理与安全设计

- 错误分层：模块级 thiserror 枚举 → `AppError`；工具错误结构化回传 Agent（NFR-3）；用户可见错误必须附"下一步怎么办"。
- **确认门（FR-04）**：按工具 `Permission` 分级——`ReadOnly` 免确认；`Write`（写文件）/ `Execute`（跑命令、起停进程）/ `Network`（下载大文件）默认确认，显示关键内容（命令行、目标路径、写入摘要）后**三选一：y 本次允许 / a 本会话允许此工具 / n 拒绝**（拒绝以结构化错误回传 Agent，由其调整方案）；`[safety] confirm_level = paranoid | standard | auto` 可调（默认 standard；auto 全部免确认，限演示 / CI 并留痕）。
- **路径收敛**：文件类工具的目标路径必须解析在工作区或数据目录内，越界拒绝（结构化错误回传 Agent）。
- **下载安全**：HTTPS + 官方域白名单（含镜像域）+ 哈希校验三重；镜像仅替换白名单内域名。
- **进程安全**：子进程统一托管（进程组 / Job Object），Drop 守卫保证取消 / 退出时杀干净，不留孤儿。
- 密钥安全：`.env` / `config.toml` 在 `.gitignore`；导出打码（NFR-2）。
- 离线模式风险：白名单建议与后果提示写入决策指南（定制 1），Agent 必须向用户明示。

## 13. 测试与验收策略

- 单元：上下文裁剪策略、确认门与路径收敛（含越界负路径）、版本校验管线（含"26.2"拒绝）、别名表检索、JVM 参数推导、Java 供给的版本解析与路径规则、节点打分。
- **Loop 级集成**：`LlmClient` trait + Fake 实现（脚本化回复与 tool_calls 序列）驱动 Agent Loop 全流程——不花真钱；断言消息流形状、工具调用顺序、停止条件、取消语义。
- 端到端验收：复用基线实验测例 T1/T3/T4/T5 作验收脚本（同输入、同评分标准），形成"通用 Agent 失败样例 ↔ 本系统通过"一一对应，用于文档与答辩演示。
- 真实 API 冒烟：`cargo test --ignored` 跑上游连通与一次真实开服。

## 14. 仓库组织与实现顺序

```text
src/
├── main.rs            # clap 入口、取消总线装配
├── cli/               # REPL、流式渲染、进度、确认交互、子命令
├── agent/             # Agent Loop：消息管理、上下文裁剪、调度、确认门、停止条件
├── llm/               # 自研客户端：SSE、tool_calls、预算守卫、用量钩子
├── tools/             # 工具系统：注册表、Schema 校验、Permission
│   ├── general/       # 通用工具：fs / shell / http / search / ask_user / load_skill
│   └── mod.rs
├── mc/                # 开服领域工具后端：java / server_jar / files / process / probe
│                      #   （选做：tunnel / diag）
├── knowledge/         # 知识库加载、上游 API 客户端、版本校验管线
├── store/             # 会话 / 档案 / 用量持久化
├── config/            # AppConfig、价格表、确认策略
├── events.rs          # 事件总线（进度 / 用量 / 轨迹）
└── assets/            # prompts/ skills/ knowledge/*.toml
```

**实现顺序（先框架、后场景）**：

- **M1 Agent 框架**：config → llm → agent（Loop + 确认门 + 上下文管理）→ tools/general → cli（REPL + 渲染 + 打断）→ store → events，含**框架级系统提示词**（§8.5 L4 基础段，随 assets/prompts 交付）。出口标准：纯通用任务验收——用自然语言让 Agent 在受控工作区完成一件多步实事（如"抓取某页面存为文件并统计行数"），全程工具调用留痕可查、可打断、有费用统计。此时它已是一个真正的（小）通用 Agent。
- **M2 开服场景包**：knowledge → mc 工具 → assets（skills / prompts）。出口标准：US1 一句话开服全流程跑通。

每模块完成后独立可编译、可运行（先跑通再美化）。
