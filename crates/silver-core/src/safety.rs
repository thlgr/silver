//! File-safety guards: defense in depth, not a boundary (bash runs as the same user); they give a
//! typed denial and an audit trail. Pass absolute paths: a relative `auth.json` misses the
//! denylist. The free predicates expand `~` and treat relative paths as home-relative.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Stable error code for a denied read.
pub const READ_DENIED_CODE: &str = "read_denied";
/// Stable error code for a denied write.
pub const WRITE_DENIED_CODE: &str = "write_denied";

/// Secret-bearing project env file basenames blocked for reading anywhere on disk
/// (mirrors Hermes '_BLOCKED_PROJECT_ENV_BASENAMES').
pub const BLOCKED_PROJECT_ENV_BASENAMES: [&str; 7] = [
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env.staging",
    ".envrc",
];

/// Documentation-only env templates that stay readable ('env.example' is the documented
/// shape substitute; Hermes tests require it to be allowed).
const ENV_TEMPLATE_BASENAMES: [&str; 3] = [".env.example", ".env.sample", ".env.template"];

/// Exact credential stores under the silver home/root that reads must never reach.
/// 'secrets.env' is the daemon's own setup file ('silver::config::secrets_env_path').
pub const CREDENTIAL_FILE_NAMES: [&str; 8] = [
    "auth.json",
    "auth.lock",
    ".anthropic_oauth.json",
    ".env",
    "secrets.env",
    "webhook_subscriptions.json",
    "auth/google_oauth.json",
    "cache/bws_cache.json",
];

/// Exact files under the silver home/root that writes must never reach (Hermes
/// 'hermes_files' plus the daemon secrets file).
const SILVER_WRITE_DENIED_FILES: [&str; 6] = [
    ".env",
    ".anthropic_oauth.json",
    "secrets.env",
    "auth/google_oauth.json",
    "cache/bws_cache.json",
    "cache/bws_cache.enc.json",
];

/// Hermano home/root subpaths the generic file tools must not rewrite. Session
/// transcripts and 'vault/' / 'mcp-tokens/' / 'browser-profile/' hold credential material.
pub const SILVER_PROTECTED_SUBPATHS: [&str; 6] = [
    "state.db",
    "sessions",
    "mcp-tokens",
    "pairing",
    "vault",
    "browser-profile",
];

/// Directory-prefix read denies under the silver home/root:
/// '(subdir, message for the directory itself, message for a file inside)'.
pub const READ_DENIED_DIRS: [(&str, &str, &str); 3] = [
    (
        "mcp-tokens",
        "is the silver MCP token directory and cannot be read directly.",
        "is a silver MCP token file and cannot be read directly.",
    ),
    (
        "browser-profile",
        "is the silver real-profile browser snapshot directory (copied cookies/logins) and cannot be read directly.",
        "is inside the silver real-profile browser snapshot (copied cookies/logins) and cannot be read directly.",
    ),
    (
        "vault",
        "is the silver credential vault directory and cannot be read directly.",
        "is inside the silver credential vault (encrypted secrets + local key) and cannot be read directly.",
    ),
];

const WRITE_DENIED_REASON: &str = "is a protected system/credential file.";

const READ_DENIED_CREDENTIAL: &str = "is a silver credential store and cannot be read directly.";

const READ_DENIED_HUB_CACHE: &str =
    "is an internal silver cache file and cannot be read directly to prevent prompt injection.";

const READ_DENIED_ENV: &str =
    "is a secret-bearing environment file and cannot be read to prevent credential leakage; read .env.example instead.";

/// Reason fragment for a Windows NT/device-namespace path. Checking the RAW string is
/// deliberate: resolving such a path is itself the NTLM-leak trigger, and namespace
/// prefixes defeat prefix-comparison denylists after normalization.
const NT_NAMESPACE_REASON: &str = "uses a Windows NT/device namespace prefix (\\??\\, \\\\.\\, \\\\?\\UNC\\, or GLOBALROOT) that can trigger outbound SMB authentication (NTLM credential leak) merely by being resolved; use a normal absolute path instead.";

/// Return true when 'raw' is a Windows NT-/device-namespace path.
///
/// Checks the raw string only, never resolves the path.
pub fn is_nt_namespace_path(raw: &str) -> bool {
    let s = raw.replace('/', "\\");
    if s.starts_with("\\??\\") {
        return true;
    }
    if s.starts_with("\\\\.\\") {
        return true;
    }
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        let upper = rest.to_ascii_uppercase();
        if upper.starts_with("UNC\\") || upper.starts_with("GLOBALROOT\\") {
            return true;
        }
    }
    false
}

/// Return the reason fragment when 'path' uses the NT/device namespace.
pub fn nt_namespace_error(path: &Path) -> Option<&'static str> {
    if is_nt_namespace_path(&path.to_string_lossy()) {
        Some(NT_NAMESPACE_REASON)
    } else {
        None
    }
}

/// Exact sensitive paths under 'home' that must never be written ('/etc' entries included).
pub fn build_write_denied_paths(home: &Path) -> Vec<PathBuf> {
    let home_files = [
        ".ssh/authorized_keys",
        ".ssh/id_rsa",
        ".ssh/id_ed25519",
        ".netrc",
        ".pgpass",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
    ];
    let mut paths: Vec<PathBuf> = home_files.iter().map(|name| home.join(name)).collect();
    paths.push(PathBuf::from("/etc/sudoers"));
    paths.push(PathBuf::from("/etc/passwd"));
    paths.push(PathBuf::from("/etc/shadow"));
    paths
}

/// Sensitive directory prefixes under 'home' that must never be written ('/etc' included).
pub fn build_write_denied_prefixes(home: &Path) -> Vec<PathBuf> {
    let home_dirs = [
        ".ssh",
        ".aws",
        ".gnupg",
        ".kube",
        ".docker",
        ".azure",
        ".config/gh",
        ".config/gcloud",
    ];
    let mut paths: Vec<PathBuf> = home_dirs.iter().map(|dir| home.join(dir)).collect();
    paths.push(PathBuf::from("/etc/sudoers.d"));
    paths.push(PathBuf::from("/etc/systemd"));
    paths
}

/// Paths that need human approval because they are not credential bytes but can execute
/// code: '~/.ssh/config' may carry ProxyCommand / Match exec.
pub fn build_write_approval_paths(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".ssh").join("config")]
}

/// Roots for the guards: the user's home and silver's config/data directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafetyRoots {
    home: PathBuf,
    silver_dirs: Vec<PathBuf>,
}

impl SafetyRoots {
    /// Build roots, canonicalising each entry where the filesystem allows it.
    pub fn new(home: impl Into<PathBuf>, silver_dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        let home = canonical_or_raw(&home.into());
        let silver_dirs = silver_dirs
            .into_iter()
            .map(|dir| canonical_or_raw(&dir))
            .collect();
        Self { home, silver_dirs }
    }

    /// Resolve roots from the process environment.
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let config = xdg_dir("XDG_CONFIG_HOME", &home, ".config").join("silver");
        let data = xdg_dir("XDG_DATA_HOME", &home, ".local/share").join("silver");
        Self::new(home, vec![config, data])
    }

    /// The OS home this guard covers.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The active silver config/data directories this guard covers.
    pub fn silver_dirs(&self) -> &[PathBuf] {
        &self.silver_dirs
    }

    /// Why a write is hard-denied, if it is. Approval-gated paths go first, so the `~/.ssh` deny
    /// does not swallow `~/.ssh/config`.
    pub fn write_denied_reason(&self, path: &Path) -> Option<&'static str> {
        let resolved = self.coordinate(path);
        if self.write_approval_required(&resolved) {
            return None;
        }
        if build_write_denied_paths(&self.home)
            .iter()
            .any(|denied| **denied == *resolved)
            || build_write_denied_prefixes(&self.home)
                .iter()
                .any(|prefix| resolved.starts_with(prefix))
        {
            return Some(WRITE_DENIED_REASON);
        }
        for base in &self.silver_dirs {
            if SILVER_WRITE_DENIED_FILES
                .iter()
                .any(|name| resolved == base.join(name))
            {
                return Some(WRITE_DENIED_REASON);
            }
            if SILVER_PROTECTED_SUBPATHS.iter().any(|sub| {
                let protected = base.join(sub);
                resolved == protected || resolved.starts_with(&protected)
            }) {
                return Some(WRITE_DENIED_REASON);
            }
        }
        None
    }

    /// Reason a read is denied, or None when it may proceed. This is the full port of
    /// Hermes 'get_read_block_error' minus the composed 'Access denied' prefix.
    pub fn read_denied_reason(&self, path: &Path) -> Option<&'static str> {
        // NT/device-namespace check runs on the RAW path before any coordinate/join:
        // resolving such a path is the NTLM-leak trigger this guard exists to prevent.
        if nt_namespace_error(path).is_some() {
            return Some(NT_NAMESPACE_REASON);
        }
        let resolved = self.coordinate(path);
        for base in &self.silver_dirs {
            if resolved.starts_with(base.join("skills").join(".hub")) {
                return Some(READ_DENIED_HUB_CACHE);
            }
            if CREDENTIAL_FILE_NAMES
                .iter()
                .any(|name| resolved == base.join(name))
            {
                return Some(READ_DENIED_CREDENTIAL);
            }
            for (subdir, dir_message, file_message) in READ_DENIED_DIRS {
                let blocked = base.join(subdir);
                if resolved == blocked {
                    return Some(dir_message);
                }
                if resolved.starts_with(&blocked) {
                    return Some(file_message);
                }
            }
        }
        if let Some(name) = resolved.file_name().and_then(|name| name.to_str()) {
            if blocked_env_basename(&name.to_ascii_lowercase()) {
                return Some(READ_DENIED_ENV);
            }
        }
        None
    }

    /// True when 'path' is approval-gated ('~/.ssh/config').
    pub fn write_approval_required(&self, path: &Path) -> bool {
        let resolved = self.coordinate(path);
        build_write_approval_paths(&self.home)
            .iter()
            .any(|gated| **gated == *resolved)
    }

    /// Expand a leading '~' and anchor relative paths at the home root. Paths produced by
    /// the workspace tools are already absolute and pass through unchanged.
    fn coordinate<'a>(&'a self, path: &'a Path) -> Cow<'a, Path> {
        let raw = path.to_string_lossy();
        if raw == "~" {
            return Cow::Borrowed(&self.home);
        }
        if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            return Cow::Owned(self.home.join(rest));
        }
        if path.is_absolute() {
            return Cow::Borrowed(path);
        }
        Cow::Owned(self.home.join(path))
    }
}

/// Reason a write is hard-denied against the process-environment roots.
pub fn write_denied_reason(path: &Path) -> Option<&'static str> {
    SafetyRoots::from_env().write_denied_reason(path)
}

/// Reason a read is denied against the process-environment roots.
pub fn read_denied_reason(path: &Path) -> Option<&'static str> {
    SafetyRoots::from_env().read_denied_reason(path)
}

/// True when 'path' is approval-gated against the process-environment roots.
pub fn write_approval_required(path: &Path) -> bool {
    SafetyRoots::from_env().write_approval_required(path)
}

/// Hermes-named alias for the write-approval predicate.
pub fn is_write_approval_required(path: &Path) -> bool {
    write_approval_required(path)
}

/// True when 'path' is blocked by the write denylist against the process-environment roots.
pub fn is_write_denied(path: &Path) -> bool {
    write_denied_reason(path).is_some()
}

/// Full model-facing write-denial message, or None when the write is allowed.
pub fn get_write_denied_error(path: &Path) -> Option<String> {
    write_denied_reason(path).map(|reason| format!("Write denied: {} {reason}", path.display()))
}

/// Full model-facing read-denial message, or None when the read is allowed.
pub fn get_read_block_error(path: &Path) -> Option<String> {
    read_denied_reason(path).map(|reason| format!("Access denied: {} {reason}", path.display()))
}

/// Build the stable, machine-readable tool-error body for a denied path.
pub fn denied_error_body(code: &str, path: &Path, message: &str) -> String {
    serde_json::json!({
        "code": code,
        "path": path.display().to_string(),
        "message": message,
    })
    .to_string()
}

/// Env file basenames that are blocked for reads. Any '.env*' name is blocked except the
/// documentation templates, so a newly added '.env.foo' cannot leak secrets by default.
fn blocked_env_basename(lower: &str) -> bool {
    if BLOCKED_PROJECT_ENV_BASENAMES.contains(&lower) {
        return true;
    }
    if ENV_TEMPLATE_BASENAMES.contains(&lower) {
        return false;
    }
    lower.starts_with(".env")
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn xdg_dir(variable: &str, home: &Path, fallback: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(fallback))
}
