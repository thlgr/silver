//! silver library surface, re-exported for integration tests and embedding.
//!
//! The binary in main.rs is a thin wrapper over these modules.

pub mod acp;
pub mod advisor;
pub mod agent_modes;
pub mod agui;
pub mod ai_memory;
pub mod anthropic;
pub mod api;
pub mod approval_memory;
pub mod atomic_file;
pub mod auth;
pub mod bedrock;
pub mod chat;
pub mod checkpoints;
pub mod codex;
pub mod config;
pub mod context_length;
pub mod copilot;
pub mod credential_pool;
pub mod db;
pub mod document_index;
pub mod fallback;
pub mod git_extras;
pub mod logging;
pub mod mcp;
pub mod moa;
pub mod monitoring;
pub mod oauth;
pub mod opencode;
pub mod presets;
pub mod profile;
pub mod provider;
pub mod routed;
pub mod run_manager;
pub mod session_search;
pub mod skills;
pub mod subagents;
pub mod terminal;
pub mod todo_store;
pub mod url_safety;
pub mod vertex;
pub mod web;
