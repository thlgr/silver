//! Filesystem skills: Markdown with YAML frontmatter, in the global `~/.agents/skills` overlaid by
//! the workspace's `.agents/skills`; on a name clash the most recently modified file wins. Every
//! name is confined to its skills directory.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;
use silver_core::services::{SkillDoc, SkillSummary, SkillUsage, SkillsBackend};
use silver_core::{CoreError, CoreResult};

/// Maximum accepted length of one skill name segment.
const MAX_NAME_LENGTH: usize = 64;

/// The main document of an Anthropic-style skill directory (`<name>/SKILL.md`).
const SKILL_FILE: &str = "SKILL.md";

/// Provenance marker written for skills created through `skill_manage`.
const CREATED_BY_AGENT: &str = "agent";

/// Concrete, filesystem-backed skills store.
pub struct SkillsStore {
    /// The global skills root, read for every run and overlaid by a workspace's own
    /// `.agents/skills`. Defaults to `~/.agents/skills`.
    global_root: PathBuf,
    /// Directory for the sidecar usage ledger (daemon-internal, not a skills root).
    data_dir: PathBuf,
    /// Serializes sidecar ledger read-modify-write cycles inside the process.
    usage_lock: Mutex<()>,
}

impl SkillsStore {
    /// Create a store whose global skills root is `<dir>/skills` and whose ledger lives under
    /// `<dir>`. Used by tests and any caller that wants both under one directory.
    pub fn new(dir: PathBuf) -> Self {
        let global_root = dir.join("skills");
        Self {
            global_root,
            data_dir: dir,
            usage_lock: Mutex::new(()),
        }
    }

    /// Create a store with an explicit global skills root (e.g. `~/.agents/skills`) and a separate
    /// data directory for the usage ledger.
    pub fn with_global_root(global_root: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            global_root,
            data_dir,
            usage_lock: Mutex::new(()),
        }
    }

    /// The daemon data directory holding the usage ledger.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The global skills root (overlaid by a workspace's `.agents/skills` per run).
    pub fn skills_dir(&self) -> PathBuf {
        PathBuf::clone(&self.global_root)
    }

    /// Sidecar usage ledger, kept next to (not inside) the skills root.
    pub fn usage_path(&self) -> PathBuf {
        self.data_dir.join("usage.json")
    }

    /// Best-effort ledger read: a missing or corrupt file behaves as empty.
    fn load_usage(&self) -> BTreeMap<String, SkillUsage> {
        let Ok(text) = std::fs::read_to_string(self.usage_path()) else {
            return BTreeMap::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Atomically persist the ledger via a temp file plus rename.
    fn save_usage(&self, usage: &BTreeMap<String, SkillUsage>) -> CoreResult<()> {
        let text = serde_json::to_string_pretty(usage).map_err(|err| {
            CoreError::Internal(format!("failed to serialize usage ledger: {err}"))
        })?;
        write_atomic(&self.usage_path(), &format!("{text}\n"))
    }

    /// Load, mutate and save the ledger under the in-process lock.
    fn update_usage(
        &self,
        name: &str,
        mutate: impl FnOnce(&mut SkillUsage),
    ) -> CoreResult<SkillUsage> {
        let _guard = self
            .usage_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mut usage = self.load_usage();
        let record = usage.entry(name.to_string()).or_default();
        mutate(record);
        let updated = SkillUsage::clone(record);
        self.save_usage(&usage)?;
        Ok(updated)
    }

    /// Record one `skill_view` load and return the refreshed counters.
    fn record_view(&self, name: &str) -> CoreResult<SkillUsage> {
        self.update_usage(name, |record| {
            record.views = record.views.saturating_add(1);
            record.last_viewed_at = Some(now_rfc3339());
        })
    }

    /// Record agent provenance for a skill written by `skill_manage`.
    fn record_created(&self, name: &str) -> CoreResult<SkillUsage> {
        self.update_usage(name, |record| {
            record.created_by = Some(CREATED_BY_AGENT.to_string());
        })
    }

    /// Record one `skill_manage` patch and return the refreshed counters.
    fn record_patch(&self, name: &str) -> CoreResult<SkillUsage> {
        self.update_usage(name, |record| {
            record.patches = record.patches.saturating_add(1);
            record.last_patched_at = Some(now_rfc3339());
        })
    }

    /// Drop a deleted skill's record so a later skill of the same name starts clean.
    fn forget_usage(&self, name: &str) -> CoreResult<()> {
        let _guard = self
            .usage_lock
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let mut usage = self.load_usage();
        if usage.remove(name).is_some() {
            self.save_usage(&usage)?;
        }
        Ok(())
    }

    /// Best-effort ledger note for a newly created or installed skill. Telemetry
    /// failures are logged and never fail the tool call.
    fn note_created(&self, content: &str, requested: &str) {
        let key = ledger_name(content, requested);
        if let Err(err) = self.record_created(&key) {
            tracing::warn!("failed to record skill creation for '{key}': {err}");
        }
    }

    /// Best-effort ledger note for a patched skill.
    fn note_patch(&self, content: &str, requested: &str) {
        let key = ledger_name(content, requested);
        if let Err(err) = self.record_patch(&key) {
            tracing::warn!("failed to record skill patch for '{key}': {err}");
        }
    }

    /// The skill roots read for a run: the workspace's `.agents/skills` (when a project root is
    /// given) and the global `~/.agents/skills`. Order does not imply precedence — a name found in
    /// more than one root resolves to the most recently modified file.
    fn roots(&self, project: Option<&Path>) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(project) = project {
            roots.push(project_skills_dir(project));
        }
        roots.push(self.skills_dir());
        roots
    }

    /// Resolve a skill name to its '<root>/<name>.md' path, rejecting any name
    /// that could escape the root.
    fn skill_path(&self, root: &Path, name: &str) -> CoreResult<PathBuf> {
        let relative = relative_name(name)?;
        let path = root.join(format!("{relative}.md"));
        // Defence in depth: the component check above already refuses traversal,
        // but never hand back a path outside the root even if it evolves.
        if !path.starts_with(root) {
            return Err(CoreError::InvalidRequest(format!(
                "invalid skill name '{name}': path traversal is not allowed"
            )));
        }
        Ok(path)
    }

    /// The (root, path) of the most recently modified match for `name` across all roots: a
    /// `<root>/<name>.md` file or, for a bare name, a categorised file whose frontmatter name
    /// matches.
    fn resolve_newest(
        &self,
        project: Option<&Path>,
        name: &str,
    ) -> CoreResult<Option<(PathBuf, PathBuf)>> {
        let mut best: Option<(SystemTime, PathBuf, PathBuf)> = None;
        for root in self.roots(project) {
            let direct = self.skill_path(&root, name)?;
            let candidate = if direct.is_file() {
                Some(direct)
            } else if !name.contains('/') && root.is_dir() {
                let mut scanned = Vec::new();
                self.collect(&root, &root, &mut scanned)?;
                scanned
                    .into_iter()
                    .find(|skill| skill.summary.name == name)
                    .map(|skill| root.join(&skill.summary.path))
                    .filter(|path| path.is_file())
            } else {
                None
            };
            if let Some(path) = candidate {
                let modified = modified_time(&path);
                if best.as_ref().is_none_or(|(seen, _, _)| modified > *seen) {
                    best = Some((modified, root, path));
                }
            }
        }
        Ok(best.map(|(_, root, path)| (root, path)))
    }

    /// The path of the newest copy of `name` across roots, if any.
    fn existing_path(&self, project: Option<&Path>, name: &str) -> CoreResult<Option<PathBuf>> {
        Ok(self.resolve_newest(project, name)?.map(|(_, path)| path))
    }

    fn relative_path(&self, root: &Path, path: &Path) -> String {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn category_for(&self, root: &Path, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(root).ok()?;
        let mut components = relative.components();
        let first = components.next()?;
        // A file directly under the skills dir has no category directory.
        components.next()?;
        match first {
            Component::Normal(segment) => segment.to_str().map(str::to_string),
            _ => None,
        }
    }

    fn build_summary(
        &self,
        root: &Path,
        path: &Path,
        frontmatter: &mut Frontmatter,
        body: &str,
    ) -> SkillSummary {
        // For an Anthropic `<name>/SKILL.md` skill the identity is the directory, not the file:
        // the fallback name is the parent directory and the path components are not categories.
        let is_skill_md = path.file_name().and_then(|name| name.to_str()) == Some(SKILL_FILE);
        let stem = if is_skill_md {
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string()
        } else {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string()
        };
        let name = frontmatter
            .name
            .take()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(stem);
        let description = frontmatter
            .description
            .take()
            .filter(|description| !description.trim().is_empty())
            .or_else(|| first_body_line(body))
            .unwrap_or_default();
        let category = match frontmatter.category.take() {
            None if !is_skill_md => self.category_for(root, path),
            category => category,
        };
        SkillSummary {
            name,
            description,
            category,
            path: self.relative_path(root, path),
        }
    }

    /// Read one Markdown file into a scannable summary plus its frontmatter gates.
    /// Parsing never fails, so malformed frontmatter is reported as a diagnostic
    /// and the skill is still listed.
    fn load_scanned(&self, root: &Path, path: &Path) -> CoreResult<ScannedSkill> {
        let content = std::fs::read_to_string(path)
            .map_err(|err| io_error("failed to read skill", path, &err))?;
        let mut split = split_frontmatter(&content);
        let diagnostics = std::mem::take(&mut split.diagnostics);
        let (mut frontmatter, body) = split.into_parts();
        let summary = self.build_summary(root, path, &mut frontmatter, &body);
        Ok(ScannedSkill {
            summary,
            platforms: frontmatter.platforms,
            required_env: frontmatter.env,
            disabled: frontmatter.disabled,
            diagnostics,
            modified: modified_time(path),
        })
    }

    fn load_doc(&self, root: &Path, path: &Path) -> CoreResult<SkillDoc> {
        let content = std::fs::read_to_string(path)
            .map_err(|err| io_error("failed to read skill", path, &err))?;
        let (mut frontmatter, body) = split_frontmatter(&content).into_parts();
        Ok(SkillDoc {
            summary: self.build_summary(root, path, &mut frontmatter, &body),
            content,
            usage: SkillUsage::default(),
        })
    }

    /// The skills tree minus entries hidden by frontmatter gating; with no known `platform`,
    /// platform gating is skipped. Hidden skills stay reachable through `view`.
    fn listed(
        &self,
        platform: Option<&str>,
        project: Option<&Path>,
    ) -> CoreResult<Vec<SkillSummary>> {
        // Merge every root, keeping one entry per skill name: when the same name appears in
        // more than one root, the most recently modified file wins.
        let mut winners: Vec<(SystemTime, SkillSummary)> = Vec::new();
        for root in self.roots(project) {
            if !root.is_dir() {
                continue;
            }
            let mut scanned = Vec::new();
            self.collect(&root, &root, &mut scanned)?;
            for entry in scanned {
                for diagnostic in &entry.diagnostics {
                    tracing::warn!(skill = %entry.summary.name, "skill frontmatter: {diagnostic}");
                }
                if entry.disabled {
                    continue;
                }
                if let Some(platform) = platform {
                    if !entry.platforms.is_empty() && !platform_matches(&entry.platforms, platform)
                    {
                        continue;
                    }
                }
                if !entry.required_env.is_empty()
                    && !entry.required_env.iter().any(|key| env_present(key))
                {
                    continue;
                }
                match winners
                    .iter_mut()
                    .find(|(_, summary)| summary.name == entry.summary.name)
                {
                    Some((seen, _)) if *seen >= entry.modified => {}
                    Some(winner) => *winner = (entry.modified, entry.summary),
                    None => winners.push((entry.modified, entry.summary)),
                }
            }
        }
        let mut skills: Vec<SkillSummary> =
            winners.into_iter().map(|(_, summary)| summary).collect();
        skills.sort_by(|a, b| {
            a.category
                .as_deref()
                .unwrap_or("")
                .cmp(b.category.as_deref().unwrap_or(""))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(skills)
    }

    fn collect(&self, root: &Path, dir: &Path, out: &mut Vec<ScannedSkill>) -> CoreResult<()> {
        // Anthropic layout: a directory holding SKILL.md *is* a skill, and its other files
        // (references, scripts, agents) are that skill's resources. Emit the one skill and do
        // not descend, so resource `.md` files are never mistaken for skills of their own.
        let skill_md = dir.join(SKILL_FILE);
        if skill_md.is_file() {
            out.push(self.load_scanned(root, &skill_md)?);
            return Ok(());
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(io_error("failed to read skills directory", dir, &err)),
        };
        for entry in entries {
            let entry = entry.map_err(|err| io_error("failed to read skills entry", dir, &err))?;
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.starts_with('.') {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|err| io_error("failed to stat skill entry", &path, &err))?;
            if file_type.is_dir() {
                // A category directory (or a nesting level above skill dirs): recurse.
                self.collect(root, &path, out)?;
            } else if file_type.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("md")
            {
                // Flat silver skill file `<name>.md`.
                out.push(self.load_scanned(root, &path)?);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SkillsBackend for SkillsStore {
    async fn list(&self) -> CoreResult<Vec<SkillSummary>> {
        self.listed(None, None)
    }

    async fn list_for_platform(
        &self,
        platform: Option<&str>,
        project_dir: Option<&Path>,
    ) -> CoreResult<Vec<SkillSummary>> {
        self.listed(platform, project_dir)
    }

    async fn view(&self, name: &str, project_dir: Option<&Path>) -> CoreResult<Option<SkillDoc>> {
        // When the same name lives in more than one root, load the most recently edited copy.
        let Some((root, path)) = self.resolve_newest(project_dir, name)? else {
            return Ok(None);
        };
        let mut doc = self.load_doc(&root, &path)?;
        doc.usage = match self.record_view(&doc.summary.name) {
            Ok(usage) => usage,
            Err(err) => {
                tracing::warn!(
                    "failed to record skill view for '{}': {err}",
                    doc.summary.name
                );
                SkillUsage::default()
            }
        };
        Ok(Some(doc))
    }

    async fn manage(
        &self,
        action: &str,
        name: &str,
        content: Option<&str>,
        project_dir: Option<&Path>,
    ) -> CoreResult<String> {
        // New skills are written under the workspace's .agents/skills when a project is
        // present; without one they fall back to the global store.
        let write_root = match project_dir {
            Some(project) => project_skills_dir(project),
            None => self.skills_dir(),
        };
        match action {
            "create" => {
                let path = self.skill_path(&write_root, name)?;
                validate_name_segments(name)?;
                if path.exists() {
                    return Err(CoreError::Conflict(format!(
                        "skill '{name}' already exists"
                    )));
                }
                let content = require_content(content, "create")?;
                validate_content(content)?;
                write_atomic(&path, content)?;
                self.note_created(content, name);
                Ok(format!("created skill '{name}'"))
            }
            "update" => {
                // Traversal/name guard first (matches create), then locate the file.
                self.skill_path(&write_root, name)?;
                validate_name_segments(name)?;
                // Update the skill wherever it already lives (project first, then global).
                let Some(path) = self.existing_path(project_dir, name)? else {
                    return Err(CoreError::InvalidRequest(format!(
                        "skill '{name}' not found"
                    )));
                };
                let content = require_content(content, "update")?;
                validate_content(content)?;
                write_atomic(&path, content)?;
                self.note_patch(content, name);
                Ok(format!("updated skill '{name}'"))
            }
            "delete" => {
                self.skill_path(&write_root, name)?;
                validate_name_segments(name)?;
                let Some(path) = self.existing_path(project_dir, name)? else {
                    return Err(CoreError::InvalidRequest(format!(
                        "skill '{name}' not found"
                    )));
                };
                let key = std::fs::read_to_string(&path)
                    .map(|content| ledger_name(&content, name))
                    .unwrap_or_else(|_| ledger_name("", name));
                std::fs::remove_file(&path)
                    .map_err(|err| io_error("failed to delete skill", &path, &err))?;
                if let Err(err) = self.forget_usage(&key) {
                    tracing::warn!("failed to forget skill usage for '{key}': {err}");
                }
                Ok(format!("deleted skill '{name}'"))
            }
            "install" => {
                let path = self.skill_path(&write_root, name)?;
                validate_name_segments(name)?;
                let content = require_content(content, "install")?;
                validate_content(content)?;
                write_atomic(&path, content)?;
                self.note_created(content, name);
                Ok(format!("installed skill '{name}'"))
            }
            other => Err(CoreError::InvalidRequest(format!(
                "unknown skill action '{other}'; use create, update, delete or install"
            ))),
        }
    }
}

// --- Frontmatter parsing ------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    category: Option<String>,
    /// Host/session platforms the skill is restricted to; empty means any.
    platforms: Vec<String>,
    /// Environment variables the skill needs; empty means none required.
    env: Vec<String>,
    /// When true the skill is hidden from every listing.
    disabled: bool,
}

#[derive(Debug)]
struct Split {
    frontmatter: Frontmatter,
    body: String,
    closed: bool,
    /// Non-fatal problems found while parsing, surfaced so listings can report them.
    diagnostics: Vec<String>,
}

impl Split {
    fn into_parts(self) -> (Frontmatter, String) {
        (self.frontmatter, self.body)
    }
}

/// One scanned skill file plus the frontmatter gates that decide whether it is listed.
#[derive(Debug)]
struct ScannedSkill {
    summary: SkillSummary,
    platforms: Vec<String>,
    required_env: Vec<String>,
    disabled: bool,
    diagnostics: Vec<String>,
    /// File modification time, used to pick the winner when the same skill name
    /// appears in more than one root (most recently edited wins).
    modified: SystemTime,
}

/// Split a document into frontmatter and body, reading name, description, category, platforms, env
/// and disabled (and their `metadata.hermes.*` mirrors). Bad values become diagnostics.
fn split_frontmatter(content: &str) -> Split {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut diagnostics = Vec::new();
    let mut lines = content.lines();
    let Some(first) = lines.next() else {
        return unclosed(Frontmatter::default(), String::new(), diagnostics);
    };
    if first.trim() != "---" {
        return unclosed(
            Frontmatter::default(),
            content.trim_start_matches('\n').to_string(),
            diagnostics,
        );
    }

    let mut frontmatter = Frontmatter::default();
    let mut sections: Vec<(usize, String)> = Vec::new();
    let mut body = Vec::new();
    let mut closed = false;
    for line in lines {
        if closed {
            body.push(line);
            continue;
        }
        if line.trim() == "---" {
            closed = true;
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(item) = trimmed.strip_prefix("- ") {
            let path = sections_path(&sections);
            let item = strip_quotes(item);
            if !item.is_empty() {
                match path.as_str() {
                    "platforms" | "metadata.hermes.platforms" => frontmatter.platforms.push(item),
                    "env" | "metadata.hermes.env" => frontmatter.env.push(item),
                    _ => {}
                }
            }
            continue;
        }
        let Some(colon) = line.find(':') else {
            continue;
        };
        let raw_key = &line[..colon];
        let indent = raw_key.len() - raw_key.trim_start().len();
        let key = raw_key.trim();
        if key.is_empty() || key.starts_with('#') {
            continue;
        }
        let value = line[colon + 1..].trim();
        while sections.last().is_some_and(|(depth, _)| *depth >= indent) {
            sections.pop();
        }
        let path = sections
            .iter()
            .map(|(_, key)| key.as_str())
            .chain(std::iter::once(key))
            .collect::<Vec<_>>()
            .join(".");
        if value.is_empty() {
            sections.push((indent, key.to_string()));
            continue;
        }
        match path.as_str() {
            "name" | "metadata.hermes.name" if frontmatter.name.is_none() => {
                frontmatter.name = Some(strip_quotes(value));
            }
            "description" | "metadata.hermes.description" if frontmatter.description.is_none() => {
                frontmatter.description = Some(strip_quotes(value));
            }
            "category" | "metadata.hermes.category" if frontmatter.category.is_none() => {
                frontmatter.category = Some(strip_quotes(value));
            }
            "platforms" | "metadata.hermes.platforms" => match parse_string_list(value) {
                Ok(items) => frontmatter.platforms = items,
                Err(diagnostic) => diagnostics.push(diagnostic),
            },
            "env" | "metadata.hermes.env" => match parse_string_list(value) {
                Ok(items) => frontmatter.env = items,
                Err(diagnostic) => diagnostics.push(diagnostic),
            },
            "disabled" | "metadata.hermes.disabled" => match parse_bool(value) {
                Ok(disabled) => frontmatter.disabled = disabled,
                Err(diagnostic) => diagnostics.push(diagnostic),
            },
            _ => {}
        }
    }

    if !closed {
        diagnostics.push("frontmatter is not closed with a '---' fence".to_string());
    }

    Split {
        frontmatter,
        body: body.join("\n"),
        closed,
        diagnostics,
    }
}

/// A split with no closed frontmatter fence.
fn unclosed(frontmatter: Frontmatter, body: String, diagnostics: Vec<String>) -> Split {
    Split {
        frontmatter,
        body,
        closed: false,
        diagnostics,
    }
}

/// Full dotted path of the innermost open mapping, used to attach list items.
fn sections_path(sections: &[(usize, String)]) -> String {
    sections
        .iter()
        .map(|(_, key)| key.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// Parse a YAML scalar-or-inline-list into trimmed, unquoted strings.
fn parse_string_list(value: &str) -> Result<Vec<String>, String> {
    let Some(inner) = value.strip_prefix('[') else {
        let scalar = strip_quotes(value);
        return Ok(if scalar.is_empty() {
            Vec::new()
        } else {
            vec![scalar]
        });
    };
    let Some(inner) = inner.strip_suffix(']') else {
        return Err(format!("malformed list value '{value}'"));
    };
    Ok(inner
        .split(',')
        .map(strip_quotes)
        .filter(|item| !item.is_empty())
        .collect())
}

/// Parse the booleans YAML commonly accepts; anything else is a diagnostic.
fn parse_bool(value: &str) -> Result<bool, String> {
    match strip_quotes(value).to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(format!("malformed boolean value '{other}'")),
    }
}

/// Case-insensitive platform comparison with the common OS aliases.
fn platform_matches(platforms: &[String], platform: &str) -> bool {
    let platform = normalize_platform(platform);
    platforms
        .iter()
        .any(|candidate| normalize_platform(candidate) == platform)
}

fn normalize_platform(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "macos" | "mac" | "osx" => "darwin".to_string(),
        "windows" | "win" => "win32".to_string(),
        other => other.to_string(),
    }
}

/// True when the named environment variable is present and non-blank.
fn env_present(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| !value.trim().is_empty())
}

/// The ledger key for a skill document: its frontmatter name, else the last
/// segment of the requested name.
fn ledger_name(content: &str, requested: &str) -> String {
    let (frontmatter, _) = split_frontmatter(content).into_parts();
    frontmatter
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            requested
                .rsplit('/')
                .next()
                .unwrap_or(requested)
                .to_string()
        })
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn strip_quotes(value: &str) -> String {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

fn first_body_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
}

// --- Validation and confinement ----------------------------------------------

/// Reject empty, absolute and traversal-bearing names. Category separators are
/// allowed; each segment is checked for traversal. A leading or trailing slash
/// and backslashes are refused so a name always stays a relative path.
fn relative_name(name: &str) -> CoreResult<String> {
    let candidate = name.trim();
    if candidate.is_empty() {
        return Err(CoreError::InvalidRequest("skill name is required".into()));
    }
    if candidate.contains('\\') {
        return Err(CoreError::InvalidRequest(format!(
            "invalid skill name '{candidate}': backslashes are not allowed"
        )));
    }
    if candidate.ends_with(".md") {
        return Err(CoreError::InvalidRequest(format!(
            "invalid skill name '{candidate}': omit the '.md' extension"
        )));
    }
    let path = Path::new(candidate);
    if path.is_absolute() {
        return Err(CoreError::InvalidRequest(format!(
            "invalid skill name '{candidate}': absolute paths are not allowed"
        )));
    }
    let mut segments = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => segments += 1,
            _ => {
                return Err(CoreError::InvalidRequest(format!(
                    "invalid skill name '{candidate}': path traversal is not allowed"
                )))
            }
        }
    }
    if segments == 0 {
        return Err(CoreError::InvalidRequest(format!(
            "invalid skill name '{candidate}': empty path"
        )));
    }
    Ok(candidate.to_string())
}

/// Enforce the upstream name rule per segment: lowercase letters, digits,
/// hyphens, dots and underscores, starting with a letter or digit.
fn validate_name_segments(name: &str) -> CoreResult<()> {
    let candidate = name.trim();
    for segment in candidate.split('/') {
        if segment.len() > MAX_NAME_LENGTH {
            return Err(CoreError::InvalidRequest(format!(
                "skill name exceeds {MAX_NAME_LENGTH} characters"
            )));
        }
        let mut chars = segment.chars();
        match chars.next() {
            Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
            _ => return Err(invalid_name(candidate)),
        }
        if !chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        {
            return Err(invalid_name(candidate));
        }
    }
    Ok(())
}

fn invalid_name(name: &str) -> CoreError {
    CoreError::InvalidRequest(format!(
        "invalid skill name '{name}'. Use lowercase letters, numbers, hyphens, dots and \
         underscores; must start with a letter or digit."
    ))
}

fn require_content<'a>(content: Option<&'a str>, action: &str) -> CoreResult<&'a str> {
    content.ok_or_else(|| CoreError::InvalidRequest(format!("skill {action} requires 'content'")))
}

/// Enforce the frontmatter contract: an opening '---' fence, a 'name' and
/// 'description', and a non-empty body.
fn validate_content(content: &str) -> CoreResult<()> {
    if content.trim().is_empty() {
        return Err(CoreError::InvalidRequest(
            "skill content cannot be empty".into(),
        ));
    }
    let stripped = content.strip_prefix('\u{feff}').unwrap_or(content);
    if !stripped.starts_with("---") {
        return Err(CoreError::InvalidRequest(
            "skill content must start with a '---' YAML frontmatter fence".into(),
        ));
    }
    let split = split_frontmatter(content);
    if !split.closed {
        return Err(CoreError::InvalidRequest(
            "skill frontmatter is not closed with a '---' fence".into(),
        ));
    }
    if split
        .frontmatter
        .name
        .as_deref()
        .is_none_or(|name| name.trim().is_empty())
    {
        return Err(CoreError::InvalidRequest(
            "skill frontmatter must include a 'name' field".into(),
        ));
    }
    if split
        .frontmatter
        .description
        .as_deref()
        .is_none_or(|description| description.trim().is_empty())
    {
        return Err(CoreError::InvalidRequest(
            "skill frontmatter must include a 'description' field".into(),
        ));
    }
    if split.body.trim().is_empty() {
        return Err(CoreError::InvalidRequest(
            "skill content must have a body after the frontmatter".into(),
        ));
    }
    Ok(())
}

// --- Atomic writes ------------------------------------------------------------

fn io_error(context: &str, path: &Path, err: &std::io::Error) -> CoreError {
    CoreError::Internal(format!("{context} {}: {err}", path.display()))
}

/// The per-project skills directory overlaid on the global store: `<root>/.agents/skills`.
fn project_skills_dir(root: &Path) -> PathBuf {
    root.join(".agents").join("skills")
}

/// A file's modification time, or the Unix epoch when it cannot be read, so an unreadable
/// file never wins a most-recently-modified comparison.
fn modified_time(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn write_atomic(path: &Path, contents: &str) -> CoreResult<()> {
    crate::atomic_file::write(path, contents.as_bytes())
        .map_err(|err| io_error("failed to write skill file", path, &err))
}
