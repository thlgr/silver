//! The view_image tool: hand the model a picture the workspace holds.

use crate::context::RunContext;
use crate::error::CoreResult;
use crate::safety;
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Largest picture the model is shown. A megabyte of pixels becomes 1.3 MB of base64 in the
/// next request, and providers refuse a single image past 5 MB.
const MAX_IMAGE_BYTES: u64 = 5_242_880;

/// Accepted extensions and the media type each one is sent as. Any other extension is refused
/// before the request, so the model gets a tool error it can act on rather than a provider 400.
const IMAGE_TYPES: [(&str, &str); 5] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// Register the view_image tool.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(ViewImage));
}

/// The view_image tool.
pub struct ViewImage;

#[async_trait::async_trait]
impl Tool for ViewImage {
    fn name(&self) -> &'static str {
        "view_image"
    }

    fn description(&self) -> &'static str {
        "Look at a picture in the workspace (png, jpg, gif, webp). Use question to say what to look for."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Image path, relative to the workspace root or absolute inside it."
                },
                "question": {
                    "type": "string",
                    "description": "What to look for in the picture, e.g. \"which button is focused?\"."
                }
            },
            "required": ["path", "question"],
            "additionalProperties": false
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        super::fs::off_runtime(ctx, args, view_image).await
    }
}

fn view_image(
    run: &RunContext,
    _cancel: &CancellationToken,
    args: &Value,
) -> CoreResult<ToolOutcome> {
    let requested = super::fs::required_str(args, "path")?;
    let question = super::fs::required_str(args, "question")?;
    // The NT/device-namespace guard runs on the raw string before any resolution.
    if let Some(reason) = safety::nt_namespace_error(Path::new(&requested)) {
        return Ok(super::fs::read_denied(Path::new(&requested), reason));
    }
    let resolved = run.resolve_path(&requested)?;
    if let Some(reason) = safety::read_denied_reason(&resolved) {
        return Ok(super::fs::read_denied(&resolved, reason));
    }
    let Some(media_type) = media_type(&resolved) else {
        return Ok(ToolOutcome::error(format!(
            "{requested} is not a picture; view_image reads png, jpg, gif and webp"
        )));
    };
    let bytes = match std::fs::read(&resolved) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(ToolOutcome::error(format!(
                "cannot read {}: {error}",
                resolved.display()
            )))
        }
    };
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Ok(ToolOutcome::error(format!(
            "{requested} is {} MB; view_image reads pictures up to {} MB",
            bytes.len() / 1_048_576,
            MAX_IMAGE_BYTES / 1_048_576
        )));
    }
    let root = &run.require_workspace()?.canonical_root;
    let receipt = format!(
        "loaded {} ({media_type}, {}) — question: {question}",
        super::fs::relative_to(root, &resolved),
        human_bytes(bytes.len())
    );
    Ok(ToolOutcome::ok(receipt).with_image(media_type, bytes))
}

/// The media type a path's extension maps to, or None when it is not a picture.
fn media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_TYPES
        .iter()
        .find(|(name, _)| *name == extension)
        .map(|(_, media_type)| *media_type)
}

/// A byte count as a small model reads it: 812 B, 84 KB, 1.4 MB.
fn human_bytes(len: usize) -> String {
    const KB: f64 = 1024.0;
    if len < 1024 {
        return format!("{len} B");
    }
    let value = len as f64 / KB;
    if value < 1000.0 {
        return format!("{value:.0} KB");
    }
    format!("{:.1} MB", value / KB)
}
