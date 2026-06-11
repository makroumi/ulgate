//! Gateway configuration.

use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub bind_addr: SocketAddr,
    pub db_path: String,
    pub llm_provider: String,
    pub llm_model: String,
    pub api_key: Option<String>,
    pub max_tokens: usize,
    pub cors_enabled: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8080".parse().unwrap(),
            db_path: "./data".into(),
            llm_provider: "groq".into(),
            llm_model: "llama-3.3-70b-versatile".into(),
            api_key: None,
            max_tokens: 4096,
            cors_enabled: true,
        }
    }
}

impl GatewayConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(port) = std::env::var("PORT") {
            if let Ok(p) = port.parse::<u16>() {
                cfg.bind_addr = format!("0.0.0.0:{}", p).parse().unwrap();
            }
        }
        if let Ok(db) = std::env::var("ULGATE_DB") {
            cfg.db_path = db;
        }
        if let Ok(provider) = std::env::var("LLM_PROVIDER") {
            cfg.llm_provider = provider;
        }
        if let Ok(model) = std::env::var("LLM_MODEL") {
            cfg.llm_model = model;
        }
        if let Ok(key) = std::env::var("LLM_API_KEY")
            .or_else(|_| std::env::var("GROQ_API_KEY"))
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .or_else(|_| std::env::var("GEMINI_API_KEY"))
        {
            cfg.api_key = Some(key);
        }

        cfg
    }

    /// Parse "groq:llama-3.3-70b-versatile" or "openai:gpt-4o" etc
    pub fn parse_llm_spec(spec: &str) -> (String, String) {
        if let Some((provider, model)) = spec.split_once(':') {
            (provider.to_string(), model.to_string())
        } else {
            ("groq".into(), spec.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.bind_addr.port(), 8080);
        assert_eq!(cfg.llm_provider, "groq");
    }

    #[test]
    fn parse_llm_spec() {
        let (p, m) = GatewayConfig::parse_llm_spec("openai:gpt-4o");
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-4o");

        let (p2, m2) = GatewayConfig::parse_llm_spec("llama-3.3-70b-versatile");
        assert_eq!(p2, "groq");
        assert_eq!(m2, "llama-3.3-70b-versatile");
    }
}
