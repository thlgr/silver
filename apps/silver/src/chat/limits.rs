//! A bot stops, and is not started, once its provider's session limit is nearly used up. Windows
//! are read every minute (Claude's also come with its turns), kept, shown on each agent and reply,
//! and the session window is checked against [STOP_AT]. A provider we cannot read never blocks.

use super::store::{now_ms, BotRow};
use super::{ChatHub, Lane, State};
use crate::routed::RoutedModel;
use anyhow::Context;
use serde_json::Value;
use silver_protocol::chat::{BotKind, LimitWindow};
use silver_protocol::providers::preset;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::MissedTickBehavior;

/// A bot stops, and does not start a turn, at this share of the session limit.
const STOP_AT: f64 = 90.0;
/// How often each provider's limit is read. Neither endpoint is meant to be polled quickly.
const EVERY: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// The provider's usage windows, the session first, or `None` when it has no way to ask.
async fn fetch(routed: &RoutedModel, provider: &str) -> Option<anyhow::Result<Vec<LimitWindow>>> {
    Some(match provider {
        "claude" => claude(routed.http()).await,
        "opencode-go" => opencode_go(routed, provider).await,
        _ => return None,
    })
}

/// Claude Code keeps its sign-in in `.credentials.json`. The token is only read: Claude Code
/// refreshes it, and a second refresh here would invalidate the one it holds.
async fn claude(http: &reqwest::Client) -> anyhow::Result<Vec<LimitWindow>> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => silver_core::dirs::home_dir()
            .context("no home directory")?
            .join(".claude"),
    };
    let file = tokio::fs::read(dir.join(".credentials.json"))
        .await
        .context("Claude Code's sign-in file could not be read")?;
    let credentials: Value = serde_json::from_slice(&file)?;
    let token = credentials["claudeAiOauth"]["accessToken"]
        .as_str()
        .context("Claude Code's sign-in file has no access token")?;
    let request = http
        .get(CLAUDE_USAGE_URL)
        .header("anthropic-beta", "oauth-2025-04-20");
    claude_windows(&get_json(request, token).await?)
}

async fn opencode_go(routed: &RoutedModel, provider: &str) -> anyhow::Result<Vec<LimitWindow>> {
    let route = routed.resolve(provider).await?;
    anyhow::ensure!(route.authenticated(), "no API key for {provider}");
    let url = format!("{}/usage", route.base_url.trim_end_matches('/'));
    opencode_windows(&get_json(routed.http().get(url), route.key()).await?)
}

async fn get_json(request: reqwest::RequestBuilder, token: &str) -> anyhow::Result<Value> {
    Ok(request
        .bearer_auth(token)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// The start of a reply, for the error that says what was not in it.
fn clip(body: &Value) -> String {
    body.to_string().chars().take(300).collect()
}

fn unix_ms(time: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|time| time.timestamp_millis())
}

/// Claude's usage windows by the key both its usage reply and its adapter name them with, the
/// session first.
const CLAUDE_WINDOWS: [(&str, &str); 4] = [
    ("five_hour", "Session"),
    ("seven_day", "Week"),
    ("seven_day_opus", "Week · Opus"),
    ("seven_day_sonnet", "Week · Sonnet"),
];

/// The windows of a Claude plan that `read` finds, as a share used out of 100 and a reset in
/// milliseconds. The session window is required; the rest are whichever the plan has.
fn claude_plan(read: impl Fn(&str) -> Option<(f64, Option<i64>)>) -> Option<Vec<LimitWindow>> {
    let window = |(key, name): (&str, &str)| {
        let (percent, resets_at) = read(key)?;
        Some(LimitWindow {
            name: name.into(),
            percent,
            resets_at,
        })
    };
    let [session, more @ ..] = CLAUDE_WINDOWS;
    Some(
        std::iter::once(window(session)?)
            .chain(more.into_iter().filter_map(window))
            .collect(),
    )
}

/// `{"five_hour": {"utilization": 35.0, "resets_at": "2026-02-06T22:00:00+00:00"}, "seven_day":
/// {..}, "seven_day_opus": null, ..}`.
fn claude_windows(body: &Value) -> anyhow::Result<Vec<LimitWindow>> {
    claude_plan(|key| {
        let window = &body[key];
        Some((
            window["utilization"].as_f64()?,
            window["resets_at"].as_str().and_then(unix_ms),
        ))
    })
    .with_context(|| format!("no five_hour window in the usage reply: {}", clip(body)))
}

/// The adapter's rate-limit info, sent with a turn's usage: `{"status": "allowed",
/// "unifiedWindows": {"five_hour": {"utilization": 0.68, "resetsAt": 1791211200}, "seven_day":
/// {..}}, ..}`. A share is out of one and a reset is in seconds.
fn claude_rate_limit(info: &Value) -> Option<Vec<LimitWindow>> {
    claude_plan(|key| {
        let window = &info["unifiedWindows"][key];
        Some((
            (window["utilization"].as_f64()? * 1000.0).round() / 10.0,
            window["resetsAt"].as_i64().map(|seconds| seconds * 1000),
        ))
    })
}

/// `{"usage": {"rolling": {"status": "ok", "percent": 1, "resetsAt": "2026-09-13T10:42:47.510Z"},
/// "weekly": {..}, "monthly": {..}}}`. The rolling window is the session and is required.
fn opencode_windows(body: &Value) -> anyhow::Result<Vec<LimitWindow>> {
    let window = |key: &str, name: &str| {
        let window = &body["usage"][key];
        // A window the relay reports as anything but `ok` is cut off, so it counts as full.
        let cut_off = window["status"]
            .as_str()
            .is_some_and(|status| status != "ok");
        Some(LimitWindow {
            name: name.into(),
            percent: if cut_off {
                100.0
            } else {
                window["percent"].as_f64()?
            },
            resets_at: window["resetsAt"].as_str().and_then(unix_ms),
        })
    };
    let session = window("rolling", "Session")
        .with_context(|| format!("no rolling window in the usage reply: {}", clip(body)))?;
    let more = [("weekly", "Week"), ("monthly", "Month")];
    Ok(std::iter::once(session)
        .chain(more.into_iter().filter_map(|(key, name)| window(key, name)))
        .collect())
}

/// "2h 05m" or "35m".
fn span(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    if minutes >= 60 {
        format!("{}h {:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{}m", minutes.max(1))
    }
}

/// "Claude Code is at 93% of its session limit. It resets in 2h 10m."
fn describe(provider: &str, session: &LimitWindow) -> String {
    let name = preset(provider).map_or(provider, |preset| preset.label);
    let resets = session
        .resets_at
        .map(|at| format!(" It resets in {}.", span(at - now_ms())))
        .unwrap_or_default();
    format!(
        "{name} is at {:.0}% of its session limit.{resets}",
        session.percent
    )
}

/// The session window, unless it has reset since it was read.
fn session(windows: &[LimitWindow]) -> Option<&LimitWindow> {
    windows
        .first()
        .filter(|window| window.resets_at.is_none_or(|at| at > now_ms()))
}

impl ChatHub {
    /// Read every bot provider's usage limits once a minute, for as long as the daemon runs.
    /// Editing a bot reads them at once. What was read before the daemon started shows until then.
    pub async fn watch_limits(self: Arc<Self>, routed: Arc<RoutedModel>) {
        match self.db.chat_limits().await {
            Ok(saved) => self.state().limits.extend(saved),
            Err(error) => tracing::warn!(%error, "the saved usage limits could not be read"),
        }
        let mut tick = tokio::time::interval(EVERY);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = self.recheck.notified() => {}
            }
            self.read_limits(&routed).await;
        }
    }

    async fn read_limits(&self, routed: &RoutedModel) {
        let bots = match self.db.chat_bots().await {
            Ok(bots) => bots,
            Err(error) => {
                return tracing::warn!(%error, "the bots could not be read for their limits")
            }
        };
        let agents: Vec<&BotRow> = bots
            .iter()
            .filter(|bot| bot.kind == BotKind::Agent)
            .collect();
        let mut providers: Vec<Cow<str>> = Vec::new();
        for bot in &agents {
            let provider = self.provider_of(bot);
            if !providers.contains(&provider) {
                providers.push(provider);
            }
        }
        for provider in providers {
            let windows = match fetch(routed, &provider).await {
                None => continue,
                Some(Ok(windows)) => windows,
                // The last reading stands until the next round.
                Some(Err(error)) => {
                    let reason = format!("{error:#}");
                    let seen = self
                        .state()
                        .limit_errors
                        .insert(provider.to_string(), reason.clone());
                    if seen.as_ref() != Some(&reason) {
                        tracing::warn!(%provider, "usage limits could not be read: {reason}");
                    }
                    continue;
                }
            };
            self.state().limit_errors.remove(&*provider);
            self.take_reading(&provider, windows, &agents).await;
        }
    }

    /// What the Claude adapter reported of the plan's usage with a turn, the reading the usage
    /// endpoint would give.
    pub(super) async fn hear_claude(&self, info: &Value) {
        let Some(windows) = claude_rate_limit(info) else {
            return;
        };
        // Every usage update of a turn carries the same reading.
        if self.state().limits.get("claude") == Some(&windows) {
            return;
        }
        match self.db.chat_bots().await {
            Ok(bots) => {
                let agents: Vec<&BotRow> = bots
                    .iter()
                    .filter(|bot| bot.kind == BotKind::Agent)
                    .collect();
                self.take_reading("claude", windows, &agents).await;
            }
            Err(error) => tracing::warn!(%error, "the bots could not be read for their limits"),
        }
    }

    /// A provider's new reading: kept, shown on the bots that answer with it, and the session
    /// window checked against [STOP_AT].
    async fn take_reading(&self, provider: &str, windows: Vec<LimitWindow>, agents: &[&BotRow]) {
        let over = windows
            .first()
            .filter(|session| session.percent >= STOP_AT)
            .map(|session| format!("Stopped: {}", describe(provider, session)));
        let users: Vec<&BotRow> = agents
            .iter()
            .copied()
            .filter(|bot| self.provider_of(bot) == provider)
            .collect();
        if self.keep(provider, windows).await {
            for bot in &users {
                self.publish_bot(&bot.id).await;
            }
        }
        if let Some(text) = over {
            for bot in users {
                self.stop_at_limit(bot, &text).await;
            }
        }
    }

    /// Remember a provider's latest reading, in memory and on disk; false when it is the one
    /// already held.
    async fn keep(&self, provider: &str, windows: Vec<LimitWindow>) -> bool {
        if self.state().limits.get(provider) == Some(&windows) {
            return false;
        }
        if let Err(error) = self.db.save_chat_limits(provider, &windows).await {
            tracing::warn!(%error, %provider, "the usage limits could not be saved");
        }
        self.state().limits.insert(provider.to_string(), windows);
        true
    }

    /// End what a bot is doing, and drop what waits behind it, and say why in the chat it was
    /// working in. A bot doing nothing is left alone.
    async fn stop_at_limit(&self, bot: &BotRow, text: &str) {
        let lane = {
            let state = self.state();
            if !state.workers.contains(&bot.id) {
                return;
            }
            state
                .runtime
                .get(&bot.id)
                .and_then(|live| live.lane.clone())
        };
        let lane = lane.unwrap_or_else(|| Lane {
            chat: bot.id.clone(),
            thread: None,
        });
        self.stop_turns(&bot.id, |_| true).await;
        self.notice(&lane, text, "error").await;
    }

    /// The provider a bot answers with: its own, else the daemon's.
    fn provider_of<'a>(&self, bot: &'a BotRow) -> Cow<'a, str> {
        match bot.provider.as_deref() {
            Some(provider) => Cow::Borrowed(provider),
            None => self.runs().map_or(Cow::Borrowed(""), |runs| {
                Cow::Owned(runs.current_provider())
            }),
        }
    }

    /// The bot's provider's usage windows as last read, for a reply to carry. None once the
    /// session window has reset, until the next reading.
    pub(super) fn limits_of(&self, bot: &BotRow) -> Vec<LimitWindow> {
        self.limits_in(&self.state(), bot)
    }

    /// [Self::limits_of] for a caller that already holds the state; a group has none.
    pub(super) fn limits_in(&self, state: &State, bot: &BotRow) -> Vec<LimitWindow> {
        if bot.kind != BotKind::Agent {
            return Vec::new();
        }
        state
            .limits
            .get(&*self.provider_of(bot))
            .filter(|windows| session(windows).is_some())
            .cloned()
            .unwrap_or_default()
    }

    /// Why the bot may not start a turn: its provider's session limit is nearly used up.
    pub(super) fn limit_block(&self, bot: &BotRow) -> Option<String> {
        let provider = self.provider_of(bot);
        let state = self.state();
        let session = session(state.limits.get(&*provider)?)?;
        (session.percent >= STOP_AT)
            .then(|| format!("Not started: {}", describe(&provider, session)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use silver_protocol::chat::CreateBotRequest;

    fn window(name: &str, percent: f64, resets_in: Option<i64>) -> LimitWindow {
        LimitWindow {
            name: name.into(),
            percent,
            resets_at: resets_in.map(|ms| now_ms() + ms),
        }
    }

    #[test]
    fn claude_reports_its_windows_session_first() {
        let body = json!({
            "five_hour": { "utilization": 35.0, "resets_at": "2026-02-06T22:00:00+00:00" },
            "seven_day": { "utilization": 14.0, "resets_at": "2026-02-12T20:00:00+00:00" },
            "seven_day_opus": null,
            "seven_day_sonnet": { "utilization": 39.0, "resets_at": "2026-02-09T14:00:00+00:00" },
        });
        let windows = claude_windows(&body).unwrap();
        let named: Vec<(&str, f64)> = windows.iter().map(|w| (&*w.name, w.percent)).collect();
        assert_eq!(
            named,
            [("Session", 35.0), ("Week", 14.0), ("Week · Sonnet", 39.0)]
        );
        assert_eq!(windows[0].resets_at, Some(1_770_415_200_000));
        // Without a session window there is nothing to stop on.
        assert!(claude_windows(&json!({ "seven_day": { "utilization": 14.0 } })).is_err());
    }

    #[test]
    fn the_adapter_reports_the_same_windows_as_shares_of_one() {
        // As captured from claude-agent-acp 0.85.1.
        let info = json!({
            "status": "allowed", "resetsAt": 1_791_211_200, "rateLimitType": "five_hour",
            "unifiedWindows": {
                "five_hour": { "utilization": 0.68, "resetsAt": 1_791_211_200 },
                "seven_day": { "utilization": 0.58, "resetsAt": 1_791_417_600 },
            },
        });
        let windows = claude_rate_limit(&info).unwrap();
        let named: Vec<(&str, f64)> = windows.iter().map(|w| (&*w.name, w.percent)).collect();
        assert_eq!(named, [("Session", 68.0), ("Week", 58.0)]);
        assert_eq!(windows[0].resets_at, Some(1_791_211_200_000));
        // Without a session window there is nothing to stop on.
        let week = json!({ "unifiedWindows": { "seven_day": { "utilization": 0.58 } } });
        assert!(claude_rate_limit(&week).is_none());
        assert!(claude_rate_limit(&json!({ "status": "allowed" })).is_none());
    }

    #[test]
    fn opencode_go_reports_its_windows_session_first() {
        let body = json!({ "usage": {
            "rolling": { "status": "ok", "percent": 41, "resetsAt": "2026-09-13T10:42:47.510Z" },
            "weekly": { "status": "ok", "percent": 90, "resetsAt": "2026-09-14T00:00:00.510Z" },
            "monthly": { "status": "ok", "percent": 19, "resetsAt": "2026-10-11T01:55:50.510Z" },
        } });
        let windows = opencode_windows(&body).unwrap();
        let named: Vec<(&str, f64)> = windows.iter().map(|w| (&*w.name, w.percent)).collect();
        assert_eq!(named, [("Session", 41.0), ("Week", 90.0), ("Month", 19.0)]);
        assert_eq!(windows[0].resets_at, Some(1_789_296_167_510));

        let cut_off = json!({ "usage": { "rolling": { "status": "rate-limited" } } });
        assert_eq!(opencode_windows(&cut_off).unwrap()[0].percent, 100.0);
        assert!(opencode_windows(&json!({ "usage": { "rolling": { "status": "ok" } } })).is_err());
        let odd = opencode_windows(&json!({ "limits": { "weekly": 3 } })).unwrap_err();
        assert!(
            format!("{odd:#}").contains(r#"{"limits":{"weekly":3}}"#),
            "{odd:#}"
        );
    }

    #[test]
    fn a_limit_is_told_with_its_reset() {
        let session = window("Session", 92.6, Some(2 * 3_600_000 + 10 * 60_000 + 5_000));
        assert_eq!(
            describe("claude", &session),
            "Claude Code is at 93% of its session limit. It resets in 2h 10m."
        );
        assert_eq!(
            describe("opencode-go", &window("Session", 90.0, None)),
            "OpenCode Go is at 90% of its session limit."
        );
        assert_eq!(span(35 * 60_000 + 59_000), "35m");
        assert_eq!(span(-5), "1m");
    }

    async fn hub() -> Arc<ChatHub> {
        let dir = std::env::temp_dir().join(format!("silver-chat-limits-{}", uuid::Uuid::now_v7()));
        let db = crate::db::Db::open(&dir.join("state.db")).await.unwrap();
        db.migrate().await.unwrap();
        ChatHub::new(db)
    }

    async fn bot_on(hub: &Arc<ChatHub>, name: &str, provider: &str) -> BotRow {
        let view = hub
            .create_bot(CreateBotRequest {
                name: name.into(),
                provider: Some(provider.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        hub.bot(&view.id).await.unwrap()
    }

    #[tokio::test]
    async fn a_bot_carries_and_obeys_its_providers_limit_until_the_window_resets() {
        let hub = hub().await;
        let claude = bot_on(&hub, "Claude", "claude").await;
        let go = bot_on(&hub, "Go", "opencode-go").await;
        let other = bot_on(&hub, "Other", "gemini").await;
        let read = |windows| hub.state().limits.insert("claude".into(), windows);

        read(vec![
            window("Session", 89.0, Some(60_000)),
            window("Week", 95.0, Some(60_000)),
        ]);
        assert_eq!(
            hub.limits_of(&claude).len(),
            2,
            "a reply carries every window"
        );
        assert!(hub.limit_block(&claude).is_none(), "under the line it runs");

        read(vec![window("Session", 93.0, Some(3_600_000))]);
        let reason = hub.limit_block(&claude).expect("over the line it is held");
        assert!(
            reason.starts_with("Not started: Claude Code is at 93%"),
            "{reason}"
        );
        assert!(
            hub.limit_block(&go).is_none(),
            "another provider is not held"
        );
        assert!(hub.limits_of(&go).is_empty());
        assert!(
            hub.limit_block(&other).is_none(),
            "no limit to read never holds"
        );
        assert!(hub.limits_of(&other).is_empty());

        read(vec![window("Session", 93.0, Some(-1))]);
        assert!(
            hub.limit_block(&claude).is_none(),
            "a window that reset holds no one"
        );
        assert!(hub.limits_of(&claude).is_empty());
    }

    #[tokio::test]
    async fn a_claude_turn_updates_the_claude_bots_reading() {
        let hub = hub().await;
        let claude = bot_on(&hub, "Claude", "claude").await;
        let go = bot_on(&hub, "Go", "opencode-go").await;
        let info = |used: f64| json!({ "unifiedWindows": { "five_hour": { "utilization": used, "resetsAt": 4_102_444_800_i64 } } });

        hub.hear_claude(&info(0.35)).await;
        let held = hub.bot_view(&claude.id).await.unwrap().limits;
        assert_eq!(held.len(), 1);
        assert_eq!((held[0].name.as_str(), held[0].percent), ("Session", 35.0));
        assert_eq!(hub.db.chat_limits().await.unwrap()["claude"], held);
        assert!(hub.bot_view(&go.id).await.unwrap().limits.is_empty());

        hub.hear_claude(&info(0.93)).await;
        assert!(hub
            .limit_block(&claude)
            .is_some_and(|reason| reason.starts_with("Not started: Claude Code is at 93%")));
        hub.hear_claude(&json!({ "status": "allowed" })).await;
        assert_eq!(
            hub.bot_view(&claude.id).await.unwrap().limits[0].percent,
            93.0
        );
    }

    #[tokio::test]
    async fn an_agent_row_carries_the_last_reading_and_it_is_saved() {
        let hub = hub().await;
        let claude = bot_on(&hub, "Claude", "claude").await;
        let go = bot_on(&hub, "Go", "opencode-go").await;
        let group = hub
            .create_bot(CreateBotRequest {
                kind: Some(BotKind::Group),
                name: "Both".into(),
                members: vec![claude.id.clone(), go.id.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        let windows = vec![
            window("Session", 35.0, Some(3_600_000)),
            window("Week", 14.0, Some(86_400_000)),
        ];

        assert!(hub.keep("claude", windows.clone()).await);
        assert!(
            !hub.keep("claude", windows.clone()).await,
            "the same reading is not news"
        );

        assert_eq!(hub.bot_view(&claude.id).await.unwrap().limits, windows);
        assert!(hub.bot_view(&go.id).await.unwrap().limits.is_empty());
        assert!(hub.bot_view(&group.id).await.unwrap().limits.is_empty());
        assert_eq!(hub.db.chat_limits().await.unwrap()["claude"], windows);
    }
}
