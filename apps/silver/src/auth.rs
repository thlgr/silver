//! Provider credential store behind `/login`: `<data_dir>/auth.json`, written 0600 and atomically.
//! A missing or corrupt file is an empty store. [RoutedModel](crate::routed::RoutedModel) reads it
//! on every request, so a sign-in applies on the next turn. Keys only ever render redacted.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use silver_core::redact::REDACTED;

/// Failure modes of the credential store.
#[derive(Debug, Error)]
pub enum AuthError {
    /// The provider id is not one of the shared presets.
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    /// Storing a credential for a provider that has neither a key nor an OAuth grant.
    #[error("{0} needs an API key or an OAuth sign-in")]
    MissingCredential(String),
    /// The store could not be written.
    #[error("credential store io error at {path}")]
    Io {
        /// Path involved in the failure.
        path: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The store could not be serialized.
    #[error("credential store serialization failed")]
    Serialize(#[from] serde_json::Error),
}

/// One provider's stored credential and endpoint overrides.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProviderCredential {
    /// API key typed at `/login`, when the provider uses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Endpoint override; the preset's base URL is used when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Model override; the preset's default model is used when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// When the credential was last written.
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

impl ProviderCredential {
    /// Whether a key was stored for this provider.
    pub fn has_key(&self) -> bool {
        self.api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
    }
}

impl fmt::Debug for ProviderCredential {
    /// Never render the key itself.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderCredential")
            .field("api_key", &self.api_key.as_ref().map(|_| REDACTED))
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// The on-disk shape of the store.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuthFile {
    /// Provider whose credential routes new runs; absent means the config.toml model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Credentials by provider id.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCredential>,
}

/// The credential store, safe to share across tasks.
pub struct AuthStore {
    path: PathBuf,
    state: RwLock<AuthFile>,
}

impl AuthStore {
    /// Open the store; a missing, unreadable or corrupt file yields an empty one, logged without
    /// its contents.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %path.display(), "credential store is unreadable; starting empty");
                AuthFile::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => AuthFile::default(),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "could not read the credential store");
                AuthFile::default()
            }
        };
        Self {
            path,
            state: RwLock::new(state),
        }
    }

    /// The resolved backing file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A copy of the whole store.
    pub fn snapshot(&self) -> AuthFile {
        AuthFile::clone(&self.read())
    }

    /// The provider routing new runs, when one is active.
    pub fn active(&self) -> Option<String> {
        Option::clone(&self.read().active)
    }

    /// The stored credential for provider, if any.
    pub fn credential(&self, provider: &str) -> Option<ProviderCredential> {
        self.read().providers.get(provider).cloned()
    }

    /// Store a provider's key and optional endpoint/model. Absent fields keep their value, and a
    /// call carrying nothing for a provider with no entry is refused, so `/login` cannot store a
    /// blank one.
    pub fn set_credential(
        &self,
        provider: &str,
        api_key: Option<&str>,
        base_url: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), AuthError> {
        preset_for(provider)?;
        let key = api_key.map(str::trim).filter(|key| !key.is_empty());
        let has_override = base_url.map(str::trim).is_some_and(|url| !url.is_empty())
            || model.map(str::trim).is_some_and(|model| !model.is_empty());
        if key.is_none() && !has_override && self.credential(provider).is_none() {
            return Err(AuthError::MissingCredential(provider.to_string()));
        }
        {
            let mut state = self.write();
            let entry = state.providers.entry(provider.to_string()).or_default();
            if let Some(key) = key {
                entry.api_key = Some(key.to_string());
            }
            if let Some(base_url) = base_url.map(str::trim).filter(|url| !url.is_empty()) {
                entry.base_url = Some(base_url.to_string());
            }
            if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
                entry.model = Some(model.to_string());
            }
            entry.updated_at = Utc::now();
        }
        self.save()
    }

    /// Point new runs at provider. The caller has already checked it can authenticate.
    pub fn activate(&self, provider: &str) -> Result<(), AuthError> {
        preset_for(provider)?;
        self.write().active = Some(provider.to_string());
        self.save()
    }

    /// Hand routing back to the config.toml model.
    pub fn deactivate(&self) -> Result<(), AuthError> {
        self.write().active = None;
        self.save()
    }

    /// Drop a provider's stored credential; true when one was present. Forgetting the active
    /// provider also hands routing back to config.toml, so a run never uses a cleared key.
    pub fn forget(&self, provider: &str) -> Result<bool, AuthError> {
        let removed = {
            let mut state = self.write();
            let removed = state.providers.remove(provider).is_some();
            if state.active.as_deref() == Some(provider) {
                state.active = None;
            }
            removed
        };
        self.save()?;
        Ok(removed)
    }

    /// Persist the current state atomically, 0600 on Unix.
    fn save(&self) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec_pretty(&*self.read())?;
        crate::atomic_file::write_private(&self.path, &bytes).map_err(|source| AuthError::Io {
            path: self.path.display().to_string(),
            source,
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, AuthFile> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, AuthFile> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl fmt::Debug for AuthStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.read();
        f.debug_struct("AuthStore")
            .field("path", &self.path)
            .field("active", &state.active)
            .field(
                "providers",
                &state.providers.keys().collect::<Vec<&String>>(),
            )
            .finish()
    }
}

/// The shared preset for a provider id, or [AuthError::UnknownProvider].
fn preset_for(
    provider: &str,
) -> Result<&'static silver_protocol::providers::ProviderPreset, AuthError> {
    silver_protocol::providers::preset(provider)
        .ok_or_else(|| AuthError::UnknownProvider(provider.to_string()))
}
