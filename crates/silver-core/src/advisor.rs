//! Hints for the model from a fast outside classifier such as Jev, about steps a small model often
//! misses (reading a project's build docs, searching for current facts).

use serde::Serialize;
use std::collections::BTreeMap;

/// One tool call as the advisor sees it.
#[derive(Clone, Debug, Serialize)]
pub struct Step {
    pub tool: String,
    /// The command of a shell call, else the arguments as JSON.
    pub input: String,
    /// The exit status and the head and tail of the output, where errors usually are.
    pub result: String,
}

impl Step {
    pub fn new(tool: &str, args: &serde_json::Value, content: &str) -> Self {
        let input = match args.get("command").and_then(serde_json::Value::as_str) {
            Some(command) => command.to_string(),
            None => args.to_string(),
        };
        let result = match serde_json::from_str::<serde_json::Value>(content) {
            Ok(envelope) if envelope.get("exit_code").is_some() => format!(
                "exit {}\n{}",
                envelope["exit_code"],
                clip(envelope["output"].as_str().unwrap_or_default(), 600)
            ),
            _ => clip(content, 600),
        };
        Self {
            tool: tool.to_string(),
            input: clip(&input, 300),
            result,
        }
    }
}

/// Keep a quarter of `max` from the head and the rest from the tail.
fn clip(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.trim().chars().collect();
    if chars.len() <= max {
        return chars.into_iter().collect();
    }
    let head: String = chars[..max / 4].iter().collect();
    let tail: String = chars[chars.len() - (max - max / 4)..].iter().collect();
    format!("{head} … {tail}")
}

/// What the advisor made of the run at one point.
#[derive(Clone, Debug, Default)]
pub struct Advice {
    /// Its yes-probability for each question it asked, shown to the user.
    pub answers: BTreeMap<String, f64>,
    /// Hints for the model; empty when none applies.
    pub hints: Vec<String>,
}

#[async_trait::async_trait]
pub trait Advisor: Send + Sync {
    /// Look at the run: at the start of a task (no steps), after a round of tool calls, or
    /// when `answer` is about to end the turn. None when it is switched off or failed, since
    /// a hint is never worth failing a run.
    async fn advise(&self, task: &str, steps: &[Step], answer: Option<&str>) -> Option<Advice>;
}
