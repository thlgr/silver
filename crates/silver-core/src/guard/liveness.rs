//! Turn liveness watchdog: aborts a turn that stalls silently while its lease still renews. An
//! abort is bound to the observed generation and revalidated under the lock, so a turn that
//! resumed is never cancelled.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

// Tokio's clock so idle time follows the watcher's own sleeps (and a paused test clock).
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_TURN_LIVENESS_TIMEOUT_S: f64 = 600.0;
pub const DEFAULT_TURN_LIVENESS_POLL_S: f64 = 15.0;
pub const MIN_TURN_LIVENESS_POLL_S: f64 = 0.01;

const CONFIG_TIMEOUT_KEY: &str = "agent.turn_liveness.timeout_s";
const CONFIG_POLL_KEY: &str = "agent.turn_liveness.poll_s";

fn warn_invalid_value(key: &str, raw: f64, default: f64) {
    tracing::warn!("Invalid {key} in config.yaml: {raw:?} - falling back to default {default:.1}.");
}

/// One duration knob, NaN and Inf rejected: NaN would disable the timeout through the `> 0` check,
/// and Inf would freeze the poll loop.
fn resolve_finite_seconds(raw: Option<f64>, default: f64, key: &str) -> f64 {
    match raw {
        Some(value) if value.is_finite() => value,
        Some(value) => {
            warn_invalid_value(key, value, default);
            default
        }
        None => default,
    }
}

/// (timeout_s, poll_s); timeout_s <= 0 opts out. Invalid values warn and fall back to defaults.
pub fn resolve_turn_liveness_settings(
    timeout_raw: Option<f64>,
    poll_raw: Option<f64>,
) -> (Option<f64>, f64) {
    let timeout_s = resolve_finite_seconds(
        timeout_raw,
        DEFAULT_TURN_LIVENESS_TIMEOUT_S,
        CONFIG_TIMEOUT_KEY,
    );
    let mut poll_s =
        resolve_finite_seconds(poll_raw, DEFAULT_TURN_LIVENESS_POLL_S, CONFIG_POLL_KEY);
    if poll_s <= 0.0 {
        warn_invalid_value(CONFIG_POLL_KEY, poll_s, DEFAULT_TURN_LIVENESS_POLL_S);
        poll_s = DEFAULT_TURN_LIVENESS_POLL_S;
    }
    let timeout_s = if timeout_s <= 0.0 {
        None
    } else {
        Some(timeout_s)
    };
    (timeout_s, poll_s)
}

/// One activity-clock observation. The generation must be revalidated by commit_abort
/// under the shared lock.
#[derive(Clone, Debug)]
pub struct ActivitySnapshot {
    pub generation: u64,
    pub idle_seconds: f64,
}

#[derive(Debug)]
struct ClockState {
    turn_active: bool,
    generation: u64,
    last_activity: Option<Instant>,
}

/// Activity clock shared by the loop and the watcher; touch bumps a generation under the lock that
/// snapshot and commit_abort check.
#[derive(Debug)]
pub struct ActivityClock {
    state: Mutex<ClockState>,
}

impl ActivityClock {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ClockState {
                turn_active: false,
                generation: 0,
                last_activity: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ClockState> {
        // A poisoned lock must not wedge the watchdog: take the inner state anyway.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stamp progress; bumps generation.
    pub fn touch(&self) {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        state.last_activity = Some(Instant::now());
    }

    pub fn set_turn_active(&self, active: bool) {
        self.lock().turn_active = active;
    }

    pub fn is_turn_active(&self) -> bool {
        self.lock().turn_active
    }

    /// Stop renewing the lease so a wedge the interrupt cannot unwind expires via TTL.
    pub fn deactivate_turn(&self) {
        self.lock().turn_active = false;
    }

    /// None when the turn is no longer active.
    pub fn snapshot(&self) -> Option<ActivitySnapshot> {
        let state = self.lock();
        if !state.turn_active {
            return None;
        }
        let idle_seconds = match state.last_activity {
            Some(activity) => activity.elapsed().as_secs_f64().max(0.0),
            None => 0.0,
        };
        Some(ActivitySnapshot {
            generation: state.generation,
            idle_seconds,
        })
    }

    /// Revalidate the sampled generation under the same lock; true when the turn has NOT
    /// resumed since the sample (so the abort may commit).
    pub fn commit_abort(&self, snapshot: &ActivitySnapshot) -> bool {
        let state = self.lock();
        state.turn_active && state.generation == snapshot.generation
    }
}

impl Default for ActivityClock {
    fn default() -> Self {
        Self::new()
    }
}

fn poll_duration(poll_s: f64) -> Duration {
    let poll_s = if poll_s.is_finite() {
        poll_s.max(MIN_TURN_LIVENESS_POLL_S)
    } else {
        MIN_TURN_LIVENESS_POLL_S
    };
    Duration::from_secs_f64(poll_s)
}

/// Poll the clock until cancel or the turn ends; once idle reaches timeout_s, call on_stall with
/// whether the abort committed. A committed abort deactivates the turn and stops the watcher.
pub async fn watch_turn_liveness<F>(
    clock: Arc<ActivityClock>,
    timeout_s: f64,
    poll_s: f64,
    cancel: CancellationToken,
    mut on_stall: F,
) where
    F: FnMut(ActivitySnapshot, bool) + Send,
{
    // A resolved timeout <= 0 is opted out; a non-finite one is defended against because
    // the idle comparison would otherwise behave erratically.
    if !timeout_s.is_finite() || timeout_s <= 0.0 {
        return;
    }
    let poll = poll_duration(poll_s);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(poll) => {}
        }
        if cancel.is_cancelled() {
            return;
        }
        let snapshot = match clock.snapshot() {
            Some(snapshot) => snapshot,
            None => return,
        };
        if snapshot.idle_seconds < timeout_s {
            continue;
        }
        let committed = clock.commit_abort(&snapshot);
        on_stall(snapshot, committed);
        if committed {
            clock.deactivate_turn();
            return;
        }
    }
}
