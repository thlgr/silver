//! The skills tools: list, view and manage skills through the run's SkillsBackend.

use crate::error::CoreResult;
use crate::services::{SkillDoc, SkillSummary, SkillUsage};
use crate::tool::{Tool, ToolContext, ToolOutcome, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use silver_protocol::RiskLevel;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Register the three skills tools.
pub fn register(registry: &mut ToolRegistry) {
    registry.register(Arc::new(SkillsListTool));
    registry.register(Arc::new(SkillViewTool));
    registry.register(Arc::new(SkillManageTool));
}

fn unavailable() -> ToolOutcome {
    ToolOutcome::error("skills are unavailable")
}

fn summary_json(summary: &SkillSummary) -> Value {
    json!({
        "name": summary.name,
        "description": summary.description,
        "category": summary.category,
    })
}

/// Tier 1 listing: name + description (and category) only.
struct SkillsListTool;

#[async_trait]
impl Tool for SkillsListTool {
    fn name(&self) -> &'static str {
        "skills_list"
    }

    fn description(&self) -> &'static str {
        "List available skills (name + description). Use skill_view(name) to load full content."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "description": "Optional category filter to narrow results"
                }
            },
            "required": []
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let Some(backend) = ctx.run.services.skills.as_ref() else {
            return Ok(unavailable());
        };
        // An empty platform means the run context could not identify one, in
        // which case the backend must not gate on platform.
        let platform = ctx.run.platform.trim();
        let platform = if platform.is_empty() {
            None
        } else {
            Some(platform)
        };
        let skills = match backend.list_for_platform(platform, ctx.run.cwd()).await {
            Ok(skills) => skills,
            Err(err) => return Ok(ToolOutcome::error(err.to_string())),
        };
        if skills.is_empty() {
            return Ok(ToolOutcome::ok(
                json!({
                    "success": true,
                    "skills": [],
                    "categories": [],
                    "message": "No skills found in skills/ directory."
                })
                .to_string(),
            ));
        }

        let category = args.get("category").and_then(Value::as_str);
        let mut filtered: Vec<&SkillSummary> = skills
            .iter()
            .filter(|skill| category.is_none_or(|want| skill.category.as_deref() == Some(want)))
            .collect();
        filtered.sort_by(|a, b| {
            a.category
                .as_deref()
                .unwrap_or("")
                .cmp(b.category.as_deref().unwrap_or(""))
                .then_with(|| a.name.cmp(&b.name))
        });

        let categories: Vec<&str> = filtered
            .iter()
            .filter_map(|skill| skill.category.as_deref())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let listed: Vec<Value> = filtered.iter().map(|skill| summary_json(skill)).collect();
        let payload = json!({
            "success": true,
            "skills": listed,
            "categories": categories,
            "count": listed.len(),
            "hint": "Use skill_view(name) to see full content, tags, and linked files"
        });
        Ok(ToolOutcome::ok(payload.to_string()))
    }
}

/// Load a skill's full document.
struct SkillViewTool;

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &'static str {
        "skill_view"
    }

    fn description(&self) -> &'static str {
        "Skills allow for loading information about specific tasks and workflows, as well as scripts and templates. Load a skill's full content. Pass file_path to read a linked file within the skill."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "A skill name from <available_skills>."
                },
                "file_path": {
                    "type": "string",
                    "description": "OPTIONAL: Path to a linked file within the skill (e.g., 'references/api.md', 'templates/config.yaml', 'scripts/validate.py'). Omit to get the main skill content."
                }
            },
            "required": ["name"]
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Read
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let Some(backend) = ctx.run.services.skills.as_ref() else {
            return Ok(unavailable());
        };
        let name = match args.get("name").and_then(Value::as_str) {
            Some(name) if !name.trim().is_empty() => name,
            _ => return Ok(ToolOutcome::error("skill_view requires a 'name'")),
        };
        if let Some(file_path) = args.get("file_path").and_then(Value::as_str) {
            if !file_path.trim().is_empty() {
                return Ok(ToolOutcome::error(
                    "skill_view does not support linked files; skills are single Markdown documents",
                ));
            }
        }
        match backend.view(name, ctx.run.cwd()).await {
            Ok(Some(doc)) => Ok(ToolOutcome::ok(view_json(&doc, name).to_string())),
            Ok(None) => Ok(ToolOutcome::error(format!(
                "Skill '{name}' not found. Use a name from <available_skills> in the system prompt, or continue without a skill."
            ))),
            Err(err) => Ok(ToolOutcome::error(err.to_string())),
        }
    }
}

fn view_json(doc: &SkillDoc, requested: &str) -> Value {
    json!({
        "success": true,
        "name": if doc.summary.name.is_empty() { requested } else { doc.summary.name.as_str() },
        "description": doc.summary.description,
        "category": doc.summary.category,
        "path": doc.summary.path,
        "content": doc.content,
        "usage": usage_json(&doc.usage),
    })
}

fn usage_json(usage: &SkillUsage) -> Value {
    json!({
        "views": usage.views,
        "last_viewed_at": usage.last_viewed_at,
        "patches": usage.patches,
        "last_patched_at": usage.last_patched_at,
        "created_by": usage.created_by,
    })
}

/// Create, update, delete or install a skill.
struct SkillManageTool;

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &'static str {
        "skill_manage"
    }

    fn description(&self) -> &'static str {
        "Create, update, delete or install skills — your procedural memory for recurring task types. A skill is a Markdown document with YAML frontmatter (name, description, category) and an instruction body. 'create' writes a new skill, 'update' replaces the full content of an existing one, 'delete' removes it, and 'install' stores a supplied document. Names are lowercase and may use hyphens, dots and underscores."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "delete", "install"],
                    "description": "The action to perform: 'create' a new skill, 'update' an existing skill's full content, 'delete' it, or 'install' a supplied document."
                },
                "name": {
                    "type": "string",
                    "description": "Skill name (lowercase; letters, numbers, hyphens, dots and underscores; max 64 chars)."
                },
                "content": {
                    "type": "string",
                    "description": "Full skill text (YAML frontmatter with name and description plus a markdown body). Required for create, update and install."
                }
            },
            "required": ["action", "name"]
        })
    }

    fn risk(&self, _args: &Value) -> RiskLevel {
        RiskLevel::Write
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    async fn execute(&self, ctx: &ToolContext<'_>, args: Value) -> CoreResult<ToolOutcome> {
        let Some(backend) = ctx.run.services.skills.as_ref() else {
            return Ok(unavailable());
        };
        let action = match args.get("action").and_then(Value::as_str) {
            Some(action @ ("create" | "update" | "delete" | "install")) => action,
            Some(other) => {
                return Ok(ToolOutcome::error(format!(
                    "unknown skill action '{other}'; use create, update, delete or install"
                )))
            }
            None => {
                return Ok(ToolOutcome::error(
                    "skill_manage requires an 'action' of create, update, delete or install",
                ))
            }
        };
        let name = match args.get("name").and_then(Value::as_str) {
            Some(name) if !name.trim().is_empty() => name,
            _ => return Ok(ToolOutcome::error("skill_manage requires a 'name'")),
        };
        let content = args.get("content").and_then(Value::as_str);
        match backend.manage(action, name, content, ctx.run.cwd()).await {
            Ok(result) => Ok(ToolOutcome::ok(
                json!({
                    "success": true,
                    "action": action,
                    "name": name,
                    "message": result,
                })
                .to_string(),
            )),
            Err(err) => Ok(ToolOutcome::error(err.to_string())),
        }
    }
}
