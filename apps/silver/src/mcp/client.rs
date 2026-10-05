//! MCP connection lifecycle: initialize, tools/list, lazy reconnect with a capped cooldown,
//! tools/call forwarding, and the manager that registers every configured server's tools.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use serde_json::{json, Value};
use silver_core::tool::{Tool, ToolOutcome, ToolRegistry};
use tokio::sync::Mutex;

use super::transport::{build_transport, jsonrpc_notification, jsonrpc_request, McpTransport};
use super::{
    matches_name_filter, render_call_tool_result, McpError, McpTool, CLIENT_NAME,
    CONNECT_RETRY_BASE_BACKOFF_SECS, CONNECT_RETRY_MAX_BACKOFF_SECS, DEFAULT_TOOL_TIMEOUT_SECS,
    LATEST_PROTOCOL_VERSION, MCP_LIST_MAX_PAGES,
};

/// Factory invoked on every connect/reconnect so a dead transport is rebuilt from config.
pub type TransportFactory = Arc<
    dyn Fn(&crate::config::McpServerConfig) -> Result<Box<dyn McpTransport>, McpError>
        + Send
        + Sync,
>;

/// One tool advertised by a server's tools/list, before registry naming and normalization.
#[derive(Clone, Debug)]
pub struct McpToolDef {
    /// Raw MCP tool name (the server's own name, not the registry name).
    pub name: String,
    /// Advertised description; empty means the server sent none.
    pub description: String,
    /// Raw JSON Schema from inputSchema/input_schema.
    pub input_schema: Value,
    /// True only when the server's annotations carry readOnlyHint exactly true
    /// (mcp_tool_registration._annotation_read_only_hint); unknown is write-capable.
    pub read_only: bool,
}

impl McpToolDef {
    fn from_json(item: &Value) -> Option<Self> {
        let name = item.get("name").and_then(Value::as_str)?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input_schema = item
            .get("inputSchema")
            .or_else(|| item.get("input_schema"))
            .cloned()
            .unwrap_or(Value::Null);
        let read_only = item
            .get("annotations")
            .and_then(|annotations| annotations.get("readOnlyHint"))
            .and_then(Value::as_bool)
            == Some(true);
        Some(Self {
            name,
            description,
            input_schema,
            read_only,
        })
    }
}

/// Capabilities reported by the initialize result (mcp_tool_transport._advertises_tools).
#[derive(Clone, Copy, Debug, Default)]
pub struct McpCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
}

impl McpCapabilities {
    fn from_initialize_result(result: &Value) -> Self {
        match result.get("capabilities") {
            // No capability information at all: legacy fallback assumes tools (Hermes).
            None => Self {
                tools: true,
                resources: true,
                prompts: true,
            },
            Some(capabilities) => Self {
                tools: presents(capabilities, "tools"),
                resources: presents(capabilities, "resources"),
                prompts: presents(capabilities, "prompts"),
            },
        }
    }
}

fn presents(capabilities: &Value, key: &str) -> bool {
    capabilities
        .get(key)
        .map(|value| !value.is_null())
        .unwrap_or(false)
}

struct ConnState {
    transport: Option<Box<dyn McpTransport>>,
    next_id: i64,
    tools: Vec<McpToolDef>,
    capabilities: McpCapabilities,
    connect_failures: u32,
    retry_after: Option<Instant>,
}

/// One MCP server connection. Cheap to share behind an Arc; a single mutex serializes
/// client-initiated RPCs exactly like Hermes' per-server _rpc_lock.
pub struct McpServerConnection {
    config: crate::config::McpServerConfig,
    factory: TransportFactory,
    state: Mutex<ConnState>,
}

impl McpServerConnection {
    /// Configured server name (the prefix of every registered tool name).
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Build a connection using the real stdio/HTTP transports.
    pub fn new(config: crate::config::McpServerConfig) -> Self {
        let factory: TransportFactory = Arc::new(build_transport);
        Self::with_factory(config, factory)
    }

    /// Build a connection with an injected transport factory (tests, alternate transports).
    pub fn with_factory(config: crate::config::McpServerConfig, factory: TransportFactory) -> Self {
        Self {
            config,
            factory,
            state: Mutex::new(ConnState {
                transport: None,
                next_id: 1,
                tools: Vec::new(),
                capabilities: McpCapabilities::default(),
                connect_failures: 0,
                retry_after: None,
            }),
        }
    }

    /// The server config this connection was built from.
    pub fn config(&self) -> &crate::config::McpServerConfig {
        &self.config
    }

    fn request_timeout(&self) -> Duration {
        let seconds = if self.config.timeout_seconds == 0 {
            DEFAULT_TOOL_TIMEOUT_SECS
        } else {
            self.config.timeout_seconds
        };
        Duration::from_secs(seconds)
    }

    /// Connect if not already connected, honoring the per-server cooldown
    /// (mcp_tool_discovery._connect_cooldown_active).
    pub async fn connect(&self) -> Result<(), McpError> {
        let mut state = self.state.lock().await;
        self.connect_locked(&mut state).await
    }

    async fn connect_locked(&self, state: &mut ConnState) -> Result<(), McpError> {
        if state.transport.is_some() {
            return Ok(());
        }
        if let Some(deadline) = state.retry_after {
            if Instant::now() < deadline {
                return Err(McpError::NotConnected {
                    server: self.name().to_string(),
                });
            }
        }
        match self.try_connect(state).await {
            Ok(()) => {
                state.connect_failures = 0;
                state.retry_after = None;
                Ok(())
            }
            Err(err) => {
                state.connect_failures = state.connect_failures.saturating_add(1);
                state.retry_after = Some(Instant::now() + connect_backoff(state.connect_failures));
                Err(err)
            }
        }
    }

    async fn try_connect(&self, state: &mut ConnState) -> Result<(), McpError> {
        let transport = (self.factory)(&self.config)?;
        state.transport = Some(transport);
        let params = json!({
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
        });
        let result = match self.rpc(state, "initialize", params).await {
            Ok(result) => result,
            Err(err) => {
                self.drop_transport(state).await;
                return Err(err);
            }
        };
        state.capabilities = McpCapabilities::from_initialize_result(&result);
        if let Some(transport) = state.transport.as_mut() {
            let initialized = jsonrpc_notification("notifications/initialized", None);
            if let Err(err) = transport.notify(initialized).await {
                self.drop_transport(state).await;
                return Err(err);
            }
        }
        if state.capabilities.tools {
            match self.list_tools(state).await {
                Ok(tools) => state.tools = tools,
                Err(err) => {
                    self.drop_transport(state).await;
                    return Err(err);
                }
            }
        } else {
            tracing::info!(
                server = %self.name(),
                "MCP server does not advertise 'tools'; skipping tools/list"
            );
            state.tools = Vec::new();
        }
        Ok(())
    }

    async fn rpc(
        &self,
        state: &mut ConnState,
        method: &str,
        params: Value,
    ) -> Result<Value, McpError> {
        let id = state.next_id;
        state.next_id = state.next_id.saturating_add(1);
        let request = jsonrpc_request(id, method, &params);
        let timeout = self.request_timeout();
        let transport = state
            .transport
            .as_mut()
            .ok_or_else(|| McpError::NotConnected {
                server: self.name().to_string(),
            })?;
        transport.exchange(request, timeout).await
    }

    async fn list_tools(&self, state: &mut ConnState) -> Result<Vec<McpToolDef>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MCP_LIST_MAX_PAGES {
                tracing::warn!(
                    server = %self.name(),
                    pages = MCP_LIST_MAX_PAGES,
                    "MCP tools/list pagination exceeded the page cap; truncating"
                );
                break;
            }
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.rpc(state, "tools/list", params).await?;
            if let Some(items) = result.get("tools").and_then(Value::as_array) {
                for item in items {
                    if let Some(def) = McpToolDef::from_json(item) {
                        tools.push(def);
                    }
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// The tools discovered on the last successful connect.
    pub async fn tools(&self) -> Vec<McpToolDef> {
        let state = self.state.lock().await;
        Vec::clone(&state.tools)
    }

    /// Capabilities from the last successful initialize.
    pub async fn capabilities(&self) -> McpCapabilities {
        self.state.lock().await.capabilities
    }

    async fn drop_transport(&self, state: &mut ConnState) {
        if let Some(mut transport) = state.transport.take() {
            transport.shutdown().await;
        }
    }

    /// Forward one tools/call. Transport failures drop the transport so the next call
    /// reconnects lazily; a read-only tool is retried once because replaying it is safe
    /// (mcp_tool_handlers._track_inflight_rpc retry_safe semantics).
    pub async fn call_tool(&self, tool: &str, args: Value, read_only: bool) -> ToolOutcome {
        let mut attempt = 0u8;
        loop {
            let mut state = self.state.lock().await;
            if state.transport.is_none() {
                if let Err(err) = self.connect_locked(&mut state).await {
                    return ToolOutcome::error(err.message());
                }
            }
            let params = json!({"name": tool, "arguments": args});
            let failure = match self.rpc(&mut state, "tools/call", params).await {
                Ok(result) => {
                    state.connect_failures = 0;
                    state.retry_after = None;
                    return render_call_tool_result(&result, self.name());
                }
                Err(err) => err,
            };
            // A JSON-RPC error means the server answered; the transport is healthy.
            if matches!(failure, McpError::Rpc { .. }) {
                return ToolOutcome::error(failure.message());
            }
            self.drop_transport(&mut state).await;
            // Allow the immediate lazy reconnect on the retry path.
            state.retry_after = Some(Instant::now());
            drop(state);
            if attempt == 0 && read_only {
                attempt += 1;
                continue;
            }
            return ToolOutcome::error(failure.message());
        }
    }

    /// Kill the stdio child (if any) and mark the connection dead.
    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        self.drop_transport(&mut state).await;
    }
}

/// Geometric, capped connect cooldown (mcp_tool_discovery._record_connect_failure).
fn connect_backoff(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(63);
    let scaled = CONNECT_RETRY_BASE_BACKOFF_SECS.saturating_mul(1u64 << exponent);
    Duration::from_secs(scaled.min(CONNECT_RETRY_MAX_BACKOFF_SECS))
}

/// The include/exclude predicate for one server's advertised tool names
/// (mcp_tool_registration._make_tool_filter): empty allowed_tools means every tool.
pub fn allowed_tool_filter(
    config: &crate::config::McpServerConfig,
) -> impl Fn(&str) -> bool + 'static {
    let patterns: BTreeSet<String> = config.allowed_tools.iter().cloned().collect();
    move |tool_name: &str| patterns.is_empty() || matches_name_filter(tool_name, &patterns)
}

/// Owns every live MCP connection so the daemon can register their tools and later shut them
/// all down. Call connect_all once at startup.
pub struct McpManager {
    connections: std::sync::Mutex<Vec<Arc<McpServerConnection>>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    /// A manager with no connections.
    pub fn new() -> Self {
        Self {
            connections: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Connect every enabled [[mcp.server]] entry and return every discovered tool, ready to
    /// register. A server that fails to connect contributes no tools but is retained so a later
    /// call can reconnect it; failures are logged, never fatal to the other servers.
    pub async fn connect_all(
        &self,
        config: &crate::config::Config,
        extra: &[crate::config::McpServerConfig],
    ) -> Vec<Arc<dyn Tool>> {
        let connections: Vec<Arc<McpServerConnection>> = config
            .enabled_mcp_servers()
            .into_iter()
            .cloned()
            .chain(extra.iter().cloned())
            .map(|server| Arc::new(McpServerConnection::new(server)))
            .collect();

        let connects = connections.iter().map(|connection| async move {
            match connection.connect().await {
                Ok(()) => None,
                Err(err) => Some((connection.name().to_string(), err.message())),
            }
        });
        for failure in join_all(connects).await.into_iter().flatten() {
            tracing::warn!(
                server = %failure.0,
                error = %failure.1,
                "MCP server failed to connect; its tools are unavailable until reconnect"
            );
        }

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for connection in &connections {
            let allowed = allowed_tool_filter(&connection.config);
            for def in connection.tools().await {
                if !allowed(&def.name) {
                    tracing::debug!(
                        server = %connection.name(),
                        tool = %def.name,
                        "skipping MCP tool filtered by allowed_tools"
                    );
                    continue;
                }
                tools.push(Arc::new(McpTool::new(Arc::clone(connection), def)));
            }
        }
        self.connections
            .lock()
            .expect("mcp connections lock poisoned")
            .extend(connections);
        tools
    }

    /// Register every tool returned by connect_all into a registry being assembled.
    pub fn register_into(registry: &mut ToolRegistry, tools: Vec<Arc<dyn Tool>>) -> usize {
        let count = tools.len();
        for tool in tools {
            registry.register(tool);
        }
        count
    }

    /// Kill every stdio child and drop every connection. The daemon shutdown hook.
    pub async fn shutdown(&self) {
        let connections = std::mem::take(
            &mut *self
                .connections
                .lock()
                .expect("mcp connections lock poisoned"),
        );
        for connection in connections {
            connection.shutdown().await;
        }
    }

    /// Names of every managed connection (connected or backing off).
    pub fn server_names(&self) -> Vec<String> {
        self.connections
            .lock()
            .expect("mcp connections lock poisoned")
            .iter()
            .map(|connection| connection.name().to_string())
            .collect()
    }
}
