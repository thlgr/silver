//! silver: the authoritative agent process, serving the HTTP API and the embedded web UI.

use anyhow::Context;
use clap::Parser;
use silver::api::{self, AppState};
use silver::auth::AuthStore;
use silver::checkpoints::Checkpoints;
use silver::config::Config;
use silver::credential_pool::{CredentialPoolModel, PoolStrategy};
use silver::db::Db;
use silver::document_index::DbDocumentIndex;
use silver::fallback::FallbackModel;
use silver::mcp::McpManager;
use silver::memory_fs::FsMemoryStore;
use silver::oauth::OAuthManager;
use silver::routed::{build_transport, RoutedModel};
use silver::run_manager::{ApprovalSetup, RunManager};
use silver::session_search::DbSessionSearch;
use silver::skills::SkillsStore;
use silver::terminal::TerminalManager;
use silver::todo_store::TodoStore;
use silver::web::HttpWebBackend;
use silver_core::agent::{Agent, AgentConfig};
use silver_core::memory::MemoryStore;
use silver_core::model::Model;
use silver_core::services::DocumentIndex;
use silver_core::services::ToolServices;
use silver_core::session::SessionSearch;
use silver_core::tool::{ApprovalPolicy, ToolRegistry};
use silver_protocol::providers::ProviderKind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long a graceful shutdown waits for connections the streams did not close.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(name = "silver", version, about = "silver agent: HTTP API and web UI")]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the bind address.
    #[arg(long)]
    bind: Option<String>,
    /// Override the data directory.
    #[arg(long = "data-dir")]
    data_dir: Option<PathBuf>,
    /// Skip approval prompts for the whole process (pins the approval mode to off).
    #[arg(long)]
    yolo: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let (config, config_path, data_dir, frozen_yolo) = load_config(args)?;
    // Mode CLIs (opencode, grok, ...) come from the login shell's PATH, which a GUI-launched
    // process does not have; hydrate once so resolving an ACP mode never stalls a request.
    silver::agent_modes::hydrate_path();

    // Opt-in monitoring: off unless [monitoring] enabled with an endpoint. The emitter is a
    // global so producers and the run watcher can reach it without a threaded handle.
    if silver::monitoring::install(&config.monitoring) {
        tracing::info!(
            endpoint = config.monitoring.endpoint.as_deref().unwrap_or_default(),
            "opt-in monitoring enabled"
        );
        silver::monitoring::spawn_rate_limit_watcher();
    }

    let db = open_database(&data_dir).await?;
    let checkpoints = build_checkpoints(&db, &data_dir, &config);

    let (endpoint, key, context_length) = resolve_endpoint(&config, &data_dir).await?;
    let primary = build_primary(&config, &endpoint, &key);
    let model = build_fallback_chain(&config, &endpoint, primary);
    let aux_model = build_aux_model(&config, &endpoint);

    let (mcp, tools) = build_tools(&config).await;
    let (agent_config, subagent_config) = build_agent_configs(&config, context_length);
    let approval_policy = build_approval_policy(&config);

    let fs_memory = Arc::new(FsMemoryStore::new(PathBuf::clone(&data_dir)));
    let memory = Arc::clone(&fs_memory) as Arc<dyn MemoryStore>;
    let search: Arc<dyn SessionSearch> = Arc::new(DbSessionSearch::new(Db::clone(&db)));
    let documents: Arc<dyn DocumentIndex> = Arc::new(DbDocumentIndex::new(Db::clone(&db)));

    // OAuth owns the token store and any in-flight login state; the credential store owns the
    // keys typed at '/login'. Both are read per request, so signing in takes effect next turn.
    let oauth = Arc::new(OAuthManager::new(&config));
    let auth = Arc::new(AuthStore::open(config.auth_store()));
    let routed = build_routed(&config, model, &auth, &oauth, &endpoint, key);
    if let Some(active) = routed.active_provider() {
        tracing::info!(provider = %active, "runs route through the signed-in provider");
    }
    let acting = build_acting(&config, &routed).await;

    // The compaction budget is sized per run for the model that run uses: a session override
    // or a provider switch must not keep compacting at the startup model's window.
    let context_resolver = Arc::new(silver::context_length::ModelContextResolver::new(
        Arc::clone(&routed),
        PathBuf::clone(&data_dir),
        config.model.context_length,
    ));
    let (agent, skills, advisor) = build_agent(
        &config,
        acting,
        tools,
        agent_config,
        approval_policy,
        aux_model.as_ref(),
        &context_resolver,
        &endpoint,
        &auth,
        &config_path,
        &data_dir,
    );

    configure_lsp(&config);
    let (subagents, agent_store) = build_subagents(Arc::clone(&agent), &config, subagent_config);
    let chat = silver::chat::ChatHub::new(Db::clone(&db));
    let mut services = build_services(
        &config,
        skills,
        Arc::clone(&checkpoints),
        subagents,
        Arc::clone(&memory),
        search,
        documents,
    );
    services.team = Some(Arc::new(silver::chat::TeamBackend(Arc::clone(&chat))));
    let runs = build_run_manager(
        Db::clone(&db),
        agent,
        Arc::clone(&memory),
        &config,
        &context_resolver,
        services,
        aux_model,
        frozen_yolo,
        config_path,
        &auth,
    );
    // The emergency stop is persistent: a sentinel left behind by an earlier process holds
    // new runs from the moment the daemon starts.
    if runs.is_paused() {
        tracing::warn!(
            sentinel = %runs.estop_path().display(),
            "emergency stop is engaged; new runs are held until resumed"
        );
    }
    chat.attach(Arc::clone(&runs));
    silver::acp::set_broker(Arc::<silver::chat::ChatHub>::clone(&chat));
    chat.recover().await?;
    let state = AppState {
        db,
        chat,
        runs: Arc::clone(&runs),
        config: Arc::clone(&config),
        memory: fs_memory,
        started_at: chrono::Utc::now(),
        checkpoints: Some(checkpoints),
        oauth: Some(oauth),
        auth: Some(auth),
        routes: Some(routed),
        context_length,
        context_resolver: Some(context_resolver),
        advisor: Some(advisor),
        agents: Some(agent_store),
        shutdown: CancellationToken::new(),
    };

    serve(state, &config, &runs, &data_dir).await?;

    mcp.shutdown().await;
    silver_core::lsp::shutdown().await;
    tracing::info!("silver stopped");
    Ok(())
}

/// The agent, its skills store and its advisor, assembled from the resolved collaborators.
#[expect(
    clippy::too_many_arguments,
    reason = "the agent's collaborators are assembled once"
)]
fn build_agent(
    config: &Config,
    acting: Arc<dyn Model>,
    tools: Arc<ToolRegistry>,
    agent_config: AgentConfig,
    approval_policy: ApprovalPolicy,
    aux_model: Option<&Arc<dyn Model>>,
    context_resolver: &Arc<silver::context_length::ModelContextResolver>,
    endpoint: &Endpoint,
    auth: &Arc<AuthStore>,
    config_path: &PathBuf,
    data_dir: &PathBuf,
) -> (
    Arc<Agent>,
    Arc<SkillsStore>,
    Arc<silver::advisor::JevAdvisor>,
) {
    let toolsets = resolve_toolsets(config);
    let skills = Arc::new(SkillsStore::with_global_root(
        silver::config::global_skills_dir(),
        PathBuf::clone(data_dir),
    ));
    // Built even when off, so the web UI can switch it on without a restart.
    let advisor = Arc::new(silver::advisor::JevAdvisor::new(
        reqwest::Client::clone(&endpoint.http),
        Arc::clone(auth),
        Arc::clone(&skills),
        toolsets.allows("web"),
        config.agent.jev_hints,
        Some(PathBuf::clone(config_path)),
    ));
    let agent = Arc::new(
        Agent::new(acting, tools, agent_config, approval_policy)
            .with_aux_model(aux_model.map(Arc::clone))
            .with_advisor(Some(
                Arc::clone(&advisor) as Arc<dyn silver_core::advisor::Advisor>
            ))
            .with_context_resolver(
                Arc::clone(context_resolver) as Arc<dyn silver_core::model::ContextLengthResolver>
            )
            .with_toolsets(toolsets)
            .with_tool_names(&config.tools.enabled, &config.tools.disabled),
    );
    (agent, skills, advisor)
}

/// Bind the API listener and serve until a shutdown signal stops the runs.
async fn serve(
    state: AppState,
    config: &Config,
    runs: &Arc<RunManager>,
    data_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.server.bind)
        .await
        .with_context(|| format!("bind {}", config.server.bind))?;
    tracing::info!(bind = %config.server.bind, data_dir = %data_dir.display(), "silver listening");

    // A client still connected would otherwise hold the graceful shutdown open: the run and
    // chat streams watch this token and end with the signal.
    let shutdown = state.shutdown.clone();
    let serving = axum::serve(
        listener,
        api::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown({
        // Stop runs as soon as the signal arrives: their event streams hold connections
        // open, and a graceful shutdown waits for every connection to close.
        let runs = Arc::clone(runs);
        let shutdown = shutdown.clone();
        async move {
            shutdown_signal().await;
            runs.shutdown().await;
            shutdown.cancel();
        }
    });

    tokio::select! {
        result = serving => result.context("serve")?,
        // The streams end on the token, so the server normally stops at once; this is the
        // backstop for a connection that never closes.
        () = async {
            shutdown.cancelled().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!(
            "a connection did not close within {}s; stopping anyway",
            SHUTDOWN_GRACE.as_secs()
        ),
    }
    Ok(())
}

/// Parse arguments, load and validate the config, and prepare the data directory.
fn load_config(args: Args) -> anyhow::Result<(Arc<Config>, PathBuf, PathBuf, bool)> {
    // --yolo / SILVER_YOLO_MODE=1 pins the approval mode to off for this process.
    let frozen_yolo = args.yolo || std::env::var("SILVER_YOLO_MODE").as_deref() == Ok("1");
    let mut config = if args.config.is_some() {
        silver::config::load_secrets_env();
        Config::load(args.config.as_deref()).context("load configuration")?
    } else {
        Config::load_default().context("load configuration")?
    };
    // The default config file is where a runtime approval-mode change is persisted.
    let config_path = args.config.unwrap_or_else(silver::config::config_file_path);
    if let Some(bind) = args.bind {
        config.server.bind = bind;
    }
    if let Some(data_dir) = args.data_dir {
        config.data.directory = Some(data_dir);
    }
    config.validate().context("validate configuration")?;
    let config = Arc::new(config);

    let data_dir = config.data_dir();
    create_private_dir(&data_dir)
        .with_context(|| format!("create data directory {}", data_dir.display()))?;
    silver::logging::init(&data_dir);
    Ok((config, config_path, data_dir, frozen_yolo))
}

/// Open the database, apply the migration and recover interrupted runs. Retention runs off the
/// startup path, bounded so a large database never delays the listener.
async fn open_database(data_dir: &std::path::Path) -> anyhow::Result<Db> {
    let db = Db::open(&data_dir.join("state.db"))
        .await
        .context("open database")?;
    db.migrate().await.context("apply migrations")?;
    let recovered = db
        .recover_interrupted_runs()
        .await
        .context("recover interrupted runs")?;
    if recovered > 0 {
        tracing::warn!(
            count = recovered,
            "marked interrupted runs as failed after restart"
        );
    }
    {
        let db = Db::clone(&db);
        tokio::spawn(async move {
            if tokio::time::timeout(
                silver::db::STARTUP_MAINTENANCE_BUDGET,
                db.run_startup_maintenance(),
            )
            .await
            .is_err()
            {
                tracing::debug!("startup maintenance exceeded its budget");
            }
        });
    }
    Ok(db)
}

/// The checkpoint store and its bounded startup prune.
fn build_checkpoints(db: &Db, data_dir: &std::path::Path, config: &Config) -> Arc<Checkpoints> {
    let checkpoints = Arc::new(Checkpoints::new(
        Db::clone(db),
        data_dir,
        &config.checkpoints,
    ));
    {
        let checkpoints = Arc::clone(&checkpoints);
        tokio::spawn(async move {
            match checkpoints.prune_on_startup().await {
                Ok(removed) if removed > 0 => {
                    tracing::info!(removed, "pruned checkpoint history on startup")
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "checkpoint prune on startup failed"),
            }
        });
    }
    checkpoints
}

/// A transport endpoint shared by the primary, the credential pool, the fallback chain and the
/// auxiliary model.
struct Endpoint {
    kind: ProviderKind,
    base_url: String,
    model_name: String,
    http: reqwest::Client,
}

impl Endpoint {
    fn transport(&self, key: &str) -> Arc<dyn Model> {
        build_transport(self.kind, &self.base_url, key, &self.model_name, &self.http)
            .expect("the configured endpoint always resolves a base URL")
    }
}

/// Resolve the configured endpoint, its key and the startup context window.
async fn resolve_endpoint(
    config: &Config,
    data_dir: &std::path::Path,
) -> anyhow::Result<(Endpoint, String, Option<usize>)> {
    let kind = config.kind();
    let api_key_env = config.resolved_api_key_env();
    let api_key = std::env::var(api_key_env)
        .ok()
        .filter(|value| !value.is_empty());
    let needs_key = config
        .provider_preset()
        .is_none_or(|preset| preset.requires_key);
    if api_key.is_none() && kind != ProviderKind::Ollama && needs_key {
        tracing::warn!(
            env = %api_key_env,
            "model API key is not set; sign in from the web UI, add it to secrets.env, or export the environment variable"
        );
    }
    let base_url = config.resolved_base_url();
    let model_name = config.resolved_model();
    let key = api_key.unwrap_or_default();
    // One TLS policy for model traffic: CA bundle plus the ssl_verify toggle.
    let http = silver::config::http_client_builder(&config.security)
        .context("build http client")?
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build http client")?;
    // Only a fallback: each run re-resolves the window for the model it actually uses.
    let context_length = silver::provider::resolve_model_context_length(
        &http,
        base_url,
        &key,
        model_name,
        data_dir,
        kind,
        config.model.context_length,
    )
    .await;
    let endpoint = Endpoint {
        kind,
        base_url: base_url.to_string(),
        model_name: model_name.to_string(),
        http,
    };
    Ok((endpoint, key, context_length))
}

/// The primary model: a credential pool when several keys are configured, else one transport.
fn build_primary(config: &Config, endpoint: &Endpoint, key: &str) -> Arc<dyn Model> {
    let credentials = config.credential_pool();
    if credentials.len() <= 1 {
        return endpoint.transport(key);
    }
    let mut models: Vec<Arc<dyn Model>> = Vec::with_capacity(credentials.len());
    for credential in &credentials {
        let credential_key = credential.api_key().unwrap_or_default();
        if credential_key.is_empty() && endpoint.kind != ProviderKind::Ollama {
            tracing::warn!(
                env = %credential.api_key_env,
                "credential pool key is not set; its requests will use an empty key"
            );
        }
        models.push(endpoint.transport(&credential_key));
    }
    tracing::info!(keys = models.len(), "credential pool configured");
    Arc::new(CredentialPoolModel::new(PoolStrategy::FillFirst, models))
}

/// Ordered fallback routes, wrapped with the primary at index 0; a route whose credential env
/// var is unset is skipped since it could never authenticate.
fn build_fallback_chain(
    config: &Config,
    endpoint: &Endpoint,
    primary: Arc<dyn Model>,
) -> Arc<dyn Model> {
    if !config.fallbacks_configured() {
        return primary;
    }
    let mut chain: Vec<Arc<dyn Model>> = vec![primary];
    for entry in config.fallback_chain() {
        let fallback_kind = config.fallback_kind(entry);
        let fallback_model = config.resolved_fallback_model(entry);
        let fallback_base_url = config.resolved_fallback_base_url(entry);
        let fallback_key_env = config.resolved_fallback_api_key_env(entry);
        let fallback_key = std::env::var(fallback_key_env)
            .ok()
            .filter(|value| !value.is_empty());
        if fallback_key.is_none() && fallback_kind != ProviderKind::Ollama {
            tracing::warn!(
                env = %fallback_key_env,
                model = %fallback_model,
                "fallback model API key is not set; skipping route"
            );
            continue;
        }
        let provider: Arc<dyn Model> = build_transport(
            fallback_kind,
            fallback_base_url,
            fallback_key.as_deref().unwrap_or_default(),
            fallback_model,
            &endpoint.http,
        )
        .expect("a fallback route always resolves a base URL");
        chain.push(provider);
    }
    if chain.len() > 1 {
        tracing::info!(routes = chain.len(), "model fallback chain configured");
        Arc::new(FallbackModel::new(chain))
    } else {
        // Every configured route was skipped (missing credential); keep the primary alone.
        chain
            .pop()
            .expect("fallback chain always contains the primary")
    }
}

/// One auxiliary route for summarization and session titles, never used unless configured.
fn build_aux_model(config: &Config, endpoint: &Endpoint) -> Option<Arc<dyn Model>> {
    if !config.auxiliary_configured() {
        return None;
    }
    let aux_kind = config.auxiliary_kind();
    let aux_model_name = config.resolved_aux_model();
    let aux_base_url = config.resolved_aux_base_url();
    let aux_key_env = config.resolved_aux_api_key_env();
    let aux_key = std::env::var(aux_key_env)
        .ok()
        .filter(|value| !value.is_empty());
    if aux_key.is_none() && aux_kind != ProviderKind::Ollama {
        tracing::warn!(
            env = %aux_key_env,
            "auxiliary model API key is not set; auxiliary route disabled"
        );
        return None;
    }
    Some(
        build_transport(
            aux_kind,
            aux_base_url,
            aux_key.as_deref().unwrap_or_default(),
            aux_model_name,
            &endpoint.http,
        )
        .expect("the auxiliary route always resolves a base URL"),
    )
}

/// Connect MCP servers and assemble the tool registry the agent shares.
async fn build_tools(config: &Config) -> (Arc<McpManager>, Arc<ToolRegistry>) {
    // MCP servers connect before the registry is shared with the agent so their tools are
    // visible to the very first run. The manager retains every connection for shutdown.
    let mcp = Arc::new(McpManager::new());
    let mcp_tools = mcp.connect_all(config).await;
    let mut registry = ToolRegistry::new();
    silver_core::tools::register_default_tools(&mut registry);
    let registered = McpManager::register_into(&mut registry, mcp_tools);
    if registered > 0 {
        tracing::info!(tools = registered, "registered MCP tools");
    }
    silver_core::tools::team::register(&mut registry);
    // Delegation needs a subagent runner, registered here only when the operator left it on.
    if config.delegation.enabled {
        silver_core::tools::delegate::register(&mut registry, config.delegation.max_concurrent);
    }
    (mcp, Arc::new(registry))
}

fn build_approval_policy(config: &Config) -> ApprovalPolicy {
    ApprovalPolicy {
        write_requires_approval: config.tools.write_requires_approval,
        command_requires_approval: config.tools.command_requires_approval,
        deny_commands: Vec::clone(&config.tools.deny_commands),
    }
}

/// The agent's config and the subagent's, which is the same with an iteration budget of its own
/// so one batch cannot spend the run's budget four times over.
fn build_agent_configs(
    config: &Config,
    context_length: Option<usize>,
) -> (AgentConfig, AgentConfig) {
    let agent_config = AgentConfig {
        max_iterations: config.agent.max_iterations,
        max_tool_calls_per_run: config.agent.max_tool_calls_per_run,
        model_timeout: Duration::from_secs(config.agent.model_timeout_seconds),
        local_model_timeout: Duration::from_secs(config.agent.local_model_timeout_seconds),
        tool_timeout: Duration::from_secs(config.tools.tool_timeout_seconds),
        run_timeout: Duration::from_secs(config.server.run_timeout_seconds),
        turn_liveness_timeout_s: if config.agent.turn_liveness_timeout_seconds > 0.0 {
            Some(config.agent.turn_liveness_timeout_seconds)
        } else {
            None
        },
        turn_liveness_poll_s: config.agent.turn_liveness_poll_seconds,
        empty_guard_enabled: config.agent.empty_guard_enabled,
        empty_cost_threshold_usd: config.agent.empty_cost_threshold_usd,
        memory_max_prompt_bytes_per_file: config.memory.max_prompt_bytes_per_file as usize,
        summarize_with_main_model: config.agent.summarize_with_main_model,
        guardrails: silver_core::guard::tool_guardrails::ToolCallGuardrailConfig {
            hard_stop_enabled: config.agent.tool_call_hard_stop,
            ..Default::default()
        },
        context_length,
        reasoning_effort: config.resolved_reasoning_effort(),
        reasoning_budget: config.model.reasoning_budget,
        ..AgentConfig::default()
    };
    let subagent_config = AgentConfig {
        max_iterations: config.delegation.max_iterations,
        ..AgentConfig::clone(&agent_config)
    };
    (agent_config, subagent_config)
}

/// The routing model `/login` steers, with the configured endpoint as its fallback route.
fn build_routed(
    config: &Config,
    model: Arc<dyn Model>,
    auth: &Arc<AuthStore>,
    oauth: &Arc<OAuthManager>,
    endpoint: &Endpoint,
    key: String,
) -> Arc<RoutedModel> {
    Arc::new(
        RoutedModel::new(
            model,
            Arc::clone(auth),
            Arc::clone(oauth),
            reqwest::Client::clone(&endpoint.http),
        )
        // The `custom` preset names no endpoint; a route for the configured provider borrows
        // config.toml's so `/model` under it keeps working.
        .with_configured_endpoint(silver::routed::ConfiguredEndpoint {
            provider: String::clone(&config.model.provider),
            base_url: String::clone(&endpoint.base_url),
            model: String::clone(&endpoint.model_name),
            key,
        }),
    )
}

/// The acting model: the routed model, wrapped in Mixture of Agents when enabled. A reference
/// whose provider has no credential is dropped with a warning rather than failing startup.
async fn build_acting(
    config: &Config,
    routed: &Arc<RoutedModel>,
) -> Arc<dyn silver_core::model::Model> {
    let routed_model = Arc::clone(routed) as Arc<dyn silver_core::model::Model>;
    if !config.moa.enabled {
        return routed_model;
    }
    let mut references = Vec::new();
    for slot in &config.moa.references {
        let label = slot.label();
        let provider = slot.provider.trim();
        let transport = if provider.is_empty() {
            Ok(Arc::clone(&routed_model))
        } else {
            routed
                .transport_for(provider, Some(slot.model.as_str()))
                .await
        };
        match transport {
            Ok(model) => references.push(silver::moa::Reference {
                label: if label.is_empty() {
                    model.name().to_string()
                } else {
                    label
                },
                model,
                model_id: Some(String::clone(&slot.model)).filter(|model| !model.trim().is_empty()),
            }),
            Err(error) => {
                tracing::warn!(%error, reference = %label, "skipping MoA reference model")
            }
        }
    }
    if references.is_empty() {
        tracing::warn!("MoA is enabled but no reference model could be built; running plain");
        return routed_model;
    }
    let timeout = match config.moa.reference_timeout_seconds {
        0 => None,
        seconds => Some(Duration::from_secs(seconds)),
    };
    tracing::info!(references = references.len(), "Mixture of Agents enabled");
    Arc::new(silver::moa::MoaModel::new(
        routed_model,
        references,
        timeout,
    ))
}

/// Resolve the toolset selection, falling back to every toolset on an invalid name.
fn resolve_toolsets(config: &Config) -> silver_core::toolset::ToolsetSelection {
    match silver_core::toolset::resolve_toolsets(
        &config.tools.default_toolsets,
        &config.tools.disabled_toolsets,
        &silver_core::toolset::builtin_toolsets(),
    ) {
        Ok(selection) => selection,
        Err(error) => {
            tracing::warn!(%error, "invalid toolset config; exposing every toolset");
            silver_core::toolset::ToolsetSelection::all()
        }
    }
}

/// Install the process-wide LSP manager before the tools get it; a duplicate is not fatal.
fn configure_lsp(config: &Config) {
    if let Err(error) = silver_core::lsp::configure(silver_core::lsp::LspConfig::from_parts(
        config.lsp_enabled(),
        Vec::clone(&config.lsp.servers),
        config.lsp.timeout_seconds,
    )) {
        tracing::warn!(%error, "could not configure LSP");
    }
}

/// The subagent runner and the definition store the daemon keeps behind it.
fn build_subagents(
    agent: Arc<Agent>,
    config: &Arc<Config>,
    subagent_config: AgentConfig,
) -> (
    Arc<silver::subagents::DaemonSubagents>,
    Arc<silver::subagents::AgentStore>,
) {
    let agent_store = Arc::new(silver::subagents::AgentStore::new(
        silver::config::global_agents_dir(),
    ));
    let subagents = Arc::new(silver::subagents::DaemonSubagents::new(
        agent,
        Arc::clone(&agent_store),
        Arc::clone(config),
        subagent_config,
    ));
    (subagents, agent_store)
}

fn build_services(
    config: &Config,
    skills: Arc<SkillsStore>,
    checkpoints: Arc<Checkpoints>,
    subagents: Arc<silver::subagents::DaemonSubagents>,
    memory: Arc<dyn MemoryStore>,
    search: Arc<dyn SessionSearch>,
    documents: Arc<dyn DocumentIndex>,
) -> ToolServices {
    ToolServices {
        terminal: Some(Arc::new(TerminalManager::with_env_passthrough(Vec::clone(
            &config.tools.env_passthrough,
        )))),
        todos: Some(Arc::new(TodoStore::new())),
        skills: Some(skills),
        web: Some(Arc::new(HttpWebBackend::with_security(&config.security))),
        checkpoints: Some(checkpoints as Arc<dyn silver_core::services::CheckpointSink>),
        lsp: silver_core::lsp::manager_if_enabled(),
        subagents: Some(subagents as Arc<dyn silver_core::subagent::Subagents>),
        env_passthrough: Vec::clone(&config.tools.env_passthrough),
        memory: Some(memory),
        session_search: Some(search),
        documents: Some(documents),
        team: None,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the daemon's managers are assembled once"
)]
fn build_run_manager(
    db: Db,
    agent: Arc<Agent>,
    memory: Arc<dyn MemoryStore>,
    config: &Arc<Config>,
    context_resolver: &Arc<silver::context_length::ModelContextResolver>,
    services: ToolServices,
    aux_model: Option<Arc<dyn Model>>,
    frozen_yolo: bool,
    config_path: PathBuf,
    auth: &Arc<AuthStore>,
) -> Arc<RunManager> {
    RunManager::new(
        db,
        agent,
        memory,
        Arc::clone(config),
        Some(Arc::clone(context_resolver)),
        services,
        ApprovalSetup {
            aux_model,
            frozen: frozen_yolo,
            config_path: Some(config_path),
        },
    )
    .with_auth(Arc::clone(auth))
}

/// Ctrl+C, or SIGTERM from `kill`, systemd or docker: both stop the daemon gracefully.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    drop(tokio::signal::ctrl_c().await);
    tracing::info!("shutdown signal received");
}

/// Create the data directory 0700 when this process is the one creating it. State can
/// hold transcripts and tool output, so it must not be world-readable.
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}
