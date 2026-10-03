//! OpenCode Zen / Go relay. The transport is chosen per model: Claude and Qwen take Anthropic
//! Messages, GPT, Grok and Muse the Responses API, and the open models (DeepSeek, GLM, Kimi,
//! MiniMax) chat/completions. Every request carries `x-opencode-session` for prompt-cache affinity.

use async_trait::async_trait;
use silver_core::error::{CoreError, CoreResult};
use silver_core::model::{Model, ModelRequest, ModelStream};
use tokio_util::sync::CancellationToken;

use crate::anthropic::AnthropicProvider;
use crate::codex::CodexProvider;
use crate::provider::OpenAiCompatibleProvider;

/// The API surface a model is served on behind the relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Chat,
    Anthropic,
    Responses,
}

/// Which relay a base URL points at. The two serve different model sets and route a few families
/// differently (Zen's Claude and Go's MiniMax take Messages).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Zen,
    Go,
}

/// Streaming client for an OpenCode Zen / Go relay.
pub struct OpenCodeProvider {
    base_url: String,
    api_key: String,
    model: String,
    http: reqwest::Client,
    family: Family,
}

impl OpenCodeProvider {
    /// Create a transport for an OpenCode relay endpoint.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let base_url = base_url.trim().trim_end_matches('/').to_string();
        Self {
            family: family_for(&base_url),
            base_url,
            api_key: api_key.into(),
            model: model.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Replace the HTTP client, e.g. to apply the daemon TLS policy.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// The headers every OpenCode request carries. The affinity header is the conversation's
    /// stable cache key; a request without one is left alone.
    fn headers(&self, request: &ModelRequest) -> Vec<(String, String)> {
        match request
            .cache_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            Some(key) => vec![("x-opencode-session".to_string(), key.to_string())],
            None => Vec::new(),
        }
    }
}

#[async_trait]
impl Model for OpenCodeProvider {
    fn name(&self) -> &str {
        &self.model
    }

    fn is_local(&self) -> bool {
        silver_protocol::providers::is_local_endpoint(&self.base_url)
    }

    async fn stream(
        &self,
        mut request: ModelRequest,
        cancel: CancellationToken,
    ) -> CoreResult<ModelStream> {
        if self.api_key.trim().is_empty() {
            return Err(CoreError::ProviderUnavailable(
                "OpenCode needs an API key; sign in with /login".to_string(),
            ));
        }
        let model = normalize_model_id(if request.model.trim().is_empty() {
            &self.model
        } else {
            &request.model
        });
        request.model = model;

        let headers = self.headers(&request);
        let http = reqwest::Client::clone(&self.http);
        let surface = surface_for_request(self.family, &request.model, request.has_pictures());
        match surface {
            Surface::Chat => {
                OpenAiCompatibleProvider::new(
                    self.base_url.as_str(),
                    self.api_key.as_str(),
                    self.model.as_str(),
                )
                .with_http_client(http)
                .with_extra_headers(headers)
                .stream(request, cancel)
                .await
            }
            Surface::Anthropic => {
                AnthropicProvider::new(
                    self.base_url.as_str(),
                    self.api_key.as_str(),
                    self.model.as_str(),
                )
                .with_http_client(http)
                .with_extra_headers(headers)
                .stream(request, cancel)
                .await
            }
            Surface::Responses => {
                CodexProvider::responses_api(
                    self.base_url.as_str(),
                    self.api_key.as_str(),
                    self.model.as_str(),
                )
                .with_http_client(http)
                .with_extra_headers(headers)
                .stream(request, cancel)
                .await
            }
        }
    }
}

/// The surface a request goes out on: a picture on a DeepSeek model takes the Anthropic one,
/// because the relay's chat/completions shim rejects an inline image with a bare 400
/// (opencode#40811) while DeepSeek's own Anthropic-format endpoint takes image blocks.
fn surface_for_request(family: Family, model: &str, has_pictures: bool) -> Surface {
    let surface = surface_for(family, model);
    if has_pictures && surface == Surface::Chat && model.starts_with("deepseek") {
        Surface::Anthropic
    } else {
        surface
    }
}

/// The relay a base URL points at: Go's relay path is `/zen/go`, Zen's is `/zen`.
fn family_for(base_url: &str) -> Family {
    if base_url.contains("/zen/go") {
        Family::Go
    } else {
        Family::Zen
    }
}

/// The bare model slug the relay wants, stripping a `opencode/`, `opencode-zen/` or
/// `opencode-go/` namespace a config may carry.
fn normalize_model_id(model: &str) -> String {
    let trimmed = model.trim();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["opencode-zen/", "opencode-go/", "opencode/"] {
        if lower.starts_with(prefix) {
            return trimmed[prefix.len()..].to_string();
        }
    }
    trimmed.to_string()
}

/// The API surface a model is served on, by family and model-id prefix. Anything unmatched
/// falls through to chat/completions.
fn surface_for(family: Family, model: &str) -> Surface {
    let model = model.to_ascii_lowercase();
    let starts_with_any =
        |prefixes: &[&str]| prefixes.iter().any(|prefix| model.starts_with(prefix));
    match family {
        Family::Go => {
            if starts_with_any(&["gpt-", "grok-", "muse-spark"]) {
                Surface::Responses
            } else if starts_with_any(&["minimax-", "qwen", "union-alpha"]) {
                Surface::Anthropic
            } else {
                Surface::Chat
            }
        }
        Family::Zen => {
            if starts_with_any(&["claude-", "union-alpha"]) {
                Surface::Anthropic
            } else if starts_with_any(&["gpt-", "grok-", "muse-spark"]) {
                Surface::Responses
            } else if starts_with_any(&["qwen"]) {
                Surface::Anthropic
            } else {
                Surface::Chat
            }
        }
    }
}
