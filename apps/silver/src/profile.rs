//! Named profiles: `<config base>/profiles/<name>`, selected by `SILVER_PROFILE` and resolved by
//! [crate::config]. This module creates, lists and deletes them.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Directory, under the config base, that holds one sub-directory per named profile.
pub const PROFILES_DIR: &str = "profiles";

pub use crate::config::active_profile;
pub use crate::config::PROFILE_ENV;

/// Names no profile may take: the installation itself and common system binaries.
///
/// Mirrors Hermes _RESERVED_NAMES, with hermes replaced by silver.
pub const RESERVED_NAMES: [&str; 6] = ["silver", "default", "test", "tmp", "root", "sudo"];

/// Human-readable form of the profile id rule (mirrors Hermes _PROFILE_NAME_RULE).
pub const PROFILE_NAME_RULE: &str =
    "Use lowercase letters, numbers, '-' or '_', starting with a letter or number, up to 64 characters";

/// One profile's summary, mirroring the shape Hermes list_profiles() returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileInfo {
    /// Canonical profile id (default for the base install).
    pub name: String,
    /// Profile home directory.
    pub path: PathBuf,
    /// True for the base install (<config base> itself, never under profiles/).
    pub is_default: bool,
    /// model.name from the profile's config.toml, when configured.
    pub model: Option<String>,
    /// model.provider from the profile's config.toml, when configured.
    pub provider: Option<String>,
}

/// Result of a delete request: removed, or declined at the confirmation prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The profile directory was removed.
    Removed(PathBuf),
    /// An interactive confirmation was declined; nothing changed.
    Cancelled(PathBuf),
}

/// The parent of profiles/: SILVER_CONFIG_DIR as is, else config_dir() without the trailing
/// `profiles/<name>` when a profile is active.
pub fn config_base_dir() -> PathBuf {
    if std::env::var_os("SILVER_CONFIG_DIR").is_some() {
        return crate::config::config_dir();
    }
    let dir = crate::config::config_dir();
    if crate::config::active_profile().is_some() {
        match dir.parent().and_then(Path::parent) {
            Some(base) => base.to_path_buf(),
            None => dir,
        }
    } else {
        dir
    }
}

/// The directory holding every named profile: <config base>/profiles.
pub fn profiles_root() -> PathBuf {
    config_base_dir().join(PROFILES_DIR)
}

/// Normalize a user-supplied profile name: trim, lowercase, and collapse default
/// case-insensitively. Mirrors Hermes normalize_profile_name.
pub fn normalize_profile_name(name: &str) -> Result<String> {
    let stripped = name.trim();
    if stripped.is_empty() {
        bail!("profile name cannot be empty");
    }
    if stripped.eq_ignore_ascii_case("default") {
        return Ok("default".to_string());
    }
    Ok(stripped.to_ascii_lowercase())
}

/// The profile id rule, plus the reserved-name check. default is always accepted.
///
/// Mirrors Hermes validate_profile_name: pass an already-normalized id.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name == "default" {
        return Ok(());
    }
    if !crate::config::is_valid_profile_name(name) {
        bail!(invalid_profile_name_message(name));
    }
    if RESERVED_NAMES.contains(&name) {
        bail!(
            "Profile name {name:?} is reserved — it collides with either the Hermano installation itself or a common system binary. Pick a different name."
        );
    }
    Ok(())
}

/// Best-effort valid id derived from name ('My Work' -> 'my-work').
///
/// Mirrors Hermes _suggest_profile_name, including the my-work fallback.
pub fn suggest_profile_name(name: &str) -> String {
    let lowered = name.trim().to_ascii_lowercase();
    let mut candidate = String::new();
    for ch in lowered.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_' {
            candidate.push(ch);
        } else if !candidate.ends_with('-') {
            candidate.push('-');
        }
    }
    let candidate = candidate.trim_matches(|c| c == '-' || c == '_');
    let candidate: String = candidate.chars().take(64).collect();
    if crate::config::is_valid_profile_name(&candidate) {
        candidate
    } else {
        "my-work".to_string()
    }
}

/// The error copy Hermes emits for a malformed profile name.
fn invalid_profile_name_message(name: &str) -> String {
    let suggestion = suggest_profile_name(name);
    format!(
        "{name:?} is not a valid profile name. {PROFILE_NAME_RULE} (for example: {suggestion}). Then run `silver profile create {suggestion}`."
    )
}

/// normalize + validate in one step; returns the canonical id.
fn canon_valid(name: &str) -> Result<String> {
    let canon = normalize_profile_name(name)?;
    validate_profile_name(&canon)?;
    Ok(canon)
}

/// A profile's home directory; `default` is the config base. Only the id rule applies, not the
/// reserved list, so an existing profiles/test still resolves.
pub fn profile_path(name: &str) -> Result<PathBuf> {
    profile_path_at(&config_base_dir(), name)
}

/// Pure form of [profile_path] against an explicit config base.
pub fn profile_path_at(base: &Path, name: &str) -> Result<PathBuf> {
    let canon = normalize_profile_name(name)?;
    if canon == "default" {
        return Ok(base.to_path_buf());
    }
    if !crate::config::is_valid_profile_name(&canon) {
        bail!(invalid_profile_name_message(&canon));
    }
    Ok(base.join(PROFILES_DIR).join(canon))
}

/// True when a profile directory exists. default is always present.
pub fn profile_exists(name: &str) -> bool {
    match normalize_profile_name(name) {
        Ok(canon) if canon == "default" => true,
        Ok(canon) => profile_path_at(&config_base_dir(), &canon)
            .map(|path| path.is_dir())
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// model.name and model.provider from a profile's config.toml, accepting the string form of
/// `model`; (None, None) when missing or unreadable.
pub fn read_config_model(profile_dir: &Path) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(profile_dir.join("config.toml")) else {
        return (None, None);
    };
    let Ok(root) = text.parse::<toml::Value>() else {
        return (None, None);
    };
    let Some(model) = root.get("model") else {
        return (None, None);
    };
    if let Some(name) = model.as_str() {
        return (nonempty(name), None);
    }
    let Some(table) = model.as_table() else {
        return (None, None);
    };
    let name = table
        .get("name")
        .or_else(|| table.get("default"))
        .and_then(|value| value.as_str());
    let provider = table.get("provider").and_then(|value| value.as_str());
    (name.and_then(nonempty), provider.and_then(nonempty))
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Every profile, default first, then named ones by id; hidden staging directories are skipped.
pub fn list_profiles() -> Vec<ProfileInfo> {
    list_profiles_at(&config_base_dir())
}

/// Pure form of [list_profiles] against an explicit config base.
pub fn list_profiles_at(base: &Path) -> Vec<ProfileInfo> {
    let mut profiles = Vec::new();
    if base.is_dir() {
        let (model, provider) = read_config_model(base);
        profiles.push(ProfileInfo {
            name: "default".to_string(),
            path: base.to_path_buf(),
            is_default: true,
            model,
            provider,
        });
    }
    let root = base.join(PROFILES_DIR);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return profiles;
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name == "default" || name.starts_with('.') {
            continue;
        }
        if !crate::config::is_valid_profile_name(&name) {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }
        names.push(name);
    }
    names.sort();
    for name in names {
        let path = root.join(&name);
        let (model, provider) = read_config_model(&path);
        profiles.push(ProfileInfo {
            name,
            path,
            is_default: false,
            model,
            provider,
        });
    }
    profiles
}

/// Create <config base>/profiles/<name> and return its path.
///
/// Rejects an empty/invalid name, a reserved name, default, and an existing directory.
pub fn create_profile(name: &str) -> Result<PathBuf> {
    create_profile_at(&config_base_dir(), name)
}

/// Pure form of [create_profile] against an explicit config base.
pub fn create_profile_at(base: &Path, name: &str) -> Result<PathBuf> {
    let canon = canon_valid(name)?;
    if canon == "default" {
        bail!("Cannot create a profile named 'default' — it is the built-in profile.");
    }
    let path = base.join(PROFILES_DIR).join(&canon);
    if path.exists() {
        bail!(
            "A profile named '{canon}' already exists. Switch to it with `silver -p {canon}`, see all profiles with `silver profile list`, or choose a different name."
        );
    }
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create profile directory {}", path.display()))?;
    Ok(path)
}

/// Delete a profile directory; without `force` the user must retype its name. `default` and
/// reserved names are refused.
pub fn delete_profile(name: &str, force: bool) -> Result<DeleteOutcome> {
    delete_profile_at(&config_base_dir(), name, force)
}

/// Pure form of [delete_profile] against an explicit config base.
pub fn delete_profile_at(base: &Path, name: &str, force: bool) -> Result<DeleteOutcome> {
    let canon = canon_valid(name)?;
    if canon == "default" {
        bail!(
            "Cannot delete the default profile (the base install). Remove {} manually to delete everything.",
            base.display()
        );
    }
    let path = base.join(PROFILES_DIR).join(&canon);
    if !path.is_dir() {
        bail!("No profile named '{canon}'. See your profiles with: silver profile list");
    }
    if !force {
        print!("Type '{canon}' to confirm: ");
        drop(io::stdout().flush());
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 || line.trim() != canon {
            return Ok(DeleteOutcome::Cancelled(path));
        }
    }
    std::fs::remove_dir_all(&path)
        .with_context(|| format!("remove profile directory {}", path.display()))?;
    Ok(DeleteOutcome::Removed(path))
}
