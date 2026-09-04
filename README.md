# Minecraft Host Agent（MCHA）

一个用 Rust 从零构建的、高度场景定制化的 AI Agent。本项目是程序设计训练课程大作业。

> 仓库 `minecraft-host-agent`，正式名 **Minecraft Host Agent**，简称 **MCHA**（标识符小写 `mcha`）。命名沿用归档主线决议 D15。

> **当前状态**：**M1 Agent 框架已交付**（Agent Loop + 通用工具集 + REPL 多轮会话 + R5/R6 骨架，设计文档 v1.7）。M2 开服场景包（领域工具 + 知识库 + Skills）待开工。下文"配置说明 / 演示用例"随 M1 可用。

## 项目简介

本项目从真实的生活 / 学习 / 娱乐场景出发，针对一个通用 Agent 解决不好的具体痛点，用 Rust 设计并实现一个场景定制化的 AI Agent，其中包含**至少两项**针对该场景的专门优化或定制逻辑，使它在这个场景下明显优于通用 Agent，或能做到通用 Agent 做不到的事。

- **痛点场景**：MC Java 版好友联机开服（知识分散、决策分支多、故障黑盒、内网穿透劝退，详见设计文档 §2）
- **场景定制方案**：①开服决策指南（Skill）+ 全链路领域工具；②版本兼容知识库 + 实时校验工具（详见设计文档 §3）

### M1 已实现（Agent 框架，场景无关）

- **Agent Loop**（唯一执行引擎）：模型提议工具调用 → Rust 工具后端执行 → 结果回传再决策；停止条件含模型自然结束、Ctrl-C 打断、预算耗尽、轮数保险丝（默认 40）
- **通用工具集**：`run_command` / `read_file` / `write_file` / `edit_file` / `list_dir` / `http_get_text` / `http_download`（断点续传 + sha256 校验）/ `web_search`（接口就绪，后端未内置）/ `ask_user` / `load_skill`
- **安全边界**：文件路径收敛（工作区 + 数据目录）、危险操作确认门（y 本次 / a 本会话 / n 拒绝，级别 paranoid / standard / auto 可配）、命令超时与进程清理
- **多轮会话 REPL**：inline 滚动流 + 语义化渲染块（工具调用 / 结果 / 进度条 / 思考块）；斜杠命令 `/exit` `/usage` `/help` `/sessions`
- **R3 模型配置**：`mcha setup` 向导（3 项必填 + 连接测试）、`config set/list/test`
- **R4 进度与打断**：LLM 流式直显、下载进度条、命令输出滚动；Ctrl-C 随时打断当前回合（会话保留，未完成调用回填占位保证消息流合法）
- **R5 历史管理**：会话 = 完整消息流 JSONL 逐条落盘；`sessions list/show/export`（导出自动打码密钥与公网 IP）；`--continue` / `--resume`
- **R6 用量统计**：每次调用（含重试与失败）强制记账，按价格表换算费用；预算硬上限超限自动中断；`usage` 全局账本

### M2 已实现（开服场景包）

- **服务器设施**：版本兼容查证、Java 受管自动安装、四渠道服务端下载（原版 / Paper / Spigot / Fabric，哈希校验）、配置生成（EULA / properties / 离线白名单 UUID / 启动脚本）、独立窗口启动与就绪检测、端口探测与 MC 协议 ping、部署前确定性校验 `check_plan`
- **mod 场景**：`search_mods` / `resolve_mod` / `install_mods` 三段式（中文别名表、依赖闭包、Modrinth + CurseForge 双源——未配置 Key 自动走国内镜像，暮色森林等 CF 独占 mod 零配置可装）
- **部署档案**：`save_profile` / `load_profile`（方案快照与产物清单，跨会话复用）
- **内网穿透（樱花frp）**：一次性人工步骤只剩**注册 / 登录、实名、粘贴访问密钥**；之后 Agent 全自动编排——

  ```text
  > 我们没有公网 IP，朋友要能连进来
  ⏺ check_tunnel      账号快照（实名 / 等级 / 流量 / frpc 在位；未配置密钥时给 /token 引导）
  ⏺ ensure_frpc       官方 frpc 下载 + MD5 校验（同版本幂等跳过）
  ⏺ select_tunnel_node 确定性打分（内地优先 → 负载低 → 非 BETA）
  ⏺ ask_user          你确认节点（默认推荐第一位）
  ⏺ create_tunnel     建隧道（同名同端口自动复用）
  ⏺ start_tunnel      独立窗口启动 frpc → 轮询在线 → TCP + MC ping 端到端验证
  → 连接说明卡片：朋友连接地址、流量余额、注意事项
  ```

  密钥配置两条路：`mcha setup` 可选步骤（含注册与登录入口、实名认证、密钥获取的分步可点击指引）或会话内 `/token`（隐藏输入，密钥不经过模型）。隧道流量由樱花frp 计量，MCHA 只展示不计费。

作业背景与完整要求见 [`docs/`](./docs)：

| 文档                                                         | 内容                                                  |
| ------------------------------------------------------------ | ----------------------------------------------------- |
| [`docs/background.md`](./docs/background.md)                 | 作业背景：为什么要做场景定制化 Agent                  |
| [`docs/requirements.md`](./docs/requirements.md)             | 作业要求：目标、R1–R6 固定功能、提交物与评分标准      |
| [`docs/agent-architecture.md`](./docs/agent-architecture.md) | 技术参考：工具调用、Agent Loop、MCP / Skills / 知识库 |
| [`docs/quick-start.md`](./docs/quick-start.md)               | 流程指南：定选题 → 迭代设计文档 → 实现 → 展示         |

## 功能规划

对照作业固定功能要求（R1–R6，详见 [`docs/requirements.md`](./docs/requirements.md)）的实现清单：

- [x] **R1 核心逻辑用 Rust 实现**：Agent Loop、工具系统与全部工具后端、自研 LLM 客户端（SSE 流式 / tool_calls 拼装 / 重试）、上下文裁剪均在 Rust 主控流程中
- [x] **R2 用户交互界面**：CLI 交互式终端（REPL 多轮会话，可触发 Agent 任务并实时展示过程与结果）
- [x] **R3 可自定义模型配置**：Endpoint / API Key / 上下文长度 / 思考模式 / API 价格，`setup` 向导 + `config set` + 手编 TOML
- [x] **R4 实时进度渲染与打断**：流式输出、进度条、命令输出滚动；Ctrl-C 随时打断当前回合
- [x] **R5 上下文历史管理**：完整消息流 JSONL 落盘，可查看 / 恢复 / 导出（自动打码）
- [x] **R6 Token 用量与价格统计**：逐调用计量与费用换算、预算上限超限自动中断、三层展示（退出汇总 / `usage` 账本 / `sessions show` 明细）

> M2 开服场景包（领域工具 / 知识库 / Skills / 场景提示词）已分步交付：服务器设施（M2.1）→ mod 场景与 CurseForge 双源（M2.2）→ 内网穿透（M2.3，见下文"开服场景能力"）。

## 环境要求

- Rust 1.85+（项目使用 edition 2024）
- 任意 OpenAI 兼容 Chat API（智谱 GLM / DeepSeek / OpenAI 等）与对应 API Key

## 快速上手（三步跑起来）

**第 1 步：构建并安装命令**

```bash
cargo install --path .   # 编译并把 `mcha` 装到 ~/.cargo/bin（之后任意目录可用）
```

> 不想安装也可以：把下文所有 `mcha` 换成 `cargo run --release --` 即可，例如
> `cargo run --release -- config list`。

**第 2 步：首次运行，完成配置向导**

```bash
mcha
```

检测到没有配置时会自动进入向导，必填只有 3 项：

1. **API 提供方**——选预设（智谱 GLM / DeepSeek / OpenAI）或填自定义 Endpoint；
2. **模型名**——预设项有默认值，回车即可；
3. **API Key**——隐藏输入，自动写入 `~/.mcha/.env`（权限 600，不入仓库）。

完成后自动做一次**连接测试**（发一条最小对话请求，显示延迟与模型应答），成功即进入会话。

> 手工方式：`mcha setup` 重跑向导；或 `mcha config set model.endpoint <地址>` 逐项配置、
> `mcha config test` 单独测试。Key 也可以直接 `export MCHA_API_KEY=...`。

**第 3 步：对话**

进入会话后直接用自然语言下任务，Agent 会自己决定调用哪些工具并实时展示每一步：

```text
$ mcha
> 列出当前目录，告诉我这个项目是做什么的
  ⏺ list_dir(path=.)
  ⎿ ✓ README.md  Cargo.toml  src/ …
  这是一个 Rust 编写的 AI Agent 项目……

> 把 README 里的"已知边界"一节总结成 3 句话
  ⏺ read_file(path=README.md)
  ⎿ ✓ （文件内容）
  ……

> /exit
  ── 会话结束 ──
  输入 3.2K tokens · 输出 0.9K tokens · 费用 ¥0.0210 · 用时 4 分钟
  轨迹已保存：~/.mcha/sessions/2026-0901-1700-ab12.jsonl
```

**会话内你会用到的**：

| 操作 | 效果 |
| --- | --- |
| 直接输入中文 | 给 Agent 下任务，它会自主多步执行 |
| `Ctrl-C` | 打断当前回合（工具执行中也可以），会话保留 |
| `/usage` | 看本会话累计 token 与费用 |
| `/token` | 配置樱花frp 访问密钥（朋友跨网络联机用，隐藏输入） |
| `/exit` 或 `Ctrl-D` | 退出并打印费用汇总与轨迹文件路径 |
| `mcha --continue` | 下次接着上次会话继续聊 |

**几个值得试的任务**（都是真实的多步工具调用）：

```text
> 抓取 https://www.rust-lang.org 的首页文本，存成 rust-homepage.txt，然后数一下它有多少行
> 在工作区建一个 notes.md，写上今天的待办，再把它改成 5 行
> 当前目录里哪个 Rust 文件最长？（提示：它可能会用 run_command 跑 wc/find）
```

**费用与安全，先知道这几点**：

- 每次对话调用都计 token。退出时打印汇总；`mcha usage` 看全局账本；`mcha sessions show <id>` 看每次调用的明细。
- 默认**只统计 token 用量**（费用记 0 并标注"仅 token 数"）；想显示费用，在 `config.toml` 的 `[[prices]]` 里填入模型单价（元/百万 token）即可。
- 默认预算 ¥10/会话（超限自动中断）；改预算：`mcha config set budget.limit_cny 5`。
- 默认确认级别 `standard`：Agent 要**写文件 / 执行命令 / 下载**时会先弹确认，`y` 本次允许、`a` 本会话允许此工具、`n` 拒绝（拒绝后它会自己换方案）。想全自动可设 `auto`（演示用）。

## 构建与运行

```bash
# 构建
cargo build --release

# 首次运行：自动进入配置向导（3 项必填 + 连接测试）
cargo run --release

# 常用方式
mcha                      # 进入交互会话（REPL）
mcha new "列一下当前目录"  # 预填首条消息
mcha --continue           # 接续最近会话
mcha --resume             # 交互式选择恢复历史会话

# 配置与数据
mcha setup                # 重跑配置向导
mcha config list          # 查看生效配置
mcha config set budget.limit_cny 5
mcha config test          # 连接测试

# 会话与用量
mcha sessions list        # 历史会话
mcha sessions show <id>   # 查看完整消息流（含工具调用与结果）
mcha sessions export <id> # 导出 JSON（自动打码）
mcha usage                # 全局用量账本（R6）

# 测试与静态检查
cargo test
cargo clippy --all-targets -- -D warnings
```

会话内命令：`/exit` 退出（Ctrl-D / 提示符处 Ctrl-C 同效）、`/usage` 本会话累计、`/sessions` 列历史、`/help` 帮助。回合执行中按 Ctrl-C 打断当前操作（会话保留）。

## 配置说明

- **数据目录**：`~/.mcha/`（Windows `%APPDATA%\mcha\`；可用 `MCHA_DATA` 覆盖）。布局：`config.toml`（配置）、`.env`（仅 API Key，权限 600）、`sessions/`（会话轨迹）、`usage/`（用量账本）、`runtime/`（运行时附件）。
- **工作区**：文件与命令工具的作用范围，默认当前目录，可用 `MCHA_WORKSPACE` 覆盖。
- **API Key**：写在 `~/.mcha/.env` 的 `MCHA_API_KEY=...`（变量名可经 `model.api_key_env` 更改），永不写入 config.toml 与仓库。可选 Key：`MCHA_CURSEFORGE_KEY`（CurseForge 官方 API，未配置自动走国内镜像）、`MCHA_NATFRP_TOKEN`（樱花frp 访问密钥，内网穿透用；`mcha setup` 或会话内 `/token` 配置）。
- **受管运行时**：`runtime/` 下按版本存放自动下载的 JRE（`runtime/jdk-<major>/`）与 frpc（`runtime/frpc/<版本>/`，含启动脚本 frpc-start，访问密钥固化在脚本内），删除对应目录即卸载。
- **config.toml 全景**（首次 setup 自动生成，含注释）：

  ```toml
  [model]
  endpoint = "https://open.bigmodel.cn/api/paas/v4"
  model = "glm-5.2"
  context_len = 256000        # 上下文长度（token），裁剪依据
  thinking = false            # 思考模式开关

  [[prices]]                  # 元 / 百万 token；无条目的模型费用记 0 并标注
  model = "glm-5.2"
  input_per_m = 2.0
  output_per_m = 8.0

  [budget]
  limit_cny = 10.0            # 费用硬上限（按会话累计），超限自动中断

  [safety]
  confirm_level = "standard"  # paranoid（全部确认）| standard（写/执行/下载确认）| auto（免确认，演示用）

  [search]
  backend = ""                # 空 = 无搜索后端（web_search 返回结构化提示）

  [agent]
  max_tool_calls_per_turn = 40   # 单回合工具调用保险丝
  command_timeout_secs = 120     # run_command 默认超时
  large_output_bytes = 8192      # 工具结果转存附件的阈值
  ```

- **价格表（可选）**：默认只输出 token 用量，费用记 0 并标注"仅 token 数"。想换算费用时，在 `[[prices]]` 条目按官方价格页填写（元 / 百万 token）。

## 演示用例

- **M1 通用任务**（设计 §14 出口标准）：对 Agent 说"抓取某页面存为文件并统计行数"一类多步实事——它会依次调用 `http_get_text` → `write_file` → `run_command`，全程工具调用留痕（`sessions show` 可回放）、可 Ctrl-C 打断、退出时显示 token 与费用汇总。
- **M2.1/M2.2 开服 + mod**（基线实验 T4 对照）：对 Agent 说"我们 5 个人，2 个正版 3 个离线，Fabric 1.21.1，装暮色森林和 JEI"——查证版本 → 自动装 Java → 下载 Fabric 服务端 → 生成配置 → 解析安装 mod（暮色森林经 CurseForge 镜像通道，零配置）→ 起服 → `mc_ping` 验证 → 交付连接说明。
- **M2.3 内网穿透**：在同一会话继续说"我们没有公网 IP，朋友要能连进来"——`check_tunnel`（未配置密钥则引导 `/token`）→ 下载 frpc → 打分选节点（你确认）→ 建隧道 → 独立窗口启动 frpc → TCP + MC ping 端到端验证 → 朋友用公网地址直连；之后"朋友连不上了"会走 `tunnel_status` 排障。

## 项目结构

```text
.
├── docs/           # 文档（课程参考资料 + 本项目选题 / 设计文档）
├── experiments/    # 通用 Agent 基线实验（手册与运行记录）
├── src/            # Rust 源代码
│   ├── agent/      # Agent Loop：消息模型、上下文裁剪、调度、确认门、停止条件
│   ├── llm/        # 自研 OpenAI 兼容客户端：SSE、tool_calls、重试、用量
│   ├── tools/      # 工具系统：注册表、Schema 校验、路径收敛、通用工具集
│   ├── cli/        # REPL、语义化渲染、setup 向导、管理子命令
│   ├── config/     # AppConfig、价格表、config set（保留注释）
│   ├── store/      # 会话 JSONL、用量账本、导出打码
│   ├── events.rs   # 事件总线（进度 / 用量 → 渲染块）
│   ├── cancel.rs   # 协作式取消令牌（Ctrl-C 打断）
│   └── assets/     # 框架 system prompt、价格预设
├── Cargo.toml
└── README.md
```

## 已知边界

- 符号链接不做解析（路径收敛为词法规范化，目标平台 Windows 上风险低）
- `run_command` 超时 / 取消时终止直接子进程，不追杀整个进程树
- `web_search` 仅接口就绪，未内置搜索后端（决议 D103：默认如实告知）
- 思考模式（thinking）按 GLM 系 OpenAI 兼容语义发送；思考全文不入史，仅留"已思考 Ns"占位
- Forge 自动安装为指导模式；日志诊断（FR-18）为选做项，尚未交付
- 内网穿透：frpc 与服务器一样运行在独立窗口，**关闭窗口即断开隧道**；樱花frp 建隧道依赖其平台的实名认证与账号等级，节点可用性以平台实时状态为准

## 开发时间线

| 阶段       | 截止时间       | 内容                                    |
| ---------- | -------------- | --------------------------------------- |
| 选题确认   | 08-30 23:59:59 | 在网络学堂"Agent选题确认"讨论区发布选题 |
| 选题互评   | 09-01 23:59:59 | 给至少 3 位同学的选题写评论             |
| 公开展示   | 09-06 23:59:59 | 发布设计文档摘要和项目链接              |
| 作品互试   | 09-08 23:59:59 | 至少试用 3 位同学的作品并提交反馈       |
| 分课堂展示 | 09-10 上午     | 5 分钟展示 + 2 分钟提问，互相评分       |
