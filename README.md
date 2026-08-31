# Minecraft Host Agent（MCHA）

> 仓库 `minecraft-host-agent`，简称 **MCHA**（标识符小写 `mcha`，决议 D15）。面向 Minecraft Java 版好友联机场景的"开服管家"。

一个用 Rust 从零构建的、高度场景定制化的 AI Agent：**Minecraft Host Agent**——面向 Minecraft Java 版好友联机场景的"开服管家"。本项目是程序设计课程大作业。

> **当前状态**：MVP（M1）已实现——一句话开服主流程（需求理解 → 决策树 → 部署 → 就绪）全部走通，R1–R6 固定功能完成。故障诊断（FR-09）与樱花frp 穿透编排（FR-08）随 P1 互试版交付。

## 项目简介

玩家用一句自然语言描述需求（"我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存"），Agent 在本机完成方案推导、服务端部署、Java 供给、连接说明交付的全流程。

- **痛点场景**：开服知识分散且过时、决策分支多（账号 × 服务端 × 版本 × 网络）、报错黑盒、内网穿透劝退（详见 `docs/project-design.md` §2 与 `docs/topic-statement.md`）
- **场景定制方案**：
  1. **开服决策树工作流**——全部决策分支固化为 Rust 确定性流程，LLM 只负责理解需求与措辞追问；
  2. **版本兼容知识库 + 实时校验**——版本/mod 事实只来自内置知识库（L1）与 Mojang / PaperMC / Fabric / Modrinth 官方 API（L2），LLM 无凭记忆作答的通道，幻觉版本号被拒并给就近建议；
  3. **Java 全自动供给**——系统无合适 Java 时自动下载 Adoptium 受管 JRE（zip/tar.gz 免安装、sha256 校验、不碰系统配置）；
  4. **MC 崩溃日志诊断**（P1）——错误模式库确定性匹配优先。

## 功能清单（R1–R6）

- [x] **R1 核心逻辑用 Rust 实现**：决策树、执行流水线、全部 API 编排、进程与文件管理均在 Rust 单二进制内；LLM 客户端亦为自研薄实现（reqwest + SSE）
- [x] **R2 用户交互界面**：CLI 交互式终端（clap + dialoguer + indicatif），`mcha new` 触发开服任务
- [x] **R3 可自定义模型配置**：`config.toml` + `.env`，支持 Endpoint / API Key / 上下文长度 / 思考模式 / 价格表（内置常见模型预设）与 `config set` 改写
- [x] **R4 实时进度渲染与打断**：下载字节进度、步骤进度条、模型文本与开服日志滚动直显、就绪等待计时（上限可配置 `[deploy] ready_timeout_secs`）；Ctrl-C 经 CancellationToken 干净退出，不留孤儿进程
- [x] **R5 上下文历史管理**：任务轨迹逐轮落盘（`sessions/`，LLM 与部署执行步骤齐备，失败原因随轨迹落盘），可查看（`sessions show`）、导出（`sessions export`，自动打码）；服务端启动日志落盘 `<安装目录>/<spec_id>/server/mcha-launch.log`；开服档案可保存 / 加载（`profiles/`）
- [x] **R6 Token 用量与价格统计**：每次调用强制生成 UsageRecord（无 usage 的上游记次数并标注），按价格表换算费用，`usage` 汇总展示，预算超限自动中断

## 环境要求

- Rust 1.85+（edition 2024）
- 网络：可访问 Mojang / Modrinth / Adoptium 等 API（国内网络建议配置 Adoptium 镜像，见下）
- 无需预装 Java：缺 Java 时工具自动安装受管 JRE

## 快速上手（引导脚本 + 向导）

不想手动装环境？运行引导脚本（幂等，先检测后安装）：

```powershell
# Windows（PowerShell，自动装 rustup + VS Build Tools 并完成 cargo install）
powershell -ExecutionPolicy Bypass -File scripts\bootstrap-windows.ps1
```

```bash
# Linux / macOS
bash scripts/bootstrap.sh
```

脚本完成后运行上手向导，必填仅 3 项（endpoint / 模型名 / API Key），其余回车即默认：

```bash
mcha setup     # 配置向导 + 可选把 mcha 注册进 PATH（复制到 ~/.cargo/bin）
mcha new "我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存"
```

以后想改配置，无需手编 TOML：

```bash
mcha config wizard   # 问答式修改（保留你文件里的注释）
```

## 构建与运行（手动方式）

Windows 前置：安装 [rustup](https://rustup.rs)（stable-msvc）与 [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)（勾选"使用 C++ 的桌面开发"，提供链接器）。

```bash
# 构建
cargo build --release

# 安装为全局命令（可选；装进 ~/.cargo/bin，任意目录可用 mcha）
cargo install --path .

# 运行测试（不含联网测试）
cargo test

# 联网冒烟测试（真实上游 API）
cargo test -- --ignored
```

## 配置说明

数据目录：`~/.mcha/`（Windows 为 `%APPDATA%\mcha\`）。推荐用向导完成配置；也可手动编辑：

```bash
# 1. 生成配置模板
cargo run --release -- config init

# 2. 编辑配置文件：~/.mcha/config.toml
#    必填两项：
#      model.endpoint  —— OpenAI 兼容 API 地址（默认填了智谱 open.bigmodel.cn）
#      model.model     —— 模型名
# 3. 在 ~/.mcha/.env 中填入 API Key：
#      MCHA_API_KEY=你的密钥

# 4. 修改配置项示例
cargo run --release -- config set model.model glm-5.2
cargo run --release -- config set budget.limit 10      # 费用预算（超限自动中断）
cargo run --release -- config set model.thinking true  # 思考模式（视模型支持）
cargo run --release -- config set workspace.path D:\mc-servers  # 服务端安装根目录（FR-19）
```

**工作区**（FR-19）：服务端安装位置在开服流程中会**交互询问**（默认当前目录，并显示来源注记）；也可预设，优先级为**开服时交互输入** > 环境变量 `MCHA_WORKSPACE` > `config.toml [workspace] path` > 默认当前目录。支持 `~` 展开（Windows 为用户主目录）与相对路径。**服务端文件（eula.txt / server.properties / world / mcha-launch.log 等）直接落在该目录**，不再有子目录层级；若目标目录已有服务器文件会要求确认，防止混用。开服档案元数据始终存于数据目录；Java 运行时始终装在数据目录受管位置，不污染系统。

国内网络建议启用 Adoptium 镜像（`[network]` 节，模板内有现成注释行）：

```toml
[network]
adoptium_mirror = "https://mirrors.tuna.tsinghua.edu.cn/Adoptium"
```

## 演示用例

### 1. 一句话开服（主流程）

```bash
cargo run --release -- new "我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存，MC 1.21.1"
```

工具将：调用 LLM 解析需求 → 决策树推导完整方案（混合认证 EasyAuth、Fabric 服、
Java 21、内存分配、白名单）→ 就缺失信息追问（≤3 轮）→ 展示方案摘要与风险提示 →
确认后自动完成部署 → 输出"朋友们怎么连"。

### 2. 无 LLM 的手动方案（离线演示兜底）

```bash
cargo run --release -- plan
```

### 3. 查看历史与开销

```bash
cargo run --release -- sessions list            # 任务列表
cargo run --release -- sessions show <任务ID>    # 逐步轨迹（LLM/工具/决策/执行）
cargo run --release -- sessions export <任务ID>  # 导出完整上下文 JSON（已打码）
cargo run --release -- usage                    # token 与费用统计
cargo run --release -- profiles                 # 开服档案（可复用）
```

## 项目结构

```text
.
├── docs/                    # 课程文档（勿改）+ 本项目设计文档 / 选题陈述
├── experiments/             # 通用 Agent 基线实验记录
├── src/
│   ├── main.rs              # clap 入口、取消总线装配
│   ├── cli/                 # ui：子命令、交互问答、进度渲染（R2/R4）
│   ├── agent.rs             # agent-core：需求理解窄循环、工具系统
│   ├── llm.rs               # OpenAI 兼容自研客户端、SSE、预算守卫（R6）
│   ├── knowledge/           # 静态知识库、五家上游 API 客户端、版本校验
│   ├── provision/           # 决策树引擎、Java 供给、部署流水线、进程托管
│   ├── store.rs             # 档案 / 会话 / 用量持久化（R5/R6）
│   ├── config.rs            # 配置、价格表（R3）
│   ├── events.rs            # 事件总线：进度 / 用量 / 轨迹三视图
│   ├── spec.rs              # ServerSpec / ServerSpecDraft 核心数据结构
│   └── assets/              # 定制内容：知识库 TOML、指南、提示词
├── scripts/                 # 环境引导脚本（bootstrap-windows.ps1 / bootstrap.sh）
├── Cargo.toml
└── README.md
```

## 开发时间线

| 阶段       | 截止时间       | 内容                                    |
| ---------- | -------------- | --------------------------------------- |
| 选题确认   | 08-30 23:59:59 | 在网络学堂"Agent选题确认"讨论区发布选题 |
| 选题互评   | 09-01 23:59:59 | 给至少 3 位同学的选题写评论             |
| 公开展示   | 09-06 23:59:59 | 发布设计文档摘要和项目链接              |
| 作品互试   | 09-08 23:59:59 | 至少试用 3 位同学的作品并提交反馈       |
| 分课堂展示 | 09-10 上午     | 5 分钟展示 + 2 分钟提问，互相评分       |
