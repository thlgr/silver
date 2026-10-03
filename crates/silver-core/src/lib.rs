//! silver-core: the agent loop, tools, memory and detections. Providers are transport only, and
//! the core must not depend on Axum, SQLite or adapter details.

pub mod advisor;
pub mod agent;
pub mod context;
pub mod error;
pub mod event;
pub mod lsp;
pub mod memory;
pub mod model;
pub mod model_metadata;
pub mod plan;
pub mod pricing;
pub mod prompt;
pub mod redact;
pub mod safety;
mod sandbox;
pub mod services;
pub mod session;
pub mod subagent;
pub mod tool;
pub mod tools;
pub mod toolset;
pub mod workspace;

pub mod guard;

pub use error::{CoreError, CoreResult};
