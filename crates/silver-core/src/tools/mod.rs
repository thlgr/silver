//! Concrete core tools.

pub mod bash;
pub mod command;
pub mod delegate;
pub mod documents;
pub mod execute_code;
pub mod fs;
pub mod lsp;
pub mod patch;
pub mod process;
pub mod replace;
pub mod session_search;
pub mod skills;
pub mod team;
pub mod todo;
pub mod vision;
pub mod web;
pub mod write;

use crate::tool::ToolRegistry;

/// Register every core tool except delegation, which the daemon registers with its runner, and
/// the team tools, which belong to the chat.
pub fn register_default_tools(registry: &mut ToolRegistry) {
    fs::register(registry);
    vision::register(registry);
    write::register(registry);
    command::register(registry);
    execute_code::register(registry);
    bash::register(registry);
    process::register(registry);
    session_search::register(registry);
    documents::register(registry);
    todo::register(registry);
    skills::register(registry);
    lsp::register(registry);
    web::register(registry);
}
