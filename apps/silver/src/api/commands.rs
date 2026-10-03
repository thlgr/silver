//! Slash-command catalog, so every client renders the same menu and /help.

use axum::Json;
use silver_protocol::commands::{CommandInfo, COMMANDS};

/// `GET /v1/commands`: every slash command in /help order.
pub async fn list() -> Json<&'static [CommandInfo]> {
    Json(COMMANDS)
}
