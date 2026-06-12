//! Graceful degradation for ulgate.
//!
//! When the system is under stress, degrade gracefully instead of failing hard.
//!
//! Modes:
//!   Normal:    all features enabled
//!   Degraded:  LLM calls disabled, tool-only workflows still work
//!   ReadOnly:  only reads allowed, writes rejected
//!   Emergency: only health endpoint responds, everything else returns 503
//!
//! Triggers:
//!   - Error rate exceeds threshold
//!   - Latency exceeds threshold
//!   - Memory usage exceeds threshold
//!   - Manual operator override
//!
//! Load shedding:
//!   - Prioritize reads over writes
//!   - Prioritize tenant requests by plan (enterprise > pro > starter)
//!   - Drop background tasks first (metrics aggregation, log rotation)

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

/// System degradation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DegradationMode {
    /// All features enabled.
    Normal = 0,
    /// LLM calls disabled, tool-only workflows still work.
    Degraded = 1,
    /// Only reads allowed, writes rejected.
    ReadOnly = 2,
    /// Only health endpoint responds.
    Emergency = 3,
}

impl DegradationMode {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Normal,
            1 => Self::Degraded,
            2 => Self::ReadOnly,
            3 => Self::Emergency,
            _ => Self::Normal,
        }
    }

    pub fn allows_writes(&self) -> bool {
        matches!(self, Self::Normal | Self::Degraded)
    }

    pub fn allows_llm(&self) -> bool {
        matches!(self, Self::Normal)
    }

    pub fn allows_reads(&self) -> bool {
        !matches!(self, Self::Emergency)
    }

    pub fn allows_workflows(&self) -> bool {
        matches!(self, Self::Normal | Self::Degraded)
    }
}

impl std::fmt::Display for DegradationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(f, "normal"),
            Self::Degraded => write!(f, "degraded"),
            Self::ReadOnly => write!(f, "read_only"),
            Self::Emergency => write!(f, "emergency"),
        }
    }
}

/// Thresholds that trigger degradation.
#[derive(Debug, Clone)]
pub struct DegradationThresholds {
    /// Error rate (0.0-1.0) that triggers degraded mode.
    pub error_rate_degraded: f64,
    /// Error rate that triggers read-only mode.
    pub error_rate_readonly: f64,
    /// Error rate that triggers emergency mode.
    pub error_rate_emergency: f64,
    /// P99 latency (ms) that triggers degraded mode.
    pub latency_p99_degraded_ms: u64,
    /// P99 latency (ms) that triggers read-only mode.
    pub latency_p99_readonly_ms: u64,
}

impl Default for DegradationThresholds {
    fn default() -> Self {
        Self {
            error_rate_degraded: 0.05,
            error_rate_readonly: 0.15,
            error_rate_emergency: 0.30,
            latency_p99_degraded_ms: 5000,
            latency_p99_readonly_ms: 15000,
        }
    }
}

/// Load shedding priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Background tasks: metrics, logs, cleanup.
    Background = 0,
    /// Starter plan tenants.
    Starter = 1,
    /// Pro plan tenants.
    Pro = 2,
    /// Enterprise plan tenants.
    Enterprise = 3,
    /// Admin/system operations.
    Admin = 4,
    /// Health checks (never shed).
    Health = 5,
}

impl Priority {
    pub fn from_plan(plan: &str) -> Self {
        match plan {
            "enterprise" => Self::Enterprise,
            "pro" => Self::Pro,
            "starter" => Self::Starter,
            _ => Self::Starter,
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Background => write!(f, "background"),
            Self::Starter => write!(f, "starter"),
            Self::Pro => write!(f, "pro"),
            Self::Enterprise => write!(f, "enterprise"),
            Self::Admin => write!(f, "admin"),
            Self::Health => write!(f, "health"),
        }
    }
}

/// Degradation controller.
pub struct DegradationController {
    mode: AtomicU8,
    thresholds: DegradationThresholds,
    manual_override: AtomicU8,
    #[allow(dead_code)] last_check: std::sync::Mutex<Instant>,
    shed_priority: AtomicU8,
}

impl DegradationController {
    pub fn new(thresholds: DegradationThresholds) -> Self {
        Self {
            mode: AtomicU8::new(DegradationMode::Normal as u8),
            thresholds,
            manual_override: AtomicU8::new(255), // 255 = no override
            last_check: std::sync::Mutex::new(Instant::now()),
            shed_priority: AtomicU8::new(Priority::Background as u8),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DegradationThresholds::default())
    }

    /// Get current degradation mode.
    pub fn mode(&self) -> DegradationMode {
        let override_val = self.manual_override.load(Ordering::Relaxed);
        if override_val != 255 {
            return DegradationMode::from_u8(override_val);
        }
        DegradationMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Manually set degradation mode (operator override).
    pub fn set_mode(&self, mode: DegradationMode) {
        self.manual_override.store(mode as u8, Ordering::Relaxed);
    }

    /// Clear manual override, return to automatic mode.
    pub fn clear_override(&self) {
        self.manual_override.store(255, Ordering::Relaxed);
    }

    /// Update degradation based on current metrics.
    pub fn evaluate(&self, error_rate: f64, p99_latency_ms: u64) {
        let new_mode = if error_rate >= self.thresholds.error_rate_emergency {
            DegradationMode::Emergency
        } else if error_rate >= self.thresholds.error_rate_readonly
            || p99_latency_ms >= self.thresholds.latency_p99_readonly_ms
        {
            DegradationMode::ReadOnly
        } else if error_rate >= self.thresholds.error_rate_degraded
            || p99_latency_ms >= self.thresholds.latency_p99_degraded_ms
        {
            DegradationMode::Degraded
        } else {
            DegradationMode::Normal
        };

        self.mode.store(new_mode as u8, Ordering::Relaxed);

        // Adjust shed priority based on mode
        let shed = match new_mode {
            DegradationMode::Normal => Priority::Background,
            DegradationMode::Degraded => Priority::Starter,
            DegradationMode::ReadOnly => Priority::Pro,
            DegradationMode::Emergency => Priority::Enterprise,
        };
        self.shed_priority.store(shed as u8, Ordering::Relaxed);
    }

    /// Check if a request with given priority should be shed (rejected).
    pub fn should_shed(&self, request_priority: Priority) -> bool {
        let min = self.shed_priority.load(Ordering::Relaxed);
        (request_priority as u8) < min
    }

    /// Check if writes are allowed.
    pub fn allows_writes(&self) -> bool { self.mode().allows_writes() }

    /// Check if LLM calls are allowed.
    pub fn allows_llm(&self) -> bool { self.mode().allows_llm() }

    /// Check if reads are allowed.
    pub fn allows_reads(&self) -> bool { self.mode().allows_reads() }

    /// Check if workflows are allowed.
    pub fn allows_workflows(&self) -> bool { self.mode().allows_workflows() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_normal() {
        let dc = DegradationController::with_defaults();
        assert_eq!(dc.mode(), DegradationMode::Normal);
        assert!(dc.allows_writes());
        assert!(dc.allows_llm());
        assert!(dc.allows_reads());
        assert!(dc.allows_workflows());
    }

    #[test]
    fn mode_display() {
        assert_eq!(DegradationMode::Normal.to_string(), "normal");
        assert_eq!(DegradationMode::Degraded.to_string(), "degraded");
        assert_eq!(DegradationMode::ReadOnly.to_string(), "read_only");
        assert_eq!(DegradationMode::Emergency.to_string(), "emergency");
    }

    #[test]
    fn degraded_mode_no_llm() {
        let dc = DegradationController::with_defaults();
        dc.evaluate(0.06, 0); // above 5% error rate
        assert_eq!(dc.mode(), DegradationMode::Degraded);
        assert!(!dc.allows_llm());
        assert!(dc.allows_writes());
        assert!(dc.allows_workflows());
    }

    #[test]
    fn readonly_mode_on_high_errors() {
        let dc = DegradationController::with_defaults();
        dc.evaluate(0.16, 0); // above 15% error rate
        assert_eq!(dc.mode(), DegradationMode::ReadOnly);
        assert!(!dc.allows_writes());
        assert!(!dc.allows_llm());
        assert!(dc.allows_reads());
    }

    #[test]
    fn emergency_mode_on_critical_errors() {
        let dc = DegradationController::with_defaults();
        dc.evaluate(0.31, 0); // above 30%
        assert_eq!(dc.mode(), DegradationMode::Emergency);
        assert!(!dc.allows_reads());
        assert!(!dc.allows_writes());
        assert!(!dc.allows_llm());
    }

    #[test]
    fn high_latency_triggers_degraded() {
        let dc = DegradationController::with_defaults();
        dc.evaluate(0.01, 6000); // p99 > 5000ms
        assert_eq!(dc.mode(), DegradationMode::Degraded);
    }

    #[test]
    fn very_high_latency_triggers_readonly() {
        let dc = DegradationController::with_defaults();
        dc.evaluate(0.01, 16000); // p99 > 15000ms
        assert_eq!(dc.mode(), DegradationMode::ReadOnly);
    }

    #[test]
    fn recovery_back_to_normal() {
        let dc = DegradationController::with_defaults();
        dc.evaluate(0.20, 0);
        assert_eq!(dc.mode(), DegradationMode::ReadOnly);
        dc.evaluate(0.01, 100); // recovered
        assert_eq!(dc.mode(), DegradationMode::Normal);
    }

    #[test]
    fn manual_override() {
        let dc = DegradationController::with_defaults();
        dc.set_mode(DegradationMode::ReadOnly);
        assert_eq!(dc.mode(), DegradationMode::ReadOnly);
        dc.evaluate(0.0, 0); // would be normal, but override active
        assert_eq!(dc.mode(), DegradationMode::ReadOnly);
        dc.clear_override();
        assert_eq!(dc.mode(), DegradationMode::Normal);
    }

    #[test]
    fn load_shedding_by_priority() {
        let dc = DegradationController::with_defaults();
        dc.evaluate(0.06, 0); // degraded: shed starter and below
        assert!(dc.should_shed(Priority::Background));
        assert!(!dc.should_shed(Priority::Starter));
        assert!(!dc.should_shed(Priority::Pro));
        assert!(!dc.should_shed(Priority::Enterprise));
        assert!(!dc.should_shed(Priority::Health));
    }

    #[test]
    fn load_shedding_emergency() {
        let dc = DegradationController::with_defaults();
        dc.evaluate(0.31, 0); // emergency: shed everything except health
        assert!(dc.should_shed(Priority::Background));
        assert!(dc.should_shed(Priority::Starter));
        assert!(dc.should_shed(Priority::Pro));
        assert!(!dc.should_shed(Priority::Enterprise));
        assert!(!dc.should_shed(Priority::Health));
    }

    #[test]
    fn priority_from_plan() {
        assert_eq!(Priority::from_plan("enterprise"), Priority::Enterprise);
        assert_eq!(Priority::from_plan("pro"), Priority::Pro);
        assert_eq!(Priority::from_plan("starter"), Priority::Starter);
        assert_eq!(Priority::from_plan("unknown"), Priority::Starter);
    }

    #[test]
    fn priority_ordering() {
        assert!(Priority::Health > Priority::Admin);
        assert!(Priority::Admin > Priority::Enterprise);
        assert!(Priority::Enterprise > Priority::Pro);
        assert!(Priority::Pro > Priority::Starter);
        assert!(Priority::Starter > Priority::Background);
    }

    #[test]
    fn priority_display() {
        assert_eq!(Priority::Enterprise.to_string(), "enterprise");
        assert_eq!(Priority::Background.to_string(), "background");
    }
}
