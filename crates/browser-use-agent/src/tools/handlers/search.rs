//! `search` tool: a LOCALLY-executed DuckDuckGo (Lite) web search.
//!
//! This is the async re-implementation of the legacy Python `search` action
//! (a `browser_use` `Controller` action that fetched
//! `lite.duckduckgo.com/lite/` over HTTP and parsed the result HTML). Only the
//! *search logic* is ported — the surrounding `Controller` / DB / session
//! scaffolding (and the unrelated `request_human_control` action) are dropped.
//! Like the other handlers it implements the full trait stack
//! ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it can be driven by
//! the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator),
//! mirroring the `tool_search` tool's structure: a non-FS,
//! fetch-parse-and-return tool that spawns no process.
//!
//! # Relationship to [`web_search`](super::web_search)
//!
//! [`web_search`](super::web_search) is the HOSTED, provider-executed web search
//! (the provider runs the search server-side; the client only declares + passes
//! through the result — it performs *no* local HTTP). This `search` tool is the
//! opposite: it performs a REAL local HTTP GET against DuckDuckGo Lite and parses
//! the returned HTML itself, exactly as the Python action did. The two are
//! complementary, not duplicates: `web_search` needs a capable provider; `search`
//! works against any provider because the client does the work.
//!
//! # Network seam (testability)
//!
//! The HTTP fetch lives behind the [`SearchBackend`] trait, with the real
//! [`HttpSearchBackend`] (a `reqwest` client) injected by default and a fake
//! substitutable in tests. This mirrors how the `browser` / `python` / `mcp`
//! handlers inject their backends (`BrowserTool::with_backend`,
//! `McpTool::new(Arc<dyn McpClient>)`), so the tool's parsing/formatting logic is
//! unit-tested deterministically with fixture HTML — no network is touched.
//!
//! # HTML parsing
//!
//! The Python original used BeautifulSoup. This crate intentionally carries no
//! HTML-parser dependency (the existing browser tooling reads the DOM from a real
//! browser over CDP, never by parsing HTML strings), so to keep the dependency
//! footprint unchanged we extract the few fields we need with targeted `regex`
//! over the *specific, stable* DuckDuckGo Lite markup — the same fixed selectors
//! BeautifulSoup keyed on (`a.result-link`, `td.result-snippet`). The extraction
//! is faithful to the Python logic and fully fixture-tested in `search_tests.rs`.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regex::Regex;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// The tool name surfaced to the model.
pub const SEARCH_TOOL_NAME: &str = "search";

/// The DuckDuckGo Lite search endpoint the real backend fetches.
const DDG_LITE_BASE_URL: &str = "https://lite.duckduckgo.com/lite/";

/// Browser-like `User-Agent` (ported verbatim from the Python action's headers).
const DDG_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36";

/// `Accept` header (ported verbatim from the Python action's headers).
const DDG_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/// `Accept-Language` header (ported verbatim from the Python action's headers).
const DDG_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

/// Request timeout (the Python action used `timeout=30.0`).
const SEARCH_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Max characters of a result title in the formatted output. Titles are trimmed
/// (with an ellipsis counted within the cap) to keep the model-facing text token
/// efficient.
const MAX_TITLE_CHARS: usize = 30;

/// Max characters of a result description (snippet) in the formatted output.
const MAX_DESCRIPTION_CHARS: usize = 125;

/// A single parsed search result.
///
/// Mirrors the Python action's `{title, url, description}` dict.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    /// The result's title (the `a.result-link` text).
    pub title: String,
    /// The result's destination URL (the DuckDuckGo redirect, unwrapped).
    pub url: String,
    /// The result's snippet (the following `td.result-snippet` text), if any.
    pub description: String,
}

/// Typed request for the `search` tool.
///
/// Mirrors the Python `SearchParams { query }`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchRequest {
    /// The search query to look up on the web.
    pub query: String,
}

impl SearchRequest {
    /// Convenience constructor from a bare query.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
        }
    }
}

/// An error from the search backend's HTTP fetch.
///
/// Reproduces the failure cases the Python `_search_duckduckgo` raised: a
/// challenge/CAPTCHA page, a non-2xx HTTP status, and a transport error.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// DuckDuckGo returned a challenge/anti-bot page (HTTP 202, or the body
    /// mentions "anomaly").
    #[error(
        "DuckDuckGo is showing a challenge/CAPTCHA – too many requests or suspicious activity."
    )]
    Challenge,
    /// The server returned a client/server error status.
    #[error("HTTP {status}: {snippet}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// The first 200 chars of the response body (matching the Python
        /// `response.text[:200]`).
        snippet: String,
    },
    /// A transport-level error (connection, timeout, decoding).
    #[error("{0}")]
    Request(String),
}

/// The network seam: fetch the raw DuckDuckGo Lite HTML for a query.
///
/// Implemented for real by [`HttpSearchBackend`] and by a fake in tests, so the
/// tool's parsing/formatting can be exercised without a real network — mirroring
/// the `browser` / `python` / `mcp` backend seams.
#[async_trait::async_trait]
pub trait SearchBackend: Send + Sync {
    /// Fetch the DuckDuckGo Lite result HTML for `query`.
    async fn fetch(&self, query: &str) -> Result<String, SearchError>;
}

/// The real [`SearchBackend`]: a `reqwest` client against DuckDuckGo Lite.
pub struct HttpSearchBackend {
    client: reqwest::Client,
    base_url: String,
}

impl HttpSearchBackend {
    /// Construct the backend with a default client and the DuckDuckGo Lite
    /// endpoint.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(SEARCH_REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url: DDG_LITE_BASE_URL.to_string(),
        }
    }
}

impl Default for HttpSearchBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SearchBackend for HttpSearchBackend {
    async fn fetch(&self, query: &str) -> Result<String, SearchError> {
        // `reqwest`'s `.query()` produces application/x-www-form-urlencoded
        // output (space -> `+`); the encoded byte set differs from Python's
        // `quote_plus` on a few characters (e.g. `~`, `*`), but DuckDuckGo
        // decodes both to the same query, so results are equivalent. Redirects
        // are followed by default, matching `follow_redirects=True`.
        let response = self
            .client
            .get(&self.base_url)
            .query(&[("q", query)])
            .header(USER_AGENT, DDG_USER_AGENT)
            .header(ACCEPT, DDG_ACCEPT)
            .header(ACCEPT_LANGUAGE, DDG_ACCEPT_LANGUAGE)
            .send()
            .await
            .map_err(|err| SearchError::Request(err.to_string()))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| SearchError::Request(err.to_string()))?;

        classify_response(status, &body)?;
        Ok(body)
    }
}

/// Classify an HTTP response the way the Python action did: a challenge page
/// (status 202 or an "anomaly" body) first, then any `>= 400` status as an
/// error, otherwise success.
pub fn classify_response(status: u16, body: &str) -> Result<(), SearchError> {
    if status == 202 || body.to_ascii_lowercase().contains("anomaly") {
        return Err(SearchError::Challenge);
    }
    if status >= 400 {
        let snippet: String = body.chars().take(200).collect();
        return Err(SearchError::Http { status, snippet });
    }
    Ok(())
}

/// The async `search` tool.
///
/// Holds the injected [`SearchBackend`]. Cheap to clone (the backend is behind
/// an `Arc`).
#[derive(Clone)]
pub struct SearchTool {
    backend: Arc<dyn SearchBackend>,
}

impl Default for SearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The backend is an opaque trait object; show only the tool identity.
        f.debug_struct("SearchTool").finish_non_exhaustive()
    }
}

impl SearchTool {
    /// Construct the tool backed by the real [`HttpSearchBackend`].
    pub fn new() -> Self {
        Self::with_backend(Arc::new(HttpSearchBackend::new()))
    }

    /// Construct the tool with a custom backend (used by tests).
    pub fn with_backend(backend: Arc<dyn SearchBackend>) -> Self {
        Self { backend }
    }

    /// The tool name surfaced to the model.
    pub fn name(&self) -> &'static str {
        SEARCH_TOOL_NAME
    }
}

/// Approval key: the query identifies a call for session caching, mirroring the
/// shape the other non-FS tools use (`tool_search.rs`, `web_search.rs`). This
/// tool is read-only and benign, so the key is rarely consulted; it exists to
/// satisfy the [`Approvable`] contract uniformly.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct SearchApprovalKey {
    query: String,
}

impl Approvable<SearchRequest> for SearchTool {
    type ApprovalKey = SearchApprovalKey;

    fn approval_keys(&self, req: &SearchRequest) -> Vec<Self::ApprovalKey> {
        vec![SearchApprovalKey {
            query: req.query.clone(),
        }]
    }

    /// `search` touches no filesystem; request the default sandbox permissions
    /// (no escalation), mirroring the other non-FS tools.
    fn sandbox_permissions(&self, _req: &SearchRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }
}

impl Sandboxable for SearchTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide (today everything resolves to
        // `SandboxType::None`). Keeps the seam uniform with the other non-FS
        // tools.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // The tool never produces a sandbox denial, so this is moot; `true` keeps
        // it uniform with the other tools.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<SearchRequest, ExecOutput> for SearchTool {
    fn parallel_safe(&self, _req: &SearchRequest) -> bool {
        // A read-only HTTP GET + pure parse mutates no shared state, so it is safe
        // to run concurrently with other tools — matching the parallel-safe
        // stance of `tool_search` / `web_search`.
        true
    }

    async fn run(
        &self,
        req: &SearchRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox is exercised (the tool does no FS I/O); acknowledge the
        // attempt to make the seam explicit, matching the other tools.
        let _ = attempt;

        let query = req.query.trim();
        if query.is_empty() {
            return Err(ToolError::Rejected(
                "search query must not be empty".to_string(),
            ));
        }

        // A fetch failure is surfaced to the model as a soft error (nonzero exit
        // with the message on stderr), mirroring the Python action's
        // `ActionResult(error="Search failed: …")` and the MCP handler's
        // model-facing error mapping — not a hard tool error.
        match self.backend.fetch(query).await {
            Ok(html) => {
                let results = parse_lite_results(&html);
                let stdout = if results.is_empty() {
                    format!("No results found for \"{query}\".")
                } else {
                    format_results(query, &results)
                };
                Ok(ExecOutput {
                    exit_code: 0,
                    stdout,
                    stderr: String::new(),
                })
            }
            Err(err) => Ok(ExecOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!("Search failed: {err}"),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (parsing + formatting) — ported from the Python action.
// ---------------------------------------------------------------------------

/// Format parsed results into the readable text block the model sees.
///
/// Faithful to the Python action's `extracted_content` layout: a header (count +
/// the "you already have the results" guidance), then a numbered list with each
/// result's title, `URL:` line, and optional snippet, blank-line separated. The
/// title and description are truncated ([`MAX_TITLE_CHARS`] /
/// [`MAX_DESCRIPTION_CHARS`]) for token efficiency; URLs are kept intact so they
/// remain usable.
pub fn format_results(query: &str, results: &[SearchResult]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(results.len() * 4 + 1);
    lines.push(format!(
        "Search results for \"{query}\" ({} results):\n\
         You already have the results below – do NOT navigate to a search engine.\n\
         If these snippets are not enough, navigate directly to the result URLs for more detail.\n",
        results.len()
    ));
    for (i, result) in results.iter().enumerate() {
        lines.push(format!(
            "{}. {}",
            i + 1,
            truncate_chars(&result.title, MAX_TITLE_CHARS)
        ));
        lines.push(format!("   URL: {}", result.url));
        if !result.description.is_empty() {
            lines.push(format!(
                "   {}",
                truncate_chars(&result.description, MAX_DESCRIPTION_CHARS)
            ));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

/// Truncate `text` to at most `max` characters (Unicode scalar values). When it
/// must cut, the last kept character is an ellipsis `…`, so the result is never
/// longer than `max` and the truncation is visible. Trailing whitespace before
/// the ellipsis is trimmed so the text reads cleanly.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    // Reserve one character for the ellipsis.
    let prefix: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", prefix.trim_end())
}

/// Unwrap a DuckDuckGo redirect URL to its real destination.
///
/// Ported from the Python `_extract_real_url`:
/// * protocol-relative `//host/…` gets an `https:` scheme;
/// * a `duckduckgo.com/l/?uddg=…` redirect is unwrapped to its `uddg` target
///   (form-decoded, matching `parse_qs` + `unquote`);
/// * ad links (`duckduckgo.com/y.js`) and non-`http(s)` schemes are dropped
///   (returns `None`).
pub fn extract_real_url(ddg_url: &str) -> Option<String> {
    if ddg_url.is_empty() {
        return None;
    }

    let with_scheme = if let Some(rest) = ddg_url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        ddg_url.to_string()
    };

    let mut url = with_scheme.clone();
    if with_scheme.contains("duckduckgo.com/l/") && with_scheme.contains("uddg=") {
        if let Some(target) = query_param(&with_scheme, "uddg") {
            url = target;
        }
    }

    // Ad links – skip.
    if url.contains("duckduckgo.com/y.js") {
        return None;
    }

    // Only allow http/https to prevent unsafe URLs (javascript:, data:, …).
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }

    Some(url)
}

/// Collapse runs of whitespace into a single space and trim the ends.
///
/// Ported from the Python `_normalize_whitespace`
/// (`re.sub(r"\s+", " ", text).strip()`).
pub fn normalize_whitespace(text: &str) -> String {
    whitespace_regex()
        .replace_all(text.trim(), " ")
        .into_owned()
}

/// Parse search results out of a DuckDuckGo Lite HTML response.
///
/// Ported from the Python `_parse_lite_results`: for each `a.result-link`, take
/// its (entity-decoded) text as the title and unwrap its `href`; skip empty /
/// "more info" / duplicate / `duckduckgo.com` results; and attach the snippet
/// from the first following `td.result-snippet` that precedes the next result
/// link.
pub fn parse_lite_results(html: &str) -> Vec<SearchResult> {
    let anchors = collect_anchors(html);
    let snippets = collect_snippets(html);

    let mut results: Vec<SearchResult> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (idx, anchor) in anchors.iter().enumerate() {
        if anchor.title.is_empty() || anchor.title.eq_ignore_ascii_case("more info") {
            continue;
        }

        let Some(url) = extract_real_url(&anchor.href) else {
            continue;
        };
        if seen.contains(&url) || is_duckduckgo_result_host(&url) {
            continue;
        }
        seen.insert(url.clone());

        // The snippet is the first `result-snippet` after this anchor and before
        // the next one (matching the Python sibling-walk that stops at the next
        // result link).
        let next_pos = anchors.get(idx + 1).map_or(usize::MAX, |a| a.pos);
        let description = snippets
            .iter()
            .find(|s| s.pos > anchor.pos && s.pos < next_pos)
            .map(|s| s.text.clone())
            .unwrap_or_default();

        results.push(SearchResult {
            title: anchor.title.clone(),
            url,
            description,
        });
    }

    results
}

fn is_duckduckgo_result_host(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"))
}

/// A raw `a.result-link` extracted from the HTML, with its byte offset.
struct RawAnchor {
    pos: usize,
    href: String,
    title: String,
}

/// A raw `td.result-snippet` extracted from the HTML, with its byte offset.
struct RawSnippet {
    pos: usize,
    text: String,
}

/// Extract every `a.result-link` anchor (offset, href, title) in document order.
fn collect_anchors(html: &str) -> Vec<RawAnchor> {
    anchor_regex()
        .captures_iter(html)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            let attrs = caps.get(1).map_or("", |m| m.as_str());
            let inner = caps.get(2).map_or("", |m| m.as_str());
            if !has_class(attrs, "result-link") {
                return None;
            }
            Some(RawAnchor {
                pos: whole.start(),
                href: attr_value(attrs, AttrName::Href).unwrap_or_default(),
                // Strip tags, decode entities, then trim. DuckDuckGo Lite titles
                // are plain text, so this matches the Python `get_text(strip=True)`
                // title extraction; on any inline markup it yields the cleaner
                // space-preserving text rather than BeautifulSoup's node-join.
                title: text_from_html(inner, "").trim().to_string(),
            })
        })
        .collect()
}

/// Extract every `td.result-snippet` (offset, normalized text) in document order.
fn collect_snippets(html: &str) -> Vec<RawSnippet> {
    td_regex()
        .captures_iter(html)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            let attrs = caps.get(1).map_or("", |m| m.as_str());
            let inner = caps.get(2).map_or("", |m| m.as_str());
            if !has_class(attrs, "result-snippet") {
                return None;
            }
            Some(RawSnippet {
                pos: whole.start(),
                // `get_text(separator=" ")` then normalize whitespace.
                text: normalize_whitespace(&text_from_html(inner, " ")),
            })
        })
        .collect()
}

/// Strip HTML tags (replacing each with `separator`) and decode entities.
fn text_from_html(html: &str, separator: &str) -> String {
    let without_tags = tag_regex().replace_all(html, separator);
    decode_entities(&without_tags)
}

/// Whether a tag's attribute string declares `class` containing `class_name`.
fn has_class(attrs: &str, class_name: &str) -> bool {
    attr_value(attrs, AttrName::Class)
        .is_some_and(|value| value.split_whitespace().any(|c| c == class_name))
}

/// The attributes we extract from a tag.
#[derive(Clone, Copy)]
enum AttrName {
    Href,
    Class,
}

/// Extract a quoted attribute value from a tag's attribute string.
fn attr_value(attrs: &str, name: AttrName) -> Option<String> {
    let re = match name {
        AttrName::Href => href_regex(),
        AttrName::Class => class_regex(),
    };
    re.captures(attrs)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// Read a single query parameter's value, form-decoded (matching `parse_qs`:
/// `+` becomes a space and `%XX` is percent-decoded).
fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    // Drop any fragment before splitting pairs.
    let query = query.split('#').next().unwrap_or(query);
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode_form(v));
        }
    }
    None
}

/// Form-decode a query component: `+` -> space, `%XX` -> byte, then UTF-8.
fn percent_decode_form(value: &str) -> String {
    let spaced = value.replace('+', " ");
    let bytes = spaced.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Hex digit value of an ASCII byte, or `None`.
fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode the common HTML character references in one pass.
///
/// Covers the named references that appear in DuckDuckGo snippets plus all
/// numeric references (`&#NN;` / `&#xHH;`); unknown named references are left
/// intact (BeautifulSoup decodes the full set — this is the practical subset).
fn decode_entities(text: &str) -> String {
    entity_regex()
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let body = &caps[1];
            if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
                return decode_codepoint(u32::from_str_radix(hex, 16).ok())
                    .unwrap_or_else(|| caps[0].to_string());
            }
            if let Some(dec) = body.strip_prefix('#') {
                return decode_codepoint(dec.parse::<u32>().ok())
                    .unwrap_or_else(|| caps[0].to_string());
            }
            match body {
                "amp" => "&",
                "lt" => "<",
                "gt" => ">",
                "quot" => "\"",
                "apos" => "'",
                "nbsp" => " ",
                // Typographic punctuation.
                "hellip" => "…",
                "mdash" => "—",
                "ndash" => "–",
                "rsquo" => "\u{2019}",
                "lsquo" => "\u{2018}",
                "rdquo" => "\u{201D}",
                "ldquo" => "\u{201C}",
                "laquo" => "«",
                "raquo" => "»",
                "middot" => "·",
                "bull" => "•",
                // Common symbols.
                "copy" => "©",
                "reg" => "®",
                "trade" => "™",
                "times" => "×",
                "divide" => "÷",
                "deg" => "°",
                "euro" => "€",
                "pound" => "£",
                "cent" => "¢",
                "sect" => "§",
                // Common Western-European accented letters.
                "aacute" => "á",
                "agrave" => "à",
                "acirc" => "â",
                "auml" => "ä",
                "aring" => "å",
                "ccedil" => "ç",
                "eacute" => "é",
                "egrave" => "è",
                "ecirc" => "ê",
                "euml" => "ë",
                "iacute" => "í",
                "iuml" => "ï",
                "ntilde" => "ñ",
                "oacute" => "ó",
                "ocirc" => "ô",
                "ouml" => "ö",
                "uacute" => "ú",
                "uuml" => "ü",
                "szlig" => "ß",
                // Unknown named reference: leave the original text intact
                // (BeautifulSoup decodes the full HTML5 set; this is the
                // practical subset DuckDuckGo emits, plus all numeric refs).
                _ => return caps[0].to_string(),
            }
            .to_string()
        })
        .into_owned()
}

/// Map a numeric character-reference code point to its string, if valid.
fn decode_codepoint(code: Option<u32>) -> Option<String> {
    code.and_then(char::from_u32).map(|c| c.to_string())
}

// --- Cached regexes (compiled once; patterns are constant) -----------------
//
// The tag regexes use `[^>]*` for the attribute span, which assumes attribute
// values contain no literal `>` — true for the fixed DuckDuckGo Lite markup
// (see the module doc). On non-conforming markup a `>` inside an attribute
// value would truncate the match (dropping that result), never panic.

fn anchor_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<a\b([^>]*)>(.*?)</a>").expect("valid anchor regex"))
}

fn td_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<td\b([^>]*)>(.*?)</td>").expect("valid td regex"))
}

fn tag_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<[^>]*>").expect("valid tag regex"))
}

fn href_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(?:^|\s)href\s*=\s*["']([^"']*)["']"#).expect("valid href regex")
    })
}

fn class_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(?:^|\s)class\s*=\s*["']([^"']*)["']"#).expect("valid class regex")
    })
}

fn whitespace_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").expect("valid whitespace regex"))
}

fn entity_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"&(#[0-9]+|#[xX][0-9a-fA-F]+|[a-zA-Z][a-zA-Z0-9]*);")
            .expect("valid entity regex")
    })
}
