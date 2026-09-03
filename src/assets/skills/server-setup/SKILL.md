# Skill：开服决策指南（server-setup）

> 适用任务：在本机搭建一台 Minecraft Java 版服务器并交付给用户（目标：用户
> 给出游戏版本、玩家账号情况、服务端类型后，Agent 自主完成部署，交付一台
> 本机 `127.0.0.1:25565` 可登录游玩的完整服务器）。
>
> 本指南是操作规程：列出哪些决策必须做、按什么规则做、用哪个工具查证、
> 漏了会怎样。执行中的所有副作用都通过工具调用完成。

## 0. 红线（任何时候不得违反）

1. **版本事实不凭记忆**：MC 版本是否存在、需要哪个 Java、下载地址与哈希，
   一律先用 `check_version_compat` 查证，再向用户陈述。Wiki（`wiki_search` /
   `wiki_page`）只能作背景知识与交叉验证，不能作为下载依据。
2. **用户点名即定案**：用户明确指定服务端软件（如"就要 Spigot"）时，按点名
   执行，不得改判；用户没点名时才按本指南推荐（插件玩法 → Paper，模组 →
   Fabric）。
3. **EULA 必问**：写 `eula.txt` 前必须用 `ask_user` 征得用户对 Minecraft EULA
   （https://aka.ms/MinecraftEULA）的同意；`write_server_files` 的
   `eula_accepted=false` 会被拒绝。
4. **离线模式必警示**：`online-mode=false` 时必须向用户说明"任何人可伪装用户
   名进服"的风险，并默认开启白名单；用户明确拒绝白名单时，在 `check_plan`
   中以 `whitelist_disabled_ack=true` 留痕放行。
5. **部署前必过 `check_plan`**：校验未通过不得开始下载与写文件，先补齐缺项。
6. **Mod 与 Forge 边界**：mod 的检索 / 解析 / 安装按 §10 执行（自动化当前
   仅支持 Fabric；数据源 Modrinth 为主，CurseForge 为可选扩展通道）；Forge
   为指导模式（见 §8）。两源都无收录的 mod（如 OptiFine）必须如实说明。

## 1. 信息收集（缺什么问什么，用 `ask_user` 选项式提问）

| 决策项 | 说明 | 缺省处理 |
| --- | --- | --- |
| MC 版本 | 如 1.21.1。用户说"最新版"时用 `check_version_compat` 查证后确认 | 必问 |
| 账号情况 | 全正版 / 全离线 / 混合（决定 online-mode） | 必问 |
| 服务端类型 | 原版（纯净生存）/ Paper（插件）/ Fabric（模组）；点名 Forge 见 §8 | 用户不明确时按玩法推荐 |
| 玩家数量 | 影响 -Xmx 与 max-players | 默认按 ≤5 人 |
| 服务器目录 | 默认 `<工作区>/server/` | 缺省即可 |

不需要问的：Java 版本（查证后自动供给）、端口（默认 25565）、EULA（§0.3 的
专门确认）。

## 2. 查证与方案推导

1. `check_version_compat(mc_version, software)` → 确认版本存在、拿到 Java 大
   版本要求与各渠道可用性。版本不存在时按返回的相近版本建议询问用户。
2. `sys_info()` → 拿总内存 / 可用内存。
3. **-Xmx 推荐公式**：基线 = 玩家 ≤5 人 → 2048MB，6–10 人 → 4096MB；上限 =
   总内存 − 1024MB；下限 512MB。`check_plan` 会按同一公式复核。
4. **online-mode 决策表**：全正版 → `true`；全离线 → `false` + 白名单；混合 →
   `false` + 白名单 + 向用户说明正版账号在离线模式下不会验证。
5. **白名单**：离线模式默认开启；让用户提供白名单玩家名（1–16 字母数字下划
   线），生成时自动计算离线 UUID。
6. 向用户给出**方案摘要**（软件 × 版本 × Java × 内存 × 端口 × online-mode ×
   白名单 + 风险提示），并询问 EULA 同意（§0.3）。

## 3. 部署前校验

调用 `check_plan`（全部方案字段）。未通过时按缺项清单回环：改方案或问用户；
通过后进入执行序。

## 4. 执行序（每步失败即停下处理，见 §7）

```text
check_java(required_major)          # 已有匹配版本则复用
  └─ 无 → ensure_java(major)        # 受管安装，返回 java 绝对路径（记下它）
fetch_server_jar(software, version) # 下载 + 哈希校验 → server/server.jar
write_server_files(...)             # eula / properties / whitelist / start 脚本
start_server(server_dir, ...)        # 独立窗口启动，等待 Done (x.xxx)!
probe_port(mode=connect) / mc_ping  # 连通验证
```

## 5. 交付卡片（部署成功后必须输出）

- 连接地址：`127.0.0.1:25565`（本机游玩）；同局域网好友用主机内网 IP:端口。
- 白名单名单与加人方法（告诉 Agent 名字即可）。
- 手动启动方式：`start.bat`（Windows 双击）/ `start.sh`（Unix）；`start_server`
  弹出的独立窗口与手动双击完全一致，**关闭 mcha 不影响服务器**。
- 停服方法：在服务器窗口按 Ctrl+C（或输入 stop 回车），世界自动保存；
  mcha 不提供远程停服。
- 日志位置：`server/logs/latest.log`；端口占用、Java 版本不符等常见问题直接
  把现象告诉 Agent。
- 离线模式风险提示（若适用）。

## 6. 验收标准

`mc_ping` 返回"MC 服务器响应正常"且 MOTD / 版本 / 人数齐全 → 交付成立。

## 7. 失败处理

- **工具返回结构化错误**：先读错误文本（内含建议），决定重试 / 换渠道 / 问用
  户。下载失败可换渠道（Paper ↔ Fabric 互为替代）；端口占用用 `probe_port`
  换端口。
- **就绪超时**：`start_server` 报"未就绪"时先用 `server_status` 看端口与
  日志尾部（日志毫无新写入通常是 start 脚本中 Java 路径失效）；首次生成世
  界慢属正常，可让用户在服务器窗口停服后用更长超时重来。
- **崩溃**：`start_server` 返回日志尾部——Java 版本不符、端口占用、内存不足
  都在尾部有特征；结合 `check_java` / `sys_info` 定位后修正方案重跑。
- **每一步的结果都如实转述给用户**，不要静默重试超过 2 次。

## 8. Forge 指导模式（D7 修订）

当前版本不提供 Forge 自动安装。用户要 Forge 时：
1. 说明原因（安装链路复杂，后续版本支持）。
2. 给出人工步骤概要：从 files.minecraftforge.net 下载对应版本 installer →
   `java -jar forge-<版本>-installer.jar --installServer`（需先有原版
   server.jar 与匹配 Java）→ 用生成的 run 脚本启动。
3. 建议确认：如果目的是玩模组，Fabric 是否可接受？

## 9. 检索的使用边界

`wiki_search` / `wiki_page`（mcwiki 源）：查版本沿革、Java 版本历史、玩法术
语等背景信息，或交叉验证知识库结论。mcmod 源（MC百科）：mod 的中文名称、
简介与中文语境背景（`source="mcmod"`）。检索结果不得替代 §0.1 的上游查证。

## 10. mod 场景（Fabric）

适用：用户点名要装 mod（清单安装），或描述玩法偏好求推荐。mod 自动化当前
仅支持 Fabric；用户要 Forge 时按 §8 引导。

### 10.1 清单安装（用户点名 mod）

1. 目标环境缺一问一：MC 版本 × Fabric 服务端，两个都必须明确。
2. `resolve_mod(mods=[...], mc_version, loader="fabric")` 一次解析全部：
   中文别名 → Modrinth 精确匹配（零命中自动转 CurseForge）→ 版本匹配 →
   依赖闭包 → 输出意图清单。CurseForge 独占项目（别名表标注
   source="curseforge"，如暮色森林）直接走 CurseForge 通道：已配置 Key 走
   官方 API，未配置自动走国内镜像（免 key，功能完整）。
   - **多命中**：用 `ask_user` 请用户选择后重试，不要擅自替用户挑。
   - **零命中**：两源都没有时工具会如实说明——转述原因，不要假装能装。
   - **依赖不满足**：按错误信息建议调整 MC 版本或换 mod，与用户确认。
3. `check_plan(..., mods=意图清单)` 部署前复核兼容性（第 9 项）。
4. `install_mods(server_dir, manifest=意图清单)` 安装（会触发用户确认门）。
   同名同哈希自动跳过、不同则报冲突——冲突时问用户，绝不覆盖手动安装。
   CurseForge 项以 sha1 校验（强度低于 Modrinth 双哈希，输出会如实标注）；
   若返回"未开放第三方分发"，如实告知用户须从 CurseForge 页面手动下载。
5. 安装后需重启生效：请用户在服务器窗口 Ctrl+C（或输入 stop）停服，确认后
   重新 `start_server`，并用 `server_status` 确认日志出现 mod 加载记录
   （"X mods loaded" 等）。
6. 部署完成后建议 `save_profile` 记录方案与 mod 清单（含版本与哈希），日后
   `load_profile` 即可对照复用。

### 10.2 自然语言推荐（用户描述玩法偏好）

1. `ask_user` 问清偏好：玩法向 / 性能向 / 视觉向、想要什么体验。
2. 把偏好翻译成多组英文关键词，多次调 `search_mods`（如 survival、
   storage、minimap、performance、shaders）。
3. 汇总 3–6 个候选：说明推荐理由、下载量、依赖情况（前置如 Fabric API /
   Cloth Config 需一并装）。
4. `ask_user` 让用户勾选确认 → 走 10.1 的 resolve → check_plan → install。

### 10.3 mod 红线

- 版本兼容事实只信 `resolve_mod` / `check_plan` 的查证结果，不凭记忆。
- 意图清单必须**原样**传给 `install_mods`；下载 URL 与哈希由安装时实时重取
  Modrinth 得到，不要在清单里手写或修改。
- mod 装完必须提示"重启服务器后生效"。
