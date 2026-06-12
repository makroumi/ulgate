//! Production traffic shadowing for ulgate.
//!
//! Records incoming requests and their responses for:
//!   - Offline replay testing
//!   - Load testing with real traffic patterns
//!   - A/B testing workflow changes
//!   - Debugging production issues
//!
//! Shadowed traffic is stored in a ring buffer (bounded memory).
//! Can be exported for analysis.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// One shadowed request/response pair.
#[derive(Debug, Clone)]
pub struct ShadowedRequest {
    pub timestamp_ms: u64,
    pub method: String,
    pub path: String,
    pub body: String,
    pub response_status: u16,
    pub response_body: String,
    pub latency_ms: u64,
    pub tenant_id: Option<String>,
}

/// Shadow traffic recorder with bounded ring buffer.
pub struct TrafficShadow {
    buffer: Mutex<VecDeque<ShadowedRequest>>,
    capacity: usize,
    enabled: std::sync::atomic::AtomicBool,
    total_recorded: std::sync::atomic::AtomicU64,
    /// Sample rate: 1.0 = record all, 0.1 = record 10%.
    sample_rate: f64,
}

impl TrafficShadow {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            enabled: std::sync::atomic::AtomicBool::new(false),
            total_recorded: std::sync::atomic::AtomicU64::new(0),
            sample_rate: 1.0,
        }
    }

    pub fn with_sample_rate(mut self, rate: f64) -> Self {
        self.sample_rate = rate.clamp(0.0, 1.0);
        self
    }

    /// Enable/disable recording.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record a request/response pair.
    pub fn record(
        &self,
        method: &str,
        path: &str,
        body: &str,
        response_status: u16,
        response_body: &str,
        latency_ms: u64,
        tenant_id: Option<&str>,
    ) {
        if !self.is_enabled() {
            return;
        }

        // Sampling
        if self.sample_rate < 1.0 {
            let hash = simple_hash(path.as_bytes()) as f64 / u64::MAX as f64;
            if hash > self.sample_rate {
                return;
            }
        }

        let req = ShadowedRequest {
            timestamp_ms: now_ms(),
            method: method.to_string(),
            path: path.to_string(),
            body: body.chars().take(4096).collect(),
            response_status,
            response_body: response_body.chars().take(4096).collect(),
            latency_ms,
            tenant_id: tenant_id.map(String::from),
        };

        let mut buf = self.buffer.lock().unwrap();
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(req);
        self.total_recorded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Get recent shadowed requests.
    pub fn recent(&self, limit: usize) -> Vec<ShadowedRequest> {
        let buf = self.buffer.lock().unwrap();
        buf.iter().rev().take(limit).cloned().collect()
    }

    /// Export all buffered requests.
    pub fn export_all(&self) -> Vec<ShadowedRequest> {
        let buf = self.buffer.lock().unwrap();
        buf.iter().cloned().collect()
    }

    /// Filter by path prefix.
    pub fn filter_by_path(&self, prefix: &str) -> Vec<ShadowedRequest> {
        let buf = self.buffer.lock().unwrap();
        buf.iter()
            .filter(|r| r.path.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// Filter by tenant.
    pub fn filter_by_tenant(&self, tenant_id: &str) -> Vec<ShadowedRequest> {
        let buf = self.buffer.lock().unwrap();
        buf.iter()
            .filter(|r| r.tenant_id.as_deref() == Some(tenant_id))
            .cloned()
            .collect()
    }

    /// Filter by status code.
    pub fn filter_errors(&self) -> Vec<ShadowedRequest> {
        let buf = self.buffer.lock().unwrap();
        buf.iter()
            .filter(|r| r.response_status >= 400)
            .cloned()
            .collect()
    }

    /// Stats.
    pub fn total_recorded(&self) -> u64 {
        self.total_recorded.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn buffer_size(&self) -> usize {
        self.buffer.lock().unwrap().len()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Clear the buffer.
    pub fn clear(&self) {
        self.buffer.lock().unwrap().clear();
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn simple_hash(data: &[u8]) -> u64 {
    let mut h: u64 = 0x9e3779b97f4a7c15;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x517cc1b727220a95);
        h = h.rotate_left(31);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default() {
        let shadow = TrafficShadow::new(100);
        assert!(!shadow.is_enabled());
        shadow.record("GET", "/health", "", 200, "{}", 10, None);
        assert_eq!(shadow.buffer_size(), 0);
    }

    #[test]
    fn record_when_enabled() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        shadow.record("GET", "/health", "", 200, "{}", 10, None);
        assert_eq!(shadow.buffer_size(), 1);
        assert_eq!(shadow.total_recorded(), 1);
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let shadow = TrafficShadow::new(3);
        shadow.set_enabled(true);
        for i in 0..5 {
            shadow.record("GET", &format!("/path/{}", i), "", 200, "{}", 10, None);
        }
        assert_eq!(shadow.buffer_size(), 3);
        let recent = shadow.recent(10);
        assert!(recent[0].path.contains("4"));
    }

    #[test]
    fn recent_returns_newest_first() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        shadow.record("GET", "/first", "", 200, "{}", 10, None);
        shadow.record("GET", "/second", "", 200, "{}", 10, None);
        let recent = shadow.recent(10);
        assert_eq!(recent[0].path, "/second");
        assert_eq!(recent[1].path, "/first");
    }

    #[test]
    fn filter_by_path() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        shadow.record("POST", "/v1/run", "{}", 200, "{}", 100, None);
        shadow.record("GET", "/v1/health", "", 200, "{}", 5, None);
        shadow.record("POST", "/v1/run", "{}", 200, "{}", 150, None);

        let runs = shadow.filter_by_path("/v1/run");
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn filter_by_tenant() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        shadow.record("GET", "/", "", 200, "{}", 10, Some("acme"));
        shadow.record("GET", "/", "", 200, "{}", 10, Some("beta"));
        shadow.record("GET", "/", "", 200, "{}", 10, Some("acme"));

        let acme = shadow.filter_by_tenant("acme");
        assert_eq!(acme.len(), 2);
    }

    #[test]
    fn filter_errors() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        shadow.record("GET", "/ok", "", 200, "{}", 10, None);
        shadow.record("POST", "/bad", "{}", 400, "error", 10, None);
        shadow.record("POST", "/fail", "{}", 500, "error", 10, None);

        let errors = shadow.filter_errors();
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn export_all() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        for i in 0..10 {
            shadow.record("GET", &format!("/{}", i), "", 200, "{}", i as u64, None);
        }
        let all = shadow.export_all();
        assert_eq!(all.len(), 10);
    }

    #[test]
    fn clear_buffer() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        shadow.record("GET", "/", "", 200, "{}", 10, None);
        assert_eq!(shadow.buffer_size(), 1);
        shadow.clear();
        assert_eq!(shadow.buffer_size(), 0);
        assert_eq!(shadow.total_recorded(), 1); // total not reset
    }

    #[test]
    fn body_truncated() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        let big = "x".repeat(10000);
        shadow.record("POST", "/", &big, 200, &big, 10, None);
        let recent = shadow.recent(1);
        assert!(recent[0].body.len() <= 4096);
        assert!(recent[0].response_body.len() <= 4096);
    }

    #[test]
    fn latency_and_timestamp_recorded() {
        let shadow = TrafficShadow::new(100);
        shadow.set_enabled(true);
        shadow.record("GET", "/", "", 200, "{}", 42, None);
        let recent = shadow.recent(1);
        assert_eq!(recent[0].latency_ms, 42);
        assert!(recent[0].timestamp_ms > 0);
    }
}
