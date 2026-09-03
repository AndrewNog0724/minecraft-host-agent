# MCHA 设计决策记录

> 本文档独立维护项目的全部设计决议与文档修订历史，面向开发过程与答辩备查。
> 面向用户的正式设计见 `docs/project-design.md`，其中不出现决议编号与版本注释。

## 编号规则

- **D1–D10、D15**：自旧版设计沿用，继续有效（个别条目有修订，见表内标注）。
- **D100 起**：v2 重构（Agent-First）新增决议。
- **D11–D14、D16–D29**：属归档实现主线（`archive/v0.12-mvp-line`），不再使用，避免编号歧义。

## 沿用决议

| # | 决议 | 内容 | v2 状态 |
| --- | --- | --- | --- |
| D1 | 界面形态 | CLI/TUI；Web（只读状态页）列 P2 | 沿用，深化为多轮会话（D101） |
| D2 | Java 供给 | 全自动受管安装，不降级；设计见 project-design §8.7 | 沿用，执行主体变为 `ensure_java` 工具 |
| D3 | 价格表 | 内置常见模型预设随包分发，注明来源与日期，用户可覆盖 | 沿用 |
| D4 | 数据目录 | `~/.mcha/`（Windows：`%APPDATA%\mcha\`；原名 `~/.mc-host-agent/`，D15 定名时统一更名，不迁移旧目录） | 沿用 |
| D5 | 基岩跨平台 | 不做 | 沿用 |
| D6 | 提案提交方式 | ~~tool-calling（`submit_spec` 工具）交卷~~ | **废除**：ServerSpec 不再是流程关卡（D105）；结构化提交精神由工具参数 Schema 普遍承载 |
| D7 | Forge | MVP 不做；P2 或降级为安装指导 | **修订**（2026-09-02，随 v2.0）：分步纳入——M2.1 实现 Vanilla / Paper / Spigot / Fabric 四渠道，Forge 走指导模式（Skill 写明人工步骤概要 + 建议确认 Fabric 替代），正式 Forge 渠道 M2.x 补 |
| D8 | LLM SDK | 不引入，自研薄客户端 | 沿用 |
| D9 | 内网穿透选型 | 樱花frp 默认；自建 frp / Tailscale 为 P2 备选；playit 不做 | 沿用，编排主体变为 Agent + `tunnel_*` 工具 |
| D10 | 定制内容体系 | 五层载体（代码/数据/API/指南/Prompt + 错误模式库）；不引入 RAG / embedding | **修订**：L0"决策树进 Rust 代码"废除，Skills 升为一级公民（D104）；其余沿 project-design §8.5 |
| D15 | 命名规范 | 正式名 **Minecraft Host Agent**，仓库 `minecraft-host-agent`，简称 **MCHA**（行文）/**`mcha`**（标识符）；Cargo 包名 `minecraft-host-agent`，二进制/CLI 命令 `mcha`；数据目录 `~/.mcha/`；环境变量 `MCHA_API_KEY` / `MCHA_DATA` / `MCHA_WORKSPACE`；技术概念 "Agent" 不属于产品命名不改 | 沿用 |

## v2 新决议

| # | 决议 | 内容 |
| --- | --- | --- |
| D100 | **Agent-First 总纲** | Agent Loop 是唯一执行引擎、全阶段运行；一切副作用由 LLM 经工具调用发起、Rust 工具后端执行；Rust 职责 = Loop + 工具 + 校验 + 安全边界，不做任务编排。旧 NFR-1（"LLM 只做需求理解、副作用不出 Rust"）废除。废因：归档线产品被定性为"LLM 问询员 + 自动化脚本"，非 Agent |
| D101 | 多轮会话式交互 | CLI REPL 为主形态，会话持续、无任务轮次边界；`mcha new "…"` 为预填首条消息的快捷方式；废除"≤3 轮澄清"上限；失控防护改由轮数保险丝（默认 40）+ 预算守卫承担 |
| D102 | 先框架、后场景 | M1 实现通用 Agent 框架（Loop + 通用工具 + REPL + R5/R6 骨架），M2 叠加开服场景包（领域工具 + 知识库 + Skills + 提示词）；禁止在框架就绪前先写场景流程代码 |
| D103 | web_search 后端 | 工具接口 P0 定义；默认无后端（如实告知用户），可选 DuckDuckGo HTML 抓取（免 key，脆弱性自担）或 Serper 等（配 key）；领域事实主通道为知识库 + 上游 API |
| D104 | Skills 定制内容 | 领域流程知识写成 `assets/skills/<name>/SKILL.md`（决策指南、诊断规程、操作手册），`load_skill` 按需加载；替代旧"决策树 Rust 引擎"；保留红线"版本事实不进 Prompt" |
| D105 | ServerSpec 降格 | ServerSpec 保留为部署档案（Profile）格式，由 Agent 经 `save_profile` 工具落盘、`load_profile` 读回复用；不再是决策树输出物或流程关卡 |
| D106 | 确认门与路径收敛 | 工具按 Permission 分级（ReadOnly/Write/Execute/Network）；写与执行默认确认、可按会话授权；文件路径收敛工作区与数据目录；`confirm_level` 可配（paranoid/standard/auto） |
| D107 | CLI 会话界面 | inline 滚动流（不采用全屏 TUI）+ 语义化渲染块（思考 / 工具调用 / 结果 / 确认 / 用量分块，规范见 project-design §8.6）；风格对标 Claude Code / Codex；渲染器为事件总线订阅者，与 store 落盘同源；M1 交付 |
| D108 | 用量展示时机 | 会话过程中不刷用量行；会话结束（`/exit` / Ctrl-D / 提示符处 Ctrl-C）时打印一次汇总（输入 / 输出 token、费用、时长）+ 轨迹文件路径。R6"清晰展示"由三层满足：退出汇总、`mcha usage` 全局账本、`sessions show` 每次调用明细；预算告警 / 超限中断即时提示例外。计量层（UsageRecord 逐调用强制生成）不受影响 |
| D109 | 回合原子性 | 上下文裁剪以完整回合为单位；打断时未完成的 tool_calls 回填"用户中断"错误后入史，半截 assistant 文本丢弃——会话消息流任何时刻结构合法，可直接导出 / 恢复 |
| D110 | 确认门交互粒度 | 确认弹窗三选一：y 本次允许 / a 本会话允许此工具 / n 拒绝（拒绝结构化回传，Agent 自行调整）；auto 模式全免（演示 / CI） |
| D111 | 部署前确定性校验 | `check_plan` 工具：Agent 部署前调用，按 checklist 确定性核对方案完整性（online-mode 与账号一致、Java 匹配、内存、白名单配套等）；"不漏分支"由决策指南约束 + 确定性验收双保险 |
| D112 | LLM 客户端细则 | 429/5xx 指数退避重试 ≤2 次且计入调用次数；连接 15s / 单次整体 300s；thinking 计量照收入输出 token、思考全文不入史（渲染后仅留占位）；价格表缺失模型费用记 0 并标注 |
| D113 | 配置与首次启动 | 首次运行检测无配置自动进入 `setup` 向导：必填仅 3 项（endpoint 预设快捷项 / 模型名 / API Key 隐藏输入写 .env），其余默认值；完成后自动连接测试（最小对话请求验证连通与延迟）再进入会话；`config set`（toml_edit 保注释）/ `list` / `test` 子命令；启动校验缺失项打印可复制模板。继承归档主线两段式向导的实测设计（原 D12/D14 思路） |
| D114 | 渲染块折叠与可读性 | 真机试用反馈的 UI 细节定稿：① 思考流直显超 4 行折叠（2000 字符保险丝），"已思考 Ns"在思考结束时立即挂出（不迟到到正文末尾）；② 工具调用行加动词短语（run_command → "运行命令"）；③ 命令输出直显超 12 行省略并注明行数，结果摘要压单行 + 渲染层 120 字符保险丝；④ 回合内跨板块自动补空行、提示符前留空行。**折叠 / 截断只影响终端显示**：完整内容照常落盘（R5）与回传模型 |
| D115 | 下载镜像默认 | Mojang 资源默认走 BMCLAPI（域名重写实现），Adoptium 二进制默认清华 TUNA；`[network]` 配置段可切 `off` / 自定义；实际使用的域记入轨迹；白名单域同步扩充（§8.10/§12）。裁定理由：国内直连不稳定，默认保首次成功率 |
| D116 | sys_info 工具 | 新增只读环境探测工具（OS / 架构 / 总内存 / 可用内存 / CPU 核数，sysinfo crate）：-Xmx 推荐依据，后续诊断复用；免确认、跨平台、可单测 |
| D117 | Java 供给拆分 | `check_java`（ReadOnly 探测：PATH / JAVA_HOME / 受管目录）与 `ensure_java`（Network 安装）拆为两个工具——确认门是工具级静态属性，混装会迫使纯探测也弹确认 |
| D118 | 服务器交付语义 | start_server 托管进程由 mcha 管理（Drop 守卫防孤儿，mcha 退出即停）；服务器长期运行以交付的 start.bat / start.sh 为准——"Agent 演示期间管理，最终交付物是目录 + 脚本"；下载 jar 统一改名 server.jar（原始名入轨迹） |
| D119 | 离线白名单 ack | check_plan 中"离线模式配白名单"为默认要求；用户明确拒绝时 Agent 传 `whitelist_disabled_ack=true` 表示已确认风险并放行留痕——确定性闸门与用户意愿两全 |
| D120 | 领域检索通道 | 新增 `wiki_search` / `wiki_page`（ReadOnly）+ `[retrieval]` 来源注册；mcwiki 后端（MediaWiki API，实测可用）随 M2.1 落地；mcmod 后端（HTML 解析，无官方 API）随 M2.2 mod 步骤落地、规格本批写入 §8.11。事实优先级红线扩展：版本存在性 / 下载 URL / 哈希以上游 API 为权威，Wiki / 百科定位为背景知识、交叉验证与中文语境补充 |
| D121 | M2 分步实施 | M2.1 = 服务器设施（四渠道 + 检索通道 + Skill + 场景提示词），出口标准 US1 精简版（版本 + 账号 + 端类型 → 127.0.0.1:25565 可登录）；mod 安装（FR-12/13）、Profile（FR-16 save/load）、穿透（FR-17）、诊断（FR-18）后置 M2.2 / M2.3。裁定理由：小步迭代、决策细致不跑偏 |

## 设计文档修订历史

| 日期 | 版本 | 说明 |
| --- | --- | --- |
| 2026-08-28 | v0.1a | 需求文档初版（原 `project-requirements.md`） |
| 2026-08-28 | v0.1b | 设计文档初版（原设计草稿） |
| 2026-08-28 | v0.2 | 合并为单一活文档；决议 D1–D8；新增 Java 自动供给完整设计 |
| 2026-08-28 | v0.3 | 决议 D9：樱花frp 为穿透默认方案 |
| 2026-08-28 | v0.4 | 新增定制内容分层体系（决议 D10） |
| 2026-09-01 | v0.4.1 | 全局定名规范化（沿用归档主线决议 D15） |
| 2026-09-01 | v1.0 | **v2 重构（Agent-First）**：废除旧 NFR-1 与决策树 Rust 引擎（D6 废除、D10 修订）；确立 Agent Loop 唯一执行引擎、工具调用唯一副作用通道（D100）；多轮会话式交互（D101）；先框架后场景路线（D102）；新增决议 D103–D106；架构、数据流、模块全面重写；Java 供给与穿透编排细节保留为工具后端设计。同日：决议记录与修订历史移出设计文档，独立维护于本文档 |
| 2026-09-01 | v1.1 | 设计文档面向最终用户精简：移除与早期方案的对比性叙述；取消 P0–P2 优先级分层；FR-12 拆分为 mod 清单安装（FR-12）与 mod 自然语言推荐（FR-13），后续编号顺延（原 FR-13–18 → FR-14–19）；故障诊断（定制 4）标记为选做 |
| 2026-09-01 | v1.2 | 新增会话界面设计（D107）：inline 滚动流 + 语义化渲染块规范（§4.2 效果示意、§8.6 详设），FR-06 扩充；选型补 crossterm / termimad |
| 2026-09-01 | v1.3 | 用量展示调整（D108）：会话中不再逐回合显示用量行，改为退出时一次性汇总；明确退出方式（`/exit` / Ctrl-D / Ctrl-C）；FR-08 与 §4.2/§8.6 同步 |
| 2026-09-01 | v1.4 | 设计细化评审：回合原子性与打断语义（D109）；确认门 y/a/n 粒度（D110）；新增 `check_plan` 部署前校验工具（D111）；LLM 客户端重试 / 思考模式 / 价格缺失细则（D112）；run_command / edit_file / web_search / ask_user 语义细化；Profile 字段草案恢复；白名单离线 UUID 实测教训留痕；终端符号降级、斜杠命令与会话恢复交互；Skills 指南以中文撰写 |
| 2026-09-01 | v1.5 | 配置与首次启动设计补全（D113）：setup 向导（必填 3 项 + 连接测试）、配置文件全景（[model]/[[prices]]/[budget]/[safety]/[search]）、config set/list/test、启动校验；FR-05 与 §9 R3 同步 |
| 2026-09-01 | v1.6 | 系统提示词分层澄清：框架级 system prompt（通用角色 + 工具纪律 + 安全规则）属 M1 交付，场景提示词（开服管家角色 + Skills 清单）属 M2；最终 system prompt = 框架基础段（固定）+ 场景段（注入），§8.5 L4 与 §14 同步 |
| 2026-09-01 | v1.7 | **M1 实现期技术选型落定**（§10 表同步）：新增 async-trait（Tool / Interaction / LlmClient 的 dyn 对象安全需要）与 futures-util（Stream 驱动）；取消令牌改为手写 AtomicBool + Notify（约 40 行，答辩可解释）；termimad 移出 M1 选型（Markdown 静态块渲染属 M2 方案摘要场景，届时引入）。M1 Agent 框架实现完成：Loop + 通用工具集 + REPL + R5/R6 骨架，49 项单元 / Loop 级测试，出口标准链路经本地 mock OpenAI SSE 服务端到端验证 |
| 2026-09-01 | v1.8 | 渲染可读性定稿（D114）：思考折叠 / 动词解释 / 输出省略 / 板块空行，§8.6 表同步；实测另修复两处环境缺陷——Windows 下 `mask.rs` 类型推断歧义（E0283）、setup 连接测试嵌套 tokio 运行时 panic |
| 2026-09-01 | v1.9 | 二次 UI 细节：横幅改为分组排版（标题 / 空行 / 三行信息 / 操作提示，去掉工具枚举）；"无价格预设"提示从会话中移至退出汇总一次性给出；确认门改单键（crossterm raw 模式免回车，Esc/Ctrl-C 取消，Windows 只认 Press 事件，非 tty 自动退化行读取）；确认门前空行经新增 `Event::Blank` 由渲染器顺序保证（确认块由交互线程直接打印，跨线程时序脆弱）；"已思考 Ns"补齐斜体样式并加固事件时机（任何非思考增量都立即收起思考段）；实测发现清华平台在正文分块上附带 `reasoning_content: ""`，空字符串需按字段缺失处理（否则思考段一直不闭合、"已思考"迟到到正文末尾），已加 SSE 回归测试 |
| 2026-09-02 | v2.0 | **M2.1 服务器设施方案定稿并落入实现细则**：新增决议 D115–D121（镜像默认 BMCLAPI/TUNA、sys_info、check_java/ensure_java 拆分、服务器交付语义、离线白名单 ack、领域检索通道、M2 分步）；D7 修订为 Forge 分步纳入（M2.1 指导模式）；设计文档新增 §8.10（服务器设施实现细节：版本比较器 / 渠道与镜像表 / 文件生成 / 进程生命周期 / 连通验证 / check_plan checklist）与 §8.11（检索通道：wiki_search / wiki_page，mcwiki 随 M2.1、mcmod 规格留档 M2.2）；FR-20 新增（领域检索通道）；§8.2/8.4/8.5/8.6/8.7/10/11/12/13/14 同步；选型新增 sysinfo / md-5 / scraper（M2.2） |
| 2026-09-02 | v2.1 | **M2.1 服务器设施实现完成（S1–S9）**：knowledge 模块（MC 版本比较器、java_compat / server_software TOML、Mojang / Paper / Fabric / Adoptium 上游客户端）+ 14 个领域工具（check_version_compat / sys_info / check_java / ensure_java / fetch_server_jar / write_server_files / start_server / stop_server / server_status / probe_port / mc_ping / check_plan / wiki_search / wiki_page）；实现期实测修正并同步文档：① Paper 旧 v2 API 已 410 下线，迁移 Fill v3（§8.10/§8.4/§11）；② 26.x 年份版本线为 Java 25（官方 javaVersion 实测），java_compat 收拢 1.20.5 开区间并新增 26.x 区间；③ B站 Wiki 无 TextExtracts，正文走 parse+去标签；④ 清华 TUNA 拒绝空 UA（403），MCHA 统一 UA=mcha/<版本>；⑤ start_server spawn 对 ETXTBSY 重试（NFR-3）；新增依赖 zip/tar/flate2/sha1/md-5/sysinfo。验证：97 项单元 / Loop 级测试 + 10 项真实上游冒烟全过（JRE 受管安装、四渠道下载含哈希校验、wiki 检索、SLP ping、错误版本就近建议、离线集成流程） |
| 2026-09-03 | v2.2 | **确认门可用性修复（用户实测反馈）**：① 弹窗内容空白——通用摘要生成器只认识 run_command / write_file / http_download 的字段名，领域工具参数名不同取不到值；修复：`Tool` trait 新增 `confirm_summary` 覆写点（§8.2 确认门内容的工具自述），write_server_files / ensure_java / fetch_server_jar / start_server / stop_server 五个工具展示方案要点（目录 / 渠道 / online-mode / 端口 / 白名单 / 命令行），框架层双兜底（覆写为空 → 通用规则 → 参数摘要，绝不弹空框）；② 无操作引导——单键确认路径弹框后直接等待按键，用户不知道按什么；修复：弹框后打印"请按一个键：[y] 本次允许 · [a] 本会话允许此工具 · [n] 拒绝 · [Esc/Ctrl-C] 取消"。回归测试 5 项（各工具确认内容非空且含关键信息） |
| 2026-09-03 | v2.3 | **终端交互四项修复（用户实测反馈）**：① ask_user 提示语反复换行、键入不可见——dialoguer 绘制与渲染线程 / spinner steady-tick 并发输出互相打乱；修复：交互激活闸（`ui_active`，交互期间渲染器停靠排队）+ ask_user 不叠加 spinner（交互控件独占终端），§8.6 同步；② 交付语后日志直接刷出——start_server 读者任务就绪后停止向事件流发送并**放弃发送端**（缓冲保留供 server_status），§8.6 同步；③ 回合结束后无法回到提示符、Ctrl-D 无反应、Ctrl-C 直接杀进程无用量汇总——根因是日志读者长期持有事件发送端，`renderer.await` 永不返回，REPL 卡死在回合收尾（此时 Ctrl-D 无渲染、二次 Ctrl-C 走默认行为杀进程）；①②的放弃发送端修复即解除卡死，提示符恢复后提示符处 Ctrl-D（EOF）/ Ctrl-C（D108 汇总退出）按既有设计生效；④ 上下文窗口默认 128k → **256k**（config 默认值 / 模板 / 文档样例 / README 同步）。新增回归测试：读者放弃发送端、配置默认值 |
