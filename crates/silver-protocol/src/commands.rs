//! The slash-command catalog, served at `GET /v1/commands`; the web UI renders menus and /help
//! from it.

use serde::Serialize;

/// One slash command a client offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CommandInfo {
    /// Canonical name without the leading slash.
    pub name: &'static str,
    /// Alternative names, without the leading slash.
    pub aliases: &'static [&'static str],
    /// One-line description for /help.
    pub summary: &'static str,
    /// Argument placeholder appended to the name, or empty.
    pub usage: &'static str,
    /// /help group heading.
    pub category: &'static str,
}

/// Every slash command, in /help order.
pub const COMMANDS: &[CommandInfo] = &[
    CommandInfo {
        name: "help",
        aliases: &["h"],
        summary: "show this command list",
        usage: "",
        category: "General",
    },
    CommandInfo {
        name: "copy",
        aliases: &[],
        summary: "copy the last reply to the clipboard",
        usage: "",
        category: "General",
    },
    CommandInfo {
        name: "clear",
        aliases: &[],
        summary: "clear the transcript from this view until reload",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "status",
        aliases: &[],
        summary: "show local model, session, token and run status",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "sessions",
        aliases: &[],
        summary: "search your sessions by title or content",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "usage",
        aliases: &[],
        summary: "show the current session's token usage",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "context",
        aliases: &[],
        summary: "show the context window and its usage breakdown",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "title",
        aliases: &[],
        summary: "show or set the current session title",
        usage: "[text]",
        category: "Session",
    },
    CommandInfo {
        name: "model",
        aliases: &[],
        summary: "open the model menu, name a model, or switch provider",
        usage: "[model|provider:model|--provider <id> [model]]",
        category: "Session",
    },
    CommandInfo {
        name: "preset",
        aliases: &[],
        summary: "open the preset menu, or switch this chat to a preset by name",
        usage: "[name]",
        category: "Session",
    },
    CommandInfo {
        name: "agents",
        aliases: &[],
        summary: "list the subagents you can delegate to, and edit the custom ones",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "plan",
        aliases: &[],
        summary: "plan first: explore read-only, write a plan, change nothing until you approve it",
        usage: "[off|<description>]",
        category: "Session",
    },
    CommandInfo {
        name: "yolo",
        aliases: &[],
        summary: "bypass approval prompts for this session",
        usage: "[on|off]",
        category: "Session",
    },
    CommandInfo {
        name: "approvals",
        aliases: &[],
        summary: "show or set the global daemon approval mode (Shift+Tab cycles it)",
        usage: "[manual|smart|off]",
        category: "Session",
    },
    CommandInfo {
        name: "export",
        aliases: &[],
        summary: "download the transcript as a Markdown file",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "retry",
        aliases: &[],
        summary: "re-submit the last user prompt",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "steer",
        aliases: &[],
        summary: "inject a message into the active run",
        usage: "<prompt>",
        category: "Session",
    },
    CommandInfo {
        name: "stop",
        aliases: &[],
        summary: "stop the active run",
        usage: "",
        category: "Session",
    },
    CommandInfo {
        name: "goal",
        aliases: &[],
        summary: "set and drive a standing objective with continuation prompts",
        usage: "[text|status|pause|resume|clear|budget N]",
        category: "Session",
    },
    CommandInfo {
        name: "loop",
        aliases: &["proactive"],
        summary: "re-fire a prompt after each turn or on a timer",
        usage: "[interval] <prompt> [--times N] | status|pause|resume|stop",
        category: "Session",
    },
    CommandInfo {
        name: "heartbeat",
        aliases: &["hb"],
        summary: "recurring prompt that fires only into an idle session",
        usage: "every <interval> <prompt> | status|pause|resume|clear",
        category: "Session",
    },
    CommandInfo {
        name: "focus",
        aliases: &[],
        summary: "hide tool lines; show only prompts and final replies",
        usage: "[on|off]",
        category: "Display",
    },
    CommandInfo {
        name: "verbose",
        aliases: &[],
        summary: "tool-progress verbosity",
        usage: "[off|new|all]",
        category: "Display",
    },
    CommandInfo {
        name: "theme",
        aliases: &[],
        summary: "switch the app's color theme",
        usage: "[dark|light|system]",
        category: "Display",
    },
    CommandInfo {
        name: "quit",
        aliases: &["exit"],
        summary: "exit the TUI",
        usage: "",
        category: "Exit",
    },
    CommandInfo {
        name: "compress",
        aliases: &[],
        summary: "compress the conversation context",
        usage: "",
        category: "Daemon (planned)",
    },
    CommandInfo {
        name: "undo",
        aliases: &[],
        summary: "remove the newest user turn(s) from the session history",
        usage: "[N]",
        category: "Session",
    },
    CommandInfo {
        name: "rollback",
        aliases: &["checkpoints"],
        summary: "list or restore a filesystem checkpoint",
        usage: "[N]",
        category: "Session",
    },
    CommandInfo {
        name: "diff",
        aliases: &[],
        summary: "show the working-tree diff",
        usage: "[staged|all|session]",
        category: "Session",
    },
    CommandInfo {
        name: "login",
        aliases: &["auth"],
        summary: "open the provider menu, or sign in to one by name",
        usage: "[provider|use <provider>|list]",
        category: "Session",
    },
    CommandInfo {
        name: "logout",
        aliases: &[],
        summary: "forget a provider's stored key and OAuth sign-in",
        usage: "<provider>",
        category: "Session",
    },
    CommandInfo {
        name: "worktree",
        aliases: &["wt"],
        summary: "list, create or remove git worktrees",
        usage: "[list|new [name]|remove <name>]",
        category: "Session",
    },
];

/// Look a command up by canonical name at compile time; an unknown name fails the build.
pub const fn command(name: &str) -> &'static CommandInfo {
    let mut i = 0;
    while i < COMMANDS.len() {
        if str_eq(COMMANDS[i].name, name) {
            return &COMMANDS[i];
        }
        i += 1;
    }
    panic!("unknown slash command")
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}
