//! mcmod（MC百科）检索后端（设计 §8.11/§8.12）：`wiki_search` / `wiki_page`
//! 的 `source=mcmod` 实现。
//!
//! 无官方 API：搜索页（`search.mcmod.cn/s?key=`）与主页
//! （`www.mcmod.cn/class|item/<id>.html`）均为服务端渲染 HTML，以 CSS 选择器
//! 解析。定位红线：背景知识与中文语境补充；版本存在性 / 下载事实以上游
//! API（Modrinth 等）为权威。请求间最小间隔自律；解析失败结构化报错。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use scraper::{Html, Selector};

use crate::knowledge::upstream::send_get;
use crate::knowledge::upstream::urlencode;

/// 单请求超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// 请求间自律间隔（无官方 API，礼貌抓取）。
const MIN_INTERVAL: Duration = Duration::from_millis(800);
/// 主页基址（URL 前缀强约束：仅此域的 class/item 详情页）。
const WWW_BASE: &str = "https://www.mcmod.cn";

/// 请求前自律等待：返回需要 sleep 的时长（锁内计算，锁外等待）。
async fn throttle() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let wait = {
        let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let wait = match *last {
            Some(at) => MIN_INTERVAL
                .checked_sub(at.elapsed())
                .unwrap_or(Duration::ZERO),
            None => Duration::ZERO,
        };
        *last = Some(Instant::now());
        wait
    };
    if wait > Duration::ZERO {
        tokio::time::sleep(wait).await;
    }
}

/// fetch 带超时的文本响应（状态码非 2xx 返回错误）。
async fn get_text(http: &reqwest::Client, url: &str) -> Result<String, String> {
    throttle().await;
    let response = tokio::time::timeout(REQUEST_TIMEOUT, send_get(http, url))
        .await
        .map_err(|_| format!("请求超时（{url}）"))?
        .map_err(|err| format!("请求失败（{url}）：{err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}（{url}）", status.as_u16()));
    }
    tokio::time::timeout(REQUEST_TIMEOUT, response.text())
        .await
        .map_err(|_| format!("读取响应超时（{url}）"))?
        .map_err(|err| format!("读取响应失败：{err}"))
}

/// 清理 MC百科正文中的站内标记（`[mark:title_menu]`、`[h1=…]` 等 bbcode
/// 风格标签）与不间断空格。
pub(crate) fn clean_mcmod_text(text: &str) -> String {
    let no_tags = strip_mcmod_marks(text);
    no_tags
        .replace('\u{a0}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 剥除 `[xxx]` 与 `[xxx=yyy]` 形式的站内标记（内容须为字母开头的标记名，
/// 如 `[mark:title_menu]`、`[h1=…]`；`[0]` 等普通方括号文本保留）。
fn strip_mcmod_marks(text: &str) -> String {
    let is_mark = |content: &str| {
        let mut chars = content.chars();
        match chars.next() {
            Some(first) if first.is_ascii_alphabetic() => chars.all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '=' | '-' | '.' | '/')
            }),
            _ => false,
        }
    };
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        match after.find(']') {
            Some(close) if close <= 40 && is_mark(&after[1..close]) => {
                rest = &after[close + 1..];
            }
            _ => {
                // 未闭合、过长或不像标记：按普通文本保留
                out.push('[');
                rest = &rest[open + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// 搜索
// ---------------------------------------------------------------------------

/// 搜索结果条目。
pub(crate) struct McmodHit {
    pub title: String,
    pub intro: String,
    pub url: String,
}

/// 从搜索结果页 HTML 提取条目（结构见 `.search-result-list .result-item`：
/// `.head a` 为标题与详情链接、`.body` 为简介）。
pub(crate) fn parse_search_html(html: &str) -> Vec<McmodHit> {
    let document = Html::parse_document(html);
    let Ok(item_sel) = Selector::parse(".search-result-list .result-item") else {
        return Vec::new();
    };
    // 标题链接是 .head 的直接子元素（分类链接嵌在 .class-category 内，需排除）
    let Ok(link_sel) = Selector::parse(".head > a") else {
        return Vec::new();
    };
    let Ok(body_sel) = Selector::parse(".body") else {
        return Vec::new();
    };
    document
        .select(&item_sel)
        .filter_map(|item| {
            let link = item.select(&link_sel).next()?;
            let title = clean_mcmod_text(&link.text().collect::<String>());
            if title.is_empty() {
                return None;
            }
            let href = link.value().attr("href")?;
            let url = normalize_page_url(href)?;
            let intro = item
                .select(&body_sel)
                .next()
                .map(|b| clean_mcmod_text(&b.text().collect::<String>()))
                .unwrap_or_default();
            Some(McmodHit { title, intro, url })
        })
        .collect()
}

/// `wiki_search(source=mcmod)`：搜索页解析，返回文本清单。
pub(crate) async fn search(
    http: &reqwest::Client,
    search_base: &str,
    query: &str,
    limit: usize,
) -> Result<String, String> {
    if search_base.trim().is_empty() {
        return Err("检索来源 mcmod 未启用（config [retrieval] mcmod 设为空即禁用）".to_string());
    }
    let url = format!(
        "{}/s?key={}",
        search_base.trim_end_matches('/'),
        urlencode(query)
    );
    let html = get_text(http, &url).await?;
    let hits = parse_search_html(&html);
    if hits.is_empty() {
        return Ok(format!(
            "MC百科 未检索到「{query}」相关条目；可换关键词重试。"
        ));
    }
    let mut lines = vec![format!(
        "MC百科 检索「{query}」（{count} 条）：",
        count = hits.len().min(limit)
    )];
    for hit in hits.iter().take(limit) {
        let short_intro: String = hit.intro.chars().take(80).collect();
        lines.push(format!(
            "- {title} ｜ {intro}｜ {url}",
            title = hit.title,
            intro = short_intro,
            url = hit.url
        ));
    }
    lines.push("MC百科 结果只作背景知识；版本存在性与下载事实以上游 API 为权威。".to_string());
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// 主页摘要
// ---------------------------------------------------------------------------

/// 归一化主页 URL：仅允许 `www.mcmod.cn/class|item/<id>(.html)` 形态。
/// 接受搜索结果里的完整 URL、`class/2021.html`、`class/2021`、`2021` 等写法。
pub(crate) fn normalize_page_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // 去 scheme + 域（必须为 www.mcmod.cn）
    let path = if let Some(rest) = trimmed
        .strip_prefix("https://www.mcmod.cn/")
        .or_else(|| trimmed.strip_prefix("http://www.mcmod.cn/"))
        .or_else(|| trimmed.strip_prefix("//www.mcmod.cn/"))
    {
        rest.to_string()
    } else if trimmed.starts_with('/') {
        trimmed.trim_start_matches('/').to_string()
    } else if trimmed.starts_with("http") || trimmed.contains("://") {
        // 其他域：拒绝
        return None;
    } else {
        // 裸 id 或 class/2021 形式
        trimmed.to_string()
    };
    let path = path.split(['?', '#']).next()?.trim_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        [kind, id_file] if *kind == "class" || *kind == "item" => {
            let id = id_file.trim_end_matches(".html");
            if !id.bytes().all(|b| b.is_ascii_digit()) || id.is_empty() {
                return None;
            }
            Some(format!("{WWW_BASE}/{kind}/{id}.html"))
        }
        [id] if id.bytes().all(|b| b.is_ascii_digit()) => {
            Some(format!("{WWW_BASE}/class/{id}.html"))
        }
        _ => None,
    }
}

/// 从主页 HTML 提取摘要：`<title>` + meta description + 长段落聚合。
pub(crate) fn parse_page_html(html: &str) -> (String, String) {
    let document = Html::parse_document(html);
    let title = document
        .select(&Selector::parse("title").expect("固定选择器"))
        .next()
        .map(|t| clean_mcmod_text(&t.text().collect::<String>()))
        .unwrap_or_default();
    let meta = document
        .select(&Selector::parse(r#"meta[name="description"]"#).expect("固定选择器"))
        .next()
        .and_then(|m| m.value().attr("content"))
        .map(clean_mcmod_text)
        .unwrap_or_default();
    // 摘要级正文：文档顺序聚合足够长的段落（导航 / 菜单均为短文本或链接列表）
    let p_sel = Selector::parse("p").expect("固定选择器");
    let paragraphs = document.select(&p_sel);
    let mut body = String::new();
    for p in paragraphs {
        let text = clean_mcmod_text(&p.text().collect::<String>());
        if text.chars().count() < 20 {
            continue;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&text);
        if body.chars().count() > 6000 {
            break;
        }
    }
    (
        title,
        if meta.is_empty() {
            body
        } else {
            format!("{meta}\n\n{body}")
        },
    )
}

/// `wiki_page(source=mcmod)`：主页摘要（截断到 max_chars）。
pub(crate) async fn page(
    http: &reqwest::Client,
    title: &str,
    max_chars: usize,
) -> Result<String, String> {
    let url = normalize_page_url(title).ok_or_else(|| {
        format!("无效的 MC百科 页面标识「{title}」；应来自 wiki_search 结果的链接")
    })?;
    let html = get_text(http, &url).await?;
    let (page_title, summary) = parse_page_html(&html);
    let total = summary.chars().count();
    let header = if page_title.is_empty() {
        url.clone()
    } else {
        page_title
    };
    if total > max_chars {
        let head: String = summary.chars().take(max_chars).collect();
        return Ok(format!(
            "「{header}」（共 {total} 字，已截断到 {max_chars}，可提高 max_chars）：\n{head}"
        ));
    }
    Ok(format!("「{header}」（{total} 字）：\n{summary}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按真实页面结构裁剪的搜索结果 fixture（2026-09-03 实测结构）。
    const SEARCH_FIXTURE: &str = r#"
<html><body>
<p class="info">找到约 10624 条结果。</p>
<div class="search-result-list">
  <div class="result-item">
    <div class="head">
      <div class="class-category"><ul><li><a class="c_1" href="//www.mcmod.cn/class/category/1-1.html"></a></li></ul></div>
      <a target="_blank" href="https://www.mcmod.cn/class/2021.html">机械动力 (<em>Create</em>)</a>
    </div>
    <div class="body">[mark:title_menu]6.0 版本&nbsp;已经更新！[h1=欢迎]机械动力（<em>Create</em>）是一个围绕着建筑、装饰和机械的新兴科技模组，为玩家提供全新的建筑与自动化体验。</div>
    <div class="foot"><span class="info"><span>地址：</span><span class="value"><a href="https://www.mcmod.cn/class/2021.html">www.mcmod.cn/class/2021.html</a></span></span></div>
  </div>
  <div class="result-item">
    <div class="head"><a target="_blank" href="//www.mcmod.cn/item/330128.html">应力</a></div>
    <div class="body">Create 的能源单位，用于机械供能。</div>
  </div>
</div>
</body></html>"#;

    #[test]
    fn search_fixture_parses_items() {
        let hits = parse_search_html(SEARCH_FIXTURE);
        assert_eq!(hits.len(), 2, "应解析出两个条目");
        assert_eq!(hits[0].title, "机械动力 (Create)");
        assert_eq!(hits[0].url, "https://www.mcmod.cn/class/2021.html");
        assert!(hits[0].intro.starts_with("6.0 版本"), "{}", hits[0].intro);
        assert!(!hits[0].intro.contains("[mark"), "{}", hits[0].intro);
        assert!(!hits[0].intro.contains('\u{a0}'), "nbsp 应被替换");
        assert_eq!(hits[1].url, "https://www.mcmod.cn/item/330128.html");
    }

    #[test]
    fn page_url_normalization_accepts_only_mcmod_shapes() {
        assert_eq!(
            normalize_page_url("https://www.mcmod.cn/class/2021.html").as_deref(),
            Some("https://www.mcmod.cn/class/2021.html")
        );
        assert_eq!(
            normalize_page_url("//www.mcmod.cn/class/2021.html").as_deref(),
            Some("https://www.mcmod.cn/class/2021.html")
        );
        assert_eq!(
            normalize_page_url("class/2021").as_deref(),
            Some("https://www.mcmod.cn/class/2021.html")
        );
        assert_eq!(
            normalize_page_url("2021").as_deref(),
            Some("https://www.mcmod.cn/class/2021.html")
        );
        // 其他域 / 非数字 id / 其他路径形态 → 拒绝
        assert!(normalize_page_url("https://evil.example.com/class/1.html").is_none());
        assert!(normalize_page_url("class/abc.html").is_none());
        assert!(normalize_page_url("post/1.html").is_none());
        assert!(normalize_page_url("javascript:alert(1)").is_none());
    }

    #[test]
    fn clean_strips_marks_and_collapses() {
        assert_eq!(
            clean_mcmod_text("[mark:title_menu]你好[mc=1]世界"),
            "你好世界"
        );
        // scraper 的 text() 已把 &nbsp; 解码为 U+00A0，此处按解码后输入测试
        assert_eq!(clean_mcmod_text("a\u{a0}b\u{a0}\u{a0}c"), "a b c");
        // 未闭合的方括号按普通文本保留
        assert_eq!(clean_mcmod_text("数组 [0] 写法"), "数组 [0] 写法");
    }

    #[test]
    fn page_fixture_extracts_title_meta_paragraphs() {
        let html = r#"<html><head><title>机械动力 (Create) - MC百科</title>
        <meta name="description" content="模组机械动力 (Create)的介绍页" /></head>
        <body><nav><p>短导航</p></nav>
        <div><p>机械动力是一个围绕着建筑、装饰和机械的新兴科技模组，为玩家提供全新的建筑与自动化体验，并且尽可能预留自定义空间。</p></div>
        </body></html>"#;
        let (title, summary) = parse_page_html(html);
        assert_eq!(title, "机械动力 (Create) - MC百科");
        assert!(summary.contains("介绍页"), "{summary}");
        assert!(summary.contains("建筑与自动化体验"), "{summary}");
        assert!(!summary.contains("短导航"), "短段落应被过滤：{summary}");
    }

    /// 真实上游冒烟（`cargo test --ignored`）。
    /// search.mcmod.cn 偶发抖动（无官方 SLA），失败重试一次再判定。
    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_mcmod_search_finds_create() {
        let http = reqwest::Client::builder()
            .user_agent("mcha/0.2")
            .build()
            .unwrap();
        let mut last_err = String::new();
        for _ in 0..2 {
            match search(&http, "https://search.mcmod.cn", "create", 5).await {
                Ok(content) => {
                    assert!(content.contains("机械动力"), "{content}");
                    return;
                }
                Err(reason) => last_err = reason,
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        panic!("MC百科检索两次均失败：{last_err}");
    }
}
