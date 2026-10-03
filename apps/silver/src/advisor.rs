//! Jev, TypeSafe's classifier on OpenRouter, as the run advisor (`agent.jev_hints`): one request of
//! yes/no questions per call, with thresholds and hint texts kept here. Any failure gives no hints.

use crate::auth::AuthStore;
use crate::skills::SkillsStore;
use serde_json::{json, Value};
use silver_core::advisor::{Advice, Advisor, Step};
use silver_core::services::SkillsBackend;
use silver_protocol::AdvisorStatus;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/systemone";
const MODEL: &str = "~typesafe/jev-latest";
/// Jev answers in under a second; a slow reply is not worth holding the run for.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Tool calls sent with their whole results; older ones keep only the start of theirs, which
/// is enough to tell whether the docs were read.
const RECENT_STEPS: usize = 8;
const EARLIER_RESULT_CHARS: usize = 150;
const YES: f64 = 0.8;
const NO: f64 = 0.3;

/// Every question, by the name its answer is reported under.
const QUESTIONS: &[(&str, &str)] = &[
    ("current", "Does `task` depend on facts that change over time, such as recent events, news, current versions, releases or prices?"),
    ("outside", "Does `task` name a specific third-party project, library, framework or tool (for example by URL or by name) that the assistant must set up, build, install, configure or use?"),
    ("build", "Does `task` ask to clone, build, compile, install or set up a software project?"),
    ("blocked", "In the latest `steps`, did a build or configure step fail because a dependency, library, package or tool is missing?"),
    ("installing", "Is the assistant trying to install system packages (pacman, apt, dnf, brew, sudo)?"),
    ("repeating", "Do the latest `steps` repeat similar attempts that keep failing, without progress toward `task`?"),
    ("fetch_failing", "In the latest `steps`, does downloading a web page or file keep failing (for example 404 Not Found or empty output)?"),
    ("patching", "In the latest `steps`, is the assistant editing the project's own source or build files to get around a build error?"),
    ("finished", "Did a step's `result` show the requested build finishing successfully (for example the final link step with no errors)?"),
    // Asked about results, not attempts: a curl of a README that returned 404 is not reading it.
    ("read_docs", "Did any step's `result` contain the project's build instructions, such as the commands its docs give to configure and compile it?"),
    ("gave_up", "Does `answer` say that `task` is not finished, or ask the user to install something or do a step themselves?"),
    ("skipped_docs", "Does `answer` mention a documented build command (for example a preset, a recursive clone or a submodule update) that no step's `input` ran?"),
    ("edited", "Did any step edit or patch the project's own source or build files?"),
    ("searched", "Did the assistant search the web or fetch a web page anywhere in `steps`?"),
];
/// Asked at every check: what the task is.
const TASK_QUESTIONS: &[&str] = &["current", "outside", "build"];
const TOOLS_QUESTIONS: &[&str] = &[
    "blocked",
    "installing",
    "repeating",
    "fetch_failing",
    "patching",
    "finished",
    "read_docs",
];
const ANSWER_QUESTIONS: &[&str] = &["gave_up", "read_docs", "skipped_docs", "edited", "searched"];

pub struct JevAdvisor {
    http: reqwest::Client,
    auth: Arc<AuthStore>,
    skills: Arc<SkillsStore>,
    web_tools: bool,
    enabled: AtomicBool,
    /// Where the switch is saved; None in tests.
    config_path: Option<PathBuf>,
}

impl JevAdvisor {
    pub fn new(
        http: reqwest::Client,
        auth: Arc<AuthStore>,
        skills: Arc<SkillsStore>,
        web_tools: bool,
        enabled: bool,
        config_path: Option<PathBuf>,
    ) -> Self {
        let advisor = Self {
            http,
            auth,
            skills,
            web_tools,
            enabled: AtomicBool::new(enabled),
            config_path,
        };
        if enabled && advisor.key().is_none() {
            tracing::warn!(
                "agent.jev_hints is on but no OpenRouter key is stored (/login) or set in \
                 OPENROUTER_API_KEY; no hints until there is one"
            );
        }
        advisor
    }

    pub fn status(&self) -> AdvisorStatus {
        AdvisorStatus {
            enabled: self.enabled.load(Ordering::Relaxed),
            has_key: self.key().is_some(),
            questions: QUESTIONS
                .iter()
                .map(|(name, text)| (name.to_string(), text.to_string()))
                .collect(),
        }
    }

    /// Switch the advisor for the next check of every run, and save it as agent.jev_hints.
    pub fn set_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        if let Some(path) = &self.config_path {
            crate::config::persist_setting(path, "agent", "jev_hints", enabled.into())?;
        }
        self.enabled.store(enabled, Ordering::Relaxed);
        Ok(())
    }

    fn key(&self) -> Option<String> {
        self.auth
            .credential("openrouter")
            .and_then(|credential| credential.api_key)
            .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
            .filter(|key| !key.trim().is_empty())
    }

    /// How the model can search the web: the web tools, else a skill that says it does.
    async fn search(&self) -> Option<String> {
        if self.web_tools {
            return Some("search with web_search".into());
        }
        let skills = self.skills.list().await.ok()?;
        let skill = skills.iter().find(|skill| {
            let description = skill.description.to_lowercase();
            description.contains("search") && description.contains("web")
        })?;
        Some(format!(
            "load the {0} skill with skill_view(name=\"{0}\") and search the web with its commands",
            skill.name
        ))
    }

    /// The yes-probability of each named question.
    async fn ask(&self, state: Value, names: &[&str]) -> Option<BTreeMap<String, f64>> {
        let key = self.key()?;
        let questions: serde_json::Map<String, Value> = QUESTIONS
            .iter()
            .filter(|(name, _)| names.contains(name))
            .map(|(name, text)| {
                (
                    name.to_string(),
                    json!({ "type": "noul", "instructions": text }),
                )
            })
            .collect();
        let body = json!({ "model": MODEL, "state": state, "questions": questions });
        let reply = async {
            self.http
                .post(ENDPOINT)
                .bearer_auth(key)
                .json(&body)
                .timeout(TIMEOUT)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await
        }
        .await;
        let mut reply = match reply {
            Ok(reply) => reply,
            Err(_) => {
                tracing::warn!(?reply, "jev gave no answers; no hints this time");
                return None;
            }
        };
        let answers = match reply.as_object_mut().and_then(|obj| obj.get_mut("answers")) {
            Some(Value::Object(map)) => std::mem::take(map),
            _ => {
                tracing::warn!(?reply, "jev gave no answers; no hints this time");
                return None;
            }
        };
        let mut out = std::collections::BTreeMap::new();
        for (name, answer) in answers {
            if let Some(prob) = answer.get("noul").and_then(Value::as_f64) {
                out.insert(name, prob);
            }
        }
        Some(out)
    }
}

#[async_trait::async_trait]
impl Advisor for JevAdvisor {
    async fn advise(&self, task: &str, steps: &[Step], answer: Option<&str>) -> Option<Advice> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }
        let split = steps.len().saturating_sub(RECENT_STEPS);
        let earlier: Vec<Value> = steps[..split]
            .iter()
            .map(|step| {
                let start: String = step.result.chars().take(EARLIER_RESULT_CHARS).collect();
                json!({ "tool": step.tool, "input": step.input, "result": start })
            })
            .collect();
        let mut state = json!({ "task": task, "steps": &steps[split..] });
        if !earlier.is_empty() {
            state["earlier_steps"] = json!(earlier);
        }
        let mut names = TASK_QUESTIONS.to_vec();
        match answer {
            None if steps.is_empty() => {}
            None => names.extend(TOOLS_QUESTIONS),
            Some(answer) => {
                state["answer"] = json!(answer);
                names.extend(ANSWER_QUESTIONS);
            }
        }
        let answers = self.ask(state, &names).await?;
        let yes = |name: &str| answers.get(name).is_some_and(|v| *v >= YES);
        let no = |name: &str| answers.get(name).is_some_and(|v| *v <= NO);
        let hints = hints(
            task,
            self.search().await.as_deref(),
            steps.is_empty(),
            answer.is_some(),
            &yes,
            &no,
        );
        Some(Advice { answers, hints })
    }
}

const FOLLOW_DOCS: &str = "You have read the build instructions. Run their commands \
    yourself, exactly and in order (the clone flags such as --recursive or a submodule update, \
    then the documented configure and build commands or presets), before you install packages or \
    give up: those steps often fetch the missing dependencies themselves.";

/// The hints the answers call for. `search` says how the model can search the web, if it can.
fn hints(
    task: &str,
    search: Option<&str>,
    first: bool,
    answering: bool,
    yes: &dyn Fn(&str) -> bool,
    no: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let outside_build = yes("outside") && yes("build");
    let docs = match search {
        Some(search) if yes("outside") => format!(
            "{search} for the project's official build instructions, and read its README or \
             BUILDING file"
        ),
        _ => "read the project's README, BUILDING or docs files".to_string(),
    };
    let mut hints = Vec::new();
    if first && outside_build {
        hints.push(format!(
            "Hint: before you build or set up this project, {docs}. Follow the documented \
             steps (clone flags such as --recursive, submodules, presets, bundled dependency \
             managers) instead of guessing them."
        ));
    }
    if let Some(search) = search.filter(|_| yes("current")) {
        if first {
            hints.push(format!(
                "Hint: this depends on current information your training data may not have, so \
                 {search} before you answer."
            ));
        } else if answering && no("searched") {
            hints.push(format!(
                "You answered from memory, but this depends on current information, so {search} \
                 first, then answer again."
            ));
        }
    }
    if !first && !answering && yes("repeating") {
        let mut hint = "Hint: your last attempts keep failing the same way. Stop repeating \
                        them and change approach."
            .to_string();
        if let Some(search) = search.filter(|_| yes("fetch_failing")) {
            hint.push_str(&format!(
                " To read something from the web, {search} instead of downloading pages \
                 yourself."
            ));
        }
        hints.push(hint);
    }
    if !first && !answering && (yes("blocked") || yes("installing")) && no("read_docs") {
        hints.push(format!(
            "Hint: something seems to be missing, and you have not read this project's build \
             instructions yet. Before you install anything or give up, {docs}: many projects \
             fetch or build their own dependencies (git submodules, vcpkg, conan, CMake \
             presets)."
        ));
    }
    // Building someone else's project is not changing it: a small model rewrites compiler flags
    // rather than use the toolchain version the user said is installed, so it gets their words
    // back and the command that undoes the edits.
    let fix_environment = format!(
        "Undo those edits (`git checkout -- .` in the project) and fix the environment instead. \
         The user said: \"{}\". Use what that tells you (for example point JAVA_HOME or CC at \
         the version it names) and reconfigure from a clean build directory.",
        task.chars().take(300).collect::<String>()
    );
    if !first && !answering && outside_build && yes("patching") {
        hints.push(format!(
            "Hint: you are editing the project's own files to get around a build error. \
             {fix_environment}"
        ));
    }
    // A small model that has finished keeps going (runs tests, switches generators) and can
    // break the build it just made.
    if !first && !answering && outside_build && yes("finished") {
        hints.push(
            "Hint: the build looks finished. Check that its output exists, then answer the user; \
             don't start work they did not ask for, such as running tests or changing the build \
             setup."
                .to_string(),
        );
    }
    // Read but not followed: a small model takes the prerequisites list for "install these"
    // and skips the documented commands that would fetch them.
    if !first && !answering && (yes("blocked") || yes("installing")) && yes("read_docs") {
        hints.push(format!("Hint: {FOLLOW_DOCS}"));
    }
    if answering && outside_build && yes("gave_up") {
        if no("read_docs") {
            hints.push(format!(
                "You have not checked this project's build instructions yet. Before you stop, \
                 {docs}; they may show a way to finish. Then continue the task, or answer if \
                 they do not help."
            ));
        } else if yes("skipped_docs") {
            hints.push(format!(
                "Your answer mentions documented build steps you have not run. {FOLLOW_DOCS}"
            ));
        }
    }
    if answering && outside_build && yes("edited") {
        hints.push(format!(
            "You changed the project's own files to build it. {fix_environment} If it still \
             fails without the edits, say so and why."
        ));
    }
    hints
}
