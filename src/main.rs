//! ulgate server binary.
//!
//! Usage:
//!   ulgate [--port 8080] [--db ./data] [--llm groq:llama-3.3-70b-versatile]
//!
//! Environment variables:
//!   PORT              listen port (default: 8080)
//!   ULGATE_DB         database path (default: ./data)
//!   LLM_PROVIDER      llm provider: groq, openai, anthropic, ollama, gemini
//!   LLM_MODEL         model name
//!   GROQ_API_KEY      Groq API key
//!   OPENAI_API_KEY    OpenAI API key
//!   ANTHROPIC_API_KEY Anthropic API key
//!   GEMINI_API_KEY    Google Gemini API key

use std::sync::{Arc, RwLock};
use uldb::engine::{Engine, EngineConfig};
use ulflow::llm::LLM;
use ulgate::config::GatewayConfig;
use ulgate::handlers::AppState;
use ulgate::server;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Parse CLI args
    let mut port = None;
    let mut db_path = None;
    let mut llm_spec = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => { i += 1; port = args.get(i).cloned(); }
            "--db" | "-d" => { i += 1; db_path = args.get(i).cloned(); }
            "--llm" | "-l" => { i += 1; llm_spec = args.get(i).cloned(); }
            "--help" | "-h" => { print_help(); return; }
            _ => {}
        }
        i += 1;
    }

    let mut config = GatewayConfig::from_env();
    if let Some(p) = port { if let Ok(n) = p.parse::<u16>() {
        config.bind_addr = format!("0.0.0.0:{}", n).parse().unwrap();
    }}
    if let Some(d) = db_path { config.db_path = d; }
    if let Some(spec) = llm_spec {
        let (p, m) = GatewayConfig::parse_llm_spec(&spec);
        config.llm_provider = p;
        config.llm_model = m;
    }

    println!("ulgate v{}", env!("CARGO_PKG_VERSION"));
    println!("db:       {}", config.db_path);
    println!("provider: {}:{}", config.llm_provider, config.llm_model);
    println!();

    // Open database
    let engine = Arc::new(RwLock::new(
        Engine::open(EngineConfig::new(&config.db_path))
            .expect("failed to open database")
    ));

    // Build LLM
    let llm = build_llm(&config);
    if llm.is_some() {
        println!("LLM ready: {}:{}", config.llm_provider, config.llm_model);
    } else {
        println!("Warning: no LLM configured. Set API key env var.");
        println!("  GROQ_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY");
    }

    // Build app state
    let registry = Arc::new(ulgate::handlers::build_default_registry(Arc::clone(&engine)));
    let state = Arc::new(AppState {
        engine,
        registry,
        llm,
        start_time: std::time::Instant::now(),
        version: env!("CARGO_PKG_VERSION").into(),
    });

    // Run server
    server::run(&config.bind_addr.to_string(), state)
        .expect("server failed");
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
    println!("  --llm, -l SPEC     LLM: provider:model (e.g. groq:llama-3.3-70b-versatile)");
    println!("  --help, -h         Show this help");
    println!();
    println!("ENVIRONMENT:");
    println!("  GROQ_API_KEY       Groq API key");
    println!("  OPENAI_API_KEY     OpenAI API key");
    println!("  ANTHROPIC_API_KEY  Anthropic API key");
    println!("  GEMINI_API_KEY     Google Gemini API key");
    println!();
    println!("EXAMPLES:");
    println!("  GROQ_API_KEY=xxx ulgate --port 8080 --llm groq:llama-3.3-70b-versatile");
    println!("  OPENAI_API_KEY=xxx ulgate --llm openai:gpt-4o");
    println!("  ulgate --llm ollama:llama3");
    println!();
    println!("ENDPOINTS:");
    println!("  GET  /v1/health           Health check");
    println!("  GET  /v1/tools            List available tools");
    println!("  POST /v1/tools/call       Call a tool");
    println!("  POST /v1/run              Run an AI workflow");
    println!("  POST /v1/chat             Chat with the LLM");
    println!("  POST /v1/db/put           Store a document");
    println!("  GET  /v1/db/get?key=...   Retrieve a document");
    println!("  GET  /v1/db/search?q=...  Search documents");
}
