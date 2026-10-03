//! Per-language LSP server detection: a builtin extension table, replaced by the `[lsp]` servers
//! list when set. None when the binary is not on PATH, so nothing doomed is spawned.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// One builtin language server definition.
#[derive(Clone, Copy, Debug)]
pub struct BuiltinServer {
    /// Stable server id, also the config key a user would name.
    pub id: &'static str,
    /// Binary names tried in order on PATH.
    pub binaries: &'static [&'static str],
    /// Extra CLI arguments appended after the resolved binary.
    pub args: &'static [&'static str],
    /// Lower-case extensions (including the dot) this server handles.
    pub extensions: &'static [&'static str],
    /// Project-root marker files, nearest-first walk from the source file.
    pub root_markers: &'static [&'static str],
}

/// The builtin server table.
pub const BUILTIN_SERVERS: &[BuiltinServer] = &[
    BuiltinServer {
        id: "rust-analyzer",
        binaries: &["rust-analyzer"],
        args: &[],
        extensions: &[".rs"],
        root_markers: &["Cargo.toml", "Cargo.lock"],
    },
    BuiltinServer {
        id: "pyright",
        binaries: &["pyright-langserver", "pyright"],
        args: &["--stdio"],
        extensions: &[".py", ".pyi"],
        root_markers: &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "pyrightconfig.json",
        ],
    },
    BuiltinServer {
        id: "pylsp",
        binaries: &["pylsp"],
        args: &[],
        extensions: &[".py", ".pyi"],
        root_markers: &["pyproject.toml", "setup.py", "setup.cfg", "tox.ini"],
    },
    BuiltinServer {
        id: "typescript-language-server",
        binaries: &["typescript-language-server"],
        args: &["--stdio"],
        extensions: &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"],
        root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
    },
    BuiltinServer {
        id: "gopls",
        binaries: &["gopls"],
        args: &[],
        extensions: &[".go"],
        root_markers: &["go.work", "go.mod", "go.sum"],
    },
    BuiltinServer {
        id: "clangd",
        binaries: &["clangd"],
        args: &["--background-index"],
        extensions: &[".c", ".h", ".cc", ".cpp", ".cxx", ".hh", ".hpp", ".hxx"],
        root_markers: &["compile_commands.json", "compile_flags.txt", ".clangd"],
    },
];

/// A resolved server ready to spawn for a particular file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSpec {
    /// The server id.
    pub id: String,
    /// argv, with argv[0] resolved to a concrete path on PATH.
    pub command: Vec<String>,
    /// LSP languageId for textDocument/didOpen.
    pub language_id: String,
    /// Project-root markers for the nearest-root walk.
    pub root_markers: Vec<String>,
}

/// The LSP languageId for a path (used in didOpen). Unknown extensions fall
/// back to plaintext, matching the reference behaviour.
pub fn language_id_for(path: &Path) -> &'static str {
    match extension_of(path).as_str() {
        ".py" | ".pyi" => "python",
        ".ts" | ".mts" | ".cts" => "typescript",
        ".tsx" => "typescriptreact",
        ".js" | ".mjs" | ".cjs" => "javascript",
        ".jsx" => "javascriptreact",
        ".go" => "go",
        ".rs" => "rust",
        ".c" | ".h" => "c",
        ".cc" | ".cpp" | ".cxx" | ".hh" | ".hpp" | ".hxx" => "cpp",
        _ => "plaintext",
    }
}

/// The lower-case extension including its leading dot, or "" when there is none.
pub fn extension_of(path: &Path) -> String {
    match path.extension().and_then(OsStr::to_str) {
        Some(extension) => format!(".{}", extension.to_ascii_lowercase()),
        None => String::new(),
    }
}

/// The first builtin server whose extension list contains the file extension.
pub fn builtin_for_path(path: &Path) -> Option<&'static BuiltinServer> {
    let extension = extension_of(path);
    if extension.is_empty() {
        return None;
    }
    BUILTIN_SERVERS
        .iter()
        .find(|server| server.extensions.contains(&extension.as_str()))
}

/// Detect a server for a path using the process PATH.
pub fn detect_server(path: &Path, explicit: &[String]) -> Option<ServerSpec> {
    detect_server_with_path(path, explicit, std::env::var_os("PATH").as_deref())
}

/// Detect a server for a path against a caller-supplied PATH. When explicit is
/// non-empty only those servers are considered; otherwise the builtin table
/// applies. None means the binary is not on PATH.
pub fn detect_server_with_path(
    path: &Path,
    explicit: &[String],
    path_env: Option<&OsStr>,
) -> Option<ServerSpec> {
    if !explicit.is_empty() {
        return explicit_server_for(path, explicit, path_env);
    }
    let builtin = builtin_for_path(path)?;
    let binary = find_binary(builtin.binaries, path_env)?;
    Some(spec_from_builtin(builtin, path, &binary))
}

/// Walk up from a source file for the first directory containing a root marker,
/// stopping at the workspace root; fall back to the workspace root itself.
pub fn resolve_project_root(path: &Path, workspace_root: &Path, markers: &[String]) -> PathBuf {
    if markers.is_empty() {
        return workspace_root.to_path_buf();
    }
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let mut current = Some(start);
    while let Some(directory) = current {
        if markers.iter().any(|marker| directory.join(marker).exists()) {
            return directory.to_path_buf();
        }
        if directory == workspace_root {
            break;
        }
        current = directory.parent();
    }
    workspace_root.to_path_buf()
}

/// Resolve the first name found on PATH.
pub fn find_binary(names: &[&str], path_env: Option<&OsStr>) -> Option<PathBuf> {
    names.iter().find_map(|name| which_in(name, path_env))
}

/// Resolve a binary name against a PATH value. An absolute or slash-containing
/// name is used verbatim; otherwise each PATH entry is probed. The result must
/// be a regular file with an executable bit on Unix.
pub fn which_in(name: &str, path_env: Option<&OsStr>) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_absolute() || name.contains('/') {
        return is_executable(candidate).then(|| candidate.to_path_buf());
    }
    let path_env = path_env?;
    for directory in std::env::split_paths(path_env) {
        let full = directory.join(name);
        if is_executable(&full) {
            return Some(full);
        }
    }
    None
}

fn spec_from_builtin(builtin: &'static BuiltinServer, path: &Path, binary: &Path) -> ServerSpec {
    let mut command = vec![binary.to_string_lossy().into_owned()];
    command.extend(builtin.args.iter().map(|arg| arg.to_string()));
    ServerSpec {
        id: builtin.id.to_string(),
        command,
        language_id: language_id_for(path).to_string(),
        root_markers: builtin
            .root_markers
            .iter()
            .map(|marker| marker.to_string())
            .collect(),
    }
}

fn explicit_server_for(
    path: &Path,
    explicit: &[String],
    path_env: Option<&OsStr>,
) -> Option<ServerSpec> {
    let extension = extension_of(path);
    for command_line in explicit {
        let mut parts = command_line.split_whitespace();
        let Some(binary_name) = parts.next() else {
            continue;
        };
        let Some(builtin) = builtin_for_binary(binary_name) else {
            continue;
        };
        if !builtin.extensions.contains(&extension.as_str()) {
            continue;
        }
        let Some(binary) = which_in(binary_name, path_env) else {
            continue;
        };
        let mut command = vec![binary.to_string_lossy().into_owned()];
        let extra: Vec<String> = parts.map(str::to_string).collect();
        if extra.is_empty() {
            command.extend(builtin.args.iter().map(|arg| arg.to_string()));
        } else {
            command.extend(extra);
        }
        return Some(ServerSpec {
            id: builtin.id.to_string(),
            command,
            language_id: language_id_for(path).to_string(),
            root_markers: builtin
                .root_markers
                .iter()
                .map(|marker| marker.to_string())
                .collect(),
        });
    }
    None
}

fn builtin_for_binary(name: &str) -> Option<&'static BuiltinServer> {
    let base = Path::new(name)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(name);
    BUILTIN_SERVERS
        .iter()
        .find(|server| server.id == base || server.binaries.contains(&base))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
