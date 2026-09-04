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
- **为什么优于通用 Agent**：通用 Agent 不知道开服有哪些必查分支、没有领域工具，只能科普和让玩家手敲命令；本 Agent 的指南保证分支覆盖（混合认证、内存、EULA 这些玩家没主动提的分支被主动处理），领域工具保证执行质量（校验、受管安装、窗口化启动）。
- **验收**：基线 T4 复合需求一句话，会话轨迹显示 Agent 自主覆盖全部必选分支；部署前 `check_plan` 确定性校验通过——"不漏分支"由决策指南约束与确定性验收双保险保证。

### 定制 2：版本兼容知识库 + 实时校验工具（对应 P1）

- **内容**：内置静态知识（MC 版本 → 所需 Java 大版本、加载器与 mod 生态、mod 中文别名表）+ 实时调用 Mojang / PaperMC / Fabric / Modrinth 官方 API，以**查询工具**形态暴露（`check_version_compat` / `search_mods` / `resolve_mod`）。**设计红线保留：版本类事实永远不进 Prompt**——LLM 的记忆不作事实来源，答什么必须先查什么；下载类操作经带哈希校验的工具完成（`fetch_server_jar` 内置 sha1/sha256 校验，或 Agent 用 `http_download` 时传入校验参数），校验不过即失败回环。
- **验收**：对不存在的版本号 / mod 名（含实验幻觉样例"26.2"）明确拒绝并给出可用版本列表；所有下载文件通过哈希校验。

### 定制 3：内网穿透编排工具包（对应 P4，默认樱花frp）

- **内容**：以樱花frp 为默认：服主完成一次性注册 / 实名 / 获取访问密钥后，Agent 经官方 API v4 工具（`tunnel_*`，见 §8.8）全自动完成——节点选择 → 创建 TCP 隧道 → 官方渠道下载并校验 frpc → 独立窗口拉起 → 端到端验证（TCP + MC SLP ping）→ 生成"朋友们怎么连"说明卡片。选节点、建隧道、排故障由 Agent 编排；节点打分等确定性算法下沉为工具内部逻辑。有公网 IP 的场景给端口映射指引；自建 frp / Tailscale 为备选方案。
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
⏺ check_version_compat(mc="1.21.1", software="fabric")
  ⎿ ✓ 兼容：1.21.1 ← Fabric 0.16，需 Java 21
⏺ resolve_mod(mods=["暮色森林", "JEI"], mc="1.21.1")
  ⎿ ✓ 暮色森林 ← CurseForge（the-twilight-forest）；JEI ← Modrinth；依赖已带出
    （未配置 CurseForge Key 时：CF 通道自动走国内镜像，暮色森林照常闭环）
⏺ ask_user("服务器要给谁连？", [同一局域网 | 跨网络·有公网IP | 跨网络·无公网IP])
  ⎿ ← 跨网络·无公网IP                                           ← 用户回答回显

  方案摘要（Markdown 渲染的静态块）…… + 离线模式风险提示
  确认开始部署？[y/N]                                            ← 确认门（高亮块）

⏺ ensure_java(major=21)
  ⎿ ✓ 已有 Java 21（系统 PATH，跳过下载）
⏺ fetch_server_jar(software="fabric", mc="1.21.1")
  ⎿ ✓ fabric-server.jar  38.2/38.2 MB  sha256 校验通过          ← 下载进度
⏺ write_server_files(...)   ⏺ start_server   ⏺ probe_port
  ⎿ ✓ 独立服务器窗口已就绪（12.4s，Done (3.2s)!，日志在窗口滚动）
  │ 已等待 5s：服务器日志在独立窗口滚动，此处静默等待                 ← 进度行（R4）

  服务器已就绪！本机地址 127.0.0.1:25565；停服请在服务器窗口按      ← 助理回复
  Ctrl+C，世界自动保存。离线模式说明：…

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

**Agent 框架层（与场景无关）**

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

**开服场景层**

| 编号 | 功能 | 说明 | 映射 |
| --- | --- | --- | --- |
| FR-09 | 环境感知与 Java 供给 | `sys_info` 环境探测 + `check_java` / `ensure_java`：检测 / 受管自动安装（§8.7/§8.10） | 定制1 |
| FR-10 | 服务端获取与校验 | `fetch_server_jar`：原版 / Spigot / Paper / Fabric，官方渠道 + 哈希校验 | 定制2 |
| FR-11 | 配置生成 | `write_server_files`：eula / server.properties / 启动脚本 / 白名单 | 定制1 |
| FR-12 | mod 清单安装 | 玩家给出明确 mod 清单（"装暮色森林和 JEI"）：`search_mods` / `resolve_mod` 检索、中文别名表、依赖闭包解析、版本匹配下载 | 定制2 |
| FR-13 | mod 自然语言推荐 | 玩家描述玩法偏好（"想要一些生存向的 mod"）：Agent 检索筛选、给出候选清单，经用户确认后安装 | 定制2 |
| FR-14 | 服务端生命周期 | `start_server`（独立窗口，D134）/ `server_status` / 就绪检测 / 崩溃感知 | 定制1 |
| FR-15 | 连接说明生成 | 本机 / 局域网地址 + 客户端操作指引 | — |
| FR-16 | 配置档案 | `save_profile` / `load_profile`：方案快照 + 产物清单存取 | R5 |
| FR-17 | 内网穿透编排 | `tunnel_*`：樱花frp 全自动编排（§8.8） | 定制3 |
| FR-18 | 日志诊断（选做） | `read_server_log` / `analyze_log`：模式库 + 会话式排障 | 定制4 |
| FR-19 | 升级迁移 | 备份 → mod 逐项核对 → 换版本 → 验证旧世界 | — |
| FR-20 | 领域检索通道 | `wiki_search` / `wiki_page`：MC Wiki 与 MC百科的 source 抽象检索（§8.11） | 定制2 |

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
- **NFR-2（安全）**：API Key 仅存本地（`.gitignore` 覆盖）；导出会话 / 日志自动打码公网 IP 与密钥；危险操作（执行命令、覆盖文件、启动服务器、公网暴露）经确认门（§12）。
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
│                  │   场景：ensure_java/        │  skills（指南）    │
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
| `mc`        | 开服领域工具后端：Java 供给、服务端获取、配置生成、进程生命周期（独立窗口，D134）、穿透、诊断（选做） | 领域工具调用 → 结构化结果 + 进度事件            | 定制1–4  |
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
- **上下文窗口管理（R3 的 context_len 落点）**：发送前按保守字符近似估算 token（CJK ≈ 1 token/字，ASCII ≈ 4 字符/token；不引入 tokenizer）；超限时从最老的完整回合开始裁剪，system prompt 永远保留（被裁内容全量仍落盘，R5 不受影响；工具大输出先经"占位摘要"替换再参与裁剪）。更精细的摘要压缩留作后续迭代。
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

**通用工具集（框架自带，与场景无关）**

| 工具                                                  | 职责                                                             | 边界约束                                        |
| ----------------------------------------------------- | ---------------------------------------------------------------- | ----------------------------------------------- |
| `run_command` | 执行 shell 命令（默认 cwd=工作区），回传 stdout / stderr / exit code；Windows 经 `powershell.exe -NoProfile -Command`，Unix 经 `sh -c` | 超时默认 120s（可配）；输出截断 200 行 / 8 KB（保头尾）；进程树可杀；确认门 y/a/n（§12） |
| `read_file` / `write_file` / `edit_file` / `list_dir` | 工作区文件读 / 写 / 精确替换编辑 / 列目录；edit_file 的 old_string 必须唯一匹配，多处命中报错并要求补足上下文 | 路径收敛：仅工作区与数据目录内；write/edit 确认门 y/a/n（§12） |
| `http_get_text` | 抓取网页 / API 文本（查文档、解析直链） | 超时；大响应截断 |
| `http_download` | 下载文件到工作区（断点续传，不支持 Range 时整段下载；可选 sha256 参数） | 进度事件；哈希不符即失败 |
| `web_search` | 关键词搜索，返回标题 / 链接 / 摘要列表 | 后端可配；未配置时返回结构化错误（提示改用 http_get_text 或配置 `[search]`），Agent 自行转向 |
| `ask_user` | 向用户提问：单选项列表或自由文本（默认允许选项外自由输入） | Agent 获取用户输入的唯一通道；用户 Ctrl-C = 打断当前回合（同全局语义） |
| `load_skill`                                          | 按需加载领域指南（Skills，§8.5）                                 | 只读注入                                        |

**开服领域工具（`mc` 模块）**

| 工具                                             | 职责（内部细节见 §8.7/§8.8）                                                 |
| ------------------------------------------------ | ---------------------------------------------------------------------------- |
| `check_version_compat`                           | MC × 服务端 × 加载器 × Java × mod 兼容性查证（知识库 + 上游 API；ReadOnly）  |
| `sys_info`                                       | 环境探测：OS / 架构 / 总内存 / 可用内存 / CPU 核数（-Xmx 推荐依据；ReadOnly） |
| `check_java` / `ensure_java`                     | Java 探测（ReadOnly）/ 受管自动安装（Network）——确认门只落在真实安装上（§8.7） |
| `search_mods` / `resolve_mod` / `install_mods`   | mod 三段式（§8.12）：检索（含中文别名表）→ 版本匹配 + 依赖闭包（Modrinth / CurseForge 双源）→ 意图清单 → 校验下载落盘 |
| `wiki_search` / `wiki_page`                      | 领域检索通道（§8.11）：MC Wiki / MC百科；ReadOnly      |
| `fetch_server_jar`                               | 原版 / Spigot(getbukkit 抓页解析) / Paper / Fabric 官方渠道下载 + 哈希校验   |
| `write_server_files` | eula / server.properties / whitelist / 启动脚本生成（eula 经 ask_user 确认）；whitelist 条目须含**离线 UUID**（Mojang v3 口径——缺 uuid 字段会被服务端静默丢弃，实测教训） |
| `check_plan` | 部署前确定性校验：online-mode 与账号类型一致、Java 版本匹配、内存 > 0、白名单与离线模式配套、端口未占用等 checklist 核对，缺项返回结构化清单（定制 1 的"不漏分支"第二重保险） |
| `start_server` / `server_status` | 独立终端窗口启动（与手动脚本一致，D134）、latest.log 就绪轮询、端口探测 + 日志尾部推断（D134 修订 D118 托管模型）            |
| `probe_port` / `mc_ping`                         | 本机端口验证 / SLP ping（取回 MOTD / 版本 / 人数）                           |
| `save_profile` / `load_profile`                  | 部署方案快照与产物清单存取（§8.6/§8.12）                                     |
| `tunnel_*`（七件套）                              | 樱花frp 编排（§8.8）：check_tunnel / ensure_frpc / select_tunnel_node / create_tunnel / start_tunnel / tunnel_status / delete_tunnel |
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
- 上游客户端：Mojang piston-meta、PaperMC Fill v3（旧 v2 已 410 下线，2026-09 实测迁移）、Fabric meta、Modrinth v2、Adoptium v3、SakuraFrp v4；代理与镜像在 HTTP 层统一注入。
- 版本校验管线：`semver` 解析（非法输入如"26.2"拒绝并给就近建议）→ 上游存在性核对 → 依赖闭包解析（Modrinth `dependencies` 递归）→ 产出 mod **意图清单**（URL 与哈希留到安装期实时重取）。以上均作为 `check_version_compat` 等查询工具的后端。MC 版本为纯数字点分串，排序与区间匹配用**自写数值段比较器**（约 30 行，答辩可解释），`semver` 仅作输入合法性闸（§8.10）。

### 8.5 定制内容体系（Skills / 知识 / 提示词）

定制内容按"载体 + 到达 Agent 的方式"分层——每类知识放进最不容易出错、最可维护的载体：

| 层              | 内容                                                     | 载体                                             | 到达 Agent 的方式                                                                                      |
| --------------- | -------------------------------------------------------- | ------------------------------------------------ | ------------------------------------------------------------------------------------------------------ |
| Skills 流程知识 | 开服决策指南、诊断规程、穿透操作手册、离线认证方案对比等 | `assets/skills/<name>/SKILL.md`（+ 参考文档）    | `load_skill` 按需加载：平时只在 system prompt 放一句话清单，Agent 判断需要时加载全文（上下文管理友好） |
| L1 结构化事实   | MC→Java 映射、别名表、端口常识、错误模式库               | `assets/knowledge/*.toml`                        | 仅经查询工具返回值                                                                                     |
| L2 易变事实     | 版本存在性、依赖树、下载地址、节点负载                   | 上游 API 实时查询                                | 工具返回值                                                                                             |
| L4 表达与边界 | 框架级 system prompt（通用助手角色、工具使用纪律、中文交互、安全规则——"不得尝试绕过确认门与路径收敛"）；场景提示词（开服管家角色、领域规则、Skills 清单） | `assets/prompts/*.md`（include_str! 编译期嵌入）；最终 system prompt = 框架基础段（固定）+ 场景段（按场景注入） | system prompt |

**设计红线保留：版本类事实永远不进 Prompt。** 写进 system prompt 的"1.21 需要 Java 21"一旦过时即成系统性幻觉源；放入 L1 / L2，每次使用都经工具查证。Prompt 只承载不变的规则与说话方式。Skills 指南与知识库注释以**中文**撰写——它们既是给模型的操作规程（目标模型中文理解强），也是答辩时可展示的领域资产。

**领域检索通道（§8.11）**：MC Wiki / MC百科作为第二类事实来源接入——定位是**背景知识、交叉验证与上游覆盖不到的中文语境问题**（版本沿革、术语解释、教程性内容）；版本存在性 / 下载 URL / 哈希仍**以上游 API 为权威**。检索结果只经工具返回值进入 Agent 上下文，不进 Prompt——红线不因新通道松动。

**取舍：不使用 RAG / 向量检索。** 本域事实为枚举型、KB 级、结构规整，精确查表优于语义检索；每条事实要求确定性正确；embedding 调用成本按 R6 计入，能省则省。Skills 的"按需加载"已解决长文档的上下文路由问题。

### 8.6 store / config / cli

- `store`：数据目录 `~/.mcha/`（Windows `%APPDATA%\mcha\`），布局 `{sessions, profiles, usage, runtime}/`。会话 = 消息流 JSONL 逐条追加（崩溃可恢复）+ 元数据快照；`sessions list/show/export` 查看、回放、导出 JSON（自动打码）；档案 `profiles/`。
- **Profile（部署档案）**：部署方案的结构化快照（方案 + 产物清单 + 时间戳），由 Agent 经 `save_profile` 工具落盘、`load_profile` 读回会话上下文；它是记录产物与复用载体（US3），不是流程关卡。字段定稿：`profile_id`（时间戳短 id，如 `20260903-142530`）、`created_at`、`account`（Online / Offline{whitelist} / Hybrid{auth}）、`software`（Vanilla / Spigot / Paper{build} / Fabric{loader}）、`mc_version`、`java`（required_major + runtime 路径）、`jvm_memory_mb`、`mods`（[{slug, version_id, file_name, sha1}]）、`network`（Lan / Direct{port} / Tunnel{provider, endpoint}）、`world`（New / Existing{path}）、`artifacts`（[{kind, path}]：jar / 脚本 / mods 目录 / 日志等实际产物）、`notes`（风险提示）。落盘 `~/.mcha/profiles/<profile_id>.json`，存取工具细节见 §8.12。
- `config`（R3，配置与首次启动）：
  - **存储**：`~/.mcha/config.toml`（除 Key 外的一切）+ `~/.mcha/.env`（各类 Key：`MCHA_API_KEY` 为 LLM Key，可用 `model.api_key_env` 改名；`MCHA_CURSEFORGE_KEY` 为 CurseForge API Key，可选；`MCHA_NATFRP_TOKEN` 为樱花frp 访问密钥，可选）；Key 永不写入 config.toml 与仓库。
  - **首次启动向导**：`mcha` 检测到无配置时自动进入 `setup`——必填仅 3 项（endpoint / 模型名 / API Key，endpoint 提供智谱 GLM、DeepSeek 等预设快捷项），其余全部有默认值归入"高级选项"回车即过；完成后自动执行一次**连接测试**（发最小对话请求，显示延迟与模型应答）再进入会话，配置错误在第一步就被发现。**向导可重复运行**：`mcha setup` 读取已有配置作为默认值（回车保留当前值），检测到已有 Key 时显示"已设置，回车保留"；`.env` 采用**合并写入**（只更新被修改的项，其余 Key 原样保留）。**可选步骤**：向导尾部提供 CurseForge API Key 配置（默认跳过；未配置时 CF 通道自动使用国内镜像，功能完整可用；选择配置时给出分步申请指引）与樱花frp 访问密钥配置（默认跳过；含**注册与登录两个入口** + 实名认证 + 密钥获取的分步可点击指引）。
  - **REPL `/token` 命令**：会话进行中补配樱花frp 访问密钥——隐藏输入、合并写 `.env`、同步进程环境与工具上下文；密钥不回显、不经过 LLM 上下文（D136）。
  - **启动自检（非阻断）**：交互会话启动时一次性检查可选项配置状态，对未配置项给出一条紧凑提示（CurseForge Key 缺失 → "CF 走国内镜像通道，如需官方 API 可运行 mcha setup 配置 Key"；搜索后端缺失 → "web_search 不可用，领域事实走知识库 + 上游 API"；樱花frp 密钥缺失 → "本机联机不受影响；朋友跨网络加入需 /token 或 mcha setup"，D140）；不强制、不重复提醒、全部就绪时不输出额外内容。
  - **可点击链接**：向导等用户直出文本中的网址按终端能力渲染——支持 OSC 8 的终端（Windows Terminal / iTerm2 / VTE 系 / mintty 等，白名单检测）输出显式超链接，其余回退纯文本 URL（现代终端自动识别）；`MCHA_NO_HYPERLINK=1` 强制关闭（与 `MCHA_ASCII` 同一降级约定）。工具回传 Agent 的文本内保持纯 URL，不夹带转义序列。
  - **配置文件全景**：

    ```toml
    [model]
    endpoint = "https://open.bigmodel.cn/api/paas/v4"
    model = "glm-5.2"
    context_len = 256000        # 上下文长度（token），裁剪依据
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

    [network]                   # 下载镜像（§8.10）
    mojang_mirror = "bmclapi"   # bmclapi | off | 自定义基础URL
    adoptium_mirror = "tuna"    # tuna | off
                                # CurseForge：已配 MCHA_CURSEFORGE_KEY 走官方 API；未配置自动走国内镜像（§8.12）

    [retrieval]                 # wiki 检索来源注册（§8.11）
    mcwiki = "https://wiki.biligame.com/mc/api.php"
    # mcmod = ""                # 随 mod 场景接入

    [search]
    backend = ""                # 空 = 无搜索后端（web_search 返回结构化错误）
    ```

  - **修改途径**：`mcha config set <key> <value>`（toml_edit 写回、保留注释）、`mcha config list`、`mcha setup` 重跑向导、`mcha config test` 单独连接测试；REPL 内 `/usage` 看本会话累计。
  - **启动校验**：必填项缺失时不进入会话，打印可复制的配置模板与缺失项说明。
- `cli`：`mcha`（无子命令）进入交互会话；`mcha new "…"` 预填首条消息的快捷方式；`mcha --continue` 接续最近会话、`mcha --resume` 列出历史会话选择恢复；管理子命令 `config` / `usage` / `sessions` / `profiles`。

**会话界面设计（FR-02 / FR-06 的体验层；风格对标 Claude Code / Codex 类产品）**

- **布局：inline 滚动流，不接管整屏**（不采用 ratatui 式全屏 TUI）。理由：与终端滚动缓冲共存，长会话可回看、可复制；屏幕渲染流与会话落盘的消息流同构，R5 的"非黑盒"直接可见；避免全屏 TUI 的滚动区 / 焦点管理复杂度，答辩可解释。输入为普通行输入（`> ` 前缀）；回合执行中显示活动状态与"Ctrl-C 打断"提示。
- **斜杠命令（REPL 内置）**：最小集 `/exit`（退出并显示会话汇总）、`/usage`（显示本会话累计用量）、`/help`；`/sessions`（列出 / 恢复历史会话）随 R5 完善；`/token`（会话内补配樱花frp 访问密钥，D136）。
- **终端兼容**：启动时检测 Unicode 渲染能力，不支持 `⏺ ✻ ⎿ ✓` 的环境（老 conhost / GBK 代码页）自动降级 ASCII 符号集。
- **语义化渲染块规范**——每种事件一个视觉样式，符号 + 颜色 + 缩进三层区分（效果示意见 §4.2）：

| 块 | 触发事件 | 视觉规范 |
| --- | --- | --- |
| 用户输入 | user 消息 | `> ` 前缀回显 |
| 思考 | reasoning 流式增量 | 暗灰斜体流式（`✻` 前缀）；**直显超过 4 行即折叠**为"…（思考内容较长，已折叠）"（另有 2000 字符保险丝）；结束后挂一行"已思考 Ns"——该行在思考结束时立即发出，不迟到到正文末尾 |
| 工具调用 | ToolStarted | `⏺ <动词> 工具名(关键参数摘要)`（青色，动词加粗）——动词表给每个工具配自然语言解释（如 run_command → "运行命令"），用户不必猜测工具名含义 |
| 工具结果 | ToolFinished | `⎿` 缩进挂载 + ✓/✗ + 耗时 + 结果摘要；命令 stdout/stderr **直显超过 12 行即省略**并注明"已省略 N 行输出"，摘要压成单行且渲染层再截 120 字符（全文可经 `sessions show` 回看） |
| 下载 / 命令进度 | ProgressEvent | indicatif 进度条就地更新；命令输出行以 `│` 引用条样式滚动；`start_server` 等待就绪期间每 5 秒一行进度，**服务器日志不进 Agent 界面**（在独立窗口滚动，D134） |
| 确认门 | ConfirmationRequest | 高亮块：完整命令 / 写入内容（diff 式）+ y/n/本会话允许 |
| ask_user | AskUser 事件 | dialoguer 选项列表；用户选择以 `⎿ ←` 回显；交互期间渲染器停靠、不叠加 spinner（交互控件独占终端，防重绘互相打乱） |
| 助理文本 | 助理消息 | 正常色流式直显；方案摘要等静态块以 Markdown 渲染（termimad） |
| 错误 | 错误事件 | 红色块 + "下一步怎么办"指引 |
| 用量 | 会话结束 | **不在会话过程中刷屏**；退出时打印暗色汇总块：总输入 / 输出 token、总费用、会话时长（R6 的"清晰展示"由三层满足：退出汇总、`mcha usage` 全局账本、`sessions show` 每次调用明细）。预算告警 / 超限中断时即时提示例外 |
| 退出 | `/exit`、提示符处 Ctrl-D 或 Ctrl-C | 打印会话结束汇总后退出；消息流已逐条落盘，随时可恢复（R5） |
| 打断 | 取消 | `⏹ 已打断`，会话保留、回到提示符 |

- **流式策略**：LLM 文本与思考增量按原样流式直显（等宽）；Markdown 结构化渲染只用于回合结束的静态块与方案摘要——全流式 Markdown 渲染复杂度高，留作后续迭代，不影响体验主干。**显示层折叠 / 省略只影响终端**：完整内容照常落盘（R5）、照常回传模型，界面点到为止。
- **板块呼吸感**：回合内跨板块（思考 / 文本 / 工具）时渲染器自动补空行；提示符前也预留空行——长回合里各视觉块保持清晰分隔。
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

供给拆分为两个工具：`check_java`（ReadOnly——PATH / JAVA_HOME / 受管目录扫描，`java -version` 解析 major，返回全部发现）与 `ensure_java`（Network——确认门只落在真实安装路径上，纯探测免确认）。机器内存等环境事实由 `sys_info` 提供，作为 -Xmx 推荐依据（公式见 §8.10）。

### 8.8 内网穿透编排（`tunnel_*` 后端，FR-17 / 定制 3）

**一次性人工步骤**：注册 / 登录（入口统一在首页 `www.natfrp.com` 右上角；深链勘误 D143）→ 实名认证（管理面板 `www.natfrp.com/user/` 内完成，建隧道硬前置）→ 复制访问密钥（`/user/profile`）→ 粘贴配置。注册与实名涉及合规与隐私，不做自动化；密钥入口见下（setup 向导可选步骤 + `/token` 命令，D136）。token 就绪后的全自动编排（工具后端实现规格，D135/D137）：

**事实基线（2026-09-04 抓取 API v4 OpenAPI 规范逐端点核实，D135）**

- 认证：`Authorization: Bearer <访问密钥>`；错误统一 `{code, msg}` JSON。
- `GET /user/info`：实名状态、用户组等级（节点 VIP 门槛依据）、流量 `[本日消耗, 总剩余]`、隧道数上限、限速字符串；**账户冻结是独立 schema**（`ban` 字段出现即冻结，需识别并如实转述）。
- `GET /nodes`（flag 位掩码：bit2 可建隧道 / bit3 内地 / bit6 私有 / bit8 强制访问认证 / bit9 离线 / bit10 BETA）+ `GET /node/stats`（在线 / 负载 % / uptime）。
- `GET /tunnels`（含 `online` 状态）+ `POST /tunnels`（type=tcp、local_ip、local_port、node；`remote` 对 tcp 非必填，留空由平台分配）+ `POST /tunnel/delete`。
- `GET /system/clients` → `frpc.archs.<os>_<arch>`：官方下载 URL + **MD5** 哈希（32 位十六进制，官方文档 `md5sum` 同口径）+ size；下载域 `nya.globalslb.net`。
- frpc 启动语法官方确认：`frpc -f <访问密钥>:<隧道ID>`——配置自动拉取，无需手写配置文件。natfrp 文档提示"AI 应引导用户使用 GUI 启动器"，如实留痕取舍：MCHA 是自动化 Agent，GUI 启动器不可编程驱动，frpc CLI + API v4 正是官方为程序化管理设计的路径。

**工具七件套（D137，沿用"探测 / 执行拆分 + 确认门只落真实副作用"纪律）**

| 工具 | 权限 | 语义 |
| --- | --- | --- |
| `check_tunnel` | ReadOnly | `/user/info` + `/user/data_plans`：token 有效性、实名、等级、流量余额、隧道额度、冻结状态；frpc 二进制在位与版本检查。Agent 据此走引导或继续编排 |
| `ensure_frpc` | Network | `/system/clients` 按 OS + ARCH 选包 → 下载（进度条）→ **MD5 校验** → 落 `~/.mcha/runtime/frpc/<ver>/`（Unix chmod 755）；同版本幂等跳过。确认门只落在真实下载（D117 同理） |
| `select_tunnel_node` | ReadOnly | `/nodes` + `/node/stats` + `/user/info` → **硬过滤**（在线、可建隧道、非强制访问认证、非私有、用户组等级 ≥ 节点 VIP）→ **打分**（内地优先 → 负载低 → 非 BETA → uptime 稳定）→ top N（默认 5）附入选理由。打分为工具内部确定性算法（D138），Agent 只传偏好 |
| `create_tunnel` | Network（确认门） | 隧道名约定 **`mcha-mc<本地端口>`**（确定性命名）→ `GET /tunnels` 名称查重，命中且本地端口一致则**复用**（幂等自律）→ 否则 `POST /tunnels` 创建。返回 `{tunnel_id, node_host, remote_port}`；同名但端口不同 → 结构化报错交 Agent 问用户 |
| `start_tunnel` | Execute（确认门） | **先查重**（隧道已 `online` → 提示"疑似已在运行"，不重复拉起）；**独立终端窗口**拉起 `frpc -f <token>:<tunnel_id>`（D135，与 D134 服务器窗口模型同构，mcha 退出不断隧道；Windows `cmd /k` 保窗可读错误，Unix 探测终端模拟器 + 退出留窗）；轮询 API `online=true`（2s 间隔、默认 60s 超时、5s 一行进度，R4；Esc/Ctrl-C 仅打断等待）→ TCP connect `<node_host>:<remote_port>` + mc_ping（SLP，尽力验证——服务器未启动时 TCP 仍应通） |
| `tunnel_status` | ReadOnly | 诊断快照：API 隧道在线 + 本地端口监听 + 隧道端 mc_ping + 流量余额——US2"朋友连不上了"的排障入口 |
| `delete_tunnel` | Network（强确认门） | `POST /tunnel/delete`；换节点重建与演示清理用 |

**用户使用流程（三阶段）**

1. **阶段 0·一次性配置（`mcha setup` 可选步骤，D136）**：向导在 CurseForge Key 步骤后新增"樱花frp 访问密钥"——状态行（未配置提示"朋友跨网络联机需要；仅局域网可跳过"/ 已设置回车保留）→ 申请指引（**注册与登录两个入口** + 实名认证 + 密钥获取页，可点击链接、OSC 8 降级）→ Password 隐藏输入 → `.env` 写 `MCHA_NATFRP_TOKEN`（`merge_env_file` 合并写入）。
2. **阶段 1·会话内全自动编排（US1 网络分支增量）**：开服类任务开工前先感知工作区现状（D141：`list_dir` 工作区与 `~/.mcha/profiles/`、必要时 `mc_ping` 本地端口；发现已有部署先确认沿用 / 继续 / 新开，不得默认新开）→ `ask_user` 网络拓扑 → 无公网 IP → `check_tunnel`（未配置 token → 引导 `/token` 或 `mcha setup`，密钥不经过 LLM 上下文；未实名 → 阻塞指引）→ `ensure_frpc` → `select_tunnel_node` → `ask_user` 确认节点（默认推荐第一位，D138）→ `create_tunnel` → 提示起服（若未起）→ `start_tunnel` → **连接说明卡片**（地址 `node_host:remote_port`、朋友怎么连、流量余额、限速、注意事项——Markdown 静态块）→ `save_profile` 落 `network=Tunnel{provider, endpoint}`。
3. **阶段 2·同一会话排障（US2）**："朋友连不上了" → `tunnel_status` → 按快照定位（隧道不在线 → frpc 窗口被关，重新 `start_tunnel`；本地端口不通 → 服务器窗口问题走诊断；TCP 通 SLP 不通 → 服务器未就绪）→ 修复动作。

**生命周期与失败模式**：frpc 与服务器窗口同为独立进程、与 mcha 生命周期解耦（停机顺序由用户决定）；`online=true` 但非本机拉起（用户在别处用启动器连了同一隧道）时如实提示重复。失败映射：API 401 → token 无效；403 → 实名 / 等级 / 冻结问题（附引导）；节点满载 → 换节点重试；frpc 窗口秒退（如杀软拦截）→ 结构化报错 + Agent 给手动命令逃生舱。**R6 边界**：樱花frp 非 AI API，不计 token / 费用；隧道流量为 natfrp 自有计量，MCHA 经 API 取数展示（连接卡片与 `tunnel_status`）。安全：token 仅存 `.env`（0600）并全链路打码（确认门展示 `-f ***:<id>`、`/token` 不回显、导出自动遮蔽）；建删隧道幂等自律；API 定义 AGPL-3.0 仅调用不复制定义。

### 8.9 故障诊断（定制 4，选做）

诊断是会话中的 Agent 行为（无独立流水线）：`read_server_log(path, tail)` 读日志 → `analyze_log` 内部跑错误模式库（正则 / 关键词 → 候选根因 + 修复动作 + 置信度）→ LLM 结合会话上下文定夺 → 修复经工具执行（危险动作确认）→ 验证。模式库新增 = 加一行 TOML；`1.21.11 > 26.2` 类版本比较错误本身入库（呼应基线实验）。

### 8.10 服务器设施实现细节

本节是 FR-09 / FR-10 / FR-11 / FR-14 与定制 1/2 设施部分的技术细则。

**版本表示与比较**

- MC 版本为纯数字点分串（如 `1.21.1`）；自写比较器按数值段逐段比较（缺段补 0），`semver` 仅作输入合法性闸——`26.2` 等非法输入拒绝并给就近建议（定制 2 第一道闸）。
- L1 知识文件（随包 TOML，中文注释，带来源与采集日期）：
  - `java_compat.toml`：`[[ranges]] min_mc / max_mc / java_major / note` 区间表（实测 2026-09-02 口径：26.x 年份版本线 → 25，1.20.5–1.x → 21，1.18–1.20.4 → 17，1.17 → 16/17，≤1.16 → 8+）；`check_version_compat` 的静态后端。
  - `server_software.toml`：软件 × 支持版本范围 × 下载渠道 × 哈希可信度说明（getbukkit / Fabric 无官方整包哈希如实标注）。

**下载渠道与镜像**

| 渠道 | 端点 | 哈希 | 默认镜像 |
| --- | --- | --- | --- |
| Mojang piston-meta | version_manifest_v2 → 版本 JSON → downloads.server | 官方 sha1 | BMCLAPI（下载 URL 域名重写 piston-meta / piston-data → bmclapi2.bangbang93.com） |
| PaperMC Fill v3 | `fill.papermc.io/v3/projects/paper/versions/{v}/builds/latest`（旧 v2 已 410 下线） | 官方 sha256 | 直连（国内可达） |
| Fabric meta | `/v2/versions/loader/{game}/{loader}/{installer}/server/jar` | 无官方整包哈希 → 落地计算 sha256 留痕 | 直连 |
| Spigot | getbukkit 下载页抓取（令牌 → 302 → CDN） | 无 → 同上 + 轨迹明示第三方 | 直连 |
| Adoptium v3 | 元数据走官方 API | 官方 sha256 | 二进制走清华 TUNA |

- 镜像配置：`[network] mojang_mirror = bmclapi | off | 自定义URL`；实际使用的域记入轨迹；`adoptium_mirror = tuna | off`。实测注意：清华 TUNA 拒绝空 User-Agent 请求（HTTP 403），MCHA 全部 HTTP 客户端统一携带 `mcha/<版本>` UA。
- 统一产物 `DownloadArtifact { url, hash, hash_type, trust_note }`；哈希不符即失败回环（定制 2 防线）。

**服务器文件生成（FR-11）**

- 部署目录默认 `<工作区>/server/`（工具参数可指，受路径收敛约束）。
- 产物四件：`eula.txt`（`eula_accepted=false` 时拒绝生成并提示 Agent 先经 ask_user 确认）、`server.properties`（内置完整默认键表 + Agent 覆盖项渲染，端口默认 25565）、`whitelist.json`（离线 UUID：Java `nameUUIDFromBytes(("OfflinePlayer:"+name) UTF_8)` 口径 = MD5 摘要后置 RFC 4122 version=3、variant=IETF；md-5 crate；缺 uuid 字段会被服务端静默丢弃——实测教训留痕）、**start.bat + start.sh 双脚本**（java 绝对路径 + `-Xmx` + `-jar server.jar nogui`；bat 含 `pause`，sh 含 `cd "$(dirname "$0")"`）。
- 下载的服务端 jar 统一改名 `server.jar`，原始文件名记入轨迹（交付语义配套约定，屏蔽渠道 jar 名差异）。

**进程生命周期（FR-14，D134 独立窗口模型——修订 D118 托管模型）**

- `start_server`：在**独立终端窗口**运行交付的 start 脚本——Windows 以 `CREATE_NEW_CONSOLE` 新开 cmd 跑 `start.bat`；Unix 依次探测 x-terminal-emulator / gnome-terminal / konsole / xfce4-terminal / xterm 跑 `start.sh`（无图形环境结构化报错）。与用户手动双击完全一致，"演示期管理"与"长期运行"双轨合一（D118 的双轨矛盾就此消解）；java 参数已固化在脚本内，工具参数仅 `server_dir` + `ready_timeout_secs`。
- **就绪判定**：轮询 `logs/latest.log`（0.5s 间隔）——mtime 晚于启动前快照（防上一轮旧日志中的 `Done (` 假就绪，实测教训）且匹配 `Done (x.xxx)!`；等待期每 5 秒发一行进度（R4）；Esc / Ctrl-C 仅打断等待，服务器窗口不受影响；超时区分两种失败："有新日志但无就绪特征"（附日志尾部）与"日志毫无新写入"（启动即失败嫌疑，如脚本中 Java 路径失效）。
- **重复开服守卫**：server-port 已有监听（TCP connect 探测）即拒绝——独立窗口模型下 mcha 不持进程句柄，以端口监听推断判重。
- **`stop_server` 移除**：停服交用户在服务器窗口 Ctrl+C（或输入 stop），世界自动保存；mcha 退出不再连带停服（进程与 mcha 生命周期解耦，原 D118 Drop 守卫随之作废）；就绪交付话术必须说明停服方法（用户实测反馈）。
- `server_status`：轻量只读推断——端口探测 + `logs/latest.log` 尾部。

**连通验证**

- `probe_port`（ReadOnly）：`mode=bind`（部署前端口占用检测）/ `mode=connect`（监听验证）。
- `mc_ping`（ReadOnly）：纯 Rust 手写 SLP（1.7+ 现代协议）——VarInt 长度帧 → handshake 包（next_state=1）→ status request → 解析 status 帧（packet_id 校验 + **VarInt 字符串长度前缀** + JSON：MC 协议所有 String 字段均带 VarInt 前缀，漏读曾致对真实服务器必然报"非法 JSON"——实测 vanilla 1.21.1 帧载荷 `81 01 7b …` 即 VarInt(129)+JSON，已修）；解析 version.name / players / MOTD；超时 5s；仅连本机 / 用户明确目标。

**部署前确定性校验（check_plan）**

checklist 逐项 pass/fail + 结构化缺项清单：① software × mc_version 组合可用（知识库 + 上游）；② Java 大版本匹配（java_compat 区间）；③ online-mode 与账号类型一致；④ EULA 已确认；⑤ 内存范围 `512 ≤ Xmx ≤ 总内存 − 1024`；⑥ 端口合法（1025–65535）且未被占用；⑦ 离线模式 ↔ 白名单配套——默认要求启用，用户明确拒绝时 Agent 传 `whitelist_disabled_ack=true` 放行并留痕（确定性闸门与用户意愿两全）；⑧ server_dir 冲突（已存在非空时警示）；⑨ mod 兼容性——software=fabric 且带 mod 意图清单时，逐项重验 version 的 `game_versions` / `loaders` 覆盖目标 mc_version 与 loader。

### 8.11 领域检索通道（wiki_search / wiki_page）

- **工具对（ReadOnly）**：`wiki_search(source, query, limit)` → [{标题, 纯文本摘要, URL}]；`wiki_page(source, title, max_chars)` → 页面正文纯文本（截断；超限走大输出落盘规则）。
- **来源注册**（config `[retrieval]`）：`mcwiki = https://wiki.biligame.com/mc/api.php`；`mcmod`（随 mod 场景接入）。来源域属下载白名单同级的固定清单。
- **mcwiki 后端**：标准 MediaWiki API——搜索 `action=query&list=search`；正文走 `action=parse` 取 HTML 后去标签（实测 B站 Wiki 未装 TextExtracts，`prop=extracts` 不可用）。
- **mcmod 后端**：无官方 API；`search.mcmod.cn/s?key=` 服务端渲染 HTML 解析（`scraper` 选择器提取：标题 / 英文别名 / 简介 / `class/NNN.html` 地址）；主页仅做**摘要级**正文提取（复用本节 `strip_html` 兜底）——版本适配表**不解析**（版本事实以上游 API 为权威，适配表留作后置扩展）；URL 前缀强约束（仅 `search.mcmod.cn/s` 与 `www.mcmod.cn/class|item/<id>.html` 形态）；请求间约 800ms 间隔自律、解析失败结构化报错不掩盖。
- **事实优先级红线（扩展表述）**：版本存在性 / 下载 URL / 哈希以上游 API 为权威；Wiki / 百科结果只作背景知识、交叉验证与中文语境补充——两条通道各自的结构化返回都进轨迹，可回放（R5）。

### 8.12 mod 场景实现细节

本节是 FR-12 / FR-13 / FR-16 与 FR-20 mcmod 后端的技术细则。范围裁定：mod 自动化落地 **Fabric**（`fetch_server_jar` 已支持），Forge 维持指导模式，NeoForge 不在本期；mod 数据源为 **Modrinth + CurseForge 双源**（Modrinth 为主，CurseForge 为扩展通道——官方 API 与国内镜像双基址，见下）。FR-12（清单安装）与 FR-13（自然语言推荐）共用同一套工具，差别只在入口：前者用户给清单直接 resolve，后者 Agent 先检索推荐、经确认再 resolve。

**Modrinth 上游客户端（`knowledge/upstream/modrinth.rs`）**

- API v2，免 key；遵守社区限额（请求间最小间隔自律）；统一 UA `mcha/<版本>`。
- 端点：`GET /v2/search`（facets：`project_type:mod`，可选 `versions:<mc>` / `categories:<loader>`）；`GET /v2/project/{slug}/version`（`game_versions` / `loaders` 查询参数过滤）；`GET /v2/versions?ids=[..]` 批量重取（安装期权威数据源）。
- 版本文件结构：`url` / `filename` / `hashes.sha1` / `hashes.sha512` / `size`；依赖经 `dependencies`（`project_id` + `dependency_type`，仅 `required` 入闭包）。

**CurseForge 通道（官方 / 国内镜像双基址，免 key 可用）**

- **官方 API v1**（`api.curseforge.com`，请求头 `x-api-key`）——免费但需用户在 CurseForge 开发者门户注册应用获取 key；key 为**每用户一次性可选配置**：存 `.env`（`MCHA_CURSEFORGE_KEY`，永不入 config.toml 与仓库），环境装配时读取一次、随工具上下文下发。自建 key 池转发服务与网页抓取**裁定排除**（自建转发：违反条款、配额单桶、单点运维、key 实质公开；抓取：Cloudflare 反爬、页面无权威哈希、结构脆弱——无官方 API 才允许抓取，如 getbukkit→Spigot 的逃生舱先例）。
- **国内镜像基址（无 key 默认通道）**：`https://mod.mcimirror.top/curseforge`——开源公益项目 mcmod-info-mirror，国内启动器生态共用；与官方 CF API v1 **同构**（检索 / 详情 / 文件列表 / 批量重取端点与 schema 一致，免 key，实测 sha1 与实文件核验一致）。**通道选择**：已配 key → 官方；未配 key → 自动走镜像，CF 独占 mod 开箱可用（官方 API 与 forgecdn 下载域在国内网络可达，镜像解决的是"申请 key 依赖的门户不可达"与"key 门槛"）。使用既有公益镜像与自建 key 池转发性质不同：不持有他人 key、不共享配额桶、数据为官方 API 透传。轨迹中如实标注"经社区镜像"。
- **端点兼容**：检索统一用 `GET /v1/mods/search`（官方文档原生支持；镜像仅支持 GET，POST 返回 404）——官方 / 镜像同一套代码通吃。其余端点：`GET /v1/mods/{modId}`（详情）；`GET /v1/mods/{modId}/files`（按 `gameVersion` / `modLoaderType` 过滤，镜像过滤参数实测生效）；`POST /v1/mods/files`（按 fileId 批量重取，安装期权威数据源）——实测镜像全部兼容。
- 文件结构：`fileName` / `downloadUrl` / `fileLength` / `hashes`（**sha1 单哈希**，Modrinth 双哈希；输出轨迹如实标注哈希强度差异）；`dependencies`（`relationType=required` 入闭包）；`gameVersions`（含 MC 版本与加载器名，兼容复核用）。`downloadUrl` 为 null（项目未开放第三方分发）时结构化报错并指向手动下载页，不假装能装。
- **限额自律**：官方 key 级配额较 Modrinth 紧——请求间最小间隔 + 批量端点优先；镜像为公益服务同样节流（与 Modrinth 同一社区礼仪）。
- 下载域白名单：`edge.forgecdn.net` / `mediafilez.forgecdn.net` / `media.forgecdn.net`（§12；API 下发 `downloadUrl` 为 edge 域，重定向落到 mediafilez 域，两者同入白名单）。

**双源解析流程（Modrinth 优先，自动降级）**

1. 别名表命中且标注 `source="curseforge"`（如暮色森林）→ 直接走 CF 检索（官方或镜像基址，key 有无自动选择）；
2. 其余先走 Modrinth（别名直查 / 精确匹配）；**零命中** → 自动转 CF 检索（slug / 名称精确匹配，`gameVersion` + Fabric 过滤）；
3. 依赖闭包跨源：CF 文件的 `required` 依赖按 modId 展开（回查项目详情取 slug），与 Modrinth 侧同一套 BFS（去重 + 环检测 + 深度上限）；同一 mod 两源都收录时 **Modrinth 优先**（免 key、双哈希、社区限额宽松）。

**意图清单（manifest）扩展**

- 条目增加 `source` 字段（`"modrinth" | "curseforge"`）+ 源专属 id：Modrinth 为 `version_id`，CurseForge 为 `mod_id + file_id`；`file_name` 仅展示。
- `install_mods` 按源分发批量重取（Modrinth `versions?ids` / CF `mods/files`），逐项执行源专属的域白名单与哈希校验；冲突 / 幂等 / 原子落盘策略两源一致。

**mod 工具三段式**

| 工具 | 权限 | 参数 | 语义 |
| --- | --- | --- | --- |
| `search_mods` | ReadOnly | `query`（中英皆可）、`mc_version?`、`loader?`、`limit?`（默认 5） | 别名表命中 → 直接查 slug（免检索歧义）；否则 `/v2/search` facets 检索；返回 slug / 标题 / 中文别名命中标注 / 简介 / 下载量 / 版本覆盖 |
| `resolve_mod` | ReadOnly | `mods`（字符串数组，中英/slug 混排）、`mc_version`、`loader` | 逐项：别名解析 → Modrinth 精确匹配（**唯一命中才通过**；多命中返回候选交 Agent 经 ask_user 澄清）→ 零命中自动转 CF 检索（官方或镜像基址）→ 版本匹配 → BFS 依赖闭包（跨源）→ 输出**意图清单**（带 source 标注）+ 每项入选理由（轨迹可回放，R5） |
| `install_mods` | Network | `server_dir`（路径收敛校验）、`manifest`（resolve_mod 输出的意图清单） | 按源批量重查权威数据（Modrinth `versions?ids` / CF `mods/files`）→ 逐项下载 → 源专属哈希校验（Modrinth sha1+sha512 双校验；CF sha1 校验）→ 原子落 `<server_dir>/mods/`；进度逐文件推送、取消全程生效 |

三段式理由：检索（可能多轮试探）与解析（确定性）与执行（唯一落盘点）分离，确认门只落在真实安装上（与 Java 供给的探测 / 安装拆分同理）；resolve 无副作用可放心重试，install 失败即结构化回环（NFR-3）。

**安装语义**

- **意图清单与权威数据分离**：manifest 只承载意图（source + slug + 源专属 id + file_name 展示名）；URL 与哈希在安装时按源批量端点**实时重取**——LLM 转手抄错 / 篡改均不影响正确性，"下载 URL / 哈希以上游 API 为权威"红线贯彻到安装期。
- 下载白名单：按源分发——Modrinth 仅 `cdn.modrinth.com`；CurseForge 仅 `edge.forgecdn.net` / `mediafilez.forgecdn.net` / `media.forgecdn.net`（对 API 下发的 `url` 域强校验；测试 / 自建代理基址同域放行）。
- 原子写：临时文件下载校验通过后重命名落位（tempfile 模式，同服务端 jar 下载惯例）。
- 冲突策略：目标同名文件**哈希一致则跳过**（安装幂等，中断重跑安全）；**不一致则报错列出冲突**——不静默覆盖用户手动安装，交 Agent 问用户。
- 下载失败 / 哈希不符 → 结构化错误回传 Agent（哪个文件、哪步、建议动作），失败回环。

**依赖闭包算法**

- 从每个 mod 的兼容版本出发，`dependencies` 中 `dependency_type=required` 的 project 入闭包（optional / incompatible / embedded 不入）；BFS 逐层展开。
- project_id 级去重 + 环检测（访问标记）+ 深度上限（默认 10，防清单爆炸）；每项记录 `dependency_of` 链与入选理由。
- 版本选取：该 project 在 `game_versions` 含目标 MC 版本、`loaders` 含目标加载器的版本中取最新（Modrinth 返回序即时间序）；闭包内依赖自身的目标版本以其声明的兼容范围为准。

**中文别名表（`assets/knowledge/mod_aliases.toml`）**

- L1 静态知识：`[[mods]]` 含 `slug` / `name` / `aliases[]` 与可选 `source`（`"modrinth"` 默认；`"curseforge"` 标注 CurseForge 独占项目，如"暮色森林"→ the-twilight-forest 直达 CF 通道）；初版收录 **17 个高频 Modrinth mod + CurseForge 独占代表**（JEI、Create、Sodium、Lithium、暮色森林等），带来源与采集日期，可独立更新。另设 `[[unlisted]]` **无收录名单**：两源都不存在的常见需求（OptiFine 为自有分发，2026-09 实测）→ 零命中时给出如实的结构化解释与替代建议，不假装能装。
- 双通道：别名命中直接查对应源的项目；未命中走 Modrinth 检索，零命中按上文流程降级 CurseForge。零命中且两源皆无 → 如实报错（如 OptiFine，Agent 转人工指引）。
- 红线不变：别名表只承载"叫法 → 项目"映射，**不承载版本兼容事实**（§8.5）。

**FR-13 自然语言推荐（不加专用工具）**

- Agent 规程（写入 server-setup Skill）：问清玩法偏好与内容风格 → 翻译为多组英文搜索词调 `search_mods` → 汇总候选（推荐理由 / 下载量 / 依赖情况）→ `ask_user` 勾选确认 → 走 resolve → install。LLM 判断力 + 用户确认，零新增代码。

**Profile 存取（FR-16 / US3）**

- `save_profile`（Write，确认门）：Agent 填 Profile 结构（§8.6 定稿字段）→ 工具 schemars 校验 + server_dir 存在性核对 → JSON 原子落 `~/.mcha/profiles/<profile_id>.json`（`profile_id = YYYYMMDD-HHMMSS`）→ 返回路径与摘要。
- `load_profile`（ReadOnly）：`profile_id` 或 `latest` → 全文读回上下文（含 mods 明细与 artifacts 清单），Agent 对照现状补差复用。
- CLI：`mcha profiles list` / `profiles show <id>` 薄壳查看（打码规则同会话导出，NFR-2）。

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
| 异步运行时        | tokio                        | async-std    | 事实标准；取消语义与进程管理（R4）依赖其生态           |
| HTTP 客户端       | reqwest（rustls-tls）        | ureq         | 异步流式（SSE）必需；rustls 免 Windows OpenSSL 负担    |
| SSE 解析          | eventsource-stream           | 手写行解析   | 标准协议解析自写易错                                   |
| Stream 组合子     | futures-util                 | —            | 驱动 reqwest / eventsource-stream 的 Stream            |
| trait 异步方法    | async-trait                  | 手写装箱     | Tool / Interaction / LlmClient 需要 dyn 对象安全       |
| 序列化            | serde + serde_json + toml    | —            | 事实标准；JSON 是 R5 落盘与工具参数统一格式            |
| Schema 派生与校验 | schemars + jsonschema        | 手写校验     | 工具参数 Schema 从类型派生，响应按同一 Schema 校验     |
| 错误处理          | thiserror + anyhow           | 手写         | 模块内显式错误枚举，main 层聚合                        |
| CLI 框架          | clap（derive）               | argh         | 子命令与帮助完备（R2）                                 |
| 交互问答 | dialoguer | inquire | ask_user 的选项 / 输入 / 确认控件 |
| 终端控制 | crossterm | — | 颜色、样式与光标控制；会话界面渲染基底 |
| 进度条            | indicatif                    | 手写         | 多进度条与速率显示（R4）                               |
| 取消令牌          | 手写（AtomicBool + Notify）  | tokio-util   | 语义简单（置位 + 唤醒等待方），手写约 40 行且答辩可解释 |
| 版本解析          | semver                       | 手写         | 拒绝非语义化输入是定制 2 第一道闸                      |
| 哈希校验          | sha1 + sha2（RustCrypto）    | —            | Mojang sha1 / Modrinth sha1+sha512 / Adoptium sha256   |
| 压缩解压          | zip                          | 手写         | Java 供给解压，纯 Rust                                 |
| 环境探测          | sysinfo                      | 各平台命令   | sys_info 工具；跨平台、免解析 shell 输出               |
| 哈希（MD5）       | md-5（RustCrypto）           | —            | 白名单离线 UUID（nameUUIDFromBytes v3 口径）           |
| 哈希（SHA1）      | sha1（RustCrypto）           | —            | Mojang 服务端官方 sha1 校验                            |
| tar 解包 / gzip   | tar + flate2                 | 仅 zip       | ensure_java 的 Unix 包（tar.gz）解压；Windows 用 zip   |
| HTML 解析         | scraper（mod 场景引入）     | 正则         | MC百科后端 HTML 解析（§8.11）                          |
| 正则              | regex                        | —            | 错误模式库匹配（§8.9）                                 |
| 日志与观测        | tracing + tracing-subscriber | log          | 结构化分模块日志                                       |
| 时间              | chrono                       | time         | 落盘时间戳统一 RFC3339                                 |
| 临时文件          | tempfile                     | 手写         | 原子写与下载临时目录                                   |
| LLM SDK           | **不引入**（自研薄客户端）   | async-openai | 核心业务即 API 调用编排（R1 考察点），答辩可解释 |

## 11. 数据源与外部依赖

| 数据                     | 来源                                                                                       | 用途                         | 备注                                                                          |
| ------------------------ | ------------------------------------------------------------------------------------------ | ---------------------------- | ----------------------------------------------------------------------------- |
| MC 版本清单 / 原版服务端 | Mojang piston-meta API                                                                     | 版本校验、原版服下载         | 国内默认 BMCLAPI 镜像（下行），可配直连                               |
| Mojang 资源镜像          | BMCLAPI（`bmclapi2.bangbang93.com`）                                                       | 官方域不可达时的默认下载通道 | 域名重写实现，`[network]` 可关                                        |
| MC 中文 Wiki             | B站 MC Wiki MediaWiki API（`wiki.biligame.com/mc/api.php`）                                | `wiki_search` / `wiki_page`（§8.11） | 实测 `list=search` 可用；背景知识与交叉验证通道                |
| MC百科                   | `search.mcmod.cn`（服务端渲染 HTML）                                                       | mcmod 检索后端               | 无官方 API，HTML 解析自担                                       |
| Paper 构建与下载         | PaperMC Fill API v3                                                                        | Paper 服下载                 | 同上                                                                          |
| Fabric 版本与安装器      | Fabric meta / maven                                                                        | Fabric 服搭建                | 同上                                                                          |
| Spigot jar | getbukkit 下载页抓取解析（下载页令牌 → 302 → CDN 直链） | Spigot 服下载 | 无官方哈希，轨迹明示第三方来源 |
| mod 元数据               | Modrinth API v2                                                                            | 检索、版本匹配、依赖树、下载 | 免 key，双哈希校验（sha1+sha512）                                              |
| CurseForge mod 元数据与下载 | CurseForge API v1：官方（`api.curseforge.com`，需 key）+ 国内镜像（`mod.mcimirror.top/curseforge`，免 key） | Modrinth 未收录 mod 的检索与下载（暮色森林等） | 已配 key 走官方，未配置自动走镜像（开源公益项目 mcmod-info-mirror，与官方同构、实测 sha1 一致）；自建 key 池转发与网页抓取裁定排除（§8.12） |
| JRE 二进制               | Adoptium v3 API（镜像：清华 TUNA）                                                         | Java 自动供给（§8.7）        | sha256 校验                                                                   |
| 樱花frp API / frpc       | `api.natfrp.com/v4`（OpenAPI 规范 `api.natfrp.com/docs`）；frpc 经官方分发（MD5 哈希，`nya.globalslb.net`） | 穿透编排（§8.8）             | 一次性人工：注册 / 登录 + 实名 + token；API 定义 AGPL-3.0，引用注明                  |
| 网络搜索 | 可配置后端：默认无；可选 DuckDuckGo HTML 抓取（免 key）或 Serper 等（配 key） | `web_search` 工具 | 国内可达性与反爬脆弱性诚实标注；领域事实主通道是知识库 + 上游 API，搜索是兜底 |
| LLM                      | 任意 OpenAI 兼容 Chat API（用户配置）                                                      | Agent 大脑                   | R3；无 usage 时记次数（课程 Q9）                                              |

## 12. 错误处理与安全设计

- 错误分层：模块级 thiserror 枚举 → `AppError`；工具错误结构化回传 Agent（NFR-3）；用户可见错误必须附"下一步怎么办"。
- **确认门（FR-04）**：按工具 `Permission` 分级——`ReadOnly` 免确认；`Write`（写文件）/ `Execute`（跑命令、起停进程）/ `Network`（下载大文件）默认确认，显示关键内容（命令行、目标路径、写入摘要）后**三选一：y 本次允许 / a 本会话允许此工具 / n 拒绝**（拒绝以结构化错误回传 Agent，由其调整方案）；`[safety] confirm_level = paranoid | standard | auto` 可调（默认 standard；auto 全部免确认，限演示 / CI 并留痕）。
- **路径收敛**：文件类工具的目标路径必须解析在工作区或数据目录内，越界拒绝（结构化错误回传 Agent）。
- **下载安全**：HTTPS + 官方域白名单（含镜像域）+ 哈希校验三重；镜像仅替换白名单内域名。白名单域：`piston-meta.mojang.com` / `piston-data.mojang.com`、`bmclapi2.bangbang93.com`、`api.papermc.io`、`meta.fabricmc.net`、`api.adoptium.net`、`mirrors.tuna.tsinghua.edu.cn`、`wiki.biligame.com`、`api.modrinth.com` / `cdn.modrinth.com`（mod 下载仅此域）、`search.mcmod.cn` / `www.mcmod.cn`、`edge.forgecdn.net` / `mediafilez.forgecdn.net` / `media.forgecdn.net`（CurseForge 下载，§8.12）、`mod.mcimirror.top`（CurseForge API 镜像，仅元数据不下文件，§8.12）、`api.natfrp.com`（樱花frp API）与 `nya.globalslb.net`（frpc 下载 CDN，§8.8）；getbukkit 下载域无官方哈希，轨迹明示第三方来源。
- **进程安全**：子进程统一托管（进程组 / Job Object），Drop 守卫保证取消 / 退出时杀干净，不留孤儿；例外：`start_server` 弹窗启动的服务器进程与 mcha 生命周期**有意解耦**（D134，停服由用户在服务器窗口操作）。
- 密钥安全：`.env` / `config.toml` 在 `.gitignore`；导出打码（NFR-2）——已知敏感串（LLM Key / CurseForge Key / 樱花frp token）精确替换 + 密钥样式与公网 IP 规则；确认门展示樱花frp 命令行时 token 呈 `-f ***:<隧道ID>` 形态。
- 离线模式风险：白名单建议与后果提示写入决策指南（定制 1），Agent 必须向用户明示。

## 13. 测试与验收策略

- 单元：上下文裁剪策略、确认门与路径收敛（含越界负路径）、版本校验管线（含"26.2"拒绝）、MC 版本比较器与 java_compat 区间匹配、别名表检索、Modrinth 客户端（fixture mock）、CurseForge 客户端（GET 检索 fixture mock、官方 / 镜像双基址）与双源降级、依赖闭包黄金用例（含环检测 / 深度上限）、mcmod 搜索页解析（fixture HTML）、Profile save/load 往返、离线 UUID 黄金值（与 Java nameUUIDFromBytes 语义对齐）、server.properties / 启动脚本渲染快照、check_plan 全分支、SLP 包字节构造（含 status 帧 VarInt 字符串前缀黄金向量与真实抓包形状）、独立窗口 start_server 就绪轮询（无头启动器注入假脚本：旧日志假就绪回归、端口占用守卫、"无新写入"失败区分、停服话术存在性）、镜像 URL 重写、JVM 参数推导、Java 供给的版本解析与路径规则、节点打分、wiki 客户端（fixture mock MediaWiki 响应）、natfrp 客户端（fixture mock：/user/info 正常与冻结双形态、/nodes flag 位运算、隧道查重复用、frpc 架构键映射）、隧道名约定与 `-f` 命令打码、樱花frp token 的 setup 步骤与 `/token` 命令（合并写 .env 回归）。
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
├── mc/                # 开服领域工具后端：sys_info / java / server_jar / files / process /
│                      #   probe / plan / retrieval / mods / profile（已实现）；tunnel / diag（后续）
├── knowledge/         # 知识库加载、上游 API 客户端、版本校验管线
├── store/             # 会话 / 档案 / 用量持久化
├── config/            # AppConfig、价格表、确认策略
├── cancel.rs          # 协作式取消令牌（AtomicBool + Notify，R4 打断）
├── events.rs          # 事件总线（进度 / 用量 / 轨迹）
└── assets/            # prompts/ skills/ knowledge/*.toml
```

**实现顺序（先框架、后场景）**：

- **Agent 框架（已完成）**：config → llm → agent（Loop + 确认门 + 上下文管理）→ tools/general → cli（REPL + 渲染 + 打断）→ store → events，含**框架级系统提示词**（§8.5 L4 基础段，随 assets/prompts 交付）。出口标准：纯通用任务验收——用自然语言让 Agent 在受控工作区完成一件多步实事（如"抓取某页面存为文件并统计行数"），全程工具调用留痕可查、可打断、有费用统计。此时它已是一个真正的（小）通用 Agent。
- **开服场景包（分步实施）**：
  - **服务器设施（已完成）**：知识库（MC 版本比较器 + Java 兼容区间表 + 兼容性查证工具 + Mojang 客户端）→ 环境探测与 Java 探测 → Java 自动供给 → 服务端获取（原版 → Paper → Fabric → Spigot 逐渠道）→ 配置文件生成 → 生命周期工具三件套（D134 起收敛为独立窗口模型二件套） → 端口探测 / SLP ping / 部署前校验 → 检索通道（MC Wiki 后端 + `[retrieval]` 来源注册）→ server-setup 决策指南 + 场景提示词 + 框架小改（场景段注入、技能内置根）+ 假 LLM 客户端端到端集成测试。出口标准：**US1 精简版**——版本 + 账号情况 + 端类型一句话 → 本机 `127.0.0.1:25565` 可登录、全程留痕；Forge 为指导模式；Profile（FR-16）后置。
  - **mod 场景（已完成，细则 §8.12）**：Modrinth 上游客户端（检索 / 项目版本 / 批量重取端点；fixture 单测）→ 中文别名表 + `search_mods` 检索工具 → `resolve_mod` 解析工具（别名解析 → 版本匹配 → 依赖闭包 → 意图清单；黄金用例快照）→ `install_mods` 安装工具（安装期重查 API → 双哈希校验 → 原子落盘 + 进度；冲突分支单测）→ mcmod 检索后端（HTML 解析 + URL 前缀约束）→ Profile 存取（save/load 工具 + `mcha profiles` 子命令）→ 部署前校验 mod 项 + server-setup 决策指南 mod 分支（含 FR-13 推荐规程）+ 提示词 / 工具动词表 + 假 LLM 客户端集成测试。出口标准：**US1 完整版（mod 增量）**——"Fabric 1.21.x，装 JEI 和 Sodium"一句话 → 依赖解析 + 校验下载落 `mods/` + 起服日志确认 mod 载入（`server_status` 可见）+ Profile 保存 / 读回可复现，全程留痕。
  - **mod 双源扩展：CurseForge 通道（当前步，细则 §8.12）**：API 客户端（GET 检索 / 项目文件 / 批量重取；官方 + 国内镜像双基址，key 有无自动选择）→ `resolve_mod` 双源降级（Modrinth 零命中转 CF；别名表 `source` 标注直达）→ `install_mods` 按源分发（CF 下载域白名单 + sha1 校验）→ check_plan 兼容复核扩展到 CF 清单 → setup 向导与启动自检按双通道话术更新 → mock 单测 + 端到端集成测试。出口标准：**暮色森林场景零配置闭环**——"Fabric 1.21.x，装暮色森林和 JEI"在**未配置任何 CurseForge key** 时不须人工介入即完成解析安装（经国内镜像通道）；已配 key 走官方 API。
  - **内网穿透（当前步，细则 §8.8，D135–D139）**：natfrp API v4 客户端（user / nodes / node stats / tunnels / system clients；fixture mock 单测）→ 节点打分纯函数（flag 位过滤 + 排序黄金用例）→ `check_tunnel` / `ensure_frpc`（MD5 校验 + 受管目录幂等）→ `create_tunnel`（查重复用）/ `start_tunnel`（独立窗口 + online 轮询 + 端到端验证）/ `tunnel_status` / `delete_tunnel` → setup 向导樱花frp 步骤（注册 / 登录双入口指引）+ `/token` 命令 + 打码扩展 → server-setup 技能网络分支 + 提示词动词表 + FakeLlm 端到端集成测试（mock natfrp + mock 下载 + 假 SLP 服务器）。出口标准：**定制 3 验收**——无公网 IP 环境，人工步骤仅剩注册 / 实名 / 粘贴 token，Agent 在会话中自主完成 frpc 下载 → 选节点 → 建隧道 → 拉起 → TCP + MC SLP 端到端验证 → 连接卡片，外部客户端可连入，同一会话可排障。

每模块完成后独立可编译、可运行（先跑通再美化）。
