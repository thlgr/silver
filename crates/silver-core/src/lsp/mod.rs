//! A minimal Language Server Protocol client, driven by the lsp tool in tools/lsp.rs.

pub mod client;
pub mod manager;
pub mod protocol;
pub mod range_shift;
pub mod servers;

pub use client::{file_uri, uri_to_path, ClientState, LspClient, LspClientOptions};
pub use manager::{ClientStatus, LspConfig, LspManager};
pub use range_shift::{
    build_line_shift, changed_lines, shift_baseline, shift_diagnostic_range, LineShift,
};
pub use servers::{
    builtin_for_path, detect_server, detect_server_with_path, language_id_for,
    resolve_project_root, BuiltinServer, ServerSpec, BUILTIN_SERVERS,
};

use std::sync::{Arc, OnceLock};

/// Everything that can go wrong while driving a language server.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// LSP support is turned off in configuration.
    #[error("LSP is disabled")]
    Disabled,
    /// No server is installed or configured for the file's language.
    #[error("no LSP server is available for this file")]
    NoServer,
    /// The server process could not be started.
    #[error("LSP server unavailable: {0}")]
    ServerUnavailable(String),
    /// The HTTP-like framing or envelope was malformed.
    #[error(transparent)]
    Protocol(#[from] protocol::ProtocolError),
    /// The server returned a JSON-RPC error response.
    #[error(transparent)]
    Request(#[from] protocol::RequestError),
    /// The request did not answer within its bounded timeout.
    #[error("LSP request timed out: {0}")]
    Timeout(String),
    /// The global manager was configured more than once.
    #[error("LSP manager already configured")]
    AlreadyConfigured,
    /// A filesystem operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON (de)serialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

static GLOBAL: OnceLock<Arc<LspManager>> = OnceLock::new();

/// Install the process-wide manager. Call before the first request; returns an
/// error when a manager already exists.
pub fn configure(config: LspConfig) -> Result<(), LspError> {
    GLOBAL
        .set(Arc::new(LspManager::new(config)))
        .map_err(|_err| LspError::AlreadyConfigured)
}

/// The process-wide manager, or None when LSP is disabled. It is created with the default
/// configuration on first use when configure was never called.
pub fn manager_if_enabled() -> Option<Arc<LspManager>> {
    let manager = GLOBAL.get_or_init(|| Arc::new(LspManager::new(LspConfig::default())));
    manager.config().enabled.then(|| Arc::clone(manager))
}

/// Shut down every process-wide client, if the manager was ever created.
pub async fn shutdown() {
    if let Some(manager) = GLOBAL.get() {
        manager.shutdown().await;
    }
}
