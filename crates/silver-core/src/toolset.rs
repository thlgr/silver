//! Toolsets: named capability groups that scope a run's tools.

use std::collections::BTreeSet;

/// General-purpose tools that do not belong to a more specific group.
pub const CORE: &str = "core";
/// Workspace file tools (read_file, write_file, patch, ...).
pub const FILES: &str = "files";
/// Shell, process and code-execution tools.
pub const TERMINAL: &str = "terminal";
/// Web search and extraction tools.
pub const WEB: &str = "web";
/// Persistent memory and session search.
pub const MEMORY: &str = "memory";
/// Skill discovery and management.
pub const SKILLS: &str = "skills";
/// Delegating work to a subagent.
pub const DELEGATION: &str = "delegation";
/// Tools contributed by dynamically connected MCP servers.
pub const MCP: &str = "mcp";

/// Every built-in toolset in a stable order. An empty enabled list selects all
/// of these, and this is the usual defaults argument.
pub const BUILTIN_TOOLSETS: [&str; 8] =
    [CORE, FILES, TERMINAL, WEB, MEMORY, SKILLS, DELEGATION, MCP];

/// The built-in toolset owning a tool name, CORE for unknown names; the default Tool::toolset.
pub fn builtin_toolset_for(tool_name: &str) -> &'static str {
    match tool_name {
        "read_file" | "list_files" | "search_files" | "write_file" | "patch" | "view_image" => {
            FILES
        }
        "run_command" | "execute_code" | "bash" | "process_manage" => TERMINAL,
        "web_search" | "web_extract" => WEB,
        "memory" | "session_search" | "search_documents" => MEMORY,
        "skills_list" | "skill_view" | "skill_manage" => SKILLS,
        "delegate_task" => DELEGATION,
        name if name == MCP || name.starts_with("mcp_") || name.starts_with("mcp-") => MCP,
        _ => CORE,
    }
}

/// The built-in toolset names as owned strings, for use as a defaults argument.
pub fn builtin_toolsets() -> Vec<String> {
    BUILTIN_TOOLSETS
        .iter()
        .map(|name| name.to_string())
        .collect()
}

/// The toolsets selected for one run: all, or an explicit set from resolve_toolsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolsetSelection {
    unrestricted: bool,
    enabled: BTreeSet<String>,
    disabled: BTreeSet<String>,
}

impl Default for ToolsetSelection {
    /// An unrestricted selection: every toolset is allowed.
    fn default() -> Self {
        Self::all()
    }
}

impl ToolsetSelection {
    /// A selection that allows every toolset, used when the caller has no
    /// per-run restriction.
    pub fn all() -> Self {
        Self {
            unrestricted: true,
            enabled: BTreeSet::new(),
            disabled: BTreeSet::new(),
        }
    }

    /// True when toolset is visible to the model. A disabled name is never
    /// allowed, even when it is also enabled.
    pub fn allows(&self, toolset: &str) -> bool {
        if self.disabled.contains(toolset) {
            return false;
        }
        self.unrestricted || self.enabled.contains(toolset)
    }

    /// True for the unrestricted ToolsetSelection::all selection.
    pub fn is_unrestricted(&self) -> bool {
        self.unrestricted
    }

    /// The explicitly enabled toolsets (empty for an unrestricted selection).
    pub fn enabled(&self) -> &BTreeSet<String> {
        &self.enabled
    }

    /// The toolsets removed from the selection.
    pub fn disabled(&self) -> &BTreeSet<String> {
        &self.disabled
    }
}

/// A run's toolset selection: `enabled` (empty means all of `defaults`) minus `disabled`, which
/// always wins. Every name must be in `defaults`, normally builtin_toolsets plus MCP servers.
pub fn resolve_toolsets(
    enabled: &[String],
    disabled: &[String],
    defaults: &[String],
) -> Result<ToolsetSelection, String> {
    let known: BTreeSet<&str> = defaults.iter().map(String::as_str).collect();
    for name in enabled.iter().chain(disabled.iter()) {
        if !known.contains(name.as_str()) {
            return Err(format!("unknown toolset: {name}"));
        }
    }
    let enabled: BTreeSet<String> = if enabled.is_empty() {
        defaults.iter().cloned().collect()
    } else {
        enabled.iter().cloned().collect()
    };
    Ok(ToolsetSelection {
        unrestricted: false,
        enabled,
        disabled: disabled.iter().cloned().collect(),
    })
}
