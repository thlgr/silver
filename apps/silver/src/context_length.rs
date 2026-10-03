//! Per-run context-window detection for the model a run is about to use, in order: the config
//! override, a local server's native API (asked every run, never cached), the on-disk models
//! cache, the route's `/models` catalog, the models.dev registry, then the static family table.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde_json::Value;
use silver_core::model::ContextLengthResolver;
use silver_core::model_metadata::{self, ModelsCache, MODELS_CACHE_FILE};
use silver_core::pricing::ModelPrice;

use crate::routed::RoutedModel;

/// Where the registry lives.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// On-disk copy of the registry, beside the models cache.
pub const MODELS_DEV_CACHE_FILE: &str = "models_dev.json";

/// How long the on-disk registry copy is trusted before it is fetched again.
const REGISTRY_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Bound on the registry fetch, so a run is never held up by a slow network.
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(6);

/// A silver provider id → the models.dev provider id, where the two differ.
fn registry_provider_id(provider: &str) -> &str {
    match provider {
        "opencode-zen" => "opencode",
        "google" => "google",
        "moonshot" => "moonshotai",
        "moonshot-cn" => "moonshotai",
        "copilot" => "github-copilot",
        "vertex" => "google-vertex",
        "bedrock" => "amazon-bedrock",
        "openai-codex" => "openai",
        "meta-ai" => "meta",
        "azure-foundry" => "azure",
        "qwen-portal" => "alibaba",
        other => other,
    }
}

/// Sizes the compaction budget for the model each run uses.
pub struct ModelContextResolver {
    routes: Arc<RoutedModel>,
    data_dir: PathBuf,
    /// config.toml's window override, which applies to the configured model alone.
    config_override: Option<usize>,
    /// The registry, once loaded, and when it was fetched.
    registry: Mutex<Option<Arc<Value>>>,
}

impl ModelContextResolver {
    /// Build a resolver over the daemon's routes and data directory.
    pub fn new(
        routes: Arc<RoutedModel>,
        data_dir: impl Into<PathBuf>,
        config_override: Option<usize>,
    ) -> Self {
        Self {
            routes,
            data_dir: data_dir.into(),
            config_override,
            registry: Mutex::new(None),
        }
    }

    fn cache_path(&self) -> PathBuf {
        self.data_dir.join(MODELS_CACHE_FILE)
    }

    fn registry_path(&self) -> PathBuf {
        self.data_dir.join(MODELS_DEV_CACHE_FILE)
    }

    /// The local server this run's requests go to, if they go to one: the active route's
    /// endpoint or, with nobody signed in through `/login`, the configured `[model].base_url`
    /// (a local server is configured there, so it has no active route to probe).
    async fn local_base(&self) -> Option<Cow<'_, str>> {
        let Some(provider) = self.routes.active_provider() else {
            let base_url = &self.routes.configured_endpoint()?.base_url;
            return is_local_base_url(base_url).then_some(Cow::Borrowed(base_url));
        };
        let route = self.routes.resolve(&provider).await.ok()?;
        (route.authenticated()
            && (is_local_base_url(&route.base_url)
                || route.kind == silver_protocol::providers::ProviderKind::Ollama))
            .then_some(Cow::Owned(route.base_url))
    }

    /// Remember a resolved window so the next run is a lookup.
    fn remember(&self, model: &str, tokens: usize) {
        let path = self.cache_path();
        let mut cache = ModelsCache::load(&path);
        cache.insert(model, tokens);
        if let Err(error) = cache.save(&path) {
            tracing::debug!(%error, "could not persist the models cache");
        }
    }

    /// The models.dev registry: memory, then a fresh-enough disk copy, then the network.
    pub(crate) async fn registry(&self) -> Option<Arc<Value>> {
        if let Some(registry) = self
            .registry
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(Arc::clone))
        {
            return Some(registry);
        }
        let path = self.registry_path();
        let fresh_on_disk = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age < REGISTRY_TTL);
        let value: Option<Value> = if fresh_on_disk {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
        } else {
            None
        };
        let value = match value {
            Some(value) => value,
            None => match self.fetch_registry().await {
                Some(value) => value,
                // Offline: an older disk copy is still better than nothing.
                None => std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())?,
            },
        };
        let registry = Arc::new(value);
        if let Ok(mut slot) = self.registry.lock() {
            *slot = Some(Arc::clone(&registry));
        }
        Some(registry)
    }

    /// Fetch the registry and keep a copy on disk.
    async fn fetch_registry(&self) -> Option<Value> {
        let response = self
            .routes
            .http()
            .get(MODELS_DEV_URL)
            .timeout(REGISTRY_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let text = response.text().await.ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        if !value.is_object() {
            return None;
        }
        let path = self.registry_path();
        if let Some(parent) = path.parent() {
            drop(std::fs::create_dir_all(parent));
        }
        if let Err(error) = std::fs::write(&path, &text) {
            tracing::debug!(%error, "could not persist the models.dev registry");
        }
        tracing::info!(
            providers = value.as_object().map(|o| o.len()).unwrap_or(0),
            "loaded the models.dev registry"
        );
        Some(value)
    }
}

/// The context window models.dev records for a model, preferring the named provider.
pub fn registry_context_length(
    registry: &Value,
    provider: Option<&str>,
    model: &str,
) -> Option<usize> {
    let providers = registry.as_object()?;
    let limit_of = |entry: &Value| {
        entry
            .pointer("/limit/context")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0)
    };
    if let Some(provider) = provider {
        if let Some(entry) = providers
            .get(registry_provider_id(provider))
            .and_then(|entry| entry.pointer(&format!("/models/{model}")))
        {
            if let Some(limit) = limit_of(entry) {
                return Some(limit);
            }
        }
    }
    // Any provider that lists the exact id; the largest window wins when they disagree, since
    // over-estimating is recovered by the loop's context-overflow handling and
    // under-estimating is exactly the premature compaction this resolver exists to end.
    providers
        .values()
        .filter_map(|entry| entry.pointer(&format!("/models/{model}")))
        .filter_map(limit_of)
        .max()
}

/// The models.dev list price for a model, preferring the named provider, in USD per million tokens;
/// `context_over_200k` is the long-context input tier. No `cost` block means no price.
pub fn registry_price(registry: &Value, provider: Option<&str>, model: &str) -> Option<ModelPrice> {
    let providers = registry.as_object()?;
    let price_of = |entry: &Value| {
        let cost = entry.get("cost")?;
        let rate = |key: &str| cost.get(key).and_then(Value::as_f64);
        let above = cost
            .pointer("/context_over_200k/input")
            .and_then(Value::as_f64);
        Some(ModelPrice {
            input_per_million_usd: rate("input")?,
            output_per_million_usd: rate("output")?,
            cache_read_per_million_usd: rate("cache_read"),
            cache_write_per_million_usd: rate("cache_write"),
            tier_threshold_tokens: above.map(|_| 200_000),
            input_per_million_usd_above: above,
        })
    };
    let pointer = format!("/models/{model}");
    provider
        .and_then(|provider| providers.get(registry_provider_id(provider)))
        .and_then(|entry| entry.pointer(&pointer))
        .and_then(price_of)
        .or_else(|| {
            providers
                .values()
                .filter_map(|entry| entry.pointer(&pointer))
                .find_map(price_of)
        })
}

/// A model's supported reasoning efforts, mirroring opencode's mapping of the registry's
/// `reasoning_options`: an `effort` option's values (`null` as "none"), else "none"/"high" for a
/// toggle, else "high"/"max" for a token budget. None when the registry has nothing to say.
pub fn registry_reasoning_efforts(
    registry: &Value,
    provider: Option<&str>,
    model: &str,
) -> Option<Vec<String>> {
    let providers = registry.as_object()?;
    let pointer = format!("/models/{model}/reasoning_options");
    // Prefer the named provider, then any provider that lists a non-empty set.
    let options = provider
        .and_then(|provider| providers.get(registry_provider_id(provider)))
        .and_then(|entry| entry.pointer(&pointer))
        .and_then(Value::as_array)
        .or_else(|| {
            providers
                .values()
                .filter_map(|entry| entry.pointer(&pointer))
                .find(|entry| entry.as_array().is_some_and(|values| !values.is_empty()))
                .and_then(Value::as_array)
        })?;
    if options.is_empty() {
        return None;
    }
    let effort = options
        .iter()
        .find(|option| option.get("type").and_then(Value::as_str) == Some("effort"))
        .and_then(|option| option.get("values").and_then(Value::as_array));
    if let Some(values) = effort {
        let levels: Vec<String> = values
            .iter()
            .filter_map(|value| match value {
                Value::Null => Some("none".to_string()),
                Value::String(text) if !text.trim().is_empty() => {
                    Some(text.trim().to_ascii_lowercase())
                }
                _ => None,
            })
            .collect();
        return if levels.is_empty() {
            None
        } else {
            Some(levels)
        };
    }
    let toggle = options
        .iter()
        .any(|option| option.get("type").and_then(Value::as_str) == Some("toggle"));
    let budget = options
        .iter()
        .any(|option| option.get("type").and_then(Value::as_str) == Some("budget_tokens"));
    match (toggle, budget) {
        (true, true) => Some(vec!["none".into(), "high".into(), "max".into()]),
        (true, false) => Some(vec!["none".into(), "high".into()]),
        (false, true) => Some(vec!["high".into(), "max".into()]),
        (false, false) => None,
    }
}

/// A model's supported reasoning efforts, falling back to the global accepted set when the
/// registry (or the model) has nothing to say.
pub fn supported_reasoning_efforts(
    registry: Option<&Value>,
    provider: Option<&str>,
    model: &str,
) -> Vec<String> {
    registry
        .and_then(|registry| registry_reasoning_efforts(registry, provider, model))
        .unwrap_or_else(|| {
            crate::config::REASONING_EFFORTS
                .iter()
                .map(|level| level.to_string())
                .collect()
        })
}

/// Clamp an effort onto a model's supported set (forgiving, opencode rejects instead): a level
/// kept as-is; an extended level falls to the strongest supported at or below it; one weaker
/// than the model's minimum (or unknown) lands on the weakest supported.
pub fn clamp_to_supported(effort: &str, supported: &[String]) -> Option<String> {
    let normalized = effort.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    if supported.iter().any(|level| level == &normalized) {
        return Some(normalized);
    }
    let index = crate::config::REASONING_EFFORT_LADDER
        .iter()
        .position(|level| *level == normalized.as_str())
        .unwrap_or(crate::config::REASONING_EFFORT_LADDER.len());
    // The strongest supported level at-or-below the requested one; when none is weaker, fall
    // back to the weakest supported instead of raising effort.
    let below = supported
        .iter()
        .filter_map(|level| {
            crate::config::REASONING_EFFORT_LADDER
                .iter()
                .position(|step| step == level)
                .filter(|candidate| *candidate < index)
                .map(|candidate| (candidate, level))
        })
        .max_by_key(|(candidate, _)| *candidate)
        .map(|(_, level)| level);
    below.or_else(|| supported.first()).cloned()
}

#[async_trait]
impl ContextLengthResolver for ModelContextResolver {
    async fn served_model(&self, model: &str) -> Option<String> {
        let base_url = self.local_base().await?;
        model_served_for(
            &probe_local_models(self.routes.http(), &base_url).await,
            model.trim(),
        )
    }

    async fn price(&self, model: &str) -> Option<ModelPrice> {
        let registry = self.registry().await?;
        registry_price(
            &registry,
            self.routes.active_provider().as_deref(),
            model.trim(),
        )
    }

    async fn context_length(&self, model: &str) -> Option<usize> {
        let model = model.trim();
        if model.is_empty() {
            return None;
        }
        // 1. An explicit override is for the configured model only.
        if self
            .routes
            .configured_endpoint()
            .is_some_and(|endpoint| endpoint.model == model)
        {
            if let Some(value) = self.config_override.filter(|value| *value > 0) {
                return Some(value);
            }
        }
        // 2. A local server, live. Its loaded window wins over anything remembered: the
        // cache would otherwise hold the maximum learned while the model was not loaded and
        // shadow an 8k load forever.
        if let Some(base_url) = self.local_base().await {
            if let Some(window) = probe_local_window(self.routes.http(), &base_url, model).await {
                if window.loaded {
                    self.remember(model, window.tokens);
                }
                return Some(window.tokens);
            }
        }
        // 3. What an earlier run learned.
        if let Some(value) = model_metadata::cached_context_length(&self.data_dir, model) {
            return Some(value);
        }
        // 4. The active route's own catalog.
        let provider = self.routes.active_provider();
        let route = match provider.as_deref() {
            Some(provider) => self.routes.resolve(provider).await.ok(),
            None => None,
        };
        if let Some(route) = route.as_ref().filter(|route| route.authenticated()) {
            let probed = crate::provider::probe_context_length(
                self.routes.http(),
                &route.base_url,
                route.key(),
                model,
                &self.cache_path(),
                route.kind,
            )
            .await;
            if let Some(value) = probed {
                return Some(value);
            }
        }
        // 5. The models.dev registry.
        if let Some(registry) = self.registry().await {
            if let Some(value) = registry_context_length(&registry, provider.as_deref(), model) {
                self.remember(model, value);
                return Some(value);
            }
        }
        // 6. The static family table.
        model_metadata::context_length_for(model)
    }
}

/// The models cache path for a data directory, for callers that only need the file.
pub fn models_cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MODELS_CACHE_FILE)
}

// ── Local servers ────────────────────────────────────────────────────────────
// Their OpenAI-compatible `/models` says nothing about context; each native endpoint reports the
// window a model was *loaded* with, which is what the compaction budget must respect.

/// Whether a base URL points at a machine-local server, where the native probes apply.
pub fn is_local_base_url(base_url: &str) -> bool {
    let host = base_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    host == "localhost"
        || host == "::1"
        || host.starts_with("127.")
        || host.starts_with("10.")
        || host.starts_with("192.168.")
        || host.ends_with(".local")
        || (host.starts_with("172.")
            && host
                .split('.')
                .nth(1)
                .and_then(|octet| octet.parse::<u8>().ok())
                .is_some_and(|octet| (16..=31).contains(&octet)))
}

/// The scheme and host of a base URL, with any `/v1` path dropped.
fn origin_of(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("http", trimmed),
    };
    let host = rest.split('/').next().unwrap_or(rest);
    format!("{scheme}://{host}")
}

/// A local server's answer for one model: the window, and whether it is the one the model
/// is actually running with (`loaded`) or only its maximum, reported for a model that is not
/// loaded. Only a loaded window is worth caching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalWindow {
    pub tokens: usize,
    pub loaded: bool,
}

/// LM Studio: `GET /api/v0/models`, `loaded_context_length` when loaded, else the maximum.
pub fn lmstudio_window(body: &Value, model: &str) -> Option<LocalWindow> {
    let entries = body.get("data").and_then(Value::as_array)?;
    let entry = entries
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))?;
    let loaded = entry.get("state").and_then(Value::as_str) == Some("loaded");
    let pick = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .filter(|value| *value > 0)
    };
    if loaded {
        if let Some(tokens) = pick("loaded_context_length") {
            return Some(LocalWindow {
                tokens,
                loaded: true,
            });
        }
    }
    pick("max_context_length").map(|tokens| LocalWindow {
        tokens,
        loaded: false,
    })
}

/// [lmstudio_window] as a bare count, for callers that only need the number.
pub fn lmstudio_context_length(body: &Value, model: &str) -> Option<usize> {
    lmstudio_window(body, model).map(|window| window.tokens)
}

/// What a local server says about one model it serves.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalModelInfo {
    /// The model id a run would name.
    pub id: String,
    /// What the model is for: `llm`, `vlm`, `embeddings`, … as the server reports it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Whether the server has it in memory right now. A cold model costs minutes on the
    /// first prompt; a loaded one answers in seconds.
    #[serde(default)]
    pub loaded: bool,
    /// The window it is loaded with, or its maximum when it is not loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<usize>,
}

/// Parse LM Studio's `GET /api/v0/models`: which models chat (picking an embedding model breaks the
/// session), which are loaded, and their windows. The OpenAI-compatible route has none of it.
pub fn lmstudio_models(body: &Value) -> Vec<LocalModelInfo> {
    let Some(entries) = body.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_str)?.to_string();
            let loaded = entry.get("state").and_then(Value::as_str) == Some("loaded");
            let pick = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                    .filter(|value| *value > 0)
            };
            Some(LocalModelInfo {
                id,
                kind: entry
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                loaded,
                context_length: loaded
                    .then(|| pick("loaded_context_length"))
                    .flatten()
                    .or_else(|| pick("max_context_length")),
            })
        })
        .collect()
}

/// The loaded chat model LM Studio answers with when `model` is not listed (its `local-model`
/// placeholder, another provider's id). None when `model` is listed or nothing is loaded.
pub fn model_served_for(models: &[LocalModelInfo], model: &str) -> Option<String> {
    if models.iter().any(|info| info.id == model) {
        return None;
    }
    models
        .iter()
        .find(|info| info.loaded && !info.kind.starts_with("embedding"))
        .map(|info| info.id.clone())
}

/// Ask a local server what it serves, for the model picker. Only LM Studio publishes this.
pub async fn probe_local_models(http: &reqwest::Client, base_url: &str) -> Vec<LocalModelInfo> {
    let origin = origin_of(base_url);
    let Ok(response) = http
        .get(format!("{origin}/api/v0/models"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
    else {
        return Vec::new();
    };
    if !response.status().is_success() {
        return Vec::new();
    }
    match response.json::<Value>().await {
        Ok(body) => lmstudio_models(&body),
        Err(_) => Vec::new(),
    }
}

/// Ollama: `POST /api/show`, `num_ctx` when set (the loaded window), else the architecture's
/// context length from `model_info`; and `GET /api/ps`, which reports the running window.
pub fn ollama_context_length(
    show: Option<&Value>,
    ps: Option<&Value>,
    model: &str,
) -> Option<usize> {
    if let Some(ps) = ps {
        if let Some(running) = ps
            .get("models")
            .and_then(Value::as_array)
            .and_then(|models| {
                models.iter().find(|entry| {
                    entry.get("name").and_then(Value::as_str) == Some(model)
                        || entry.get("model").and_then(Value::as_str) == Some(model)
                })
            })
            .and_then(|entry| entry.get("context_length"))
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
        {
            return Some(running as usize);
        }
    }
    let show = show?;
    if let Some(parameters) = show.get("parameters").and_then(Value::as_str) {
        for line in parameters.lines() {
            let mut parts = line.split_whitespace();
            if parts.next() == Some("num_ctx") {
                if let Some(value) = parts.next().and_then(|value| value.parse::<usize>().ok()) {
                    if value > 0 {
                        return Some(value);
                    }
                }
            }
        }
    }
    show.get("model_info")
        .and_then(Value::as_object)?
        .iter()
        .find(|(key, _)| key.ends_with(".context_length"))
        .and_then(|(_, value)| value.as_u64())
        .map(|value| value as usize)
        .filter(|value| *value > 0)
}

/// llama.cpp: `GET /props`, the server's `n_ctx`.
pub fn llamacpp_context_length(props: &Value) -> Option<usize> {
    props
        .pointer("/default_generation_settings/n_ctx")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .filter(|value| *value > 0)
}

/// [probe_local_window] as a bare count, for callers that only need the number.
pub async fn probe_local_context_length(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
) -> Option<usize> {
    probe_local_window(http, base_url, model)
        .await
        .map(|window| window.tokens)
}

/// A model's window from a local server's native endpoints. Ollama's `/api/ps` and llama.cpp's
/// `/props` describe what is loaded; Ollama's `/api/show` is only a maximum.
pub async fn probe_local_window(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
) -> Option<LocalWindow> {
    let origin = origin_of(base_url);
    let timeout = Duration::from_secs(3);

    // LM Studio.
    if let Ok(response) = http
        .get(format!("{origin}/api/v0/models"))
        .timeout(timeout)
        .send()
        .await
    {
        if response.status().is_success() {
            if let Ok(body) = response.json::<Value>().await {
                if let Some(window) = lmstudio_window(&body, model) {
                    return Some(window);
                }
            }
        }
    }

    // Ollama.
    let ps = match http
        .get(format!("{origin}/api/ps"))
        .timeout(timeout)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response.json::<Value>().await.ok(),
        _ => None,
    };
    let show = match http
        .post(format!("{origin}/api/show"))
        .timeout(timeout)
        .json(&serde_json::json!({ "name": model }))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response.json::<Value>().await.ok(),
        _ => None,
    };
    if ps.is_some() || show.is_some() {
        let running = ollama_context_length(None, ps.as_ref(), model);
        if let Some(tokens) = running {
            return Some(LocalWindow {
                tokens,
                loaded: true,
            });
        }
        if let Some(tokens) = ollama_context_length(show.as_ref(), None, model) {
            return Some(LocalWindow {
                tokens,
                loaded: false,
            });
        }
    }

    // llama.cpp.
    if let Ok(response) = http
        .get(format!("{origin}/props"))
        .timeout(timeout)
        .send()
        .await
    {
        if response.status().is_success() {
            if let Ok(props) = response.json::<Value>().await {
                if let Some(tokens) = llamacpp_context_length(&props) {
                    return Some(LocalWindow {
                        tokens,
                        loaded: true,
                    });
                }
            }
        }
    }
    None
}
