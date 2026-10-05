//! The managed ai-memory server, the shared cross-harness memory of record: silver starts it (or
//! adopts one already listening), registers it as an MCP server so every run gets its `memory_*`
//! tools, and stops on shutdown (docs/adr/0002-shared-memory-via-ai-memory.md).

use crate::config::{Config, McpServerConfig, McpTransport};
use std::path::Path;
use std::time::Duration;
use tokio::process::{Child, Command};

/// The MCP server name, and the executable silver looks for on `PATH`.
pub const SERVER_NAME: &str = "ai-memory";
/// How long a freshly spawned server has to answer its health endpoint.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How often the health endpoint is polled while starting.
const POLL_EVERY: Duration = Duration::from_millis(250);
/// External harnesses to wire up, paired with the CLI whose presence means the harness is used.
const HARNESSES: [(&str, &str); 4] = [
    ("claude-code", "claude"),
    ("opencode", "opencode"),
    ("gemini-cli", "gemini"),
    ("cursor", "cursor-agent"),
];
/// How long one ai-memory installer may run before it is abandoned.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A running (or adopted) ai-memory server and the MCP entry that reaches it.
pub struct AiMemory {
    child: Option<Child>,
    server: McpServerConfig,
    endpoint: String,
}

impl AiMemory {
    /// Start ai-memory beside silver, or adopt a server already listening on the configured
    /// bind. `None` when memory is off or the binary is missing; the daemon runs either way.
    pub async fn start(config: &Config, silver_data_dir: &Path) -> Option<AiMemory> {
        if !config.memory.enabled {
            return None;
        }
        let bind = config.memory.bind.trim().to_string();
        let endpoint = format!("http://{bind}");
        let binary = config.memory.binary.as_deref().unwrap_or(SERVER_NAME);
        if healthy(&endpoint).await {
            tracing::info!(endpoint = %endpoint, "using the ai-memory server already listening");
            if config.memory.install_harnesses {
                wire_harnesses(binary).await;
            }
            return Some(AiMemory {
                child: None,
                server: server_for(&bind),
                endpoint,
            });
        }
        let fallback_dir = silver_data_dir.join(SERVER_NAME);
        let data_dir = config.memory.data_dir.as_deref().unwrap_or(&fallback_dir);
        let child = match spawn(binary, &bind, data_dir) {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    binary = %binary,
                    "ai-memory is not installed, so shared memory is off; install it and restart"
                );
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, binary = %binary, "ai-memory could not be started");
                return None;
            }
        };
        if !wait_healthy(&endpoint).await {
            // Dropping the child kills it: the command sets `kill_on_drop`.
            tracing::warn!(endpoint = %endpoint, binary = %binary, "ai-memory did not become healthy");
            return None;
        }
        tracing::info!(endpoint = %endpoint, data_dir = %data_dir.display(), "ai-memory started");
        if config.memory.install_harnesses {
            wire_harnesses(binary).await;
        }
        Some(AiMemory {
            child: Some(child),
            server: server_for(&bind),
            endpoint,
        })
    }

    /// The MCP entry to register, pointing at this server.
    pub fn mcp_server(&self) -> &McpServerConfig {
        &self.server
    }

    /// The server's base URL, for reading its read-only HTTP API.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Stop the server silver started; an adopted server is left running.
    pub async fn shutdown(mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        drop(child.start_kill());
        match child.wait().await {
            Ok(status) => tracing::info!(%status, "ai-memory stopped"),
            Err(error) => tracing::warn!(%error, "ai-memory did not stop cleanly"),
        }
    }
}

/// The MCP entry for a server listening on `bind`.
fn server_for(bind: &str) -> McpServerConfig {
    McpServerConfig {
        name: SERVER_NAME.to_string(),
        transport: McpTransport::Http,
        url: Some(format!("http://{bind}/mcp")),
        ..McpServerConfig::default()
    }
}

/// Spawn `ai-memory serve --transport http` on `bind`, storing under `data_dir`.
fn spawn(binary: &str, bind: &str, data_dir: &Path) -> std::io::Result<Child> {
    Command::new(binary)
        .arg("serve")
        .arg("--transport")
        .arg("http")
        .arg("--bind")
        .arg(bind)
        // The memory panel reads the server's read-only /api/v1, which is mounted with the web UI.
        .arg("--enable-web")
        .env("AI_MEMORY_DATA_DIR", data_dir)
        .env("AI_MEMORY_LOG_LEVEL", "warn")
        // The store is text; keep it zero-LLM and offline so no embedding model is fetched.
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .kill_on_drop(true)
        .spawn()
}

/// Poll the health endpoint until it answers or the start budget runs out.
async fn wait_healthy(endpoint: &str) -> bool {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if healthy(endpoint).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// Whether an ai-memory server is answering on `endpoint`.
async fn healthy(endpoint: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    client
        .get(format!("{endpoint}/healthz"))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// Point every installed external harness at the running server, with ai-memory's own
/// installers. Best-effort: a harness that cannot be wired is logged, never fatal.
async fn wire_harnesses(binary: &str) {
    for (harness, cli) in HARNESSES {
        if !on_path(cli) {
            continue;
        }
        run_installer(
            binary,
            &["install-mcp", "--client", harness, "--apply"],
            harness,
        )
        .await;
        run_installer(
            binary,
            &["install-hooks", "--agent", harness, "--apply"],
            harness,
        )
        .await;
    }
}

/// Run one installer, bounded so a hanging harness never holds up startup.
async fn run_installer(binary: &str, args: &[&str], harness: &str) {
    let run = Command::new(binary).args(args).status();
    match tokio::time::timeout(INSTALL_TIMEOUT, run).await {
        Ok(Ok(status)) if status.success() => {
            tracing::info!(harness, "ai-memory wired into the harness");
        }
        Ok(Ok(status)) => tracing::warn!(harness, %status, "ai-memory could not wire the harness"),
        Ok(Err(error)) => tracing::warn!(%error, harness, "ai-memory installer could not run"),
        Err(_elapsed) => tracing::warn!(harness, "ai-memory installer timed out"),
    }
}

/// Whether a command name resolves on `PATH`.
fn on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        dir.join(name).is_file()
            || dir
                .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
                .is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mcp_entry_points_at_the_bind() {
        let server = server_for("127.0.0.1:49374");
        assert_eq!(server.name, "ai-memory");
        assert_eq!(server.url.as_deref(), Some("http://127.0.0.1:49374/mcp"));
        assert!(matches!(server.transport, McpTransport::Http));
        assert!(server.enabled);
    }

    #[test]
    fn a_harness_is_detected_by_its_cli_on_path() {
        assert!(!on_path("definitely-not-a-real-cli-xyz"));
        #[cfg(unix)]
        assert!(on_path("sh"));
    }
}
