//! web_search and web_extract over the run's WebBackend: read-only, with schemas, limits and
//! formatting here; a missing backend is an error outcome.

use crate::error::CoreResult;
use crate::services::WebResult;
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use silver_protocol::RiskLevel;
use std::sync::Arc;

/// Default number of search results returned when limit is omitted.
pub const DEFAULT_SEARCH_LIMIT: usize = 5;
/// Hard cap on the number of search results.
pub const MAX_SEARCH_LIMIT: usize = 100;
/// Default per-page byte budget for web_extract.
pub const DEFAULT_EXTRACT_CHAR_LIMIT: usize = 15_000;
/// Lower bound for the web_extract byte budget.
pub const MIN_EXTRACT_CHAR_LIMIT: usize = 2_000;
/// Upper bound for the web_extract byte budget.
pub const MAX_EXTRACT_CHAR_LIMIT: usize = 500_000;
/// Maximum number of URLs accepted by one web_extract call (upstream limit).
pub const MAX_EXTRACT_URLS: usize = 5;

/// Register the web_search and web_extract tools.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(WebSearchTool));
    registry.register(Arc::new(WebExtractTool));
}

struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web. Returns up to 5 results (title, URL, snippet). The query passes through \
         to the backend, so operators like site:domain, filetype:pdf, intitle:word, -term and \
         exact phrases work when the backend supports them."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query. May include backend-supported operators: site:example.com, filetype:pdf, intitle:word, -term, exact phrases."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return. Defaults to 5, capped at 100.",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    fn risk(&self, _args: &serde_json::Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: serde_json::Value,
    ) -> CoreResult<ToolOutcome> {
        let Some(backend) = ctx.run.services.web.as_ref() else {
            return Ok(ToolOutcome::error(
                "web search is unavailable: no web backend is configured",
            ));
        };
        let Some(query) = args.get("query").and_then(|value| value.as_str()) else {
            return Ok(ToolOutcome::error("web_search requires a 'query' string"));
        };
        let query = query.trim();
        if query.is_empty() {
            return Ok(ToolOutcome::error(
                "web_search requires a non-empty 'query'",
            ));
        }

        let limit = search_limit(&args);
        match backend.search(query, limit).await {
            Ok(results) => Ok(ToolOutcome::ok(format_search_results(&results, query))),
            Err(err) => Ok(ToolOutcome::error(err.to_string())),
        }
    }
}

struct WebExtractTool;

#[async_trait]
impl Tool for WebExtractTool {
    fn name(&self) -> &'static str {
        "web_extract"
    }

    fn description(&self) -> &'static str {
        "Extract content from web page URLs. Returns clean page content (no summarization), up to \
         5 URLs per call. Pages within the byte budget (default 15000) return whole; larger pages \
         end with a truncation marker."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "urls": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of URLs to extract content from (max 5 URLs per call).",
                    "maxItems": 5
                },
                "char_limit": {
                    "type": "integer",
                    "description": "Optional per-page byte budget sent back (default 15000). Pages larger than this are truncated with a marker.",
                    "minimum": 2000
                }
            },
            "required": ["urls"]
        })
    }

    fn risk(&self, _args: &serde_json::Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        ctx: &ToolContext<'_>,
        args: serde_json::Value,
    ) -> CoreResult<ToolOutcome> {
        let Some(backend) = ctx.run.services.web.as_ref() else {
            return Ok(ToolOutcome::error(
                "web extract is unavailable: no web backend is configured",
            ));
        };
        let urls = extract_urls(&args);
        if urls.is_empty() {
            return Ok(ToolOutcome::error(
                "web_extract requires a 'urls' array with one or more URL strings",
            ));
        }
        let max_bytes = extract_char_limit(&args);

        let mut out = String::new();
        for (index, url) in urls.iter().enumerate() {
            if index > 0 {
                out.push_str("\n\n");
            }
            out.push_str(&format!("URL: {url}\n"));
            match backend.extract(url, max_bytes).await {
                Ok(text) => out.push_str(&format_extract_text(&text, max_bytes)),
                Err(err) => out.push_str(&format!("Error: {err}")),
            }
        }
        Ok(ToolOutcome::ok(out))
    }
}

/// Search limit from limit, then the max_results or count aliases, clamped to 1..=100.
fn search_limit(args: &serde_json::Value) -> usize {
    args.get("limit")
        .or_else(|| args.get("max_results"))
        .or_else(|| args.get("count"))
        .and_then(|value| value.as_u64())
        .map(|value| value.clamp(1, MAX_SEARCH_LIMIT as u64) as usize)
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
}

/// Per-page byte budget from char_limit, then the max_bytes or chars aliases, clamped.
fn extract_char_limit(args: &serde_json::Value) -> usize {
    args.get("char_limit")
        .or_else(|| args.get("max_bytes"))
        .or_else(|| args.get("chars"))
        .and_then(|value| value.as_u64())
        .map(|value| {
            value.clamp(MIN_EXTRACT_CHAR_LIMIT as u64, MAX_EXTRACT_CHAR_LIMIT as u64) as usize
        })
        .unwrap_or(DEFAULT_EXTRACT_CHAR_LIMIT)
}

/// Collect the URL list from a urls array, falling back to a single url string.
fn extract_urls(args: &serde_json::Value) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(items) = args.get("urls").and_then(|value| value.as_array()) {
        for item in items {
            if let Some(url) = item.as_str() {
                let url = url.trim();
                if !url.is_empty() {
                    urls.push(url.to_string());
                }
            }
        }
    } else if let Some(url) = args.get("url").and_then(|value| value.as_str()) {
        let url = url.trim();
        if !url.is_empty() {
            urls.push(url.to_string());
        }
    }
    urls.truncate(MAX_EXTRACT_URLS);
    urls
}

/// Format search hits as a numbered title, URL and snippet list.
fn format_search_results(results: &[WebResult], query: &str) -> String {
    if results.is_empty() {
        return format!("no results found for '{query}'");
    }
    let mut out = String::new();
    for (index, hit) in results.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        let title = if hit.title.trim().is_empty() {
            "(untitled)"
        } else {
            hit.title.trim()
        };
        out.push_str(&format!(
            "{}. {}\n{}\n{}",
            index + 1,
            title,
            hit.url,
            hit.snippet.trim()
        ));
    }
    out
}

/// Append a truncation marker when the backend returned a body at the byte cap.
fn format_extract_text(text: &str, max_bytes: usize) -> String {
    if max_bytes == 0 || text.len() < max_bytes {
        return text.to_string();
    }
    let mut capped = text.to_string();
    if capped.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !capped.is_char_boundary(cut) {
            cut -= 1;
        }
        capped.truncate(cut);
    }
    capped.push_str(&format!(
        "\n\n[... truncated: showing the first {max_bytes} bytes of this page ...]"
    ));
    capped
}
