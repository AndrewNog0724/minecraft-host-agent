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
fn latest_release_options(known_releases: Option<&[String]>) -> Vec<String> {
    let releases = known_releases.unwrap_or(&[]);
    releases.iter().rev().take(5).rev().cloned().collect()
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

    let inferred = if let Some(kind) = kind {
        match kind.as_str() {
            "online" | "online_mode" | "正版" => AccountPolicy::Online,
            "offline" | "离线" => AccountPolicy::Offline {
                whitelist: whitelist_from(answers),
            },
            "hybrid" | "混合" => AccountPolicy::Hybrid {
                auth: HybridAuth::Plugin, // 服务端类型确定后修正（见 apply_software 之后）
                whitelist: whitelist_from(answers),
            },
            _ => AccountPolicy::Online,
        }
    } else if online > 0 && offline > 0 {
        AccountPolicy::Hybrid {
            auth: HybridAuth::Plugin,
            whitelist: whitelist_from(answers),
        }
    } else if offline > 0 {
        AccountPolicy::Offline {
            whitelist: whitelist_from(answers),
        }
    } else if online > 0 {
        AccountPolicy::Online
    } else {
        // 无法判断 → 追问
        questions.push(Question {
            topic: "account_kind".into(),
            text: "朋友们用什么账号玩？(1) 全正版 (2) 全离线/盗版 (3) 混合".into(),
            options: vec!["online".into(), "offline".into(), "hybrid".into()],
        });
        return;
    };

    // offline-mode 必须有白名单（决策树硬性分支），缺失则追问
    let needs_whitelist = matches!(
        &inferred,
        AccountPolicy::Offline { whitelist } | AccountPolicy::Hybrid { whitelist, .. }
            if whitelist.is_empty()
    );
    if needs_whitelist {
        questions.push(Question {
            topic: "whitelist".into(),
            text: "请提供离线玩家的游戏 ID（逗号分隔），将设置白名单".into(),
            options: vec![],
        });
    }
    spec.account = inferred;
}

fn whitelist_from(answers: &Answers) -> Vec<String> {
    answers
        .get("whitelist")
        .map(|s| {
            s.split([',', '，', ' '])
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
            });
        }
        Ok(parsed) => {
            // 存在性校验：只认官方清单（定制 2 验收：幻觉版本号必须被拒）
            let exists_in_manifest = known_releases.is_none_or(|releases| {
                releases.iter().any(|r| {
                    knowledge::normalize_version(r)
                        .map(|v| v == parsed)
                        .unwrap_or(false)
                })
            });
            if !exists_in_manifest {
                let suggestions = known_releases
                    .map(|releases| knowledge::suggest_versions(releases, &requested, 5))
                    .unwrap_or_default();
                questions.push(Question {
                    topic: "mc_version".into(),
                    text: format!("版本 {requested} 不存在于 Mojang 官方清单，请从相近版本中选择"),
                    options: suggestions,
                });
                return;
            }
            spec.mc_version = parsed.to_string();
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
    fn 全离线必须白名单() {
        let draft = PartialSpec {
            account_kind: Some("offline".into()),
            mc_version: Some("1.20.4".into()),
            cross_network: Some(false),
            ..Default::default()
        };
        let out = derive_spec(&draft, &Answers::new(), &kb(), Some(&releases()));
        let TreeOutput::NeedInput { questions, .. } = out else {
            panic!("缺白名单应追问");
        };
        assert!(questions.iter().any(|q| q.topic == "whitelist"));
    }
}
