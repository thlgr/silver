//! Configuration loading. Precedence: CLI flags > environment > file > safe defaults.
//! API keys are read from the environment and are never persisted or logged.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use silver_protocol::providers::{preset, ProviderKind, ProviderPreset};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// Minimum length accepted for a bearer token on a non-loopback bind.
pub const MIN_BEARER_TOKEN_LEN: usize = 16;

/// The levels assumed for a model the registry says nothing about, weakest to strongest.
pub const REASONING_EFFORTS: [&str; 5] = ["none", "minimal", "low", "medium", "high"];

/// Every accepted reasoning-effort level, weakest to strongest. A model's registry entry can offer
/// the upper ones; they also order and clamp provider-native levels.
pub const REASONING_EFFORT_LADDER: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// True when `value` names an accepted reasoning-effort level (case-insensitive).
pub fn is_valid_reasoning_effort(value: &str) -> bool {
    let value = value.trim();
    REASONING_EFFORT_LADDER
        .iter()
        .any(|level| level.eq_ignore_ascii_case(value))
}

/// Patch one `[section] key` in the on-disk configuration, preserving every other key a user
/// may have added since the daemon started. The write is atomic.
pub fn persist_setting(
    path: &Path,
    section: &str,
    key: &str,
    setting: toml::Value,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut value: toml::Value = if text.trim().is_empty() {
        toml::Value::Table(toml::Table::new())
    } else {
        text.parse::<toml::Value>()?
    };
    let table = value
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("configuration root is not a table"))?
        .entry(section.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("{section} is not a table"))?;
    table.insert(key.to_string(), setting);
    let rendered = toml::to_string_pretty(&value)?;
    crate::atomic_file::write(path, rendered.as_bytes())?;
    Ok(())
}

/// On-disk config schema version; bump it with every Config::migrate step. A file without the key
/// is version 0, and a newer file is never downgraded.
pub const CONFIG_VERSION: u32 = silver_protocol::CONFIG_VERSION;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Schema version of the loaded file; 0 when the key is absent (pre-versioning schema).
    pub config_version: u32,
    pub server: ServerConfig,
    pub data: DataConfig,
    pub model: ModelConfig,
    pub tools: ToolsConfig,
    pub memory: MemoryConfig,
    pub agent: AgentSection,
    /// One optional side-model route for context summarization and session titles.
    pub auxiliary: AuxiliaryConfig,
    pub security: SecurityConfig,
    /// Opt-in outbound health/metrics monitoring. Off unless enabled.
    pub monitoring: MonitoringConfig,
    /// Local checkpoint/snapshot retention used for session undo.
    pub checkpoints: CheckpointsConfig,
    /// Opt-in spend guardrails. Off unless enabled.
    pub cost: CostConfig,
    /// Opt-in Mixture of Agents. Off unless enabled.
    pub moa: MoaConfig,
    /// OAuth token storage for remote MCP/HTTP providers.
    pub oauth: OAuthConfig,
    /// Model Context Protocol client servers.
    pub mcp: McpConfig,
    /// Language-server integration.
    pub lsp: LspConfig,
    /// Git worktree isolation for runs.
    pub worktree: WorktreeConfig,
    /// Delegating work to subagents with the delegate_task tool.
    pub delegation: DelegationConfig,
}

/// `[delegation]` limits, per subagent and per call. The defaults assume a small local model, where
/// a subagent's turn costs more than the parent's.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DelegationConfig {
    /// Whether the delegate_task tool is registered at all. Off leaves a run with no way to
    /// spawn work, which is the right answer on an endpoint that cannot afford a second turn.
    pub enabled: bool,
    /// Iteration budget for one subagent turn.
    pub max_iterations: u32,
    /// Wall clock for one subagent turn. A local model may need minutes; a subagent that runs
    /// out of it reports what it has rather than being cut mid-sentence.
    pub timeout_seconds: u64,
    /// How many tasks of one call run at the same time. The tool's schema says the same number,
    /// so the model sees the limit instead of having a call refused.
    pub max_concurrent: usize,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_iterations: 50,
            timeout_seconds: 600,
            max_concurrent: 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub max_concurrent_runs: u32,
    pub run_timeout_seconds: u64,
    pub max_message_bytes: u64,
    pub bearer_token: Option<String>,
    pub request_body_limit_bytes: u64,
    /// Browser origins allowed by the CORS layer. Empty disables CORS entirely; `"*"` allows any
    /// origin; every other entry must be a bare `scheme://host[:port]` origin.
    pub cors_allowed_origins: Vec<String>,
    /// Per-client request budget per minute; the burst allowance equals this number. 0 disables
    /// the limiter.
    pub rate_limit_per_minute: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7777".into(),
            max_concurrent_runs: 4,
            run_timeout_seconds: 1800,
            max_message_bytes: 1_048_576,
            bearer_token: None,
            request_body_limit_bytes: 2_097_152,
            cors_allowed_origins: Vec::new(),
            rate_limit_per_minute: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DataConfig {
    pub directory: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub provider: String,
    /// Wire protocol for the provider; defaults to 'openai_compatible'.
    #[serde(default = "default_model_kind")]
    pub kind: String,
    pub name: String,
    pub base_url: Option<String>,
    pub api_key_env: String,
    /// Explicit context-window override in tokens. Wins over the disk cache, the `/models`
    /// probe and the static family table.
    pub context_length: Option<usize>,
    /// Requested reasoning effort: none, minimal, low, medium or high.
    pub reasoning_effort: Option<String>,
    /// Reasoning tokens one model call may spend before silver cuts it off and asks the
    /// model to act. Unset: 4096 for a local endpoint, no limit for a hosted one. 0: no limit.
    pub reasoning_budget: Option<usize>,
    /// Ordered backup routes tried when the primary is unavailable.
    pub fallback: Vec<FallbackConfig>,
    /// Optional API-key pool for the primary route, configured as `[[model.credentials]]`.
    /// Empty means the pool is synthesized from `api_key_env`.
    pub credentials: Vec<ModelCredential>,
}

fn default_model_kind() -> String {
    "openai_compatible".to_string()
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            kind: default_model_kind(),
            name: "gpt-4o-mini".into(),
            base_url: None,
            api_key_env: "OPENAI_API_KEY".into(),
            context_length: None,
            reasoning_effort: None,
            reasoning_budget: None,
            fallback: Vec::new(),
            credentials: Vec::new(),
        }
    }
}

/// One `[[model.fallback]]` route. Only `model` is required; provider falls back to the primary's,
/// base_url and api_key_env to the provider preset, then the primary's.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FallbackConfig {
    /// Provider or preset id. Empty inherits the primary model's wire protocol.
    pub provider: String,
    /// Model id. Required; validation rejects a blank one.
    pub model: String,
    /// Base URL override. Empty falls back to the provider preset, then the primary base URL.
    pub base_url: Option<String>,
    /// Environment variable holding the API key. Empty falls back to the provider preset,
    /// then the primary key env.
    pub api_key_env: Option<String>,
}

/// The auxiliary side-model for summaries and session titles; off unless enabled with a model.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuxiliaryConfig {
    /// Whether the auxiliary route is active.
    pub enabled: bool,
    /// Model id for the route. Required when 'enabled' is true.
    pub model: String,
    /// Provider or preset id. Empty falls back to the main model's provider kind.
    pub provider: String,
    /// Base URL override. Empty falls back to the provider preset, then the main base URL.
    pub base_url: Option<String>,
    /// Environment variable holding the auxiliary API key. Empty reuses the main key.
    pub api_key_env: Option<String>,
}

/// How gated tool calls are approved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Ask for approval for every gated call.
    Manual,
    /// Let the auxiliary model auto-approve low-risk calls and ask otherwise.
    Smart,
    /// Do not ask for approval (YOLO).
    Off,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub write_requires_approval: bool,
    pub command_requires_approval: bool,
    pub max_output_bytes: u64,
    pub tool_timeout_seconds: u64,
    /// smart (the default) lets the auxiliary model approve low-risk calls and prompts for
    /// the rest; manual prompts for every gated call; off disables approval prompts.
    pub approval_mode: ApprovalMode,
    /// Seconds an approval request may wait before it is abandoned. Minimum 1.
    pub approval_timeout_seconds: u64,
    /// Glob patterns for commands that are always denied.
    pub deny_commands: Vec<String>,
    /// Allow-list of tool names. Empty means every registered tool is available.
    pub enabled: Vec<String>,
    /// Deny-list of tool names, applied after `enabled`.
    pub disabled: Vec<String>,
    /// Toolsets turned on by default. Empty selects every built-in toolset.
    pub default_toolsets: Vec<String>,
    /// Toolsets removed from every run, applied after `default_toolsets`. Defaults to
    /// `["web"]` so web search/extract schemas stay out of the prompt until re-enabled.
    pub disabled_toolsets: Vec<String>,
    /// Extra variable names copied from the daemon's environment into every command the model runs,
    /// on top of `PRESERVED_ENV`. Never list a secret: it reaches every command.
    pub env_passthrough: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            write_requires_approval: true,
            command_requires_approval: true,
            max_output_bytes: 1_048_576,
            tool_timeout_seconds: 120,
            // Smart without an auxiliary route prompts for everything, so it is never the
            // looser mode by accident.
            approval_mode: ApprovalMode::Smart,
            approval_timeout_seconds: 300,
            deny_commands: Vec::new(),
            enabled: Vec::new(),
            // A lean roster: file tools, bash, lsp, todo, memory, session_search and skill_view.
            // Keep `list_files`: without it a small model lists a directory with `bash find`, which
            // needs an approval and tens of seconds.
            disabled: [
                "run_command",
                "execute_code",
                "process_manage",
                "skill_manage",
                "skills_list",
            ]
            .iter()
            .map(|name| name.to_string())
            .collect(),
            default_toolsets: Vec::new(),
            disabled_toolsets: vec!["web".to_string()],
            env_passthrough: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Whether silver manages a local ai-memory server for shared, cross-harness memory.
    pub enabled: bool,
    /// The ai-memory executable; a bare name is looked up on `PATH`.
    pub binary: Option<String>,
    /// Where the managed ai-memory server listens (a loopback address).
    pub bind: String,
    /// The ai-memory data directory; defaults under silver's own data directory.
    pub data_dir: Option<PathBuf>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
            bind: "127.0.0.1:49374".to_string(),
            data_dir: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSection {
    pub max_iterations: u32,
    pub max_tool_calls_per_run: u32,
    /// Longest silence tolerated from a hosted model before the request or stream is
    /// declared stalled.
    pub model_timeout_seconds: u64,
    /// The same budget for a local endpoint (LM Studio, Ollama, llama.cpp, a LAN box): a
    /// small model streams a large tool call at ~100 B/s and LM Studio emits it only once
    /// it is complete, so minutes of silence are routine there.
    pub local_model_timeout_seconds: u64,
    pub turn_liveness_timeout_seconds: f64,
    pub turn_liveness_poll_seconds: f64,
    pub empty_guard_enabled: bool,
    pub empty_cost_threshold_usd: f64,
    /// Hard stops for tool-call stall loops. On by default: a small local model can otherwise
    /// fail the same tool for many minutes while the user waits. False makes them warn-only.
    pub tool_call_hard_stop: bool,
    /// Opt-in use of the MAIN model to summarize dropped context when no auxiliary model is
    /// configured. Off by default, so compaction stays deterministic without a side model.
    pub summarize_with_main_model: bool,
    /// Opt-in hints from Jev, a classifier on OpenRouter. Sends the task, commands and output
    /// excerpts to OpenRouter with the stored OpenRouter key (or OPENROUTER_API_KEY).
    pub jev_hints: bool,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            max_iterations: 500,
            max_tool_calls_per_run: 200,
            model_timeout_seconds: 120,
            local_model_timeout_seconds: 900,
            turn_liveness_timeout_seconds: 600.0,
            turn_liveness_poll_seconds: 15.0,
            empty_guard_enabled: true,
            empty_cost_threshold_usd: 0.25,
            tool_call_hard_stop: true,
            summarize_with_main_model: false,
            jev_hints: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Verify TLS certificates on every outbound HTTPS client. Default true.
    pub ssl_verify: bool,
    /// PEM/DER CA bundle added to the trust store of every outbound HTTPS client.
    pub ca_bundle: Option<PathBuf>,
    /// Domain suffixes refused by web_search/web_extract (host equal or subdomain).
    pub website_blocklist: Vec<String>,
    /// Refuse URLs whose query carries a credential-shaped parameter.
    pub block_sensitive_query_urls: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            ssl_verify: true,
            ca_bundle: None,
            website_blocklist: Vec::new(),
            block_sensitive_query_urls: false,
        }
    }
}

/// Opt-in monitoring: a fire-and-forget JSON event stream to one HTTP endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitoringConfig {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub redact: bool,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            redact: true,
        }
    }
}

/// Default `checkpoints.max_bytes`: 64 MiB.
pub const DEFAULT_CHECKPOINT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Local checkpoint retention for session undo. Enabled by default; the caps bound how much
/// snapshot history the daemon keeps on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckpointsConfig {
    /// Whether automatic checkpoints are captured.
    pub enabled: bool,
    /// Maximum number of snapshots retained per session.
    pub max_snapshots: u32,
    /// Maximum total bytes retained across snapshots.
    pub max_bytes: u64,
}

impl Default for CheckpointsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_snapshots: 50,
            max_bytes: DEFAULT_CHECKPOINT_MAX_BYTES,
        }
    }
}

/// One model slot in the Mixture of Agents configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MoaSlot {
    /// Provider id from the shared presets; empty means the daemon's own provider.
    pub provider: String,
    /// Model to ask for; empty falls back to the provider's default model.
    pub model: String,
}

impl MoaSlot {
    /// Label used in the guidance block and in logs.
    pub fn label(&self) -> String {
        match (self.provider.trim(), self.model.trim()) {
            ("", model) => model.to_string(),
            (provider, "") => provider.to_string(),
            (provider, model) => format!("{provider}/{model}"),
        }
    }
}

/// Opt-in Mixture of Agents: reference models that brief the acting model each turn. Off by
/// default, and inert until at least one reference is configured.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MoaConfig {
    /// Whether references run before each turn.
    pub enabled: bool,
    /// Advisors asked before the acting model answers.
    pub references: Vec<MoaSlot>,
    /// Seconds an advisor may take before the turn goes on without it. Zero waits forever.
    pub reference_timeout_seconds: u64,
}

impl Default for MoaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            references: Vec::new(),
            // An advisor is a side call on the critical path of every turn: an unbounded one
            // would hold the whole turn hostage to the slowest provider.
            reference_timeout_seconds: 60,
        }
    }
}

/// Opt-in per-day and per-run spend guardrails. Off by default.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CostConfig {
    /// Whether the guardrails are enforced.
    pub enabled: bool,
    /// Maximum spend per calendar day (UTC). `None` disables the daily cap.
    pub max_usd_per_day: Option<f64>,
    /// Maximum spend for a single run. `None` disables the per-run cap.
    pub max_usd_per_run: Option<f64>,
    /// Fraction of a cap at which a warning is emitted. Must satisfy `0 < warn_ratio <= 1`.
    pub warn_ratio: f64,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_usd_per_day: None,
            max_usd_per_run: None,
            warn_ratio: 0.8,
        }
    }
}

/// One entry in the `[[model.credentials]]` API-key pool.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCredential {
    /// Optional human-readable label used in logs (never the secret itself).
    pub label: Option<String>,
    /// Environment variable holding the key. Required; validated non-empty.
    pub api_key_env: String,
    /// Relative selection weight. Defaults to 1.
    pub weight: u32,
}

impl Default for ModelCredential {
    fn default() -> Self {
        Self {
            label: None,
            api_key_env: String::new(),
            weight: 1,
        }
    }
}

impl ModelCredential {
    /// Read this entry's API key from the environment. Never logged or persisted.
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.api_key_env)
            .ok()
            .filter(|value| !value.is_empty())
    }
}

/// OAuth token storage for remote MCP/HTTP providers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuthConfig {
    /// Token file path. `None` resolves to `<data_dir>/oauth.json`.
    pub token_store: Option<PathBuf>,
    /// Whether OAuth flows are enabled.
    pub enabled: bool,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            token_store: None,
            enabled: true,
        }
    }
}

/// MCP transport kinds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// Spawn a local process and speak JSON-RPC over stdio.
    #[default]
    Stdio,
    /// Connect to a Streamable HTTP endpoint.
    Http,
}

/// One `[[mcp.server]]` entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Unique server name. Required; validated non-empty and unique.
    pub name: String,
    /// Whether the server is started. Disabled entries are ignored at runtime.
    pub enabled: bool,
    /// Wire transport. Defaults to `stdio`.
    pub transport: McpTransport,
    /// Executable to spawn for `stdio` transport. Required for stdio.
    pub command: Option<String>,
    /// Arguments passed to `command`.
    pub args: Vec<String>,
    /// Extra environment variables for the spawned process.
    pub env: BTreeMap<String, String>,
    /// Endpoint URL for `http` transport. Required for http.
    pub url: Option<String>,
    /// Extra request headers for `http` transport.
    pub headers: BTreeMap<String, String>,
    /// Tool allow-list forwarded to the server. Empty means every advertised tool.
    pub allowed_tools: Vec<String>,
    /// Per-request timeout in seconds.
    pub timeout_seconds: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            allowed_tools: Vec::new(),
            timeout_seconds: 30,
        }
    }
}

/// The `[mcp]` section: an ordered list of MCP client servers.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Configured servers, in declaration order.
    pub server: Vec<McpServerConfig>,
}

/// `[lsp]` language-server integration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LspConfig {
    /// Whether LSP support is enabled.
    pub enabled: bool,
    /// Explicit server commands. Empty auto-detects from the workspace.
    pub servers: Vec<String>,
    /// Per-request timeout in seconds.
    pub timeout_seconds: u64,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: Vec::new(),
            timeout_seconds: 10,
        }
    }
}

/// Git worktree isolation for runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeConfig {
    /// Whether runs are isolated in a git worktree.
    pub enabled: bool,
    /// Directory holding worktrees. Defaults to `.worktrees`.
    pub root: Option<String>,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: Some(".worktrees".to_string()),
        }
    }
}

impl Config {
    /// Parse an optional TOML file without applying the environment or migrations.
    fn load_raw(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            Some(path) if path.exists() => {
                let text = std::fs::read_to_string(path)?;
                Ok(toml::from_str(&text)?)
            }
            _ => Ok(Config::default()),
        }
    }

    /// Load from an optional TOML file, then apply environment overrides and migrations.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut config = Self::load_raw(path)?;
        config.apply_env();
        config.apply_migrations();
        Ok(config)
    }

    /// Validate settings that can only fail at startup: the CA bundle and every configured CORS
    /// origin.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_server()?;
        self.validate_auxiliary()?;
        self.validate_fallbacks()?;
        self.validate_security()?;
        self.validate_monitoring()?;
        self.validate_cost()?;
        self.validate_credentials()?;
        self.validate_mcp()
    }

    fn validate_server(&self) -> anyhow::Result<()> {
        // Fail closed: an exposed bind must be authenticated. A short token is rejected too so a
        // placeholder cannot silently stand in for a real one.
        if self.needs_bearer_token() {
            match &self.server.bearer_token {
                Some(token) if token.len() >= MIN_BEARER_TOKEN_LEN => {}
                Some(_) => anyhow::bail!(
                    "server.bind {} is not loopback; server.bearer_token must be at least {} characters",
                    self.server.bind,
                    MIN_BEARER_TOKEN_LEN
                ),
                None => anyhow::bail!(
                    "server.bind {} is not loopback; set server.bearer_token (or bind to 127.0.0.1)",
                    self.server.bind
                ),
            }
        }
        if self.tools.approval_timeout_seconds < 1 {
            anyhow::bail!("tools.approval_timeout_seconds must be at least 1");
        }
        if self.model.context_length == Some(0) {
            anyhow::bail!("model.context_length must be greater than zero when set");
        }
        if let Some(effort) = self
            .model
            .reasoning_effort
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            if !is_valid_reasoning_effort(effort) {
                anyhow::bail!(
                    "model.reasoning_effort {effort:?} must be one of {}",
                    REASONING_EFFORT_LADDER.join(", ")
                );
            }
        }
        Ok(())
    }

    fn validate_auxiliary(&self) -> anyhow::Result<()> {
        if self.auxiliary.enabled {
            if self.auxiliary.model.trim().is_empty() {
                anyhow::bail!("auxiliary.enabled is true but auxiliary.model is empty");
            }
            if !self.auxiliary.provider.trim().is_empty()
                && ProviderKind::parse(&self.auxiliary.provider).is_none()
                && preset(&self.auxiliary.provider).is_none()
            {
                anyhow::bail!(
                    "auxiliary.provider {:?} is not a recognized provider or preset",
                    self.auxiliary.provider
                );
            }
            if self
                .auxiliary
                .api_key_env
                .as_deref()
                .is_some_and(|env| env.trim().is_empty())
            {
                anyhow::bail!("auxiliary.api_key_env must not be empty when set");
            }
        }
        if let Some(base_url) = self
            .auxiliary
            .base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            let parsed = reqwest::Url::parse(base_url)
                .with_context(|| format!("invalid auxiliary.base_url {base_url:?}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                anyhow::bail!("auxiliary.base_url must use the http or https scheme");
            }
        }
        Ok(())
    }

    fn validate_fallbacks(&self) -> anyhow::Result<()> {
        for (index, entry) in self.model.fallback.iter().enumerate() {
            if entry.model.trim().is_empty() {
                anyhow::bail!("model.fallback[{index}].model must not be empty");
            }
            if !entry.provider.trim().is_empty()
                && ProviderKind::parse(&entry.provider).is_none()
                && preset(&entry.provider).is_none()
            {
                anyhow::bail!(
                    "model.fallback[{index}].provider {:?} is not a recognized provider or preset",
                    entry.provider
                );
            }
            if entry
                .api_key_env
                .as_deref()
                .is_some_and(|env| env.trim().is_empty())
            {
                anyhow::bail!("model.fallback[{index}].api_key_env must not be empty when set");
            }
            if let Some(base_url) = entry
                .base_url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                let parsed = reqwest::Url::parse(base_url).with_context(|| {
                    format!("invalid model.fallback[{index}].base_url {base_url:?}")
                })?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    anyhow::bail!(
                        "model.fallback[{index}].base_url must use the http or https scheme"
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_security(&self) -> anyhow::Result<()> {
        if let Some(path) = &self.security.ca_bundle {
            parse_ca_bundle(path)?;
        }
        for origin in &self.server.cors_allowed_origins {
            validate_cors_origin(origin)?;
        }
        Ok(())
    }

    fn validate_monitoring(&self) -> anyhow::Result<()> {
        if self.monitoring.enabled {
            let endpoint = self
                .monitoring
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            match endpoint {
                Some(endpoint) => {
                    let parsed = reqwest::Url::parse(endpoint)
                        .with_context(|| format!("invalid monitoring.endpoint {endpoint:?}"))?;
                    if !matches!(parsed.scheme(), "http" | "https") {
                        anyhow::bail!("monitoring.endpoint must use the http or https scheme");
                    }
                }
                None => {
                    anyhow::bail!("monitoring.enabled is true but monitoring.endpoint is not set")
                }
            }
        }
        Ok(())
    }

    fn validate_cost(&self) -> anyhow::Result<()> {
        // [cost] must have a usable warning fraction and non-negative caps.
        if !self.cost.warn_ratio.is_finite()
            || self.cost.warn_ratio <= 0.0
            || self.cost.warn_ratio > 1.0
        {
            anyhow::bail!(
                "cost.warn_ratio must be greater than 0 and at most 1 (got {})",
                self.cost.warn_ratio
            );
        }
        for (field, value) in [
            ("cost.max_usd_per_day", self.cost.max_usd_per_day),
            ("cost.max_usd_per_run", self.cost.max_usd_per_run),
        ] {
            if let Some(value) = value {
                if !value.is_finite() || value < 0.0 {
                    anyhow::bail!("{field} must be a finite, non-negative amount when set");
                }
            }
        }
        Ok(())
    }

    fn validate_credentials(&self) -> anyhow::Result<()> {
        // [[model.credentials]] pool entries each need a source env var.
        for (index, entry) in self.model.credentials.iter().enumerate() {
            if entry.api_key_env.trim().is_empty() {
                anyhow::bail!("model.credentials[{index}].api_key_env must not be empty");
            }
            if entry.weight == 0 {
                anyhow::bail!("model.credentials[{index}].weight must be at least 1");
            }
        }
        Ok(())
    }

    fn validate_mcp(&self) -> anyhow::Result<()> {
        // [[mcp.server]] names are unique and each transport carries what it needs.
        let mut mcp_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (index, server) in self.mcp.server.iter().enumerate() {
            let name = server.name.trim();
            if name.is_empty() {
                anyhow::bail!("mcp.server[{index}].name must not be empty");
            }
            if !mcp_names.insert(name) {
                anyhow::bail!("mcp.server[{index}].name {name:?} is not unique");
            }
            match server.transport {
                McpTransport::Stdio => {
                    if !server
                        .command
                        .as_deref()
                        .is_some_and(|command| !command.trim().is_empty())
                    {
                        anyhow::bail!(
                            "mcp.server[{index}] ({name:?}) uses the stdio transport but has no command"
                        );
                    }
                }
                McpTransport::Http => {
                    if !server
                        .url
                        .as_deref()
                        .is_some_and(|url| !url.trim().is_empty())
                    {
                        anyhow::bail!(
                            "mcp.server[{index}] ({name:?}) uses the http transport but has no url"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Load the default configuration path after importing the secrets file.
    pub fn load_default() -> anyhow::Result<Self> {
        load_secrets_env();
        let path = config_file_path();
        let mut config = Self::load_raw(if path.exists() {
            Some(path.as_path())
        } else {
            None
        })?;
        config.apply_env();
        config.apply_migrations();
        Ok(config)
    }

    /// Run every pending schema migration and log each applied step. Idempotent.
    fn apply_migrations(&mut self) {
        for step in self.migrate() {
            tracing::info!(migration = %step, "applied config migration");
        }
    }

    /// Upgrade this configuration to [`CONFIG_VERSION`] in memory, returning a description of
    /// every migration applied. Migrating twice is a no-op. A configuration written by a newer
    /// schema is never downgraded: it is left untouched and the reason is reported.
    pub fn migrate(&mut self) -> Vec<String> {
        let mut applied = Vec::new();
        if self.config_version > CONFIG_VERSION {
            applied.push(format!(
                "config_version {} is newer than the supported {}; leaving it unchanged",
                self.config_version, CONFIG_VERSION
            ));
            return applied;
        }
        while self.config_version < CONFIG_VERSION {
            let from = self.config_version;
            match from {
                // Pre-versioning baseline: no field transformation yet. The stamp records
                // that the file was brought forward. Future schema steps join the ladder here.
                0 => {
                    migrate_v0_to_v1(self);
                    applied.push(format!(
                        "config_version {from} -> {}: baseline schema stamp",
                        from + 1
                    ));
                }
                // A gap in the ladder: advance one version so the loop terminates and report it.
                other => applied.push(format!(
                    "no migration registered for config_version {other}; advancing to {}",
                    other + 1
                )),
            }
            self.config_version = from + 1;
        }
        applied
    }

    /// Pretty TOML. Keys never appear: Config holds only the name of each key's env var.
    pub fn render_toml(&self) -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn write_to(&self, path: &Path) -> anyhow::Result<()> {
        let text = self.render_toml()?;
        crate::atomic_file::write(path, text.as_bytes())
            .with_context(|| format!("write {}", path.display()))
    }

    fn apply_env(&mut self) {
        if let Ok(value) = std::env::var("SILVER_BIND") {
            self.server.bind = value;
        }
        if let Ok(value) = std::env::var("SILVER_DATA_DIR") {
            self.data.directory = Some(PathBuf::from(value));
        }
        if let Ok(value) = std::env::var("SILVER_MODEL") {
            self.model.name = value;
        }
        if let Ok(value) = std::env::var("SILVER_MODEL_PROVIDER") {
            self.model.provider = value;
        }
        if let Ok(value) = std::env::var("SILVER_MODEL_BASE_URL") {
            self.model.base_url = Some(value);
        }
        if let Ok(value) = std::env::var("SILVER_BEARER_TOKEN") {
            self.server.bearer_token = Some(value);
        }
        if let Ok(value) = std::env::var("SILVER_MAX_CONCURRENT_RUNS") {
            if let Ok(parsed) = value.parse() {
                self.server.max_concurrent_runs = parsed;
            }
        }
        if self.security.ca_bundle.is_none() {
            self.security.ca_bundle = ca_bundle_from_env();
        }
    }

    /// `data.directory` (including `SILVER_DATA_DIR`), then `SILVER_PROFILE`
    /// (`<base>/profiles/<name>/`), then the platform data directory.
    pub fn data_dir(&self) -> PathBuf {
        resolve_dir(
            Option::clone(&self.data.directory),
            active_profile().as_deref(),
            default_data_dir(),
        )
    }

    /// Read the provider secret from the environment. Never logged or persisted.
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.model.api_key_env)
            .ok()
            .filter(|v| !v.is_empty())
    }

    /// The configured reasoning effort, lowercased and trimmed, when one is set.
    pub fn resolved_reasoning_effort(&self) -> Option<String> {
        self.model
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase)
    }

    /// True when the configured bind address is reachable beyond loopback and therefore must
    /// be protected by a bearer token.
    pub fn needs_bearer_token(&self) -> bool {
        !bind_is_loopback(&self.server.bind)
    }

    /// True when monitoring is enabled and carries a usable endpoint.
    pub fn monitoring_configured(&self) -> bool {
        self.monitoring.enabled
            && self
                .monitoring
                .endpoint
                .as_deref()
                .is_some_and(|endpoint| !endpoint.trim().is_empty())
    }

    /// The provider preset named by 'model.provider', when recognised.
    pub fn provider_preset(&self) -> Option<&'static ProviderPreset> {
        preset(&self.model.provider)
    }

    /// The wire protocol to use, falling back to the provider preset then OpenAI-compatible.
    pub fn kind(&self) -> ProviderKind {
        match ProviderKind::parse(&self.model.kind) {
            Some(kind) if kind != ProviderKind::OpenAiCompatible => kind,
            other => match self.provider_preset() {
                Some(preset) => preset.kind,
                None => other.unwrap_or(ProviderKind::OpenAiCompatible),
            },
        }
    }

    /// Resolve the base URL: explicit config, then the provider preset, then the OpenAI default.
    pub fn resolved_base_url(&self) -> &str {
        if let Some(base_url) = &self.model.base_url {
            if !base_url.trim().is_empty() {
                return base_url;
            }
        }
        if let Some(preset) = self.provider_preset() {
            if !preset.base_url.is_empty() {
                return preset.base_url;
            }
        }
        "https://api.openai.com/v1"
    }

    /// Resolve the model id: explicit config, then the provider preset default.
    pub fn resolved_model(&self) -> &str {
        if !self.model.name.trim().is_empty() {
            return &self.model.name;
        }
        if let Some(preset) = self.provider_preset() {
            if !preset.default_model.is_empty() {
                return preset.default_model;
            }
        }
        ""
    }

    /// Resolve the API key environment variable: explicit config, then the provider preset.
    pub fn resolved_api_key_env(&self) -> &str {
        if !self.model.api_key_env.trim().is_empty() {
            return &self.model.api_key_env;
        }
        if let Some(preset) = self.provider_preset() {
            if !preset.api_key_env.is_empty() {
                return preset.api_key_env;
            }
        }
        ""
    }

    /// True when at least one ordered fallback route is configured.
    pub fn fallbacks_configured(&self) -> bool {
        !self.model.fallback.is_empty()
    }

    /// The ordered fallback chain exactly as configured.
    pub fn fallback_chain(&self) -> &[FallbackConfig] {
        &self.model.fallback
    }

    /// Transport for a fallback route: explicit provider, then a known preset, then the
    /// primary model's provider kind.
    pub fn fallback_kind(&self, entry: &FallbackConfig) -> ProviderKind {
        ProviderKind::parse(&entry.provider)
            .or_else(|| preset(&entry.provider).map(|preset| preset.kind))
            .unwrap_or_else(|| self.kind())
    }

    /// Resolve a fallback model id: explicit, then the provider preset default.
    pub fn resolved_fallback_model<'a>(&'a self, entry: &'a FallbackConfig) -> &'a str {
        if !entry.model.trim().is_empty() {
            return &entry.model;
        }
        if let Some(preset) = preset(&entry.provider) {
            if !preset.default_model.is_empty() {
                return preset.default_model;
            }
        }
        ""
    }

    /// Resolve a fallback base URL: explicit, then preset, then the primary base URL.
    pub fn resolved_fallback_base_url<'a>(&'a self, entry: &'a FallbackConfig) -> &'a str {
        if let Some(base_url) = entry.base_url.as_deref() {
            if !base_url.trim().is_empty() {
                return base_url;
            }
        }
        if let Some(preset) = preset(&entry.provider) {
            if !preset.base_url.is_empty() {
                return preset.base_url;
            }
        }
        self.resolved_base_url()
    }

    /// Resolve a fallback API key env var: explicit, preset, then the primary key env.
    pub fn resolved_fallback_api_key_env<'a>(&'a self, entry: &'a FallbackConfig) -> &'a str {
        if let Some(env) = entry.api_key_env.as_deref() {
            if !env.trim().is_empty() {
                return env;
            }
        }
        if let Some(preset) = preset(&entry.provider) {
            if !preset.api_key_env.is_empty() {
                return preset.api_key_env;
            }
        }
        self.resolved_api_key_env()
    }

    /// True when the auxiliary route is enabled and names a model.
    pub fn auxiliary_configured(&self) -> bool {
        self.auxiliary.enabled && !self.auxiliary.model.trim().is_empty()
    }

    /// Transport for the auxiliary route: explicit provider, then a known preset, then the
    /// main model's provider kind.
    pub fn auxiliary_kind(&self) -> ProviderKind {
        ProviderKind::parse(&self.auxiliary.provider)
            .or_else(|| preset(&self.auxiliary.provider).map(|entry| entry.kind))
            .unwrap_or_else(|| self.kind())
    }

    /// Resolve the auxiliary model id: explicit, then preset, then the main model.
    pub fn resolved_aux_model(&self) -> &str {
        if !self.auxiliary.model.trim().is_empty() {
            return &self.auxiliary.model;
        }
        if let Some(entry) = preset(&self.auxiliary.provider) {
            if !entry.default_model.is_empty() {
                return entry.default_model;
            }
        }
        self.resolved_model()
    }

    /// Resolve the auxiliary base URL: explicit, then preset, then the main base URL.
    pub fn resolved_aux_base_url(&self) -> &str {
        if let Some(base_url) = self.auxiliary.base_url.as_deref() {
            if !base_url.trim().is_empty() {
                return base_url;
            }
        }
        if let Some(entry) = preset(&self.auxiliary.provider) {
            if !entry.base_url.is_empty() {
                return entry.base_url;
            }
        }
        self.resolved_base_url()
    }

    /// Resolve the auxiliary API key environment variable: explicit, preset, then the main one.
    pub fn resolved_aux_api_key_env(&self) -> &str {
        if let Some(env) = self.auxiliary.api_key_env.as_deref() {
            if !env.trim().is_empty() {
                return env;
            }
        }
        if let Some(entry) = preset(&self.auxiliary.provider) {
            if !entry.api_key_env.is_empty() {
                return entry.api_key_env;
            }
        }
        self.resolved_api_key_env()
    }

    /// The active profile from `SILVER_PROFILE`, fixed for the daemon's lifetime; None for the
    /// base.
    pub fn profile_name(&self) -> Option<String> {
        active_profile()
    }

    /// True when automatic checkpoints are enabled.
    pub fn checkpoints_enabled(&self) -> bool {
        self.checkpoints.enabled
    }

    /// True when spend guardrails are enforced.
    pub fn cost_enabled(&self) -> bool {
        self.cost.enabled
    }

    /// True when language-server integration is enabled.
    pub fn lsp_enabled(&self) -> bool {
        self.lsp.enabled
    }

    /// True when runs are isolated in a git worktree.
    pub fn worktree_enabled(&self) -> bool {
        self.worktree.enabled
    }

    /// The worktree root, defaulting to `.worktrees` when unset or blank.
    pub fn worktree_root(&self) -> PathBuf {
        self.worktree
            .root
            .as_deref()
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".worktrees"))
    }

    /// Resolve the OAuth token store, defaulting to `<data_dir>/oauth.json`.
    pub fn oauth_token_store(&self) -> PathBuf {
        Option::clone(&self.oauth.token_store).unwrap_or_else(|| self.data_dir().join("oauth.json"))
    }

    /// The credential store `/login` writes: `<data_dir>/auth.json`.
    pub fn auth_store(&self) -> PathBuf {
        self.data_dir().join("auth.json")
    }

    /// Every configured MCP server, in declaration order.
    pub fn mcp_servers(&self) -> &[McpServerConfig] {
        &self.mcp.server
    }

    /// The enabled MCP servers, in declaration order.
    pub fn enabled_mcp_servers(&self) -> Vec<&McpServerConfig> {
        self.mcp
            .server
            .iter()
            .filter(|server| server.enabled)
            .collect()
    }

    /// The effective API-key pool: the configured `[[model.credentials]]`, or a single entry
    /// synthesized from the primary `model.api_key_env` when none are configured.
    pub fn credential_pool(&self) -> Vec<ModelCredential> {
        if self.model.credentials.is_empty() {
            vec![ModelCredential {
                label: None,
                api_key_env: self.resolved_api_key_env().to_string(),
                weight: 1,
            }]
        } else {
            Vec::clone(&self.model.credentials)
        }
    }
}

/// Accept `"*"` or a bare http(s) `scheme://host[:port]`; a path, query, fragment or userinfo is
/// rejected so a typo cannot silently change the allow list.
pub fn validate_cors_origin(origin: &str) -> anyhow::Result<()> {
    if origin == "*" {
        return Ok(());
    }
    let parsed =
        reqwest::Url::parse(origin).with_context(|| format!("invalid CORS origin {origin:?}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("CORS origin {origin:?} must use the http or https scheme");
    }
    if parsed.host_str().is_none() {
        anyhow::bail!("CORS origin {origin:?} must include a host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("CORS origin {origin:?} must not include userinfo");
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!(
            "CORS origin {origin:?} must be a bare scheme://host[:port] origin without a path, query, or fragment"
        );
    }
    Ok(())
}

/// Baseline schema step (version 0 -> 1): the pre-versioning config had no `config_version`
/// key. No field was renamed or restructured, so the step is a no-op; the caller stamps the
/// version. This function is the seam where the next schema change adds its transformation.
fn migrate_v0_to_v1(_config: &mut Config) {}

/// Parse a PEM bundle or a single DER certificate into reqwest roots.
pub fn parse_ca_bundle(path: &Path) -> anyhow::Result<Vec<reqwest::Certificate>> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read CA bundle {}", path.display()))?;
    match reqwest::Certificate::from_pem_bundle(&bytes) {
        Ok(certificates) if !certificates.is_empty() => Ok(certificates),
        _ => reqwest::Certificate::from_der(&bytes)
            .map(|certificate| vec![certificate])
            .with_context(|| format!("parse CA bundle {}", path.display())),
    }
}

/// Environment variables honored as a CA bundle when security.ca_bundle is unset. The first
/// one that is set wins, matching the order used by common TLS clients.
const CA_BUNDLE_ENV_VARS: [&str; 4] = [
    "SILVER_CA_BUNDLE",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
];

/// The first configured CA-bundle environment variable, if any.
pub fn ca_bundle_from_env() -> Option<PathBuf> {
    CA_BUNDLE_ENV_VARS.iter().find_map(|name| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

/// Whether a bind address stays on the loopback interface.
pub fn bind_is_loopback(bind: &str) -> bool {
    let host = bind_host(bind);
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Extract the host portion of a host:port, [v6]:port or bare host bind string.
fn bind_host(bind: &str) -> String {
    let trimmed = bind.trim();
    if let Some(rest) = trimmed.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return rest[..end].to_string();
        }
    }
    match trimmed.rsplit_once(':') {
        Some((host, _port)) if !host.contains(':') => host.to_string(),
        _ => trimmed.to_string(),
    }
}

/// The client builder every outbound HTTP path shares: CA bundle and ssl_verify applied, with a
/// loud warning when verification is off. Callers add their own timeout.
pub fn http_client_builder(security: &SecurityConfig) -> anyhow::Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("silver/", env!("CARGO_PKG_VERSION")))
        .danger_accept_invalid_certs(!security.ssl_verify);
    if let Some(path) = &security.ca_bundle {
        for certificate in parse_ca_bundle(path)? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    if !security.ssl_verify {
        tracing::warn!(
            "TLS certificate verification is disabled (security.ssl_verify=false); outbound HTTPS is unauthenticated"
        );
    }
    Ok(builder)
}

/// Environment variable naming the active profile. One daemon serves one profile.
pub const PROFILE_ENV: &str = "SILVER_PROFILE";

/// A safe `profiles/<name>/` component: 1–64 ASCII alphanumerics, `-` or `_`, starting
/// alphanumeric.
pub fn is_valid_profile_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `SILVER_PROFILE`, trimmed. Unset, blank, or a name that could escape the profiles directory
/// (the last with a warning) means the base install.
pub fn active_profile() -> Option<String> {
    let raw = std::env::var(PROFILE_ENV).ok()?;
    let name = raw.trim();
    if name.is_empty() {
        return None;
    }
    if !is_valid_profile_name(name) {
        tracing::warn!(profile = %name, "ignoring invalid SILVER_PROFILE name");
        return None;
    }
    Some(name.to_string())
}

/// Join the profile subdirectory onto a base directory.
fn with_profile(base: PathBuf, profile: Option<&str>) -> PathBuf {
    match profile {
        Some(name) => base.join("profiles").join(name),
        None => base,
    }
}

/// The global skills directory, shared by every workspace and overlaid per run by a project's
/// own `.agents/skills`. `SILVER_SKILLS_DIR` wins; otherwise it is `~/.agents/skills`.
pub fn global_skills_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SILVER_SKILLS_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(base) = directories::BaseDirs::new() {
        return base.home_dir().join(".agents").join("skills");
    }
    PathBuf::from(".agents").join("skills")
}

/// The global subagent definitions directory, read for every run and overlaid by the
/// workspace's own `.silver/agents` (the project definition wins, as with skills).
pub fn global_agents_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SILVER_AGENTS_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(base) = directories::BaseDirs::new() {
        return base.home_dir().join(".silver").join("agents");
    }
    PathBuf::from(".silver").join("agents")
}

/// The platform default config directory, before any profile or environment override.
fn default_config_dir() -> PathBuf {
    if let Some(dirs) = silver_core::dirs::project_dirs() {
        dirs.config_dir().to_path_buf()
    } else {
        PathBuf::from("./silver-config")
    }
}

/// Resolve a directory from an explicit override, then the profile, then a platform base.
///
/// An explicit override wins outright: the profile is never appended to it.
fn resolve_dir(override_dir: Option<PathBuf>, profile: Option<&str>, platform: PathBuf) -> PathBuf {
    match override_dir {
        Some(dir) => dir,
        None => with_profile(platform, profile),
    }
}

/// `SILVER_CONFIG_DIR`, then `SILVER_PROFILE` (`<base>/profiles/<name>/`), then the platform
/// config directory.
pub fn config_dir() -> PathBuf {
    resolve_dir(
        std::env::var_os("SILVER_CONFIG_DIR").map(PathBuf::from),
        active_profile().as_deref(),
        default_config_dir(),
    )
}

/// `SILVER_CONFIG` outright, else `<config_dir>/config.toml`.
pub fn config_file_path() -> PathBuf {
    if let Ok(value) = std::env::var("SILVER_CONFIG") {
        return PathBuf::from(value);
    }
    config_dir().join("config.toml")
}

/// The path to the optional secrets file ('NAME=value' lines imported into the environment).
pub fn secrets_env_path() -> PathBuf {
    config_dir().join("secrets.env")
}

/// Import KEY=VALUE lines from the secrets file into the environment, returning how many were set.
/// Existing variables win; values are never logged.
pub fn load_secrets_env() -> usize {
    let Ok(contents) = std::fs::read_to_string(secrets_env_path()) else {
        return 0;
    };
    let mut loaded = 0usize;
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = strip_quotes(value.trim());
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
            loaded += 1;
        }
    }
    loaded
}

fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn default_data_dir() -> PathBuf {
    if let Some(dirs) = silver_core::dirs::project_dirs() {
        dirs.data_dir().to_path_buf()
    } else {
        PathBuf::from("./silver-data")
    }
}
