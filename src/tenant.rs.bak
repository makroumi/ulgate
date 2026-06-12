//! Multi-tenant isolation layer.
//!
//! A Tenant is the hard boundary of the system:
//!   - Each tenant has a unique ID and API key
//!   - Each tenant has its own namespace in uldb (data isolation)
//!   - Each tenant has capability constraints (what agents can do)
//!   - Each tenant has resource quotas (tokens, requests, storage)
//!   - Tenants cannot see each other's data, sessions, or workflows
//!
//! This is the "platform vs tool" boundary.
//!
//! ```text
//!   API Key "sk-tenant-abc" -> Tenant "acme_corp"
//!     namespace: 0xABCD...
//!     capabilities: [tool:code_search, tool:file_read, llm:groq, llm:budget:100000]
//!     quotas: 100K tokens/day, 1000 requests/hour, 10MB storage
//!     sessions: isolated to this tenant
//!     workflows: isolated to this tenant
//!     runs: isolated to this tenant
//! ```

use std::collections::HashMap;
use std::sync::RwLock;
use ulmp::crypto::sha256::sha256;

/// Resource quotas for a tenant.
#[derive(Debug, Clone)]
pub struct TenantQuota {
    /// Max tokens per day (0 = unlimited).
    pub max_tokens_per_day: u64,
    /// Max requests per hour (0 = unlimited).
    pub max_requests_per_hour: u64,
    /// Max storage bytes (0 = unlimited).
    pub max_storage_bytes: u64,
    /// Max concurrent sessions.
    pub max_sessions: u32,
    /// Max registered workflows.
    pub max_workflows: u32,
}

impl Default for TenantQuota {
    fn default() -> Self {
        Self {
            max_tokens_per_day: 0,
            max_requests_per_hour: 0,
            max_storage_bytes: 0,
            max_sessions: 100,
            max_workflows: 50,
        }
    }
}

impl TenantQuota {
    pub fn unlimited() -> Self {
        Self::default()
    }

    pub fn starter() -> Self {
        Self {
            max_tokens_per_day: 100_000,
            max_requests_per_hour: 500,
            max_storage_bytes: 10 * 1024 * 1024,
            max_sessions: 10,
            max_workflows: 5,
        }
    }

    pub fn pro() -> Self {
        Self {
            max_tokens_per_day: 1_000_000,
            max_requests_per_hour: 5_000,
            max_storage_bytes: 100 * 1024 * 1024,
            max_sessions: 100,
            max_workflows: 50,
        }
    }

    pub fn enterprise() -> Self {
        Self {
            max_tokens_per_day: 0, // unlimited
            max_requests_per_hour: 0,
            max_storage_bytes: 0,
            max_sessions: 1000,
            max_workflows: 500,
        }
    }
}

/// Usage tracking for a tenant.
#[derive(Debug, Clone)]
pub struct TenantUsage {
    pub tokens_today: u64,
    pub requests_this_hour: u64,
    pub storage_bytes: u64,
    pub active_sessions: u32,
    pub registered_workflows: u32,
    pub total_runs: u64,
    pub total_tokens: u64,
    /// Timestamp of last usage reset (day boundary).
    pub last_daily_reset: u64,
    /// Timestamp of last hourly reset.
    pub last_hourly_reset: u64,
}

impl Default for TenantUsage {
    fn default() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            tokens_today: 0, requests_this_hour: 0, storage_bytes: 0,
            active_sessions: 0, registered_workflows: 0,
            total_runs: 0, total_tokens: 0,
            last_daily_reset: now, last_hourly_reset: now,
        }
    }
}

impl TenantUsage {
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Reset daily counters if a new day has started.
    pub fn maybe_reset_daily(&mut self) {
        let now = Self::now_ms();
        let day_ms = 86_400_000u64;
        if now - self.last_daily_reset > day_ms {
            self.tokens_today = 0;
            self.last_daily_reset = now;
        }
    }

    /// Reset hourly counters if a new hour has started.
    pub fn maybe_reset_hourly(&mut self) {
        let now = Self::now_ms();
        let hour_ms = 3_600_000u64;
        if now - self.last_hourly_reset > hour_ms {
            self.requests_this_hour = 0;
            self.last_hourly_reset = now;
        }
    }
}

/// A tenant definition.
#[derive(Debug, Clone)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    /// SHA-256 hash of the API key (never store raw keys).
    pub api_key_hash: [u8; 32],
    /// Namespace ID in uldb (derived from tenant ID).
    pub namespace_id: u64,
    /// Capability strings for this tenant's agents.
    pub capabilities: Vec<String>,
    /// Resource quotas.
    pub quota: TenantQuota,
    /// Plan name (for display/billing).
    pub plan: String,
}

impl Tenant {
    pub fn new(id: impl Into<String>, name: impl Into<String>, api_key: &str) -> Self {
        let id = id.into();
        let namespace_id = uldb::namespace::derive_namespace_id(&id, "");
        Self {
            id: id.clone(),
            name: name.into(),
            api_key_hash: sha256(api_key.as_bytes()),
            namespace_id,
            capabilities: vec![
                "tool:*".into(),
                "llm:*".into(),
                "db:read".into(),
                "db:write".into(),
                "db:search".into(),
                "session:create".into(),
                "session:read".into(),
                "workflow:execute".into(),
                "workflow:register".into(),
            ],
            quota: TenantQuota::default(),
            plan: "default".into(),
        }
    }

    pub fn with_plan(mut self, plan: &str) -> Self {
        self.plan = plan.into();
        self.quota = match plan {
            "starter" => TenantQuota::starter(),
            "pro" => TenantQuota::pro(),
            "enterprise" => TenantQuota::enterprise(),
            _ => TenantQuota::unlimited(),
        };
        self
    }

    pub fn with_capabilities(mut self, caps: Vec<String>) -> Self {
        self.capabilities = caps;
        self
    }

    pub fn with_quota(mut self, quota: TenantQuota) -> Self {
        self.quota = quota;
        self
    }

    /// Get the namespace prefix for scoping keys in uldb.
    pub fn db_prefix(&self, key_type: &str) -> String {
        format!("t:{}:{}", self.id, key_type)
    }

    /// Scope a session key to this tenant.
    pub fn session_key(&self, session_id: &str) -> String {
        format!("t:{}:session:{}", self.id, session_id)
    }

    /// Scope a workflow key to this tenant.
    pub fn workflow_key(&self, workflow_name: &str) -> String {
        format!("t:{}:workflow:{}", self.id, workflow_name)
    }

    /// Scope a run key to this tenant.
    pub fn run_key(&self, run_id: &str) -> String {
        format!("t:{}:run:{}", self.id, run_id)
    }

    /// Scope a document key to this tenant.
    pub fn doc_key(&self, key: &str) -> String {
        format!("t:{}:doc:{}", self.id, key)
    }
}

/// Multi-tenant registry.
pub struct TenantRegistry {
    /// Tenants by ID.
    tenants: HashMap<String, Tenant>,
    /// API key hash -> tenant ID (for auth lookup).
    key_to_tenant: HashMap<[u8; 32], String>,
    /// Usage tracking per tenant.
    usage: RwLock<HashMap<String, TenantUsage>>,
}

impl TenantRegistry {
    pub fn new() -> Self {
        Self {
            tenants: HashMap::new(),
            key_to_tenant: HashMap::new(),
            usage: RwLock::new(HashMap::new()),
        }
    }

    /// Register a tenant.
    pub fn register(&mut self, tenant: Tenant) {
        self.key_to_tenant
            .insert(tenant.api_key_hash, tenant.id.clone());
        self.usage
            .write()
            .unwrap()
            .insert(tenant.id.clone(), TenantUsage::default());
        self.tenants.insert(tenant.id.clone(), tenant);
    }

    /// Look up tenant by API key (constant-time hash comparison).
    pub fn authenticate(&self, api_key: &str) -> Option<&Tenant> {
        let hash = sha256(api_key.as_bytes());
        let tenant_id = self.key_to_tenant.get(&hash)?;
        self.tenants.get(tenant_id)
    }

    /// Get tenant by ID.
    pub fn get(&self, tenant_id: &str) -> Option<&Tenant> {
        self.tenants.get(tenant_id)
    }

    /// Check if a request is within the tenant's quota.
    pub fn check_quota(&self, tenant_id: &str) -> Result<(), QuotaExceeded> {
        let mut usage_map = self.usage.write().unwrap();
        let usage = usage_map.entry(tenant_id.into()).or_default();

        // Reset counters if time windows have passed
        usage.maybe_reset_daily();
        usage.maybe_reset_hourly();

        let tenant = self.tenants.get(tenant_id).ok_or(QuotaExceeded {
            tenant: tenant_id.into(),
            resource: "tenant".into(),
            limit: 0,
            current: 0,
            message: "tenant not found".into(),
        })?;

        // Check hourly request limit
        if tenant.quota.max_requests_per_hour > 0
            && usage.requests_this_hour >= tenant.quota.max_requests_per_hour
        {
            return Err(QuotaExceeded {
                tenant: tenant_id.into(),
                resource: "requests_per_hour".into(),
                limit: tenant.quota.max_requests_per_hour,
                current: usage.requests_this_hour,
                message: "hourly request limit exceeded".into(),
            });
        }

        // Check daily token limit
        if tenant.quota.max_tokens_per_day > 0
            && usage.tokens_today >= tenant.quota.max_tokens_per_day
        {
            return Err(QuotaExceeded {
                tenant: tenant_id.into(),
                resource: "tokens_per_day".into(),
                limit: tenant.quota.max_tokens_per_day,
                current: usage.tokens_today,
                message: "daily token limit exceeded".into(),
            });
        }

        Ok(())
    }

    /// Record usage for a tenant.
    pub fn record_usage(&self, tenant_id: &str, tokens: u64) {
        let mut usage_map = self.usage.write().unwrap();
        let usage = usage_map.entry(tenant_id.into()).or_default();
        usage.tokens_today += tokens;
        usage.total_tokens += tokens;
        usage.requests_this_hour += 1;
        usage.total_runs += 1;
    }

    /// Get usage for a tenant.
    pub fn get_usage(&self, tenant_id: &str) -> TenantUsage {
        self.usage
            .read()
            .unwrap()
            .get(tenant_id)
            .cloned()
            .unwrap_or_default()
    }

    /// List all tenants (admin only).
    pub fn list(&self) -> Vec<&Tenant> {
        self.tenants.values().collect()
    }

    pub fn count(&self) -> usize {
        self.tenants.len()
    }
}

/// Quota exceeded error.
#[derive(Debug, Clone)]
pub struct QuotaExceeded {
    pub tenant: String,
    pub resource: String,
    pub limit: u64,
    pub current: u64,
    pub message: String,
}

impl std::fmt::Display for QuotaExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "quota exceeded for tenant {:?}: {} ({}/{} {})",
            self.tenant, self.message, self.current, self.limit, self.resource
        )
    }
}

impl std::error::Error for QuotaExceeded {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_creation() {
        let t = Tenant::new("acme", "Acme Corp", "sk-acme-secret");
        assert_eq!(t.id, "acme");
        assert_eq!(t.name, "Acme Corp");
        assert!(t.namespace_id > 0);
        assert!(!t.capabilities.is_empty());
    }

    #[test]
    fn tenant_plans() {
        let starter = Tenant::new("s", "S", "k").with_plan("starter");
        assert_eq!(starter.quota.max_tokens_per_day, 100_000);
        assert_eq!(starter.quota.max_requests_per_hour, 500);

        let pro = Tenant::new("p", "P", "k").with_plan("pro");
        assert_eq!(pro.quota.max_tokens_per_day, 1_000_000);

        let ent = Tenant::new("e", "E", "k").with_plan("enterprise");
        assert_eq!(ent.quota.max_tokens_per_day, 0); // unlimited
    }

    #[test]
    fn tenant_key_scoping() {
        let t = Tenant::new("acme", "Acme", "k");
        assert_eq!(t.session_key("s1"), "t:acme:session:s1");
        assert_eq!(t.workflow_key("review"), "t:acme:workflow:review");
        assert_eq!(t.run_key("run_1"), "t:acme:run:run_1");
        assert_eq!(t.doc_key("auth.py"), "t:acme:doc:auth.py");
    }

    #[test]
    fn registry_auth() {
        let mut reg = TenantRegistry::new();
        reg.register(Tenant::new("acme", "Acme Corp", "sk-acme-123"));
        reg.register(Tenant::new("beta", "Beta Inc", "sk-beta-456"));

        let t = reg.authenticate("sk-acme-123").unwrap();
        assert_eq!(t.id, "acme");

        let t2 = reg.authenticate("sk-beta-456").unwrap();
        assert_eq!(t2.id, "beta");

        assert!(reg.authenticate("sk-wrong").is_none());
    }

    #[test]
    fn registry_isolation() {
        let mut reg = TenantRegistry::new();
        reg.register(Tenant::new("a", "A", "ka"));
        reg.register(Tenant::new("b", "B", "kb"));

        let ta = reg.get("a").unwrap();
        let tb = reg.get("b").unwrap();

        // Different namespace IDs
        assert_ne!(ta.namespace_id, tb.namespace_id);

        // Different key scoping
        assert_ne!(ta.doc_key("file.py"), tb.doc_key("file.py"));
    }

    #[test]
    fn quota_unlimited() {
        let mut reg = TenantRegistry::new();
        reg.register(Tenant::new("t", "T", "k"));

        // Unlimited quota: always passes
        for _ in 0..10000 {
            assert!(reg.check_quota("t").is_ok());
        }
    }

    #[test]
    fn quota_hourly_limit() {
        let mut reg = TenantRegistry::new();
        reg.register(Tenant::new("t", "T", "k").with_plan("starter"));

        // Starter: 500 requests/hour
        for _ in 0..500 {
            reg.record_usage("t", 10);
        }

        // 501st request should fail
        assert!(reg.check_quota("t").is_err());
    }

    #[test]
    fn quota_daily_tokens() {
        let mut reg = TenantRegistry::new();
        reg.register(Tenant::new("t", "T", "k").with_plan("starter"));

        // Use up the daily token budget (100K)
        reg.record_usage("t", 100_000);

        // Next request should fail on token budget
        assert!(reg.check_quota("t").is_err());
    }

    #[test]
    fn usage_tracking() {
        let mut reg = TenantRegistry::new();
        reg.register(Tenant::new("t", "T", "k"));

        reg.record_usage("t", 500);
        reg.record_usage("t", 300);

        let usage = reg.get_usage("t");
        assert_eq!(usage.total_tokens, 800);
        assert_eq!(usage.total_runs, 2);
    }

    #[test]
    fn tenant_custom_capabilities() {
        let t = Tenant::new("restricted", "R", "k").with_capabilities(vec![
            "tool:code_search".into(),
            "db:read".into(),
            "llm:groq".into(),
        ]);
        assert_eq!(t.capabilities.len(), 3);
        assert!(t.capabilities.contains(&"tool:code_search".to_string()));
        assert!(!t.capabilities.contains(&"tool:file_write".to_string()));
    }

    #[test]
    fn quota_exceeded_display() {
        let e = QuotaExceeded {
            tenant: "acme".into(),
            resource: "tokens_per_day".into(),
            limit: 100000,
            current: 100001,
            message: "daily limit exceeded".into(),
        };
        assert!(e.to_string().contains("acme"));
        assert!(e.to_string().contains("100000"));
    }

    #[test]
    fn list_tenants() {
        let mut reg = TenantRegistry::new();
        reg.register(Tenant::new("a", "A", "ka"));
        reg.register(Tenant::new("b", "B", "kb"));
        assert_eq!(reg.count(), 2);
        assert_eq!(reg.list().len(), 2);
    }
}
