# Minecraft Host Agent（MCHA）

> 仓库 `minecraft-host-agent`，简称 **MCHA**（标识符小写 `mcha`，决议 D15）。面向 Minecraft Java 版好友联机场景的"开服管家"。

一个用 Rust 从零构建的、高度场景定制化的 AI Agent：**Minecraft Host Agent**——面向 Minecraft Java 版好友联机场景的"开服管家"。本项目是程序设计课程大作业。

> **当前状态**：MVP（M1）已实现——一句话开服主流程（需求理解 → 决策树 → 部署 → 就绪）全部走通，R1–R6 固定功能完成。故障诊断（FR-09）与樱花frp 穿透编排（FR-08）随 P1 互试版交付。

## 项目简介

玩家用一句自然语言描述需求（"我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存"），Agent 在本机完成方案推导、服务端部署、Java 供给、连接说明交付的全流程。

- **痛点场景**：开服知识分散且过时、决策分支多（账号 × 服务端 × 版本 × 网络）、报错黑盒、内网穿透劝退（详见 `docs/project-design.md` §2 与 `docs/topic-statement.md`）
- **场景定制方案**：
  1. **开服决策树工作流**——全部决策分支固化为 Rust 确定性流程，LLM 只负责理解需求与措辞追问；
  2. **版本兼容知识库 + 实时校验**——版本/mod 事实只来自内置知识库（L1）与 Mojang / PaperMC / Spigot（getbukkit 镜像）/ Fabric / Modrinth 官方 API（L2），LLM 无凭记忆作答的通道，幻觉版本号被拒并给就近建议；**玩家点名 Spigot 就用 Spigot，不改判 Paper**；
  3. **Java 全自动供给**——系统无合适 Java 时自动下载 Adoptium 受管 JRE（zip/tar.gz 免安装、sha256 校验、镜像优先默认清华 TUNA；Windows 统一装 `C:\Program Files\Java\<版本>\`，普通权限自动弹一次 UAC 提权，拒绝则回退数据目录）；开服完成即在服务端目录生成 `start.bat`（Windows）/ `start.sh`，之后可双击自启，无需 mcha 在场；
  4. **MC 崩溃日志诊断**（P1）——错误模式库确定性匹配优先。

## 功能清单（R1–R6）

- [x] **R1 核心逻辑用 Rust 实现**：决策树、执行流水线、全部 API 编排、进程与文件管理均在 Rust 单二进制内；LLM 客户端亦为自研薄实现（reqwest + SSE）
- [x] **R2 用户交互界面**：CLI 交互式终端（clap + dialoguer + indicatif），`mcha new` 触发开服任务
- [x] **R3 可自定义模型配置**：`config.toml` + `.env`，支持 Endpoint / API Key / 上下文长度 / 思考模式 / 价格表（内置常见模型预设）与 `config set` 改写
- [x] **R4 实时进度渲染与打断**：下载字节进度、步骤进度条、模型文本与开服日志滚动直显、就绪等待计时（上限可配置 `[deploy] ready_timeout_secs`）；Ctrl-C 经 CancellationToken 干净退出，不留孤儿进程
- [x] **R5 上下文历史管理**：任务轨迹逐轮落盘（`sessions/`，LLM 与部署执行步骤齐备，失败原因随轨迹落盘），可查看（`sessions show`）、导出（`sessions export`，自动打码）；服务端启动日志落盘 `<安装目录>/mcha-launch.log`；开服档案可保存 / 加载（`profiles/`）
- [x] **R6 Token 用量与价格统计**：每次调用强制生成 UsageRecord（无 usage 的上游记次数并标注），按价格表换算费用，`usage` 汇总展示，预算超限自动中断

## 环境要求

- Rust 1.85+（edition 2024）
- 网络：可访问 Mojang / Modrinth / Adoptium 等 API（国内网络默认已启用清华 TUNA Adoptium 镜像，可配置关闭，见下）
- 无需预装 Java：缺 Java 时工具自动安装——Windows 统一装到 `C:\Program Files\Java\<版本>\bin\java.exe`（需在 UAC 弹窗点一次"是"，或改用管理员终端运行则静默完成）

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

**工作区**（FR-19）：服务端安装位置在开服流程中会**交互询问**（默认当前目录，并显示来源注记）；也可预设，优先级为**开服时交互输入** > 环境变量 `MCHA_WORKSPACE` > `config.toml [workspace] path` > 默认当前目录。支持 `~` 展开（Windows 为用户主目录）与相对路径。**服务端文件（eula.txt / server.properties / start.bat / world / mcha-launch.log 等）直接落在该目录**，不再有子目录层级；若目标目录已有服务器文件会要求确认，防止混用。开服档案元数据始终存于数据目录；Java 运行时 Windows 装在 `C:\Program Files\Java\<版本>\`（决议 D21），其余平台装数据目录受管位置。

**Java 镜像（决议 D24）**：`adoptium_mirror` **默认已启用清华 TUNA**（官方 GitHub 下载渠道在国内经常不可达；镜像优先、官方回退，同一 sha256 校验）。海外网络想直连官方渠道时显式置空：

```toml
[network]
adoptium_mirror = ""
```

### 部署编排环是怎么工作的（决议 D25，v0.12）

确认方案后，部署不再是固定流水线，而是由 LLM 逐工具调用编排：`probe_workspace`（盘点工作区）→ `ensure_java`（Java 供给，Windows 可能弹一次 UAC）→ `acquire_server_jar`（多渠道获取服务端 jar）→ `write_server_files`（eula/配置/start 启动脚本）→ `start_server`（启动 + 就绪检测）→ `probe_port`（本机端口验证，返回 ready 即成功）。失败不会让任务直接崩掉：工具以结构化错误回传，模型自行重试、换渠道、抓页面解析直链或向你确认（`ask_user`）。轨迹（`mcha sessions show`）里可以看到每一轮的工具调用与结果；编排环的 token 消耗在 `mcha usage` 中以「部署编排」阶段单独计量。

### Spigot 获取失败怎么办（决议 D22/D25，v0.12）

SpigotMC 官方不提供 jar 直链（只随 BuildTools 编译分发），mcha 首选**抓取 getbukkit 下载页解析直链**（`getbukkit.org/download/spigot` → 版本令牌 → 302 → `cdn.getbukkit.org` 真直链，v0.12.1 抓站实测），失败再回退 API/直链拼接渠道。全部渠道不可达时：

1. **稍后重试**（编排环内直接说"重试"即可，或重跑 mcha）；
2. **手动放置 jar**：从浏览器打开 `https://getbukkit.org/download/spigot` 点 Download 下载 `spigot-<版本>.jar`，放进服务端安装目录后重跑 mcha——同名 jar 会被自动复用（该来源无官方哈希，轨迹会明示"第三方来源未校验"）；
3. **BuildTools 手动编译**（官方正源）：`java -jar BuildTools.jar --rev <版本>`，需要 git 与 JDK；把产物放进安装目录同样走通道 2 复用。

## 演示用例

### 1. 一句话开服（主流程）

```bash
cargo run --release -- new "我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存，MC 1.21.1"

# 点名 Spigot 开服（v0.11：用户说 spigot 就用 spigot，不会被改判成 Paper）
cargo run --release -- new "我要用 Spigot 服玩 MC 26.2 原版，不加 mod"
```

工具将：调用 LLM 解析需求 → 决策树推导完整方案（混合认证 EasyAuth、Fabric 服、
Java 需求、内存分配、白名单）→ 就缺失信息追问（≤3 轮）→ 展示方案摘要与风险提示 →
确认后由部署编排环完成（LLM 调度工具：Java 供给 → 服务端下载 → 配置 + `start.bat`
→ 启动到就绪 → 端口验证）→ 输出"朋友们怎么连"（本机连接地址 `127.0.0.1:25565`）。

### 2. 手动方案（跳过需求解析，部署仍走编排环）

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
│   ├── knowledge/           # 静态知识库、上游 API 客户端（Mojang/Paper/Spigot/Fabric/Modrinth/Adoptium）、版本校验
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
