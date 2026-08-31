//! 决策树引擎（定制 1，§5.2 / §8.5）。
//!
//! 节点 = 检测 + 规则 + 对 `ServerSpec` 的增量写入；信息不足返回 [`Question`]。
//! L0 流程知识固化在这里（决议 D10）：LLM 只负责把缺失措辞成追问，
//! **ServerSpec 的唯一构造者是本模块**（设计原则 1/2）。
//! 不引入通用规则引擎——枚举式节点逐个可解释（NFR-5）。

use std::collections::BTreeMap;

use crate::knowledge::{self, KnowledgeBase, KnowledgeError};
use crate::spec::{
    AccountPolicy, HybridAuth, JavaPlan, JavaRuntime, NetworkPlan, PartialSpec, Question,
    ServerSoftware, ServerSpec, WorldPlan,
};

/// 用户对澄清问题的回答（topic → 回答文本）。
pub type Answers = BTreeMap<String, String>;

/// 从官方清单缓存里取最近 5 个版本作为追问选项。
/// 输入顺序 = Mojang 清单顺序（发布时间倒序，最新在前）。
fn latest_release_options(known_releases: Option<&[String]>) -> Vec<String> {
    let releases = known_releases.unwrap_or(&[]);
    releases.iter().take(5).cloned().collect()
}

/// 决策树一轮推导的输出：要么齐备，要么带追问继续。
#[derive(Debug)]
pub enum TreeOutput {
    Complete(Box<ServerSpec>),
    NeedInput { questions: Vec<Question> },
}

/// 单轮推导入口：输入 = LLM 产出的 [`PartialSpec`] + 历史回答 + 知识库。
/// `known_releases` 是 Mojang 官方正式版清单缓存（None = 本轮不校验存在性，
/// 由流水线预检兜底）；传入了它就能对幻觉版本号给出就近建议（验收要求）。
pub fn derive_spec(
    draft: &PartialSpec,
    answers: &Answers,
    kb: &KnowledgeBase,
    known_releases: Option<&[String]>,
) -> TreeOutput {
    let mut spec = ServerSpec::new(spec_id_for(draft, answers, kb));
    let mut questions: Vec<Question> = Vec::new();

    // ── 节点 1：账号类型 ─────────────────────────────────────────
    apply_account(&mut spec, draft, answers, &mut questions);

    // ── 节点 2：服务端类型（mod 需求 → Fabric；插件 → Paper）──────
    apply_software(&mut spec, draft, answers, &mut questions);

    // ── 节点 3：MC 版本（知识库 + 官方清单校验）──────────────────
    apply_mc_version(&mut spec, draft, answers, known_releases, &mut questions);

    // ── 节点 4：Java 大版本（纯查表，不问用户）───────────────────
    if !spec.mc_version.is_empty() {
        let major = kb.java_major_for(&spec.mc_version).unwrap_or(21);
        spec.java = JavaPlan {
            required_major: major,
            runtime: JavaRuntime::Pending,
        };
        spec.notes.push(format!(
            "MC {} 需要 Java {major}，将由本工具自动供给",
            spec.mc_version
        ));
    }

    // ── 节点 5：玩家数 / JVM 内存（规则推导，不问用户）────────────
    apply_players_and_memory(&mut spec, draft, answers);

    // ── 节点 6：网络拓扑 ─────────────────────────────────────────
    apply_network(&mut spec, draft, answers, &mut questions);

    // ── 节点 7：存档 / 端口（默认值为主）─────────────────────────
    spec.world = WorldPlan::New { seed: None };
    if let Some(port) = answers.get("port").and_then(|p| p.trim().parse().ok()) {
        spec.port = port;
    }

    if questions.is_empty() {
        TreeOutput::Complete(Box::new(spec))
    } else {
        questions.truncate(3); // 单轮最多追问 3 个（§4.2）
        TreeOutput::NeedInput { questions }
    }
}

fn spec_id_for(draft: &PartialSpec, answers: &Answers, kb: &KnowledgeBase) -> String {
    if let Some(id) = answers.get("spec_id") {
        return slugify(id);
    }
    if let Some(id) = &draft.spec_id {
        return slugify(id);
    }
    // 取第一个 mod 做名字：中文别名先翻成可读 slug（"暮色森林"→twilightforest）
    let head = draft
        .mods
        .first()
        .map(|name| kb.alias_slug(name).unwrap_or_else(|| slugify(name)))
        .filter(|s| !s.is_empty() && s != "mc-server")
        .unwrap_or_else(|| "mc".into());
    let players = draft
        .online_players
        .unwrap_or(0)
        .saturating_add(draft.offline_players.unwrap_or(0));
    if players > 0 {
        format!("{head}-{players}p")
    } else {
        head
    }
}

fn slugify(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else if c.is_ascii_whitespace() {
                '-'
            } else {
                '\0'
            }
        })
        .filter(|c| *c != '\0')
        .collect();
    // 中文等非 ASCII 全部丢弃后可能为空，回退默认名
    if out.is_empty() {
        "mc-server".into()
    } else {
        out.trim_matches('-').to_string()
    }
}

/// 账号类回答语义归一化（决议 D20，v0.10.1 实测勘误）：
/// LLM 可能生成英文/描述性选项（如 "All offline (cracked)"），此前精确匹配
/// `"offline"/"离线"` 失败后**静默默认 Online**——离线玩家进服报"无效会话"。
/// 按关键词归一化，顺序即优先级：混合选项（如 "Mixed (some premium, some offline)"）
/// 同时含多个关键词，必须先判 hybrid。
fn classify_account_kind(kind: &str) -> Option<&'static str> {
    let lower = kind.to_lowercase();
    if lower.contains("hybrid") || lower.contains("mix") || kind.contains("混合") {
        Some("hybrid")
    } else if lower.contains("offline") || lower.contains("crack") || kind.contains("离线") {
        Some("offline")
    } else if lower.contains("online") || lower.contains("premium") || kind.contains("正版") {
        Some("online")
    } else {
        None
    }
}

fn apply_account(
    spec: &mut ServerSpec,
    draft: &PartialSpec,
    answers: &Answers,
    questions: &mut Vec<Question>,
) {
    let online = draft.online_players.unwrap_or(0);
    let offline = draft.offline_players.unwrap_or(0);
    let kind = answers
        .get("account_kind")
        .cloned()
        .or_else(|| draft.account_kind.clone());

    // ① 归一化后的显式表态（用户回答或模型转述，决议 D20）
    let explicit = kind.as_deref().and_then(classify_account_kind);
    // ② 归一化失败不静默默认（D20）：有玩家人数线索按人数推断
    let inferred = match explicit {
        Some("online") => Some(AccountPolicy::Online),
        Some("offline") => Some(AccountPolicy::Offline {
            whitelist: whitelist_from(answers),
        }),
        Some("hybrid") => Some(AccountPolicy::Hybrid {
            auth: HybridAuth::Plugin, // 服务端类型确定后修正（见 apply_software 之后）
            whitelist: whitelist_from(answers),
        }),
        _ => {
            if online > 0 && offline > 0 {
                Some(AccountPolicy::Hybrid {
                    auth: HybridAuth::Plugin,
                    whitelist: whitelist_from(answers),
                })
            } else if offline > 0 {
                Some(AccountPolicy::Offline {
                    whitelist: whitelist_from(answers),
                })
            } else if online > 0 {
                Some(AccountPolicy::Online)
            } else {
                None
            }
        }
    };
    let Some(inferred) = inferred else {
        // ③ 无任何线索 → 追问；绝不在用户未确认时开启正版验证（D20）
        questions.push(Question {
            topic: "account_kind".into(),
            text: "朋友们用什么账号玩？(1) 全正版 (2) 全离线/盗版 (3) 混合".into(),
            options: vec!["online".into(), "offline".into(), "hybrid".into()],
            allow_empty: false,
        });
        return;
    };

    // offline-mode 下白名单是防陌生人的建议项（v0.9.5 由必选改为可选）：
    // 未回答过才追问；空回答表示用户明确跳过，尊重其选择不再追问。
    // 两个状态用 "whitelist" 键是否存在于 answers 区分。
    let whitelist_answered = answers.contains_key("whitelist");
    let needs_whitelist = matches!(
        &inferred,
        AccountPolicy::Offline { whitelist } | AccountPolicy::Hybrid { whitelist, .. }
            if whitelist.is_empty()
    ) && !whitelist_answered;
    if needs_whitelist {
        questions.push(Question {
            topic: "whitelist".into(),
            text: "离线模式下任何人可用任意 ID 进入，建议设置白名单。请提供玩家游戏 ID（逗号分隔，回车跳过）".into(),
            options: vec![],
            allow_empty: true,
        });
    }
    spec.account = inferred;
}

/// 解析白名单回答：半角/全角逗号分隔（空格是 ID 的合法字符，不拆分）。
/// 空回答 → 空名单（用户明确跳过）。
fn whitelist_from(answers: &Answers) -> Vec<String> {
    answers
        .get("whitelist")
        .map(|s| {
            s.split([',', '，'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn apply_software(
    spec: &mut ServerSpec,
    draft: &PartialSpec,
    answers: &Answers,
    questions: &mut Vec<Question>,
) {
    let requested = answers
        .get("software")
        .cloned()
        .or_else(|| draft.software.clone());
    let software = match requested.as_deref().map(knowledge::parse_software) {
        Some(Some(sw)) => Some(sw),
        Some(None) => None,
        None => {
            // 未指明时按规则推断：要 mod → Fabric，否则原版
            if draft.mods.is_empty() {
                None
            } else {
                Some(ServerSoftware::Fabric {
                    loader_version: String::new(),
                    installer_version: String::new(),
                })
            }
        }
    };
    match software {
        Some(sw) => {
            // 混合认证方案由服务端类型决定（§5.2）
            if let AccountPolicy::Hybrid { auth: _, whitelist } = &spec.account {
                let resolved = knowledge::hybrid_auth_for(&sw).unwrap_or(HybridAuth::Plugin);
                spec.account = AccountPolicy::Hybrid {
                    auth: resolved,
                    whitelist: whitelist.clone(),
                };
                let auth_name = match resolved {
                    HybridAuth::Plugin => "登录插件（Paper）",
                    HybridAuth::EasyAuth => "EasyAuth（Fabric）",
                };
                spec.notes.push(format!(
                    "混合认证：采用{auth_name}，正版玩家也需按其流程登录"
                ));
            }
            if matches!(sw, ServerSoftware::Fabric { .. }) && spec.mod_names.is_empty() {
                spec.mod_names = draft.mods.clone();
            }
            spec.software = sw;
        }
        None => questions.push(Question {
            topic: "software".into(),
            text: "服务端类型？(1) 原版 vanilla (2) Paper 插件服 (3) Fabric mod 服".into(),
            options: vec!["vanilla".into(), "paper".into(), "fabric".into()],
            allow_empty: false,
        }),
    }
}

fn apply_mc_version(
    spec: &mut ServerSpec,
    draft: &PartialSpec,
    answers: &Answers,
    known_releases: Option<&[String]>,
    questions: &mut Vec<Question>,
) {
    let requested = answers
        .get("mc_version")
        .cloned()
        .or_else(|| draft.mc_version.clone());
    let Some(requested) = requested.filter(|s| !s.trim().is_empty()) else {
        questions.push(Question {
            topic: "mc_version".into(),
            text: "要玩哪个 MC 版本？".into(),
            options: latest_release_options(known_releases),
            allow_empty: false,
        });
        return;
    };

    match knowledge::normalize_version(&requested) {
        Err(KnowledgeError::BadVersion { .. }) | Err(_) => {
            // normalize_version 只会返回 BadVersion；其余分支为类型完备性兜底
            spec.notes.push(format!("版本号 {requested:?} 非法"));
            questions.push(Question {
                topic: "mc_version".into(),
                text: format!("版本号 {requested:?} 不是有效的 MC 正式版本号，请从列表选择"),
                options: latest_release_options(known_releases),
                allow_empty: false,
            });
        }
        Ok(_) => {
            // 存在性校验：只认官方清单（定制 2 验收：幻觉版本号必须被拒）。
            // 规范 id 原则（§8.4 v0.9.6）：命中清单后写**清单原文 id**，
            // 不写归一化串——"26.2.0" 与清单原文 "26.2" 语义相等，但
            // 归一化串回写会让部署 preflight 的清单比对自相矛盾。
            match known_releases.and_then(|r| knowledge::canonicalize_version(r, &requested)) {
                Some(canonical) => {
                    if canonical != requested {
                        spec.notes
                            .push(format!("版本号按官方清单校正：{requested} → {canonical}"));
                    }
                    spec.mc_version = canonical;
                }
                None => {
                    if known_releases.is_none() {
                        // 无清单缓存（离线兜底）：保留用户原文，存在性由部署 preflight 复检
                        spec.mc_version = requested.trim().to_string();
                    } else {
                        let suggestions = knowledge::suggest_versions(
                            known_releases.unwrap_or(&[]),
                            &requested,
                            5,
                        );
                        questions.push(Question {
                            topic: "mc_version".into(),
                            text: format!(
                                "版本 {requested} 不存在于 Mojang 官方清单，请从相近版本中选择"
                            ),
                            options: suggestions,
                            allow_empty: false,
                        });
                    }
                }
            }
        }
    }
}

fn apply_players_and_memory(spec: &mut ServerSpec, draft: &PartialSpec, answers: &Answers) {
    let online = draft.online_players.unwrap_or(0);
    let offline = draft.offline_players.unwrap_or(0);
    let players = if online + offline > 0 {
        online + offline
    } else {
        answers
            .get("max_players")
            .and_then(|s| s.trim().parse().ok())
            .or(draft.max_players)
            .unwrap_or(10)
    };
    spec.max_players = players.max(2);

    // JVM 内存推导：基础 2G + 玩家 >4 加 1G + 有 mod 加 1G；
    // 已知机器内存时不超过其 60%，且至少 1G
    let mut mem_mb: u32 = 2048;
    if players > 4 {
        mem_mb += 1024;
    }
    if !spec.mod_names.is_empty() {
        mem_mb += 1024;
    }
    let machine = draft.machine_memory_mb.or_else(|| {
        answers
            .get("machine_memory_mb")
            .and_then(|s| s.trim().parse().ok())
    });
    if let Some(machine_mb) = machine {
        mem_mb = mem_mb.min((machine_mb as f32 * 0.6) as u32).max(1024);
    }
    spec.jvm_memory_mb = mem_mb;
}

fn apply_network(
    spec: &mut ServerSpec,
    draft: &PartialSpec,
    answers: &Answers,
    questions: &mut Vec<Question>,
) {
    let cross = answers
        .get("cross_network")
        .map(|s| matches!(s.trim(), "y" | "Y" | "yes" | "true" | "1" | "是"))
        .or(draft.cross_network);
    match cross {
        Some(true) => {
            // 有公网 IP 场景给端口映射指引；无公网 IP → 樱花frp（P1 交付编排）
            spec.network = NetworkPlan::Direct {
                firewall_hint:
                    "在路由器上把 TCP/UDP 25565 转发到本机；Windows 防火墙放行 java.exe 入站".into(),
            };
            spec.notes.push(
                "跨网络联机：需要公网 IP 或端口映射；无公网 IP 时使用樱花frp 内网穿透（编排功能随 P1 版本交付，当前版本提供配置指引）".into(),
            );
        }
        Some(false) => spec.network = NetworkPlan::LanOnly,
        None => questions.push(Question {
            topic: "cross_network".into(),
            text: "朋友是否跨网络联机（不在同一个局域网/WiFi）？".into(),
            options: vec!["yes".into(), "no".into()],
            allow_empty: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KnowledgeBase {
        KnowledgeBase::embedded().unwrap()
    }

    fn releases() -> Vec<String> {
        ["1.21.1", "1.21", "1.20.6", "1.20.4", "1.19.2"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// 基线实验 T4 型复合需求："我们 5 个人，2 个正版 3 个离线，想玩带暮色森林的生存"
    #[test]
    fn 复合需求推导覆盖全部必选节点() {
        let draft = PartialSpec {
            online_players: Some(2),
            offline_players: Some(3),
            mods: vec!["暮色森林".into()],
            mc_version: Some("1.21.1".into()),
            ..Default::default()
        };
        // 第一轮：缺白名单与网络信息，必须追问
        let out = derive_spec(&draft, &Answers::new(), &kb(), Some(&releases()));
        let TreeOutput::NeedInput { questions } = out else {
            panic!("缺少白名单与网络信息，应追问");
        };
        let topics: Vec<&str> = questions.iter().map(|q| q.topic.as_str()).collect();
        assert!(topics.contains(&"whitelist"));
        assert!(topics.contains(&"cross_network"));

        // 第二轮：回答齐备后收敛，并覆盖决策树全部必选节点
        let mut answers = Answers::new();
        answers.insert("whitelist".into(), "a, b, c".into());
        answers.insert("cross_network".into(), "yes".into());
        let TreeOutput::Complete(spec) = derive_spec(&draft, &answers, &kb(), Some(&releases()))
        else {
            panic!("信息齐备应收敛");
        };
        assert!(matches!(
            spec.account,
            AccountPolicy::Hybrid {
                auth: HybridAuth::EasyAuth,
                ..
            }
        ));
        assert!(matches!(spec.software, ServerSoftware::Fabric { .. }));
        assert_eq!(spec.mc_version, "1.21.1");
        assert_eq!(spec.java.required_major, 21);
        assert_eq!(spec.max_players, 5);
        assert!(spec.jvm_memory_mb >= 3072, "5 人 + mod 内存应加码");
        let topics: Vec<&str> = questions.iter().map(|q| q.topic.as_str()).collect();
        assert!(topics.contains(&"whitelist"));
        assert!(topics.contains(&"cross_network"));
    }

    #[test]
    fn 幻觉版本号给出建议并被拒() {
        let draft = PartialSpec {
            mc_version: Some("26.2".into()), // 基线实验幻觉样例
            cross_network: Some(false),
            ..Default::default()
        };
        let out = derive_spec(&draft, &Answers::new(), &kb(), Some(&releases()));
        let TreeOutput::NeedInput { questions, .. } = out else {
            panic!("幻觉版本必须触发追问");
        };
        let q = questions.iter().find(|q| q.topic == "mc_version").unwrap();
        assert!(q.text.contains("不存在于 Mojang 官方清单"));
        assert!(!q.options.is_empty(), "必须给可用版本建议");
    }

    /// v0.9.6 实测缺陷回归：归一化串（26.2.0）不得进 spec，
    /// 语义命中清单时写官方原文 id（26.2），否则 preflight 比对自相矛盾。
    #[test]
    fn 归一化串校正为清单原文id() {
        let releases: Vec<String> = ["26.2", "26.1", "1.21.1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mk = |mc: &str| PartialSpec {
            account_kind: Some("offline".into()),
            software: Some("vanilla".into()),
            mc_version: Some(mc.into()),
            cross_network: Some(false),
            ..Default::default()
        };
        let mut answers = Answers::new();
        answers.insert("whitelist".into(), String::new());

        // 26.2.0 → 校正为清单原文 26.2，并留痕
        let TreeOutput::Complete(spec) =
            derive_spec(&mk("26.2.0"), &answers, &kb(), Some(&releases))
        else {
            panic!("26.2.0 语义合法应收敛");
        };
        assert_eq!(spec.mc_version, "26.2", "必须写清单原文而非归一化串");
        assert!(spec.notes.iter().any(|n| n.contains("26.2.0 → 26.2")));
        assert_eq!(spec.java.required_major, 25, "Java 需求查表不受影响");

        // 原文输入原样保留，不加校正痕迹
        let TreeOutput::Complete(spec) = derive_spec(&mk("26.2"), &answers, &kb(), Some(&releases))
        else {
            panic!("清单原文应收敛");
        };
        assert_eq!(spec.mc_version, "26.2");
        assert!(!spec.notes.iter().any(|n| n.contains("校正")));
    }

    /// v0.9.6 同类缺陷回归：清单为新版本在前，追问选项必须取前 5（最新），
    /// 不得反转成最老的 5 个版本。
    #[test]
    fn 版本追问选项取最新一批() {
        let releases: Vec<String> = [
            "26.2", "26.1", "1.21.1", "1.21", "1.20.6", "1.20.4", "1.19.2",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let draft = PartialSpec {
            account_kind: Some("offline".into()),
            software: Some("vanilla".into()),
            cross_network: Some(false),
            ..Default::default()
        };
        let out = derive_spec(&draft, &Answers::new(), &kb(), Some(&releases));
        let TreeOutput::NeedInput { questions, .. } = out else {
            panic!("缺版本应追问");
        };
        let q = questions.iter().find(|q| q.topic == "mc_version").unwrap();
        assert_eq!(
            q.options,
            releases.iter().take(5).cloned().collect::<Vec<_>>()
        );
    }

    #[test]
    fn 回答齐备后完整收敛() {
        let draft = PartialSpec {
            online_players: Some(2),
            offline_players: Some(3),
            mods: vec!["暮色森林".into()],
            mc_version: Some("1.21.1".into()),
            machine_memory_mb: Some(16_384),
            ..Default::default()
        };
        let mut answers = Answers::new();
        answers.insert("whitelist".into(), "alice, bob".into());
        answers.insert("cross_network".into(), "yes".into());
        let out = derive_spec(&draft, &answers, &kb(), Some(&releases()));
        let TreeOutput::Complete(spec) = out else {
            panic!("信息齐备应收敛");
        };
        assert_eq!(spec.spec_id, "twilightforest-5p");
        assert_eq!(
            spec.jvm_memory_mb,
            4096.min((16_384f32 * 0.6) as u32),
            "不超机器内存 60%"
        );
        assert!(matches!(spec.network, NetworkPlan::Direct { .. }));
        assert!(!spec.notes.is_empty(), "离线/混合风险必须进 notes（FR-17）");
    }

    #[test]
    fn 全离线建议白名单且追问可跳过() {
        let draft = PartialSpec {
            account_kind: Some("offline".into()),
            software: Some("vanilla".into()),
            mc_version: Some("1.20.4".into()),
            cross_network: Some(false),
            ..Default::default()
        };
        // 未回答过白名单 → 追问一次，且必须允许留空跳过
        let out = derive_spec(&draft, &Answers::new(), &kb(), Some(&releases()));
        let TreeOutput::NeedInput { questions, .. } = out else {
            panic!("缺白名单应追问");
        };
        let q = questions
            .iter()
            .find(|q| q.topic == "whitelist")
            .expect("应追问白名单");
        assert!(q.allow_empty, "白名单是建议项，必须允许跳过");

        // 空回答 = 明确跳过 → 正常收敛，白名单为空，但风险提示必须保留
        let mut answers = Answers::new();
        answers.insert("whitelist".into(), String::new());
        let out = derive_spec(&draft, &answers, &kb(), Some(&releases()));
        let TreeOutput::Complete(spec) = out else {
            panic!("跳过白名单后应收敛，不得再追问");
        };
        assert!(matches!(
            spec.account,
            AccountPolicy::Offline { whitelist } if whitelist.is_empty()
        ));
        assert!(
            !spec.notes.is_empty(),
            "离线模式风险提示必须进 notes（FR-17）"
        );
    }

    /// v0.10.1 实测回归（决议 D20）：LLM 交卷的英文选项此前被精确匹配拒收后
    /// **静默默认 Online**，离线玩家进服报"无效会话"。归一化后必须落到离线。
    #[test]
    fn 账号英文选项归一化_离线不得误判为正版() {
        assert_eq!(
            classify_account_kind("All offline (cracked)"),
            Some("offline")
        );
        assert_eq!(
            classify_account_kind("All premium (online mode)"),
            Some("online")
        );
        assert_eq!(
            classify_account_kind("Mixed (some premium, some offline)"),
            Some("hybrid"),
            "混合选项同时含 premium/offline 关键词，必须先判 hybrid"
        );
        assert_eq!(classify_account_kind("全离线"), Some("offline"));
        assert_eq!(classify_account_kind("不知道"), None);

        // 端到端：实测载荷形态（英文离线选项）→ 离线策略 + 白名单追问
        let draft = PartialSpec {
            account_kind: Some("All offline (cracked)".into()),
            software: Some("vanilla".into()),
            mc_version: Some("1.20.4".into()),
            cross_network: Some(false),
            ..Default::default()
        };
        let mut answers = Answers::new();
        answers.insert("whitelist".into(), String::new());
        let out = derive_spec(&draft, &answers, &kb(), Some(&releases()));
        let TreeOutput::Complete(spec) = out else {
            panic!("应收敛");
        };
        assert!(
            matches!(spec.account, AccountPolicy::Offline { .. }),
            "英文离线选项不得被静默判为正版验证（无效会话根因）"
        );
    }
}
