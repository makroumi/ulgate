//! SLO (Service Level Objective) enforcement for ulgate.
//!
//! Tracks latency, error rate, and availability against defined targets.
//! Provides error budget tracking and automated alerting.
//!
//! SLO types:
//!   Latency:      p99 < 500ms, p50 < 100ms
//!   Availability: error rate < 0.1% over 30 days
//!   Throughput:   > 1000 req/sec sustained
//!
//! Error budget:
//!   If availability SLO = 99.9%, error budget = 0.1% of total requests.
//!   When budget is exhausted: freeze deployments, alert on-call.
//!
//! Implementation:
//!   - Sliding window counters for p50/p95/p99 latency
//!   - Rolling 30-day error budget
//!   - Per-tenant SLO tracking
//!   - Automatic SLO violation detection

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// SLO target definition.
#[derive(Debug, Clone)]
pub struct SloTarget {
    pub name: String,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub error_budget_pct: f64,
    pub window_secs: u64,
}

impl SloTarget {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            latency_p50_ms: 100,
            latency_p95_ms: 300,
            latency_p99_ms: 500,
            error_budget_pct: 0.1,
            window_secs: 86_400 * 30,
        }
    }

    pub fn latency_p50(mut self, ms: u64) -> Self { self.latency_p50_ms = ms; self }
    pub fn latency_p95(mut self, ms: u64) -> Self { self.latency_p95_ms = ms; self }
    pub fn latency_p99(mut self, ms: u64) -> Self { self.latency_p99_ms = ms; self }
    pub fn error_budget_pct(mut self, pct: f64) -> Self { self.error_budget_pct = pct; self }
    pub fn window_secs(mut self, secs: u64) -> Self { self.window_secs = secs; self }
}

/// Current SLO status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SloStatus {
    /// All objectives met, healthy error budget.
    Good,
    /// Approaching budget exhaustion (< 10% remaining).
    Warning,
    /// Error budget exhausted. SLO violated.
    Violated,
}

impl std::fmt::Display for SloStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Good => write!(f, "good"),
            Self::Warning => write!(f, "warning"),
            Self::Violated => write!(f, "violated"),
        }
    }
}

/// One request observation for SLO tracking.
#[derive(Debug, Clone)]
struct RequestObs {
    timestamp: Instant,
    latency_ms: u64,
    is_error: bool,
}

/// SLO tracker for a single endpoint or tenant.
pub struct SloTracker {
    target: SloTarget,
    window: Mutex<VecDeque<RequestObs>>,
}

impl SloTracker {
    pub fn new(target: SloTarget) -> Self {
        Self {
            target,
            window: Mutex::new(VecDeque::new()),
        }
    }

    /// Record a request observation.
    pub fn record(&self, latency_ms: u64, is_error: bool) {
        let obs = RequestObs {
            timestamp: Instant::now(),
            latency_ms,
            is_error,
        };
        let mut w = self.window.lock().unwrap();
        w.push_back(obs);
        // Evict old observations outside window
        let cutoff = Duration::from_secs(self.target.window_secs);
        while let Some(front) = w.front() {
            if front.timestamp.elapsed() > cutoff {
                w.pop_front();
            } else {
                break;
            }
        }
    }

    /// Get current SLO report.
    pub fn report(&self) -> SloReport {
        let w = self.window.lock().unwrap();
        let total = w.len() as u64;
        if total == 0 {
            return SloReport {
                name: self.target.name.clone(),
                total_requests: 0,
                error_count: 0,
                error_rate_pct: 0.0,
                error_budget_remaining_pct: 100.0,
                p50_ms: 0,
                p95_ms: 0,
                p99_ms: 0,
                status: SloStatus::Good,
                latency_ok: true,
                budget_ok: true,
            };
        }

        let errors = w.iter().filter(|o| o.is_error).count() as u64;
        let error_rate = errors as f64 / total as f64 * 100.0;

        let mut latencies: Vec<u64> = w.iter().map(|o| o.latency_ms).collect();
        latencies.sort_unstable();

        let p50 = percentile(&latencies, 50);
        let p95 = percentile(&latencies, 95);
        let p99 = percentile(&latencies, 99);

        let budget_used_pct = error_rate;
        let budget_remaining = (self.target.error_budget_pct - budget_used_pct)
            .max(0.0) / self.target.error_budget_pct * 100.0;

        let latency_ok = p50 <= self.target.latency_p50_ms
            && p95 <= self.target.latency_p95_ms
            && p99 <= self.target.latency_p99_ms;

        let budget_ok = error_rate <= self.target.error_budget_pct;

        let status = if !budget_ok || !latency_ok {
            SloStatus::Violated
        } else if budget_remaining < 10.0 {
            SloStatus::Warning
        } else {
            SloStatus::Good
        };

        SloReport {
            name: self.target.name.clone(),
            total_requests: total,
            error_count: errors,
            error_rate_pct: error_rate,
            error_budget_remaining_pct: budget_remaining.min(100.0),
            p50_ms: p50,
            p95_ms: p95,
            p99_ms: p99,
            status,
            latency_ok,
            budget_ok,
        }
    }

    pub fn target(&self) -> &SloTarget { &self.target }
}

/// SLO report snapshot.
#[derive(Debug, Clone)]
pub struct SloReport {
    pub name: String,
    pub total_requests: u64,
    pub error_count: u64,
    pub error_rate_pct: f64,
    pub error_budget_remaining_pct: f64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub status: SloStatus,
    pub latency_ok: bool,
    pub budget_ok: bool,
}

impl SloReport {
    pub fn is_healthy(&self) -> bool {
        self.status == SloStatus::Good
    }

    pub fn summary(&self) -> String {
        format!(
            "{}: {} | p50={}ms p95={}ms p99={}ms | err={:.3}% budget={:.1}%",
            self.name,
            self.status,
            self.p50_ms,
            self.p95_ms,
            self.p99_ms,
            self.error_rate_pct,
            self.error_budget_remaining_pct,
        )
    }
}

/// Multi-endpoint SLO registry.
pub struct SloRegistry {
    trackers: HashMap<String, SloTracker>,
}

impl SloRegistry {
    pub fn new() -> Self {
        Self { trackers: HashMap::new() }
    }

    pub fn register(&mut self, target: SloTarget) {
        let name = target.name.clone();
        self.trackers.insert(name, SloTracker::new(target));
    }

    pub fn record(&self, endpoint: &str, latency_ms: u64, is_error: bool) {
        if let Some(t) = self.trackers.get(endpoint) {
            t.record(latency_ms, is_error);
        }
    }

    pub fn report(&self, endpoint: &str) -> Option<SloReport> {
        self.trackers.get(endpoint).map(|t| t.report())
    }

    pub fn all_reports(&self) -> Vec<SloReport> {
        let mut reports: Vec<SloReport> = self.trackers.values()
            .map(|t| t.report())
            .collect();
        reports.sort_by(|a, b| a.name.cmp(&b.name));
        reports
    }

    pub fn overall_status(&self) -> SloStatus {
        let reports = self.all_reports();
        if reports.iter().any(|r| r.status == SloStatus::Violated) {
            SloStatus::Violated
        } else if reports.iter().any(|r| r.status == SloStatus::Warning) {
            SloStatus::Warning
        } else {
            SloStatus::Good
        }
    }

    pub fn endpoint_count(&self) -> usize { self.trackers.len() }
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() { return 0; }
    let idx = ((pct as f64 / 100.0) * sorted.len() as f64) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker(p99: u64, budget: f64) -> SloTracker {
        SloTracker::new(
            SloTarget::new("test")
                .latency_p50(50)
                .latency_p95(200)
                .latency_p99(p99)
                .error_budget_pct(budget)
                .window_secs(3600),
        )
    }

    #[test]
    fn empty_tracker_good() {
        let t = make_tracker(500, 0.1);
        let r = t.report();
        assert_eq!(r.status, SloStatus::Good);
        assert_eq!(r.total_requests, 0);
    }

    #[test]
    fn healthy_requests_good() {
        let t = make_tracker(500, 1.0);
        for _ in 0..100 {
            t.record(50, false);
        }
        let r = t.report();
        assert_eq!(r.status, SloStatus::Good);
        assert_eq!(r.total_requests, 100);
        assert_eq!(r.error_count, 0);
        assert!(r.latency_ok);
        assert!(r.budget_ok);
    }

    #[test]
    fn high_latency_violates() {
        let t = make_tracker(100, 1.0);
        for _ in 0..100 {
            t.record(500, false);
        }
        let r = t.report();
        assert_eq!(r.status, SloStatus::Violated);
        assert!(!r.latency_ok);
    }

    #[test]
    fn high_error_rate_violates() {
        let t = make_tracker(500, 1.0);
        for _ in 0..90 {
            t.record(10, false);
        }
        for _ in 0..10 {
            t.record(10, true);
        }
        let r = t.report();
        assert!(r.error_rate_pct > 1.0);
        assert_eq!(r.status, SloStatus::Violated);
        assert!(!r.budget_ok);
    }

    #[test]
    fn error_count_tracked() {
        let t = make_tracker(500, 10.0);
        for _ in 0..95 {
            t.record(10, false);
        }
        for _ in 0..5 {
            t.record(10, true);
        }
        let r = t.report();
        assert_eq!(r.error_count, 5);
        assert_eq!(r.total_requests, 100);
        assert!((r.error_rate_pct - 5.0).abs() < 0.1);
    }

    #[test]
    fn latency_percentiles_correct() {
        let t = make_tracker(500, 1.0);
        for i in 1..=100 {
            t.record(i as u64, false);
        }
        let r = t.report();
        assert!(r.p50_ms >= 48 && r.p50_ms <= 52);
        assert!(r.p95_ms >= 93 && r.p95_ms <= 97);
        assert!(r.p99_ms >= 97 && r.p99_ms <= 100);
    }

    #[test]
    fn slo_status_display() {
        assert_eq!(SloStatus::Good.to_string(), "good");
        assert_eq!(SloStatus::Warning.to_string(), "warning");
        assert_eq!(SloStatus::Violated.to_string(), "violated");
    }

    #[test]
    fn report_summary() {
        let t = make_tracker(500, 1.0);
        t.record(10, false);
        let r = t.report();
        let summary = r.summary();
        assert!(summary.contains("test"));
        assert!(summary.contains("p50"));
        assert!(summary.contains("p99"));
    }

    #[test]
    fn registry_multi_endpoint() {
        let mut reg = SloRegistry::new();
        reg.register(SloTarget::new("api/run").latency_p99(500).error_budget_pct(1.0));
        reg.register(SloTarget::new("api/chat").latency_p99(1000).error_budget_pct(1.0));
        assert_eq!(reg.endpoint_count(), 2);

        for _ in 0..100 {
            reg.record("api/run", 50, false);
            reg.record("api/chat", 100, false);
        }

        let run_report = reg.report("api/run").unwrap();
        assert_eq!(run_report.status, SloStatus::Good);

        let all = reg.all_reports();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn registry_overall_status_violated() {
        let mut reg = SloRegistry::new();
        reg.register(SloTarget::new("good").latency_p99(500).error_budget_pct(1.0));
        reg.register(SloTarget::new("bad").latency_p99(50).error_budget_pct(1.0));

        for _ in 0..100 {
            reg.record("good", 10, false);
            reg.record("bad", 500, false); // exceeds p99=50ms
        }

        assert_eq!(reg.overall_status(), SloStatus::Violated);
    }

    #[test]
    fn registry_missing_endpoint_returns_none() {
        let reg = SloRegistry::new();
        assert!(reg.report("nonexistent").is_none());
    }

    #[test]
    fn slo_target_builder() {
        let t = SloTarget::new("workflow")
            .latency_p50(100)
            .latency_p95(300)
            .latency_p99(500)
            .error_budget_pct(0.1)
            .window_secs(3600);
        assert_eq!(t.latency_p99_ms, 500);
        assert_eq!(t.error_budget_pct, 0.1);
    }

    #[test]
    fn is_healthy() {
        let t = make_tracker(500, 1.0);
        t.record(10, false);
        assert!(t.report().is_healthy());
    }

    #[test]
    fn error_budget_exhaustion() {
        let t = make_tracker(500, 1.0);
        // 2% error rate vs 1% budget
        for _ in 0..98 {
            t.record(10, false);
        }
        for _ in 0..2 {
            t.record(10, true);
        }
        let r = t.report();
        assert_eq!(r.status, SloStatus::Violated);
        assert_eq!(r.error_budget_remaining_pct, 0.0);
    }
}
