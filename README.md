# Minecraft Host Agent（MCHA）

**说一句话，就能把服务器开起来。**

MCHA 是一个用 Rust 从零构建的 MC 开服助手：一个真正场景定制化的 AI Agent。你说中文描述需求，它在本机完成方案推导、服务端部署、mod 安装、启动验证与内网穿透，每一步工具调用都实时展示、可打断、留痕可查。

- **为什么需要它**：Minecraft Java 版好友联机开服，知识分散（版本/Java/服务端怎么选）、决策分支多（正版离线混合怎么配）、内网穿透劝退（frp 配置能吓退不少玩家）。
- **它的实现方式**：不是一个"聊天机器人 + 脚本"，而是一个带 35 个领域/通用工具的 AI Agent，由大语言模型在 Agent Loop 里自主决策调用哪个工具，Rust 后端负责执行与安全边界。版本事实一律实时查证，不凭记忆回答。

> 想了解设计与实现，请见文末[设计与文档](#设计与文档)。

## 它能做什么

**一句话开服**：对它说"我们 5 个人，2 个正版 3 个离线，Fabric 1.21.1，装暮色森林和 JEI"，它会——

1. 查证版本兼容性（需要哪个 Java、哪个加载器版本）；
2. 自动下载安装受管 JRE（不动系统 Java）；
3. 下载服务端（原版 Vanilla / Paper / Spigot / Fabric 四渠道，哈希校验）；
4. 生成全部配置（EULA、server.properties、离线白名单 UUID、start.bat 启动脚本）；
5. 在**独立终端窗口**启动服务器，轮询日志等到 `Done`，再用 MC 协议 ping 验证可登录；
6. 交付连接说明卡片（地址、白名单、手动启动方式）。

**mod 安装**：直接说 mod 的名称（内置别名表，"暮色森林"、"JEI" 都认识）。自动解析依赖闭包、按 MC 版本匹配文件、双源下载（Modrinth + CurseForge）。未配置任何 Key 时 CurseForge 自动走国内镜像，暮色森林这类 CF 独占 mod 零配置可装。

**内网穿透（樱花frp）**：没有公网 IP 也能让朋友直连。一次性人工步骤只剩注册 / 登录、实名、粘贴访问密钥（`/token` 或 `mcha setup`，有分步可点击指引，密钥不经过模型），其余全自动——下载校验官方 frpc、按你的账号情况推荐节点（确认后生效）、创建隧道（同名自动复用，不重复建）、独立窗口启动并端到端验证，最后交付**连接说明卡片**：朋友的连接地址、流量余额、注意事项。之后"朋友连不上了"这类排障它也会接手：定位是 frpc 掉线、本地端口不通还是服务器未就绪，并给出修复动作。隧道流量由樱花frp 平台计量，MCHA 只展示不计费。

**通用 Agent 能力**：文件读写编辑、执行命令、抓取网页、多步任务编排，它首先是一个 Agent 基座，开服是装在它身上的场景包（领域工具 + 决策指南 Skill + 知识库）。

**先看后动**：工作区已有服务器时，说"开服"它会先报现状（版本、目录、是否在跑），问你要**沿用 / 继续 / 新开**，不会闷头新开一个。

**会话与档案**：完整消息流（含每次工具调用与结果）逐条落盘，可回看可导出（自动打码密钥）；部署方案可存成档案，下次一键复用。

**费用透明**：每次 AI 调用强制记账（输入/输出 token、按价格表换算费用），预算硬上限超限自动中断。

## 快速开始

### 第 0 步：装好环境（没装过 Rust 的机器）

环境引导脚本一键搭好 rustup 工具链（Windows 还会自动装 MSVC Build Tools）并编译安装 `mcha`，每步先检测后安装、可重复执行：

```powershell
# Windows（PowerShell，无需管理员）
powershell -ExecutionPolicy Bypass -File scripts\bootstrap-windows.ps1
```

```bash
# Linux / macOS
bash scripts/bootstrap.sh
```

已装 Rust 1.85+ 的机器可以直接：

```bash
cargo install --path .   # 编译并把 mcha 装到 ~/.cargo/bin
```

> 不想安装也可以：把下文所有 `mcha` 换成 `cargo run --release --`，例如
> `cargo run --release -- config list`。

### 第 1 步：配置向导（必填仅 3 项）

```bash
mcha
```

检测到没有配置时会自动进入向导：

1. **API 提供方**——选预设（智谱 GLM / DeepSeek / OpenAI）或填自定义 Endpoint；
2. **模型名**——预设项有默认值，回车即可；
3. **API Key**——隐藏输入，自动写入数据目录的 `.env`（权限 600，不入仓库）。

完成后程序会自动做一次**连接测试**（发一条最小对话请求，显示延迟与模型应答），成功即进入会话。

> 需要一个任意 OpenAI 兼容的 Chat API（智谱 GLM / DeepSeek / OpenAI 等均可）。
> 手工方式：`mcha setup` 重跑向导；`mcha config set model.endpoint <地址>` 逐项配置；
> `mcha config test` 单独测试；Key 也可以直接 `export MCHA_API_KEY=...`。

### 第 2 步：自然语言开服

进入会话后直接用中文自然语言下任务，Agent 自主决定调用哪些工具并实时展示每一步。以下是一个例子：

```text
$ mcha
> 我们 5 个人，2 个正版 3 个离线，开个 1.21.1 的 Fabric 服，装暮色森林
  ⏺ load_skill("server-setup")            ← 加载开服决策指南
  ⏺ check_version_compat(...)             ← 查证版本兼容
  ⎿ ✓ 1.21.1 ← Fabric 0.16，需 Java 21
  ⏺ ask_user("服务器要给谁连？", [...])    ← 缺什么问什么
  ⎿ ← 局域网
  ⏺ ensure_java(major=21)                 ← 自动装受管 JRE
  ⏺ fetch_server_jar(...) → write_server_files(...) → check_plan(...)
  ⏺ start_server                          ← 独立窗口起服
  ⎿ ✓ Done (12.4s)! 127.0.0.1:25565 可登录
  → 连接说明卡片 + 离线模式风险提示

> /exit
  ── 会话结束 ──
  输入 3.2K tokens · 输出 0.9K tokens · 费用 ¥0.0210 · 用时 4 分钟
  轨迹已保存：~/.mcha/sessions/2026-0904-1700-ab12.jsonl
```

## 日常使用

### 命令行

| 命令                                                        | 作用                                                 |
| ----------------------------------------------------------- | ---------------------------------------------------- |
| `mcha`                                                      | 进入交互会话（无配置时自动进向导）                   |
| `mcha new "一句话任务"`                                     | 预填首条消息开新会话                                 |
| `mcha --continue`                                           | 接续最近一次会话                                     |
| `mcha --resume`                                             | 交互式选择恢复历史会话                               |
| `mcha setup`                                                | 重跑配置向导（含 CurseForge / 樱花frp 密钥可选步骤） |
| `mcha config list` / `config set <键> <值>` / `config test` | 查看 / 修改配置 / 连接测试                           |
| `mcha usage [--session <id>]`                               | 用量与费用账本                                       |
| `mcha sessions list` / `show <id>` / `export <id>`          | 历史会话：列表 / 全量消息流 / 导出 JSON（自动打码）  |
| `mcha profiles list`                                        | 已保存的部署档案                                     |

### 会话内

| 操作                | 效果                                             |
| ------------------- | ------------------------------------------------ |
| 直接输入中文        | 给 Agent 下任务，它自主多步执行                  |
| `Ctrl-C`            | 打断当前回合（工具执行中也可以），会话保留       |
| `/usage`            | 本会话累计 token 与费用                          |
| `/sessions`         | 列出历史会话                                     |
| `/token`            | 配置樱花frp 访问密钥（隐藏输入，密钥不经过模型） |
| `/help`             | 帮助                                             |
| `/exit` 或 `Ctrl-D` | 退出并打印费用汇总与轨迹文件路径                 |

**服务器与 frpc 的窗口模型**：服务器与 frpc 都由 Agent 在**独立终端窗口**中拉起。日志只在各自窗口滚动（不刷 Agent 界面），启动失败的原因也能直接在窗口里看到。停服、关隧道都在各自窗口操作（Ctrl-C 或关闭窗口）；mcha 退出不影响它们运行。

**启动自检**：会话启动时会用一行灰字提示未配置的可选项（CurseForge Key、搜索后端、樱花frp 密钥），全部就绪则保持安静。

## 配置

### 目录与环境变量

数据目录：`~/.mcha/`（Windows `%APPDATA%\mcha\`）：

```text
~/.mcha/
├── config.toml    # 配置（setup 自动生成，含注释）
├── .env           # 仅 API Key（权限 600）
├── sessions/      # 会话轨迹（完整消息流 JSONL）
├── usage/         # 用量账本
├── profiles/      # 部署档案
└── runtime/       # 受管运行时：jdk-<major>/ 与 frpc/<版本>/（删目录即卸载）
```

| 环境变量              | 作用                                                    |
| --------------------- | ------------------------------------------------------- |
| `MCHA_API_KEY`        | 模型 API Key（变量名可经 `model.api_key_env` 更改）     |
| `MCHA_CURSEFORGE_KEY` | CurseForge 官方 API（可选；未配置自动走国内镜像）       |
| `MCHA_NATFRP_TOKEN`   | 樱花frp 访问密钥（可选；`mcha setup` 或 `/token` 配置） |
| `MCHA_DATA`           | 覆盖数据目录位置                                        |
| `MCHA_WORKSPACE`      | 覆盖工作区（默认当前目录；文件与命令工具的作用范围）    |

### config.toml 全景

首次 setup 自动生成（含注释），手动编辑或 `mcha config set <键> <值>` 均可。默认已为国内网络优化（Mojang 走 bmclapi、JRE 走 TUNA、Wiki 走 biligame 镜像）：

```toml
[model]
endpoint = "https://open.bigmodel.cn/api/paas/v4"
model = "glm-5.2"
context_len = 256000        # 上下文长度（token），裁剪依据
thinking = false            # 思考模式开关
# api_key_env = "MCHA_API_KEY"   # Key 的环境变量名（Key 本体写 .env，不入库）

[[prices]]                  # 元 / 百万 token；默认注释状态——不配置则费用记 0 并标注"仅 token 数"
model = "glm-5.2"
input_per_m = 2.0
output_per_m = 8.0

[budget]
limit_cny = 10.0            # 费用硬上限（按会话累计），超限自动中断

[safety]
confirm_level = "standard"  # paranoid（全部确认）| standard（写/执行/下载确认）| auto（免确认，演示用）

[network]                   # 上游镜像（默认已选国内源，可改 off 走官方）
mojang_mirror = "bmclapi"   # bmclapi | off | 自定义基础 URL
adoptium_mirror = "tuna"    # tuna | off
# modrinth_api = ""         # Modrinth API 基址；空 = 官方（自建代理用）
# curseforge_api = ""       # CurseForge API 基址；空 = 官方（未配 Key 时自动走内置国内镜像）
# natfrp_api = ""           # 樱花frp API 基址；空 = 官方 https://api.natfrp.com/v4

[retrieval]                 # 知识检索通道
mcwiki = "https://wiki.biligame.com/mc/api.php"  # MC Wiki；空 = 禁用
mcmod = "https://search.mcmod.cn"                # MC 百科（mod 中文资料）；空 = 禁用

[search]
backend = ""                # 空 = 无搜索后端（web_search 会如实说明）

[agent]
max_tool_calls_per_turn = 40   # 单回合工具调用保险丝
command_timeout_secs = 120     # run_command 默认超时
large_output_bytes = 8192      # 工具结果转存附件的阈值
```

### 费用与安全须知

- 默认**只统计 token 用量**（费用记 0 并标注"仅 token 数"）；想显示费用，在 `[[prices]]` 里按官方价格页填单价（元 / 百万 token）。
- 默认预算 ¥10/会话，超限自动中断；改预算：`mcha config set budget.limit_cny 5`。
- 默认确认级别 `standard`：Agent 要**写文件 / 执行命令 / 下载 / 创建隧道**时会先弹确认，`y` 本次允许、`a` 本会话允许此工具、`n` 拒绝（拒绝后它会自己换方案）。想全自动可设 `auto`（仅建议演示用）。
- 所有密钥只存 `.env`（0600）；会话导出与日志自动打码密钥、隧道命令形态与公网 IP。

## 常见问题

**服务器窗口关了会怎样？** 服务器进程随之结束（数据已落盘）；frpc 窗口关闭即断开隧道，朋友会掉线。重新让 Agent 起服 / 拉隧道即可，配置都在。

**已经开过一个服，再说"开服"会重开吗？** 不会。Agent 开工前会先看工作区现状（已有 server/ 目录、worlds、档案），先问你沿用 / 继续 / 新开。

**端口 25565 被占了？** Agent 启动服务器前会检测占用，被占会拒绝并报告；换个端口让它重新生成配置即可。

**能装 Forge / OptiFine 吗？** Forge 当前为指导模式（Agent 给出步骤与命令，你手动执行）；OptiFine 无 API 收录，会建议 Sodium + Iris 替代。两种情况它都会如实说明。

**樱花frp 建不了隧道？** 多为账号问题：未实名、等级不够、节点满载。直接问它"为什么建不了隧道"，Agent 会拉取账号与隧道状态，给出具体原因和下一步引导。

**费用怎么算？** 只有 AI API 调用计入 token 与费用；下载、MC 服务器、frpc 都不经 AI 计费。樱花frp 的隧道流量由其平台计量，MCHA 仅展示。

## 已知边界

- 符号链接不做解析（路径收敛为词法规范化）
- `run_command` 超时 / 取消时终止直接子进程，不追杀整个进程树
- `web_search` 未内置搜索后端（接口就绪，默认如实告知）
- 思考模式按 GLM 系 OpenAI 兼容语义发送；思考全文不入史，仅留"已思考 Ns"占位
- 日志诊断为基础能力（读日志 + 简单分析），深度根因定位尚未交付
- 节点可用性与限速以樱花frp 平台实时状态为准

## 项目结构

```text
.
├── scripts/          # 环境引导脚本（Windows / Linux / macOS）
├── src/
│   ├── main.rs       # 入口分发
│   ├── cli/          # REPL、语义化渲染、setup 向导、管理子命令
│   ├── agent/        # Agent Loop：调度、上下文裁剪、确认门、停止条件
│   ├── llm/          # 自研 OpenAI 兼容客户端：SSE 流式、tool_calls、重试、用量
│   ├── tools/
│   │   ├── general/  # 通用工具：文件 / 命令 / HTTP / 询问 / 技能加载
│   │   └── mc/       # 领域工具：版本 / Java / 服务端 / 配置 / 生命周期 / mod / 穿透
│   ├── knowledge/    # 上游 API 客户端与检索通道（Mojang / Modrinth / CurseForge / 樱花frp / Wiki）
│   ├── config/       # AppConfig、价格表、config set（保留注释）
│   ├── store/        # 会话 JSONL、用量账本、导出打码
│   └── assets/       # 系统提示词、开服 Skill、知识库（TOML）、价格预设
└── docs/             # 设计文档与决议（见下）
```

测试：`cargo test`（单元 + 端到端集成，全程 mock 上游）；`cargo test -- --ignored` 追加真实上游冒烟。静态检查：`cargo clippy --all-targets -- -D warnings`。

## 设计与文档

- [`docs/project-design.md`](./docs/project-design.md) —— 设计文档：架构、工具清单、安全边界、测试策略
- [`docs/decisions.md`](./docs/decisions.md) —— 全部设计决议（D1–D142）与文档修订历史
- `docs/` 其余为课程提供的作业要求与参考资料

> 本项目为清华大学计算机系 2025-2026 学年夏季学期程序设计训练（Rust）课程大作业，核心业务逻辑全部以 Rust 实现（Agent Loop、工具后端、LLM 客户端）。
