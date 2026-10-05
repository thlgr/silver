//! Remembered approvals for one tool and one canonical argument set: *session* decisions last until
//! restart, *always* decisions persist in `<data_dir>/approvals.json`. Denials are never kept.

use serde_json::Value;
use silver_core::guard::tool_guardrails::canonical_tool_args;
use silver_core::hash::content_hash;
use silver_protocol::{Scope, SessionId};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Most global entries kept; the lexicographically first entries are evicted first so the
/// bound is deterministic.
const MAX_GLOBAL_ENTRIES: usize = 512;
/// Most session entries kept. Session entries never reach disk, but a long-lived daemon
/// serving many sessions must still be bounded.
const MAX_SESSION_ENTRIES: usize = 4096;

/// The on-disk file name, relative to the daemon data directory.
pub const APPROVALS_FILE: &str = "approvals.json";

struct State {
    session: BTreeSet<String>,
    global: BTreeSet<String>,
}

/// Keys are `{tool_name}/{args_hash}` for always and `{session_id}/{tool_name}/{args_hash}` for
/// session decisions, hashed like the approvals table.
pub struct ApprovalMemory {
    path: PathBuf,
    state: Mutex<State>,
}

impl ApprovalMemory {
    /// Load the persisted decisions. A missing or corrupt file starts empty (corruption is logged),
    /// so a bad write never stops the daemon starting.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(APPROVALS_FILE);
        let global = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Vec<String>>(&text) {
                Ok(keys) => keys.into_iter().take(MAX_GLOBAL_ENTRIES).collect(),
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "ignoring corrupt approvals file; starting with an empty allowlist"
                    );
                    BTreeSet::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "could not read approvals file; starting with an empty allowlist"
                );
                BTreeSet::new()
            }
        };
        Self {
            path,
            state: Mutex::new(State {
                session: BTreeSet::new(),
                global,
            }),
        }
    }

    /// The approvals file backing the global entries.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn call_key(tool_name: &str, args: &Value) -> String {
        format!("{tool_name}/{}", content_hash(&canonical_tool_args(args)))
    }

    /// Keyed by scope too: the same command or relative path means something else in
    /// another workspace, so an "always" never crosses into one.
    fn global_key(scope: &Scope, tool_name: &str, args: &Value) -> String {
        let scope = scope
            .workspace_id()
            .map_or_else(|| "global".to_string(), |id| id.to_string());
        format!("{scope}/{}", Self::call_key(tool_name, args))
    }

    fn session_key(session_id: SessionId, tool_name: &str, args: &Value) -> String {
        format!("{session_id}/{}", Self::call_key(tool_name, args))
    }

    /// True when this exact session/tool/arguments combination is remembered either for
    /// the session or for its scope.
    pub fn remembered(
        &self,
        scope: &Scope,
        session_id: SessionId,
        tool_name: &str,
        args: &Value,
    ) -> bool {
        let global = Self::global_key(scope, tool_name, args);
        let session = Self::session_key(session_id, tool_name, args);
        let state = self.state.lock().expect("approval memory lock");
        state.session.contains(&session) || state.global.contains(&global)
    }

    /// Remember a decision for the rest of this session only.
    pub fn approve_session(&self, session_id: SessionId, tool_name: &str, args: &Value) {
        let key = Self::session_key(session_id, tool_name, args);
        let mut state = self.state.lock().expect("approval memory lock");
        insert_capped(&mut state.session, key, MAX_SESSION_ENTRIES);
    }

    /// Remember a decision for this session and persist it for future runs and restarts.
    pub fn approve_always(
        &self,
        scope: &Scope,
        session_id: SessionId,
        tool_name: &str,
        args: &Value,
    ) {
        let global = Self::global_key(scope, tool_name, args);
        let session = Self::session_key(session_id, tool_name, args);
        let changed = {
            let mut state = self.state.lock().expect("approval memory lock");
            let session_changed = insert_capped(&mut state.session, session, MAX_SESSION_ENTRIES);
            let global_changed = insert_capped(&mut state.global, global, MAX_GLOBAL_ENTRIES);
            session_changed || global_changed
        };
        if changed {
            self.persist();
        }
    }

    /// Write the global set atomically: a sibling temp file followed by a rename.
    fn persist(&self) {
        let keys: Vec<String> = {
            let state = self.state.lock().expect("approval memory lock");
            state.global.iter().cloned().collect()
        };
        let json = match serde_json::to_string_pretty(&keys) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "could not serialize approvals allowlist");
                return;
            }
        };
        let temp = self.path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&temp, json) {
            tracing::warn!(
                path = %temp.display(),
                error = %err,
                "could not write approvals temp file"
            );
            return;
        }
        if let Err(err) = std::fs::rename(&temp, &self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "could not replace approvals file"
            );
        }
    }
}

/// Insert while keeping the set at or below `cap`, evicting the smallest key first.
/// Returns whether the key was newly inserted.
fn insert_capped(set: &mut BTreeSet<String>, key: String, cap: usize) -> bool {
    if !set.insert(key) {
        return false;
    }
    while set.len() > cap {
        let Some(first) = set.iter().next().cloned() else {
            break;
        };
        set.remove(&first);
    }
    true
}
