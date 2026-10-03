//! Per-agent iteration budget: the parent has max_iterations and each subagent its own
//! delegation.max_iterations, so their total can exceed the parent cap.

use std::sync::atomic::{AtomicU32, Ordering};

/// A warning ratio strictly between zero and one, else None (no warning).
pub fn normalize_budget_warning_ratio(value: Option<f64>) -> Option<f64> {
    let ratio = value?;
    if ratio.is_finite() && ratio > 0.0 && ratio < 1.0 {
        Some(ratio)
    } else {
        None
    }
}

/// Lock-free iteration counter. execute_code iterations are refunded so they do not eat into the
/// budget.
#[derive(Debug)]
pub struct IterationBudget {
    max_total: u32,
    used: AtomicU32,
}

impl IterationBudget {
    /// Create a budget that allows at most max_total consumed iterations.
    pub fn new(max_total: u32) -> Self {
        IterationBudget {
            max_total,
            used: AtomicU32::new(0),
        }
    }

    /// Try to consume one iteration. Returns true when allowed, false once the
    /// budget is exhausted (used >= max_total).
    pub fn consume(&self) -> bool {
        let mut current = self.used.load(Ordering::SeqCst);
        loop {
            if current >= self.max_total {
                return false;
            }
            match self.used.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Give back one iteration. Never drops the used count below zero.
    pub fn refund(&self) {
        let mut current = self.used.load(Ordering::SeqCst);
        loop {
            if current == 0 {
                return;
            }
            match self.used.compare_exchange_weak(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Number of iterations consumed so far.
    pub fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    /// Iterations left: max(0, max_total - used).
    pub fn remaining(&self) -> u32 {
        self.max_total
            .saturating_sub(self.used.load(Ordering::SeqCst))
    }

    /// The configured cap.
    pub fn max_total(&self) -> u32 {
        self.max_total
    }
}
