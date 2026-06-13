//! ulgate server binary.

use std::sync::{Arc, RwLock};
use uldb::engine::{Engine, EngineConfig};
use ulflow::llm::LLM;
use ulgate::auth::ApiKeyStore;
use ulgate::config::GatewayConfig;
use ulgate::handlers::AppState;
use ulgate::ratelimit::HttpRateLimiter;
use ulgate::server;
use ulgate::tenant::load_registry_from_engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let mut port = None;
    let mut db_path = None;
    let mut llm_spec = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                i += 1;
                port = args.get(i).cloned();
            }
            "--db" | "-d" => {
                i += 1;
                db_path = args.get(i).cloned();
            }
            "--llm" | "-l" => {
                i += 1;
                llm_spec = args.get(i).cloned();
            }
            "--api-key" | "-k" => {
                i += 1;
            }
            "--help" | "-h" => {
                print_help();
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let mut config = GatewayConfig::from_env();
    if let Some(p) = port {
        if let Ok(n) = p.parse::<u16>() {
            config.bind_addr = format!("0.0.0.0:{}", n).parse().unwrap();
        }
    }
    if let Some(d) = db_path {
        config.db_path = d;
    }
    if let Some(spec) = llm_spec {
        let (p, m) = GatewayConfig::parse_llm_spec(&spec);
        config.llm_provider = p;
        config.llm_model = m;
    }

    println!("ulgate v{}", env!("CARGO_PKG_VERSION"));
    println!("db:       {}", config.db_path);
    println!("provider: {}:{}", config.llm_provider, config.llm_model);
    println!();

    let engine = Arc::new(RwLock::new(
        Engine::open(EngineConfig::new(&config.db_path)).expect("failed to open database"),
    ));

    let tenants = {
        let eng = engine.read().unwrap();
        load_registry_from_engine(&eng)
    };

    let llm = build_llm(&config);
    if llm.is_some() {
        println!("LLM ready: {}:{}", config.llm_provider, config.llm_model);
    } else {
        println!("Warning: no LLM configured. Set API key env var.");
        println!("  GROQ_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY");
    }

    println!("Tenants:  {} loaded", tenants.count());

    let registry = Arc::new(ulgate::handlers::build_default_registry(Arc::clone(
        &engine,
    )));
    let mut slo_reg = ulgate::slo::SloRegistry::new();
    slo_reg.register(ulgate::slo::SloTarget::new("api/run").latency_p99(5000).error_budget_pct(1.0));
    slo_reg.register(ulgate::slo::SloTarget::new("api/chat").latency_p99(10000).error_budget_pct(1.0));

    let shadow = Arc::new(ulgate::shadow::TrafficShadow::new(10000));
    shadow.set_enabled(true);

    let probes = Arc::new(ulgate::probes::ProbeState::new());
    probes.set_startup_complete();
    probes.set_ready();

    let audit_path = std::path::Path::new(&config.db_path).join("audit.log");
    let audit = std::sync::Arc::new(std::sync::Mutex::new(
        uldb::storage::audit::AuditLog::open(audit_path).expect("failed to open audit log")
    ));
    println!("Audit:    enabled");

    let state = Arc::new(AppState {
        engine,
        registry,
        llm,
        start_time: std::time::Instant::now(),
        version: env!("CARGO_PKG_VERSION").into(),
        tenants: Arc::new(RwLock::new(tenants)),
        slo: Arc::new(slo_reg),
        degradation: Arc::new(ulgate::degradation::DegradationController::with_defaults()),
        shadow,
        probes,
        shutdown: Arc::new(ulgate::probes::ShutdownController::new()),
        audit,
        gdpr: std::sync::Arc::new(std::sync::Mutex::new(uldb::storage::gdpr::GdprManager::new())),
        oauth: std::sync::Arc::new(ulgate::oauth::OAuthValidator::new()),
        connectors: std::sync::Arc::new(std::sync::Mutex::new(uldb::connector::ConnectorRegistry::new())),
    });

    let auth = if let Ok(key) = std::env::var("ULGATE_API_KEY") {
        println!("Auth:     admin API key required (Bearer token)");
        Arc::new(ApiKeyStore::single(&key))
    } else {
        println!("Auth:     no admin key configured");
        Arc::new(ApiKeyStore::open())
    };

    let rate_limiter = Arc::new(HttpRateLimiter::new(100, 50));
    println!("Rate:     100 req/sec per IP");

    server::run(&config.bind_addr.to_string(), state, auth, rate_limiter).expect("server failed");
}

fn build_llm(config: &GatewayConfig) -> Option<LLM> {
    let api_key = config.api_key.as_deref()?;
    let llm = match config.llm_provider.as_str() {
        "openai" => LLM::openai(&config.llm_model),
        "anthropic" => LLM::anthropic(&config.llm_model),
        "ollama" => LLM::ollama(&config.llm_model),
        "groq" => LLM::groq(&config.llm_model),
        "gemini" => LLM::gemini(&config.llm_model),
        "together" => LLM::together(&config.llm_model),
        "fireworks" => LLM::fireworks(&config.llm_model),
        "mistral" => LLM::mistral(&config.llm_model),
        _ => LLM::custom(&config.llm_provider, &config.llm_model),
    };
    Some(llm.api_key(api_key))
}

fn print_help() {
    println!("ulgate - HTTP API gateway for the ULMEN ecosystem");
    println!();
    println!("USAGE:");
    println!("  ulgate [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("  --port, -p PORT    Listen port (default: 8080)");
    println!("  --db, -d PATH      Database path (default: ./data)");
    println!("  --llm, -l SPEC     LLM: provider:model");
    println!("  --help, -h         Show this help");
    println!();
    println!("ENVIRONMENT:");
    println!("  ULGATE_API_KEY     Admin API key for ulgate auth");
    println!("  GROQ_API_KEY       Groq API key");
    println!("  OPENAI_API_KEY     OpenAI API key");
    println!("  ANTHROPIC_API_KEY  Anthropic API key");
    println!("  GEMINI_API_KEY     Google Gemini API key");
    println!();
}
