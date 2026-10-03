//! Web backend: Brave or Tavily search and a page extractor. Keys come from the environment and
//! are never logged or echoed in errors.

use crate::config::SecurityConfig;
use crate::url_safety::{
    blocked_host_in_text, host_is_blocked, is_safe_url, sensitive_query_param_name,
};
use async_trait::async_trait;
use silver_core::services::{WebBackend, WebResult};
use silver_core::{CoreError, CoreResult};
use std::time::Duration;

/// Brave free-tier web search endpoint.
const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
/// Tavily search endpoint.
const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
/// Both vendors cap the requested result count server-side.
const MAX_RESULTS: usize = 20;
/// Per-request wall-clock cap so a hanging host cannot stall a tool call.
const REQUEST_TIMEOUT_SECS: u64 = 60;
/// Redirect hops followed by hand, with every target re-validated.
const MAX_REDIRECTS: usize = 5;

/// Search provider resolved from the environment. The key stays inside this value.
#[derive(Clone)]
enum SearchProvider {
    Brave(String),
    Tavily(String),
}

/// Daemon web backend. Search prefers Brave, then Tavily; extract uses plain HTTP.
pub struct HttpWebBackend {
    client: reqwest::Client,
    security: SecurityConfig,
}

impl HttpWebBackend {
    pub fn new() -> Self {
        Self::with_security(&SecurityConfig::default())
    }

    /// Build with the configured TLS policy, redirect policy and website blocklist.
    pub fn with_security(security: &SecurityConfig) -> Self {
        let client = crate::config::http_client_builder(security)
            .and_then(|builder| {
                builder
                    .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(anyhow::Error::from)
            })
            .unwrap_or_default();
        Self {
            client,
            security: SecurityConfig::clone(security),
        }
    }

    /// Apply scheme, SSRF, blocklist and sensitive-query policy to one URL.
    async fn check_url(&self, url: &str) -> CoreResult<()> {
        let host = reqwest::Url::parse(url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(|host| host.to_ascii_lowercase()))
            .unwrap_or_default();
        if !host.is_empty() && host_is_blocked(&host, &self.security.website_blocklist) {
            return Err(CoreError::ToolNotAllowed(format!(
                "website_blocked: {host} is on the website blocklist"
            )));
        }
        let sensitive = self
            .security
            .block_sensitive_query_urls
            .then(|| sensitive_query_param_name(url))
            .flatten();
        if let Some(name) = sensitive {
            return Err(CoreError::ToolNotAllowed(format!(
                "url_blocked: URL query parameter '{name}' may carry a credential"
            )));
        }
        is_safe_url(url)
            .await
            .map_err(|error| CoreError::ToolNotAllowed(error.to_string()))
    }

    async fn search_brave(
        &self,
        key: &str,
        query: &str,
        limit: usize,
    ) -> CoreResult<Vec<WebResult>> {
        let count = limit.clamp(1, MAX_RESULTS).to_string();
        let response = self
            .client
            .get(BRAVE_ENDPOINT)
            .query(&[("q", query.to_string()), ("count", count)])
            .header("X-Subscription-Token", key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|err| {
                CoreError::ProviderUnavailable(format!("Brave search request failed: {err}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(CoreError::ProviderUnavailable(format!(
                "Brave search returned HTTP {status}"
            )));
        }
        let body: serde_json::Value = response.json().await.map_err(|err| {
            CoreError::ProviderUnavailable(format!("Brave search returned invalid JSON: {err}"))
        })?;
        Ok(parse_brave_results(&body, limit))
    }

    async fn search_tavily(
        &self,
        key: &str,
        query: &str,
        limit: usize,
    ) -> CoreResult<Vec<WebResult>> {
        let payload = serde_json::json!({
            "query": query,
            "max_results": limit.clamp(1, MAX_RESULTS),
            "include_raw_content": false,
            "include_images": false,
        });
        let response = self
            .client
            .post(TAVILY_ENDPOINT)
            .bearer_auth(key)
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                CoreError::ProviderUnavailable(format!("Tavily search request failed: {err}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(CoreError::ProviderUnavailable(format!(
                "Tavily search returned HTTP {status}"
            )));
        }
        let body: serde_json::Value = response.json().await.map_err(|err| {
            CoreError::ProviderUnavailable(format!("Tavily search returned invalid JSON: {err}"))
        })?;
        Ok(parse_tavily_results(&body, limit))
    }

    async fn fetch_text(&self, url: &str, max_bytes: usize) -> CoreResult<String> {
        self.check_url(url).await?;
        let mut current = url.to_string();
        // Redirects are followed by hand so every hop re-runs the SSRF/blocklist policy.
        for _ in 0..=MAX_REDIRECTS {
            let response = self.client.get(&current).send().await.map_err(|err| {
                CoreError::ProviderUnavailable(format!("web extract request failed: {err}"))
            })?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        CoreError::ProviderUnavailable(format!(
                            "web extract redirect from {current} carried no Location header"
                        ))
                    })?;
                let next = resolve_location(&current, location)?;
                self.check_url(&next).await?;
                current = next;
                continue;
            }
            if !status.is_success() {
                return Err(CoreError::ProviderUnavailable(format!(
                    "web extract returned HTTP {status}"
                )));
            }
            let body = read_capped(response, max_bytes).await?;
            return Ok(html_to_text(&String::from_utf8_lossy(&body)));
        }
        Err(CoreError::ToolNotAllowed(format!(
            "url_blocked: too many redirects (>{MAX_REDIRECTS}) from {url}"
        )))
    }
}

impl Default for HttpWebBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WebBackend for HttpWebBackend {
    async fn search(&self, query: &str, limit: usize) -> CoreResult<Vec<WebResult>> {
        if let Some(host) = blocked_host_in_text(query, &self.security.website_blocklist) {
            return Err(CoreError::ToolNotAllowed(format!(
                "website_blocked: {host} is on the website blocklist"
            )));
        }
        match provider_from_env()? {
            SearchProvider::Brave(key) => self.search_brave(&key, query, limit).await,
            SearchProvider::Tavily(key) => self.search_tavily(&key, query, limit).await,
        }
    }

    async fn extract(&self, url: &str, max_bytes: usize) -> CoreResult<String> {
        self.fetch_text(url, max_bytes).await
    }
}

/// Resolve a possibly-relative Location header against the current URL.
fn resolve_location(current: &str, location: &str) -> CoreResult<String> {
    let base = reqwest::Url::parse(current).map_err(|error| {
        CoreError::ToolNotAllowed(format!("url_blocked: invalid URL {current}: {error}"))
    })?;
    base.join(location)
        .map(|url| url.to_string())
        .map_err(|error| {
            CoreError::ToolNotAllowed(format!(
                "url_blocked: invalid redirect target {location}: {error}"
            ))
        })
}

fn provider_from_env() -> CoreResult<SearchProvider> {
    let brave = std::env::var("BRAVE_API_KEY").ok();
    let tavily = std::env::var("TAVILY_API_KEY").ok();
    select_provider(brave.as_deref(), tavily.as_deref())
}

/// Pick Brave when its key is present, then Tavily; otherwise a clear configuration error.
fn select_provider(
    brave_key: Option<&str>,
    tavily_key: Option<&str>,
) -> CoreResult<SearchProvider> {
    if let Some(key) = non_empty(brave_key) {
        return Ok(SearchProvider::Brave(key.to_string()));
    }
    if let Some(key) = non_empty(tavily_key) {
        return Ok(SearchProvider::Tavily(key.to_string()));
    }
    Err(CoreError::ToolNotAllowed(
        "no web search provider configured: set BRAVE_API_KEY or TAVILY_API_KEY".into(),
    ))
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// Read at most max_bytes of the response body without buffering the whole page.
async fn read_capped(mut response: reqwest::Response, max_bytes: usize) -> CoreResult<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    while body.len() < max_bytes {
        let Some(chunk) = response.chunk().await.map_err(|err| {
            CoreError::ProviderUnavailable(format!("web extract read failed: {err}"))
        })?
        else {
            break;
        };
        let remaining = max_bytes - body.len();
        let take = chunk.len().min(remaining);
        body.extend_from_slice(&chunk[..take]);
    }
    Ok(body)
}

/// Map a Brave response to search hits. Brave returns web.results[].description.
fn parse_brave_results(body: &serde_json::Value, limit: usize) -> Vec<WebResult> {
    body.get("web")
        .and_then(|web| web.get("results"))
        .and_then(|results| results.as_array())
        .map(|results| {
            results
                .iter()
                .take(limit)
                .map(|item| WebResult {
                    title: string_field(item, "title"),
                    url: string_field(item, "url"),
                    snippet: string_field(item, "description"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map a Tavily search response to hits. Tavily returns results[].content.
fn parse_tavily_results(body: &serde_json::Value, limit: usize) -> Vec<WebResult> {
    body.get("results")
        .and_then(|results| results.as_array())
        .map(|results| {
            results
                .iter()
                .take(limit)
                .map(|item| WebResult {
                    title: string_field(item, "title"),
                    url: string_field(item, "url"),
                    snippet: string_field(item, "content"),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn string_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// HTML as readable text: scripts and styles dropped, block tags as breaks, common entities
/// decoded, at most one blank line between blocks. Hand-written to avoid a dependency.
pub fn html_to_text(html: &str) -> String {
    let without_scripts = strip_element(html, "script");
    let without_styles = strip_element(&without_scripts, "style");
    let stripped = strip_tags(&without_styles);
    normalize_whitespace(&decode_entities(&stripped))
}

/// Remove every element whose name matches, including its content (used for script/style).
fn strip_element(html: &str, name: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{name}");
    let close = format!("</{name}");
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find(&open) {
        let start = cursor + relative;
        out.push_str(&html[cursor..start]);
        match lower[start..].find(&close) {
            Some(relative_close) => {
                let after_close = start + relative_close;
                match lower[after_close..].find('>') {
                    Some(gt) => cursor = after_close + gt + 1,
                    None => cursor = html.len(),
                }
            }
            None => cursor = html.len(),
        }
    }
    out.push_str(&html[cursor..]);
    out
}

/// Drop all tags, turning block-level boundaries into newlines.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut tag = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                if is_block_tag(&tag) {
                    out.push('\n');
                }
                in_tag = false;
                tag.clear();
            } else {
                tag.extend(ch.to_lowercase());
            }
        } else if ch == '<' {
            in_tag = true;
            tag.clear();
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_block_tag(tag: &str) -> bool {
    let name = tag
        .trim()
        .trim_start_matches('/')
        .split(|ch: char| ch.is_whitespace() || ch == '/')
        .next()
        .unwrap_or("");
    matches!(
        name,
        "p" | "div"
            | "br"
            | "li"
            | "tr"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "blockquote"
            | "pre"
            | "table"
            | "ul"
            | "ol"
            | "hr"
    )
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Collapse horizontal whitespace and keep at most one blank line between blocks.
fn normalize_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for raw_line in text.split('\n') {
        let line = collapse_spaces(raw_line);
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        out.push_str(&line);
        blank_run = 0;
    }
    out
}

fn collapse_spaces(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut pending_space = false;
    for ch in line.chars() {
        if ch == ' ' || ch == '\t' || ch == '\r' || ch == '\u{00a0}' {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch);
        }
    }
    out
}
