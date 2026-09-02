# AGENTS.md

本文件面向在本仓库中工作的 AI 编程助手，提供项目背景、硬性约束与工作规范。

## 项目背景

这是程序设计课程大作业：**用 Rust 从零实现一个高度场景定制化的 AI Agent**。作业的完整要求在 `docs/` 目录下，动工前必读 `docs/requirements.md`；Agent 相关技术概念参考 `docs/agent-architecture.md`。

当前状态：v2 分支已完成设计文档重构（`docs/project-design.md` v1.0，**Agent-First 理念**）。旧实现主线（v0.12 及以前）因把产品做成了"LLM 问询员 + 确定性脚本"而整体归档至 `archive/v0.12-mvp-line` 分支（只作参考，不再继续）。v2 核心理念（决议 D100）：**Agent Loop 是唯一执行引擎，一切副作用操作由 LLM 经工具调用发起、由 Rust 工具后端执行**；开发顺序为先实现通用 Agent 框架（M1：Loop + 通用工具 + 多轮会话），再叠加开服场景适配（M2：领域工具 + 知识库 + Skills + 提示词）。严禁退回"LLM 问询员 + Rust 确定性流水线"的旧路线。

## 硬性约束（任何时候都不能违背）

1. **核心业务逻辑必须用 Rust 实现**（数据处理、算法流程、API 调用编排）。Web UI 可以用 TypeScript/React 等成熟前端栈，Rust 侧可作为独立后端服务或编译为 WASM 供前端调用。
2. 必须完整实现 R1–R6 六个固定模块（`docs/requirements.md` 第三节），所有设计决策都要覆盖它们：
   - R2 用户界面（Web / CLI 交互式终端 / 桌面 / 移动至少其一，可触发 Agent 任务并展示结果）
   - R3 用户可自定义 API Endpoint、API Key、上下文长度、思考模式、API 价格（配置文件或设置界面）
   - R4 超过 3 秒的任务必须实时渲染进度（如"已处理 45/120 张"），且允许用户打断
   - R5 会话 / 任务历史可查看、可保存 / 加载完整上下文（如 JSON 文件），不能是黑盒
   - R6 精确统计每次 AI API 调用的输入 / 输出 token 数并按价格换算成本，界面清晰展示，支持预算上限、超限自动中断
3. **从零开始构建**，不做对现有 Agent 项目的二次开发；引用开源库必须在设计文档中注明。
4. 所有外部 AI API 调用（LLM / Embedding / 语音 / 文生图 / 文生视频等）都要统计用量与费用，拿不到 token 数时至少统计调用次数并在文档中说明。
5. 用户必须在答辩时能解释提交的每一行 Rust 代码：生成代码要清晰、模块化，避免难以解释的技巧性写法，涉及关键实现时配合简要说明。
6. **Agent-First（v2 核心理念，决议 D100）**：一切副作用操作由 LLM 经工具调用发起、由 Rust 工具后端执行；禁止把业务流程硬编码为绕过 Agent 的确定性流水线（安全边界与确定性校验除外）。新功能的第一个问题永远是"给 Agent 增加什么工具 / 什么知识"，而不是"写什么流程代码"。

## 当前进度

- [x] 选题确认（`docs/topic-statement.md`）
- [x] 通用 Agent 基线实验（opencode + GLM-5.2 @ Windows，手册见 `experiments/general-agent-baseline.md`）
- [x] 需求 / 设计文档初稿（`docs/project-design.md` 初步定稿）
- [x] v2 设计重构：Agent-First 理念，设计文档 v1.0（2026-09-01）
- [x] M1 Agent 框架：Loop + 通用工具集 + REPL 会话 + R5/R6 骨架（§14 出口标准）
- [x] M2.1 开服场景包·服务器设施：领域工具 + 知识库 + server-setup Skill + 场景提示词（US1 精简版跑通，2026-09-02）
- [ ] M2.2 开服场景包·mod 场景：mod 工具 + mcmod 检索 + Profile（FR-12/13/16）
- [ ] M2.3 内网穿透（tunnel_*，FR-17）
- [ ] P1：日志诊断（FR-18 选做）
- [ ] 提交版设计文档定稿
- [ ] README 补全配置与演示章节

## 常用命令

```bash
cargo build                    # 构建
cargo run                      # 运行
cargo test                     # 测试
cargo fmt                      # 格式化
cargo clippy -- -D warnings    # 静态检查
```

提交前必须通过 `cargo fmt --check` 与 `cargo clippy -- -D warnings`。

## 代码规范

- Rust edition 2024，包名 `minecraft-host-agent`。
- **标识符纪律**：所有 Rust 标识符（函数、变量、类型、trait、模块、宏）与源码文件名**一律英文，严禁中文字符进入标识符**——归档主线曾出现十几个汉字的函数名，明令禁止重演。报错消息、日志、注释、文档等用户可见文本使用中文。
- **MCHA 命名规范**（沿用归档主线决议 D15）：正式名 **Minecraft Host Agent**，仓库 `minecraft-host-agent`，简称 **MCHA**（行文）/ **`mcha`**（标识符、命令、路径一律小写）；数据目录 `~/.mcha/`；环境变量 `MCHA_API_KEY` / `MCHA_DATA` / `MCHA_WORKSPACE`。**边界**：作为技术概念的 "Agent"（AI Agent、agent 模块、相关类型名等）不属于产品命名，不改。
- 模块化组织：每完成一个模块必须可编译、可运行，确认无误后再进入下一个（先跑通再美化）。
- 主流程避免 `unwrap()` / `expect()`，错误要显式处理并向上传播。
- 注释、文档、Git 提交信息使用中文；标识符使用英文。

## 工作流约定

- **设计先行（全局强制）**：每一次功能迭代，必须先迭代设计文档（`docs/project-design.md`，含决议表）并向用户汇报变更内容；待用户确认后，才能开始写代码。
- **文档与实现同步**：改动功能时，同步更新 README 与设计文档（`docs/` 下的作业要求文档是课程提供的参考，不要修改）。
- **小步提交**：每完成一个模块建议用户提交 Git，保持改动可回溯；不要主动 commit，先征求用户同意。
- **开销留痕**：与 AI 的完整对话历史和 API 开销（模型名、调用次数、token 数、费用）是课程提交物，涉及统计信息时主动保留、不要丢弃。
- **安全**：API Key 等敏感信息不得写入仓库；配置文件（`.env` / `config.toml`）须加入 `.gitignore` 后再使用。

## 仓库结构

```text
.
├── docs/           # 文档：课程提供的作业要求与参考资料（勿改）+ 本项目自有文档（如选题陈述）
├── experiments/    # 通用 Agent 基线实验（手册与运行记录）
├── src/            # Rust 源代码（当前仅 main.rs 占位）
├── Cargo.toml
├── README.md       # 项目说明（编译 / 配置 / 运行 / 演示）
└── AGENTS.md       # 本文件
```
