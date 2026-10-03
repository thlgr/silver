//! One lazily started LSP client per (server, workspace root): resolves a server for a path,
//! derives the project root, and routes requests to it.

use super::client::{
    LspClient, LspClientOptions, DEFAULT_REQUEST_TIMEOUT, DIAGNOSTICS_DOCUMENT_WAIT,
    INITIALIZE_TIMEOUT,
};
use super::servers;
use super::LspError;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

/// Lower bound for the configured per-request timeout.
pub const MIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound for the configured per-request timeout.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Lower bound for the diagnostics wait budget.
pub const MIN_WAIT_TIMEOUT: Duration = Duration::from_millis(100);
/// Upper bound for the diagnostics wait budget.
pub const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Configuration for the LSP subsystem, mapped from the daemon [lsp] section.
#[derive(Clone, Debug)]
pub struct LspConfig {
    /// Whether LSP support is enabled at all.
    pub enabled: bool,
    /// Explicit server commands. Empty means detect from the builtin table.
    pub servers: Vec<String>,
    /// Bounded initialize handshake timeout.
    pub init_timeout: Duration,
    /// Bounded per-request timeout.
    pub request_timeout: Duration,
    /// Bounded wait for fresh diagnostics after an open or change.
    pub wait_timeout: Duration,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: Vec::new(),
            init_timeout: INITIALIZE_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            wait_timeout: DIAGNOSTICS_DOCUMENT_WAIT,
        }
    }
}

impl LspConfig {
    /// The default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// A configuration that turns LSP off.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Build from the daemon [lsp] fields. A zero timeout means "use the
    /// default"; request and wait budgets are clamped to their bounds.
    pub fn from_parts(enabled: bool, servers: Vec<String>, timeout_seconds: u64) -> Self {
        let request_timeout = if timeout_seconds == 0 {
            DEFAULT_REQUEST_TIMEOUT
        } else {
            Duration::from_secs(timeout_seconds)
        };
        Self {
            enabled,
            servers,
            init_timeout: INITIALIZE_TIMEOUT,
            request_timeout: request_timeout.clamp(MIN_REQUEST_TIMEOUT, MAX_REQUEST_TIMEOUT),
            wait_timeout: DIAGNOSTICS_DOCUMENT_WAIT,
        }
    }
}

/// A snapshot of one running client.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ClientStatus {
    pub server_id: String,
    pub workspace_root: PathBuf,
    pub state: String,
}

/// Owns every language server client this process starts.
pub struct LspManager {
    config: LspConfig,
    clients: AsyncMutex<Vec<Arc<LspClient>>>,
}

impl LspManager {
    /// Build a manager. No server is spawned until a request arrives.
    pub fn new(config: LspConfig) -> Self {
        Self {
            config,
            clients: AsyncMutex::new(Vec::new()),
        }
    }

    /// The active configuration.
    pub fn config(&self) -> &LspConfig {
        &self.config
    }

    async fn ensure_client(
        &self,
        path: &Path,
        workspace_root: &Path,
    ) -> Result<Arc<LspClient>, LspError> {
        if !self.config.enabled {
            return Err(LspError::Disabled);
        }
        let spec = servers::detect_server(path, &self.config.servers).ok_or(LspError::NoServer)?;
        let root = servers::resolve_project_root(path, workspace_root, &spec.root_markers);
        {
            let clients = self.clients.lock().await;
            if let Some(client) = clients
                .iter()
                .find(|client| client.matches(&spec.id, &root))
            {
                if client.is_running() {
                    return Ok(Arc::clone(client));
                }
            }
        }
        let mut options = LspClientOptions::new(spec.id, root, spec.command);
        options.init_timeout = self.config.init_timeout;
        options.request_timeout = self.config.request_timeout;
        let client = LspClient::spawn(options).await?;
        // Another caller may have won the race; keep one process per server and root.
        let mut clients = self.clients.lock().await;
        if let Some(existing) = clients
            .iter()
            .find(|existing| existing.same_target(&client) && existing.is_running())
        {
            let existing = Arc::clone(existing);
            drop(clients);
            client.shutdown().await;
            return Ok(existing);
        }
        clients.retain(|existing| !existing.same_target(&client));
        clients.push(Arc::clone(&client));
        Ok(client)
    }

    async fn open(&self, client: &LspClient, path: &Path) -> Result<i64, LspError> {
        let language_id = servers::language_id_for(path);
        client.open_file(path, language_id).await
    }

    /// Open or refresh a file and return fresh diagnostics. An empty vector can
    /// mean clean or no verdict in budget; the server never invents stale data.
    pub async fn diagnostics(
        &self,
        workspace_root: &Path,
        path: &Path,
    ) -> Result<Arc<Vec<Value>>, LspError> {
        let client = self.ensure_client(path, workspace_root).await?;
        let version = self.open(&client, path).await?;
        drop(client.save_file(path).await);
        let fresh = client
            .wait_for_diagnostics(path, version, self.config.wait_timeout)
            .await?;
        if !fresh {
            return Ok(Arc::new(Vec::new()));
        }
        Ok(client.diagnostics_for(path, true))
    }

    /// textDocument/hover at a position.
    pub async fn hover(
        &self,
        workspace_root: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Value, LspError> {
        let client = self.ensure_client(path, workspace_root).await?;
        self.open(&client, path).await?;
        client.hover(path, line, character).await
    }

    /// textDocument/definition at a position.
    pub async fn definition(
        &self,
        workspace_root: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Value, LspError> {
        let client = self.ensure_client(path, workspace_root).await?;
        self.open(&client, path).await?;
        client.definition(path, line, character).await
    }

    /// textDocument/references at a position.
    pub async fn references(
        &self,
        workspace_root: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Value, LspError> {
        let client = self.ensure_client(path, workspace_root).await?;
        self.open(&client, path).await?;
        client.references(path, line, character, true).await
    }

    /// textDocument/documentSymbol.
    pub async fn document_symbols(
        &self,
        workspace_root: &Path,
        path: &Path,
    ) -> Result<Value, LspError> {
        let client = self.ensure_client(path, workspace_root).await?;
        self.open(&client, path).await?;
        client.document_symbols(path).await
    }

    /// textDocument/rename; returns the server's WorkspaceEdit.
    pub async fn rename(
        &self,
        workspace_root: &Path,
        path: &Path,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<Value, LspError> {
        let client = self.ensure_client(path, workspace_root).await?;
        self.open(&client, path).await?;
        client.rename(path, line, character, new_name).await
    }

    /// Shut down and forget every client.
    pub async fn shutdown(&self) {
        let clients: Vec<Arc<LspClient>> = {
            let mut guard = self.clients.lock().await;
            guard.drain(..).collect()
        };
        for client in clients {
            client.shutdown().await;
        }
    }

    /// A snapshot of every known client, for status output.
    pub async fn status(&self) -> Vec<ClientStatus> {
        let clients = self.clients.lock().await;
        clients
            .iter()
            .map(|client| ClientStatus {
                server_id: client.server_id().to_string(),
                workspace_root: client.workspace_root().to_path_buf(),
                state: client.state().name().to_string(),
            })
            .collect()
    }
}
