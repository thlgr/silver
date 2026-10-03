//! Opt-in spend caps: a daily and/or per-run USD cap checked before a run is admitted. The guard
//! never reads the database; the caller supplies the day's recorded spend and a run estimate.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Warn once projected spend reaches this fraction of a cap, when none (or an invalid one) is set.
pub const DEFAULT_WARN_RATIO: f64 = 0.8;

/// Spend guardrails, matching the daemon's [cost] section.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CostGuardConfig {
    /// Whether the guardrails are enforced.
    pub enabled: bool,
    /// Maximum spend per UTC calendar day. None disables the daily cap.
    pub max_usd_per_day: Option<f64>,
    /// Maximum spend for a single run. None disables the per-run cap.
    pub max_usd_per_run: Option<f64>,
    /// Fraction of a cap at which a warning is emitted. Normalized into
    /// 0 < warn_ratio <= 1; anything else falls back to DEFAULT_WARN_RATIO.
    pub warn_ratio: f64,
}

impl Default for CostGuardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_usd_per_day: None,
            max_usd_per_run: None,
            warn_ratio: DEFAULT_WARN_RATIO,
        }
    }
}

impl CostGuardConfig {
    /// Build a guard config from the daemon's [cost] values.
    pub fn new(
        enabled: bool,
        max_usd_per_day: Option<f64>,
        max_usd_per_run: Option<f64>,
        warn_ratio: f64,
    ) -> Self {
        Self {
            enabled,
            max_usd_per_day,
            max_usd_per_run,
            warn_ratio,
        }
    }
}

/// Why a run was refused by the spend guard.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum CostDenied {
    /// The projected run would push the UTC day's total over the daily cap.
    #[error(
        "daily spend cap reached: ${spent_usd:.4} already spent today plus a ${estimate_usd:.4} run estimate exceeds the ${cap_usd:.4} daily cap"
    )]
    DailyCap {
        /// Spend already recorded for the day.
        spent_usd: f64,
        /// Pre-run estimate for the requested run.
        estimate_usd: f64,
        /// Configured daily cap.
        cap_usd: f64,
    },
    /// The projected run alone exceeds the per-run cap.
    #[error("run estimate ${estimate_usd:.4} exceeds the per-run spend cap of ${cap_usd:.4}")]
    PerRunCap {
        /// Pre-run estimate for the requested run.
        estimate_usd: f64,
        /// Configured per-run cap.
        cap_usd: f64,
    },
}

impl CostDenied {
    /// A clear, user-facing explanation of the refusal.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// A cap that projected spend has approached but not crossed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CostWarning {
    /// The day's projected total is at or above the warning fraction.
    Daily {
        /// Spend so far plus the run estimate.
        projected_usd: f64,
        /// Configured daily cap.
        cap_usd: f64,
    },
    /// The run estimate is at or above the warning fraction of the per-run cap.
    PerRun {
        /// Pre-run estimate for the requested run.
        estimate_usd: f64,
        /// Configured per-run cap.
        cap_usd: f64,
    },
}

impl fmt::Display for CostWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CostWarning::Daily {
                projected_usd,
                cap_usd,
            } => write!(
                f,
                "projected daily spend ${projected_usd:.4} is near the ${cap_usd:.4} daily cap"
            ),
            CostWarning::PerRun {
                estimate_usd,
                cap_usd,
            } => write!(
                f,
                "projected run cost ${estimate_usd:.4} is near the ${cap_usd:.4} per-run cap"
            ),
        }
    }
}

/// Spend guard: one config plus an in-process running total for diagnostics (the database's
/// spend_since is authoritative). record() is lock-free.
#[derive(Debug)]
pub struct CostGuard {
    config: CostGuardConfig,
    recorded: AtomicU64,
}

impl CostGuard {
    /// Build a guard, normalizing out-of-range ratios and caps for directly constructed guards.
    pub fn new(config: CostGuardConfig) -> Self {
        let warn_ratio =
            if config.warn_ratio.is_finite() && config.warn_ratio > 0.0 && config.warn_ratio <= 1.0
            {
                config.warn_ratio
            } else {
                DEFAULT_WARN_RATIO
            };
        Self {
            config: CostGuardConfig {
                enabled: config.enabled,
                max_usd_per_day: sanitize_cap(config.max_usd_per_day),
                max_usd_per_run: sanitize_cap(config.max_usd_per_run),
                warn_ratio,
            },
            recorded: AtomicU64::new(0.0f64.to_bits()),
        }
    }

    /// Build a guard from the daemon's raw [cost] values.
    pub fn from_parts(
        enabled: bool,
        max_usd_per_day: Option<f64>,
        max_usd_per_run: Option<f64>,
        warn_ratio: f64,
    ) -> Self {
        Self::new(CostGuardConfig::new(
            enabled,
            max_usd_per_day,
            max_usd_per_run,
            warn_ratio,
        ))
    }

    /// The normalized configuration.
    pub fn config(&self) -> &CostGuardConfig {
        &self.config
    }

    /// Whether the guardrails are enforced.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Err when a cap would be crossed, Ok(Some) at or above a warning fraction, else Ok(None).
    /// Non-finite or negative amounts count as zero, so bad data never fabricates a denial.
    pub fn evaluate(
        &self,
        daily_spend: f64,
        run_estimate: f64,
    ) -> Result<Option<CostWarning>, CostDenied> {
        if !self.config.enabled {
            return Ok(None);
        }

        let estimate = sanitize_amount(run_estimate);

        if let Some(cap) = self.config.max_usd_per_run {
            if estimate > cap {
                return Err(CostDenied::PerRunCap {
                    estimate_usd: estimate,
                    cap_usd: cap,
                });
            }
        }

        let spent = sanitize_amount(daily_spend);
        if let Some(cap) = self.config.max_usd_per_day {
            let projected = spent + estimate;
            if projected > cap {
                return Err(CostDenied::DailyCap {
                    spent_usd: spent,
                    estimate_usd: estimate,
                    cap_usd: cap,
                });
            }
            if projected >= cap * self.config.warn_ratio {
                return Ok(Some(CostWarning::Daily {
                    projected_usd: projected,
                    cap_usd: cap,
                }));
            }
        }

        if let Some(cap) = self.config.max_usd_per_run {
            if estimate >= cap * self.config.warn_ratio {
                return Ok(Some(CostWarning::PerRun {
                    estimate_usd: estimate,
                    cap_usd: cap,
                }));
            }
        }

        Ok(None)
    }

    /// Refuse a run whose estimate exceeds the per-run cap or would cross the daily one
    /// (`daily_spend` from Db::spend_since), and warn at a cap's warning fraction.
    pub fn check_before_run(&self, daily_spend: f64, run_estimate: f64) -> Result<(), CostDenied> {
        if let Some(warning) = self.evaluate(daily_spend, run_estimate)? {
            match warning {
                CostWarning::Daily {
                    projected_usd,
                    cap_usd,
                } => tracing::warn!(
                    projected_usd,
                    cap_usd,
                    "daily spend is near the configured cost cap"
                ),
                CostWarning::PerRun {
                    estimate_usd,
                    cap_usd,
                } => tracing::warn!(
                    estimate_usd,
                    cap_usd,
                    "projected run cost is near the configured per-run cost cap"
                ),
            }
        }
        Ok(())
    }

    /// Add a completed run's cost to the running total; a non-finite or non-positive cost is
    /// ignored.
    pub fn record(&self, cost_usd: f64) {
        if !cost_usd.is_finite() || cost_usd <= 0.0 {
            return;
        }
        let mut current = self.recorded.load(Ordering::SeqCst);
        loop {
            let next = f64::from_bits(current) + cost_usd;
            match self.recorded.compare_exchange_weak(
                current,
                next.to_bits(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Total cost recorded through CostGuard::record since construction.
    pub fn recorded_usd(&self) -> f64 {
        f64::from_bits(self.recorded.load(Ordering::SeqCst))
    }
}

/// Treat a non-finite or negative amount as zero.
fn sanitize_amount(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

/// Keep only a finite, non-negative cap.
fn sanitize_cap(value: Option<f64>) -> Option<f64> {
    match value {
        Some(value) if value.is_finite() && value >= 0.0 => Some(value),
        _ => None,
    }
}
