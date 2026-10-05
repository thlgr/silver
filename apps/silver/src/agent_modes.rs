//! External coding-agent CLIs (Claude Code, OpenCode, Grok Build, ...) as selectable ACP agent
//! modes, each spawning its own CLI found on the login shell's PATH plus the usual install dirs.
//! Only local-CLI launch is supported: no registry downloads.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use silver_core::error::CoreError;

/// One external ACP agent mode.
pub struct AgentMode {
    pub id: &'static str,
    pub name: &'static str,
    /// CLI names that mean "installed on this machine".
    pub bins: &'static [&'static str],
    /// How to launch the installed CLI in its ACP mode (`{bin}` = its resolved path). None when
    /// the CLI has no ACP mode to drive; those modes report their setup steps instead.
    pub command: Option<&'static str>,
    /// What to tell someone setting it up by hand.
    pub setup: &'static str,
}

/// Every mode this build knows, in catalog order. `codex` is not here: silver drives Codex
/// through its own provider, not as an ACP agent.
pub const MODES: &[AgentMode] = &[
    AgentMode {
        id: "claude",
        name: "Claude Code",
        bins: &["claude"],
        // The CLI has no ACP mode, so Claude Code runs through the registry's adapter, which
        // uses the CLI's own sign-in. Pinned as the ACP registry pins it.
        command: Some("npx -y @agentclientprotocol/claude-agent-acp@0.85.1"),
        setup: "Install Claude Code (curl -fsSL https://claude.ai/install.sh | bash) and run `claude auth login`. It runs through an adapter that npx fetches, so Node.js must be installed too.",
    },
    AgentMode {
        id: "cursor",
        name: "Cursor",
        bins: &["cursor-agent", "agent"],
        command: Some("{bin} acp"),
        setup: "Install Cursor CLI (curl https://cursor.com/install -fsS | bash) and run `cursor-agent login`.",
    },
    AgentMode {
        id: "pi",
        name: "Pi",
        bins: &["pi"],
        command: None,
        setup: "Install pi (npm install -g @earendil-works/pi-coding-agent), run `pi` and type /login.",
    },
    AgentMode {
        id: "opencode",
        name: "OpenCode",
        bins: &["opencode"],
        command: Some("{bin} acp"),
        setup: "Install OpenCode (curl -fsSL https://opencode.ai/install | bash) and run `opencode auth login`.",
    },
    AgentMode {
        id: "grok",
        name: "Grok Build",
        bins: &["grok"],
        command: Some("{bin} agent stdio"),
        setup: "Install Grok Build (curl -fsSL https://x.ai/cli/install.sh | bash) and run `grok login --device-auth`.",
    },
    AgentMode {
        id: "gemini",
        name: "Gemini CLI",
        bins: &["gemini"],
        command: Some("{bin} --acp"),
        setup: "Install Gemini CLI (npm install -g @google/gemini-cli) and run `gemini` to sign in with Google.",
    },
    AgentMode {
        id: "copilot",
        name: "GitHub Copilot",
        bins: &["copilot"],
        command: Some("{bin} --acp --stdio"),
        setup: "Install Copilot CLI (npm install -g @github/copilot) and run `copilot login`.",
    },
    AgentMode {
        id: "qwen",
        name: "Qwen Code",
        bins: &["qwen"],
        command: Some("{bin} --acp"),
        setup: "Install Qwen Code (npm install -g @qwen-code/qwen-code), run `qwen` and type /auth.",
    },
    AgentMode {
        id: "goose",
        name: "goose",
        bins: &["goose"],
        command: Some("{bin} acp"),
        setup: "Install goose and run `goose configure`.",
    },
    AgentMode {
        id: "kimi",
        name: "Kimi Code",
        bins: &["kimi"],
        command: Some("{bin} acp"),
        setup: "Install Kimi Code (curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash) and run `kimi login`.",
    },
    AgentMode {
        id: "droid",
        name: "Factory Droid",
        bins: &["droid"],
        command: Some("{bin} exec --output-format acp-daemon"),
        setup: "Install Droid (curl -fsSL https://app.factory.ai/cli | sh) and sign in with `droid`.",
    },
    AgentMode {
        id: "amp",
        name: "Amp",
        bins: &["amp"],
        command: None,
        setup: "Install Amp (curl -fsSL https://ampcode.com/install.sh | bash) and run `amp login`.",
    },
    AgentMode {
        id: "kilo",
        name: "Kilo",
        bins: &["kilo"],
        command: Some("{bin} acp"),
        setup: "Install Kilo CLI (npm install -g @kilocode/cli) and run `kilo auth login`.",
    },
    AgentMode {
        id: "cline",
        name: "Cline",
        bins: &["cline"],
        command: Some("{bin} --acp"),
        setup: "Install Cline CLI (npm install -g cline) and run `cline auth`.",
    },
    AgentMode {
        id: "auggie",
        name: "Auggie",
        bins: &["auggie"],
        command: Some("{bin} --acp"),
        setup: "Install Auggie (npm install -g @augmentcode/auggie) and run `auggie login`.",
    },
    AgentMode {
        id: "vibe",
        name: "Mistral Vibe",
        bins: &["vibe-acp", "vibe"],
        command: None,
        setup: "Install Mistral Vibe (curl -LsSf https://mistral.ai/vibe/install.sh | bash) and run `vibe --setup`.",
    },
    AgentMode {
        id: "kiro",
        name: "Kiro CLI",
        bins: &["kiro-cli"],
        command: Some("{bin} acp"),
        setup: "Install Kiro CLI (curl -fsSL https://cli.kiro.dev/install | bash) and run `kiro-cli login`.",
    },
    AgentMode {
        id: "devin",
        name: "Devin",
        bins: &["devin"],
        command: Some("{bin} acp"),
        setup: "Install the Devin CLI (curl -fsSL https://cli.devin.ai/install.sh | bash) and run `devin auth login`.",
    },
    AgentMode {
        id: "qoder",
        name: "Qoder CLI",
        bins: &["qodercli"],
        command: Some("{bin} --acp"),
        setup: "Install Qoder CLI (npm install -g @qoder-ai/qodercli) and run `qodercli login`.",
    },
    AgentMode {
        id: "codebuddy",
        name: "CodeBuddy Code",
        bins: &["codebuddy", "cbc"],
        command: Some("{bin} --acp"),
        setup: "Install CodeBuddy Code (npm install -g @tencent-ai/codebuddy-code) and sign in with `codebuddy`.",
    },
    AgentMode {
        id: "minimax",
        name: "MiniMax Code",
        bins: &["mcode"],
        command: Some("{bin} acp"),
        setup: "Install MiniMax Code (npm install -g @minimax-ai/code) and run `mcode login`.",
    },
    AgentMode {
        id: "junie",
        name: "Junie",
        bins: &["junie"],
        command: Some("{bin} --acp=true"),
        setup: "Install Junie (curl -fsSL https://junie.jetbrains.com/install.sh | bash) and sign in with `junie`.",
    },
    AgentMode {
        id: "antigravity",
        name: "Google Antigravity",
        bins: &["agy"],
        command: None,
        setup: "Install the Antigravity CLI (curl -fsSL https://antigravity.google/cli/install.sh | bash) and sign in with `agy`.",
    },
    AgentMode {
        id: "cortex",
        name: "Cortex Code",
        bins: &["cortex"],
        command: Some("{bin} acp serve"),
        setup: "Install Cortex Code (curl -LsS https://ai.snowflake.com/static/cc-scripts/install.sh | sh) and set up a connection with `cortex`.",
    },
    AgentMode {
        id: "poolside",
        name: "Poolside",
        bins: &["pool"],
        command: Some("{bin} acp"),
        setup: "Install Poolside (curl -fsSL https://downloads.poolside.ai/pool/install.sh | sh) and run `pool login`.",
    },
];

/// The catalog entry for a mode id, or none.
pub fn mode(id: &str) -> Option<&'static AgentMode> {
    MODES.iter().find(|m| m.id == id)
}

/// Whether an agent mode can be launched here, its CLI (and what it runs through) being installed.
/// None for a preset that is not an agent mode.
pub fn installed(id: &str) -> Option<bool> {
    mode(id).map(|_| command_for(id).is_ok())
}

// MARK: PATH hydration

/// Directories the mode CLIs are looked up in, hydrated once at startup.
static SEARCH_PATH: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Login-shell PATH, via `$SHELL -ilc` with a timeout (nvm/conda can make shells slow).
fn login_shell_path() -> Option<String> {
    const MARK: &str = "__SILVER_PATH__";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = std::process::Command::new(shell)
        .args([
            "-ilc",
            &format!("printf '%s%s%s' '{MARK}' \"$PATH\" '{MARK}'"),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                drop(child.kill());
                return None;
            }
        }
    }
    let mut out = String::new();
    Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    let start = out.find(MARK)? + MARK.len();
    let end = start + out[start..].find(MARK)?;
    Some(out[start..end].to_owned())
}

/// `…/v22.1.0` → `[22, 1, 0]` (unparseable parts count as 0).
fn node_version(dir: &Path) -> Vec<u32> {
    dir.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.trim_start_matches('v')
                .split('.')
                .map(|p| p.parse().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
}

/// Install dirs to check beyond the shell PATH, including every nvm-installed node (global npm
/// CLIs live next to node).
fn well_known_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/home/linuxbrew/.linuxbrew/bin",
        "/usr/bin",
        "/bin",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let Some(home) = home else { return dirs };
    for rel in [
        ".local/bin",
        ".bun/bin",
        ".cargo/bin",
        ".volta/bin",
        ".asdf/shims",
        ".local/share/mise/shims",
        ".fnm/aliases/default/bin",
        ".local/share/pnpm",
        "Library/pnpm",
        ".npm-global/bin",
        ".opencode/bin",
        ".deno/bin",
        "go/bin",
    ] {
        dirs.push(home.join(rel));
    }
    // Newest nvm node first: numeric, not lexicographic (v22.1.0 is newer than v9.11.2).
    if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut versions: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        versions.sort_by_key(|p| std::cmp::Reverse(node_version(p)));
        dirs.extend(versions.into_iter().map(|p| p.join("bin")));
    }
    dirs
}

/// Re-detects the search path and exports it as this process's PATH, so every agent we spawn
/// (and the node/npx they need) resolves the same way. Called at startup, before any run.
pub fn hydrate_path() {
    let mut path: Vec<PathBuf> = vec![];
    let current = std::env::var("PATH").unwrap_or_default();
    let login = login_shell_path().unwrap_or_default();
    for dir in std::env::split_paths(&current)
        .chain(std::env::split_paths(&login))
        .chain(well_known_dirs())
    {
        if dir.as_os_str().is_empty() || !dir.is_dir() || path.contains(&dir) {
            continue;
        }
        path.push(dir);
    }
    if let Ok(joined) = std::env::join_paths(&path) {
        std::env::set_var("PATH", joined);
    }
    *SEARCH_PATH.lock().unwrap_or_else(PoisonError::into_inner) = path;
}

/// The resolved path of `bin` on the hydrated search path, or none. Before [hydrate_path] ran,
/// the bare process PATH is searched.
pub fn which(bin: &str) -> Option<PathBuf> {
    let dirs = SEARCH_PATH.lock().unwrap_or_else(PoisonError::into_inner);
    if dirs.is_empty() {
        let entry = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(bin))
                .find(|p| is_executable(p))
        });
        return entry;
    }
    dirs.iter()
        .map(|dir| dir.join(bin))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The spawn command for an ACP mode: the resolved CLI path plus its ACP arguments. `Ok(None)`
/// when `id` names no mode (callers keep the copilot default); an error carries the setup steps
/// when the mode is known but cannot run.
pub fn command_for(id: &str) -> Result<Option<String>, CoreError> {
    let Some(mode) = mode(id) else {
        return Ok(None);
    };
    let template = mode
        .command
        .ok_or_else(|| CoreError::ProviderUnavailable(mode.setup.to_string()))?;
    if template.starts_with("npx ") && which("npx").is_none() {
        return Err(CoreError::ProviderUnavailable(format!(
            "{} runs through an npm package, and npx was not found on PATH. {}",
            mode.name, mode.setup
        )));
    }
    // The copilot CLI may be installed out of PATH; its documented override keeps working.
    let bin = std::env::var_os("COPILOT_CLI_PATH")
        .filter(|_| mode.id == "copilot")
        .map(PathBuf::from)
        .filter(|path| is_executable(path))
        .or_else(|| mode.bins.iter().find_map(|b| which(b)))
        .ok_or_else(|| CoreError::ProviderUnavailable(mode.setup.to_string()))?;
    Ok(Some(template.replacen(
        "{bin}",
        &shell_quote(&bin.to_string_lossy()),
        1,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn claude_runs_through_its_adapter() {
        let command = mode("claude").and_then(|m| m.command).unwrap();
        assert!(command.starts_with("npx -y @agentclientprotocol/claude-agent-acp@"));
        assert!(
            !command.contains("{bin}"),
            "the adapter, not the CLI, is what is launched"
        );
    }

    #[test]
    fn every_mode_id_is_unique() {
        let ids: HashSet<_> = MODES.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), MODES.len());
    }

    #[test]
    fn detects_executables_on_search_path() {
        let dir = std::env::temp_dir().join(format!("silver-agent-modes-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-agent");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        *SEARCH_PATH.lock().unwrap() = vec![dir.clone()];
        assert!(which("fake-agent").is_none(), "not executable yet");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(which("fake-agent"), Some(bin));
        SEARCH_PATH.lock().unwrap().clear();
    }
}
