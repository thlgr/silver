//! Named tool and skill selections a session runs with.

use std::sync::Arc;

use silver_core::services::{SkillDoc, SkillSummary, SkillsBackend};
use silver_core::CoreResult;
use silver_protocol::{Preset, SkillFilter};

/// The built-in preset that follows `[tools]` in config.toml.
pub const MINIMAL: &str = "minimal";
/// The built-in preset with only the `bash` shell.
pub const PI: &str = "pi";

/// The built-in presets: Minimal (today's roster) then Pi.
pub fn builtins(minimal_tools: Vec<String>) -> Vec<Preset> {
    vec![
        Preset {
            id: MINIMAL.to_string(),
            name: "Minimal".to_string(),
            tools: minimal_tools,
            skills: SkillFilter::Except(Vec::new()),
            builtin: true,
        },
        Preset {
            id: PI.to_string(),
            name: "Pi".to_string(),
            tools: vec!["bash".to_string()],
            skills: SkillFilter::Except(Vec::new()),
            builtin: true,
        },
    ]
}

/// A skills backend narrowed by a preset's skill filter.
pub struct PresetSkills {
    pub inner: Arc<dyn SkillsBackend>,
    pub filter: SkillFilter,
}

#[async_trait::async_trait]
impl SkillsBackend for PresetSkills {
    async fn list(&self) -> CoreResult<Vec<SkillSummary>> {
        Ok(self
            .inner
            .list()
            .await?
            .into_iter()
            .filter(|skill| self.filter.allows(&skill.name))
            .collect())
    }

    async fn list_for_platform(
        &self,
        platform: Option<&str>,
        project_dir: Option<&std::path::Path>,
    ) -> CoreResult<Vec<SkillSummary>> {
        Ok(self
            .inner
            .list_for_platform(platform, project_dir)
            .await?
            .into_iter()
            .filter(|skill| self.filter.allows(&skill.name))
            .collect())
    }

    async fn view(
        &self,
        name: &str,
        project_dir: Option<&std::path::Path>,
    ) -> CoreResult<Option<SkillDoc>> {
        if !self.filter.allows(name) {
            return Ok(None);
        }
        let doc = self.inner.view(name, project_dir).await?;
        if doc
            .as_ref()
            .is_some_and(|doc| !self.filter.allows(&doc.summary.name))
        {
            return Ok(None);
        }
        Ok(doc)
    }

    async fn manage(
        &self,
        action: &str,
        name: &str,
        content: Option<&str>,
        project_dir: Option<&std::path::Path>,
    ) -> CoreResult<String> {
        self.inner.manage(action, name, content, project_dir).await
    }
}
