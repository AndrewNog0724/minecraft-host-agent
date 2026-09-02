//! 领域检索通道（决议 D120，设计 §8.11）：wiki_search / wiki_page。
//!
//! 来源经 `[retrieval]` 配置注册：mcwiki（B站 MC Wiki，标准 MediaWiki
//! API，M2.1）与 mcmod（HTML 解析，M2.2 接入）。定位红线：Wiki 结果只作
//! 背景知识 / 交叉验证，版本存在性与下载事实以上游 API 为权威。

use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;

use crate::agent::message::ToolOutcome;
use crate::knowledge::upstream::urlencode;

use super::{Tool, ToolCtx, ToolError};

/// 检索请求超时。
const RETRIEVAL_TIMEOUT: Duration = Duration::from_secs(20);
/// search 默认条数上限。
const SEARCH_LIMIT_DEFAULT: u32 = 5;
/// page 默认字符上限。
const PAGE_MAX_CHARS_DEFAULT: usize = 6000;
/// parse 原始 HTML 的护栏上限（防超大页面拖垮内存）。
const RAW_HTML_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WikiSearchArgs {
    /// 检索来源（mcwiki；mcmod 将随 M2.2 接入）
    pub source: String,
    /// 关键词（中文或英文）
    pub query: String,
    /// 返回条数上限（默认 5，最多 20）
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WikiPageArgs {
    /// 检索来源（mcwiki；mcmod 将随 M2.2 接入）
    pub source: String,
    /// 页面标题（来自 wiki_search 的结果）
    pub title: String,
    /// 返回正文字符上限（默认 6000）
    #[serde(default)]
    pub max_chars: Option<usize>,
}

/// 来源 → MediaWiki API 基址；未配置 / 未接入时返回说明性错误。
fn api_base(ctx: &ToolCtx, source: &str) -> Result<String, String> {
    match source {
        "mcwiki" if !ctx.retrieval.mcwiki.trim().is_empty() => {
            Ok(ctx.retrieval.mcwiki.trim_end_matches('/').to_string())
        }
        "mcwiki" => Err("检索来源 mcwiki 未配置（config [retrieval] mcwiki）".to_string()),
        "mcmod" => Err("MC百科检索后端随 M2.2（mod 场景包）接入，当前不可用；可用 http_get_text 手工查阅 search.mcmod.cn".to_string()),
        other => Err(format!(
            "未知检索来源「{other}」；可用：mcwiki（mcmod 将随 M2.2 接入）"
        )),
    }
}

/// MediaWiki 搜索响应（list=search）。
#[derive(Debug, Deserialize)]
struct SearchResponse {
    query: Option<SearchQuery>,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    search: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    title: String,
    snippet: String,
}

/// 去掉文本中的 HTML 标签：注释与 script/style 整块移除；`<br>` 与常见
/// 块级闭标签转换行；其余标签剥除后解码实体、压缩空行。
pub(crate) fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    loop {
        let Some(lt) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            match rest.find("-->") {
                Some(end) => rest = &rest[end + 3..],
                None => break,
            }
        } else if rest.starts_with("<script") {
            match rest.find("</script>") {
                Some(end) => rest = &rest[end + "</script>".len()..],
                None => break,
            }
        } else if rest.starts_with("<style") {
            match rest.find("</style>") {
                Some(end) => rest = &rest[end + "</style>".len()..],
                None => break,
            }
        } else {
            let Some(gt) = rest.find('>') else { break };
            let tag = rest[..gt + 1].to_ascii_lowercase();
            // 块级边界转换行，保持文本可读
            if tag.starts_with("<br")
                || tag.starts_with("</p")
                || tag.starts_with("</div")
                || tag.starts_with("</li")
                || tag.starts_with("</h")
                || tag.starts_with("</tr")
            {
                out.push('\n');
            }
            rest = &rest[gt + 1..];
        }
    }
    collapse_blank_lines(&decode_entities(&out))
}

/// 解码常见 HTML 实体（含数字形式），不认识的原样保留。
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(semi) = rest.find(';') else {
            out.push('&');
            break;
        };
        if semi > 10 {
            out.push('&');
            rest = &rest[1..];
            continue;
        }
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "nbsp" => Some(' '),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|code| {
                    // `#x1F600` 十六进制；`#39` 十进制
                    if let Some(hex) = code.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        code.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 压缩空行（3 行以上连续空行压成 2 行）并去掉行首尾空白。
fn collapse_blank_lines(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut blanks = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blanks += 1;
            if blanks > 2 {
                continue;
            }
        } else {
            blanks = 0;
        }
        collapsed.push_str(trimmed);
        collapsed.push('\n');
    }
    collapsed
}

/// fetch 带超时的文本响应（状态码非 2xx 返回错误）。
async fn get_text(ctx: &ToolCtx, url: &str) -> Result<String, String> {
    let response = tokio::time::timeout(RETRIEVAL_TIMEOUT, ctx.http.get(url).send())
        .await
        .map_err(|_| format!("请求超时（{url}）"))?
        .map_err(|err| format!("请求失败（{url}）：{err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}（{url}）", status.as_u16()));
    }
    tokio::time::timeout(RETRIEVAL_TIMEOUT, response.text())
        .await
        .map_err(|_| format!("读取响应超时（{url}）"))?
        .map_err(|err| format!("读取响应失败：{err}"))
}

/// wiki_search：MediaWiki `action=query&list=search`。
async fn search(ctx: &ToolCtx, api: &str, query: &str, limit: u32) -> Result<String, String> {
    let url = format!(
        "{api}?format=json&action=query&list=search&srsearch={}&srlimit={limit}",
        urlencode(query)
    );
    let text = get_text(ctx, &url).await?;
    let response: SearchResponse =
        serde_json::from_str(&text).map_err(|err| format!("解析搜索结果失败：{err}"))?;
    let hits = response.query.map(|q| q.search).unwrap_or_default();
    if hits.is_empty() {
        return Ok(format!(
            "MC Wiki 中未找到「{query}」相关页面；可换关键词重试。"
        ));
    }
    let mut lines = vec![format!(
        "MC Wiki 检索「{query}」（{count} 条）：",
        count = hits.len()
    )];
    for hit in hits {
        let clean = strip_html(&hit.snippet);
        let clean = clean.replace('\n', " ");
        let page_url = format!(
            "https://wiki.biligame.com/mc/{}",
            urlencode(hit.title.replace(" ", "_").as_str())
        );
        lines.push(format!("- {}：{}（{page_url}）", hit.title, clean));
    }
    Ok(lines.join("\n"))
}

/// wiki_page：MediaWiki `action=parse` 取 HTML 后去标签（B站 Wiki 无 extracts）。
async fn page(ctx: &ToolCtx, api: &str, title: &str, max_chars: usize) -> Result<String, String> {
    let url = format!(
        "{api}?format=json&action=parse&page={}&prop=text&redirects=1",
        urlencode(title)
    );
    let text = get_text(ctx, &url).await?;
    if text.len() > RAW_HTML_LIMIT {
        return Ok(format!(
            "页面「{title}」过大（{} KB），已拒绝解析",
            text.len() / 1024
        ));
    }
    #[derive(Debug, Deserialize)]
    struct ParseResponse {
        parse: Option<ParseBody>,
    }
    #[derive(Debug, Deserialize)]
    struct ParseBody {
        text: ParseText,
    }
    #[derive(Debug, Deserialize)]
    struct ParseText {
        #[serde(rename = "*")]
        html: String,
    }
    let response: ParseResponse =
        serde_json::from_str(&text).map_err(|err| format!("解析页面响应失败：{err}"))?;
    let html = response
        .parse
        .map(|p| p.text.html)
        .ok_or_else(|| format!("页面「{title}」不存在（或为特殊页面）"))?;
    let plain = strip_html(&html);
    let total = plain.chars().count();
    if total > max_chars {
        let head: String = plain.chars().take(max_chars).collect();
        return Ok(format!(
            "「{title}」全文 {total} 字符，已截断到 {max_chars}（可提高 max_chars 或分节查询）：\n{head}"
        ));
    }
    Ok(format!("「{title}」（{total} 字符）：\n{plain}"))
}

#[async_trait::async_trait]
impl Tool for WikiSearchTool {
    fn name(&self) -> &'static str {
        "wiki_search"
    }
    fn description(&self) -> String {
        "检索 MC 中文 Wiki（B站镜像）：返回标题、摘要与页面链接。用于背景知识、版本沿革与中文语境问题；版本存在性 / 下载事实仍以上游 API 为权威。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WikiSearchArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: WikiSearchArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let api = match api_base(ctx, &args.source) {
            Ok(api) => api,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let limit = args.limit.unwrap_or(SEARCH_LIMIT_DEFAULT).clamp(1, 20);
        match search(ctx, &api, &args.query, limit).await {
            Ok(content) => Ok(ToolOutcome::ok(content)),
            Err(reason) => Ok(ToolOutcome::err(reason)),
        }
    }
}

pub struct WikiSearchTool;

#[async_trait::async_trait]
impl Tool for WikiPageTool {
    fn name(&self) -> &'static str {
        "wiki_page"
    }
    fn description(&self) -> String {
        "读取 MC 中文 Wiki 页面正文（纯文本，自动截断）。标题来自 wiki_search 结果。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WikiPageArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: WikiPageArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let api = match api_base(ctx, &args.source) {
            Ok(api) => api,
            Err(reason) => return Ok(ToolOutcome::err(reason)),
        };
        let max_chars = args
            .max_chars
            .unwrap_or(PAGE_MAX_CHARS_DEFAULT)
            .clamp(500, 20000);
        match page(ctx, &api, &args.title, max_chars).await {
            Ok(content) => Ok(ToolOutcome::ok(content)),
            Err(reason) => Ok(ToolOutcome::err(reason)),
        }
    }
}

pub struct WikiPageTool;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags_and_scripts() {
        let html = r#"<div class="x"><script>alert(1)</script><p>正文一</p><style>.a{}</style><p>正文<b>二</b></p><!-- 注释 --></div>"#;
        let text = strip_html(html);
        assert!(text.contains("正文一"));
        assert!(text.contains("正文二"));
        assert!(!text.contains("alert"));
        assert!(!text.contains(".a{}"));
        assert!(!text.contains("注释"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a&nbsp;b"), "a b");
        assert_eq!(decode_entities("x&#39;y"), "x'y");
        assert_eq!(decode_entities("A&amp;B &lt;tag&gt;"), "A&B <tag>");
        assert_eq!(
            decode_entities("bad &unknown; entity"),
            "bad &unknown; entity"
        );
    }

    #[test]
    fn snippet_newlines_collapsed() {
        let text = strip_html("<p>一行</p>\n\n\n\n<p>两行</p>");
        assert!(text.lines().count() <= 4);
    }

    #[tokio::test]
    #[ignore = "真实上游冒烟：cargo test --ignored"]
    async fn live_wiki_search_finds_results() {
        let (tx, _rx) = crate::events::event_channel();
        let root = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            workspace: root.path().to_path_buf(),
            data_dir: root.path().join("data"),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
        };
        let outcome = WikiSearchTool
            .run(
                serde_json::json!({ "source": "mcwiki", "query": "Java 版本" }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("检索应成功：{outcome:?}");
        };
        assert!(content.contains("wiki.biligame.com"), "{content}");
    }

    #[tokio::test]
    async fn mcmod_source_reports_not_ready() {
        let (tx, _rx) = crate::events::event_channel();
        let root = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            workspace: root.path().to_path_buf(),
            data_dir: root.path().join("data"),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
        };
        let outcome = WikiSearchTool
            .run(
                serde_json::json!({ "source": "mcmod", "query": "暮色森林" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "mcmod 未接入应如实报错：{outcome:?}");
    }
}
