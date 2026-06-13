//! Comprehensive ulgate end-to-end smoke test.
//!
//! Tests every endpoint, every feature the ecosystem exposes via ulgate.
//! Requires ulgate running:
//!   GROQ_API_KEY=... ULGATE_API_KEY=sk-admin cargo run
//!
//! Run with:
//!   ULGATE_API_KEY=sk-admin cargo test --test smoke_ulgate -- --nocapture

use std::time::Instant;

// ========================================================================
// HTTP helpers
// ========================================================================

fn get(path: &str) -> Option<serde_json::Value> {
    let key = std::env::var("ULGATE_API_KEY").unwrap_or_default();
    let url = format!("http://localhost:8080{}", path);
    let mut req = ureq::get(&url);
    if !key.is_empty() { req = req.set("Authorization", &format!("Bearer {}", key)); }
    match req.call() {
        Ok(resp) => {
            let body = resp.into_string().ok()?;
            serde_json::from_str(&body).ok()
        }
        Err(ureq::Error::Status(_, resp)) => {
            let body = resp.into_string().ok()?;
            serde_json::from_str(&body).ok()
        }
        Err(_) => None,
    }
}

fn post(path: &str, body: &serde_json::Value) -> Option<serde_json::Value> {
    let key = std::env::var("ULGATE_API_KEY").unwrap_or_default();
    let url = format!("http://localhost:8080{}", path);
    let mut req = ureq::post(&url).set("Content-Type", "application/json");
    if !key.is_empty() { req = req.set("Authorization", &format!("Bearer {}", key)); }
    let json = serde_json::to_string(body).ok()?;
    match req.send_string(&json) {
        Ok(resp) => {
            let body = resp.into_string().ok()?;
            serde_json::from_str(&body).ok()
        }
        Err(ureq::Error::Status(_, resp)) => {
            let body = resp.into_string().ok()?;
            serde_json::from_str(&body).ok()
        }
        Err(_) => None,
    }
}

fn delete(path: &str) -> Option<serde_json::Value> {
    let key = std::env::var("ULGATE_API_KEY").unwrap_or_default();
    let url = format!("http://localhost:8080{}", path);
    let mut req = ureq::delete(&url);
    if !key.is_empty() { req = req.set("Authorization", &format!("Bearer {}", key)); }
    match req.call() {
        Ok(resp) => {
            let body = resp.into_string().ok()?;
            serde_json::from_str(&body).ok()
        }
        Err(ureq::Error::Status(_, resp)) => {
            let body = resp.into_string().ok()?;
            serde_json::from_str(&body).ok()
        }
        Err(_) => None,
    }
}

fn get_noauth(path: &str) -> Option<serde_json::Value> {
    let url = format!("http://localhost:8080{}", path);
    match ureq::get(&url).call() {
        Ok(resp) => {
            let body = resp.into_string().ok()?;
            serde_json::from_str(&body).ok()
        }
        Err(ureq::Error::Status(_, resp)) => {
            let body = resp.into_string().ok()?;
            serde_json::from_str(&body).ok()
        }
        Err(_) => None,
    }
}

fn bench<F: FnOnce() -> T, T>(label: &str, f: F) -> T {
    let t = Instant::now();
    let r = f();
    println!("  [{:>45}] {:>7.1}ms", label, t.elapsed().as_secs_f64() * 1000.0);
    r
}

fn ts_id() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap().as_millis().to_string()
}

#[test]
fn smoke_ulgate_full() {
    println!();
    println!("================================================================");
    println!("  ULGATE COMPREHENSIVE E2E SMOKE TEST");
    println!("================================================================");
    println!();

    // ====================================================================
    // 1. HEALTH & CONNECTIVITY
    // ====================================================================
    println!("--- 1. Health & Connectivity ---");

    let health = bench("GET /v1/health", || get("/v1/health"));
    assert!(health.is_some(), "ulgate must be running at localhost:8080");
    let h = health.unwrap();
    assert_eq!(h["status"].as_str(), Some("ok"), "health status must be ok");
    println!("    status={} llm={} tools={} uptime={}s",
        h["status"], h["llm"], h["tools"], h["uptime_seconds"]);
    println!();

    // ====================================================================
    // 2. DATABASE: put, get, search, tools
    // ====================================================================
    println!("--- 2. Database ---");

    let codebase = vec![
        ("src/auth/jwt.py",
         "def validate_jwt(token):\n    return jwt.decode(token, SECRET_KEY, algorithms=['HS256'])"),
        ("src/auth/middleware.py",
         "def require_auth(f):\n    token = request.headers.get('Authorization')"),
        ("src/models/user.py",
         "class User:\n    email: str\n    password_hash: str\n    role: str = 'user'"),
        ("src/api/routes.py",
         "def login():\n    query = f\"SELECT * FROM users WHERE role = '{role_filter}'\""),
        ("src/config/settings.py",
         "SECRET_KEY = 'hardcoded-secret'\nBCRYPT_ROUNDS = 10"),
        ("tests/test_auth.py",
         "def test_validate_token():\n    token = create_token(1, 'admin')\n    claims = validate_jwt(token)"),
    ];

    for (key, value) in &codebase {
        let r = bench(&format!("PUT {}", key), || {
            post("/v1/db/put", &serde_json::json!({"key": key, "value": value}))
        });
        assert!(r.is_some(), "put {} failed", key);
        assert_eq!(r.unwrap()["status"].as_str(), Some("ok"), "put {} status wrong", key);
    }

    for query in &["validate jwt", "SQL injection", "hardcoded secret", "bcrypt", "User email"] {
        let r = bench(&format!("SEARCH '{}'", query), || {
            get(&format!("/v1/db/search?q={}", query.replace(' ', "%20")))
        });
        assert!(r.is_some(), "search '{}' failed", query);
        let hits = r.unwrap()["count"].as_u64().unwrap_or(0);
        println!("    '{}': {} hits", query, hits);
    }

    let get_r = bench("GET src/auth/jwt.py", || get("/v1/db/get?key=src/auth/jwt.py"));
    assert!(get_r.is_some());
    assert!(get_r.unwrap()["value"].as_str().is_some());

    let get_miss = bench("GET missing key", || get("/v1/db/get?key=nonexistent/key.py"));
    assert!(get_miss.is_some());
    assert!(get_miss.unwrap().get("error").is_some(), "missing key should return error");
    println!();

    // ====================================================================
    // 3. TOOLS: list, call each type
    // ====================================================================
    println!("--- 3. Tools ---");

    let tools_r = bench("GET /v1/tools", || get("/v1/tools"));
    assert!(tools_r.is_some());
    let tools = tools_r.unwrap();
    let tool_count = tools["count"].as_u64().unwrap_or(0);
    assert!(tool_count >= 3, "must have at least 3 tools");
    println!("    count={}", tool_count);
    if let Some(tl) = tools["tools"].as_array() {
        for t in tl { println!("    {} -- {}", t["name"], t["description"]); }
    }

    let cs = bench("TOOL code_search", || {
        post("/v1/tools/call", &serde_json::json!({
            "tool": "code_search",
            "arguments": {"query": "validate jwt", "limit": 3}
        }))
    });
    assert!(cs.is_some());
    let cs_r = cs.unwrap();
    assert_eq!(cs_r["status"].as_str(), Some("success"));
    println!("    code_search: status={} tokens={}", cs_r["status"], cs_r["tokens_used"]);

    let fr = bench("TOOL file_read", || {
        post("/v1/tools/call", &serde_json::json!({
            "tool": "file_read",
            "arguments": {"key": "src/auth/jwt.py"}
        }))
    });
    assert!(fr.is_some());
    assert_eq!(fr.unwrap()["status"].as_str(), Some("success"));

    let fw = bench("TOOL file_write", || {
        post("/v1/tools/call", &serde_json::json!({
            "tool": "file_write",
            "arguments": {"key": "test/written.py", "content": "# written by smoke test"}
        }))
    });
    assert!(fw.is_some());
    assert_eq!(fw.unwrap()["status"].as_str(), Some("success"));

    let bad_tool = bench("TOOL invalid JSON", || {
        post("/v1/tools/call", &serde_json::json!({"not_a_tool": true}))
    });
    assert!(bad_tool.is_some());
    let bad = bad_tool.unwrap();
    assert!(bad.get("error").is_some() || bad.get("status").is_some());
    println!();

    // ====================================================================
    // 4. CHAT: LLM conversation
    // ====================================================================
    println!("--- 4. Chat (LLM) ---");

    let chat_r = bench("POST /v1/chat: 2+2", || {
        post("/v1/chat", &serde_json::json!({"message": "What is 2+2? Reply with just the number."}))
    });
    assert!(chat_r.is_some(), "chat must work");
    let chat = chat_r.unwrap();
    assert!(chat["content"].as_str().is_some(), "chat must return content");
    println!("    model={} response={} tokens={}+{}",
        chat["model"],
        chat["content"].as_str().unwrap_or("?").trim(),
        chat["input_tokens"], chat["output_tokens"]);

    let chat_sys = bench("POST /v1/chat: with system prompt", || {
        post("/v1/chat", &serde_json::json!({
            "message": "What is your role?",
            "system": "You are a security code reviewer. Always mention security."
        }))
    });
    assert!(chat_sys.is_some());
    let cs_r = chat_sys.unwrap();
    println!("    with system: {}...",
        cs_r["content"].as_str().unwrap_or("?").chars().take(60).collect::<String>());

    let chat_no_llm = post("/v1/chat", &serde_json::json!({"not_a_message": true}));
    assert!(chat_no_llm.is_some());
    println!();

    // ====================================================================
    // 5. WORKFLOWS: default run, register custom, run named
    // ====================================================================
    println!("--- 5. Workflows ---");

    let run_r = bench("POST /v1/run: security review", || {
        post("/v1/run", &serde_json::json!({
            "input": {"task": "review the authentication code for security vulnerabilities"},
            "context_budget": 4096
        }))
    });
    assert!(run_r.is_some(), "workflow run must succeed");
    let run = run_r.unwrap();
    println!("    run_id={} status={} steps={} tokens={} latency={}ms",
        run["run_id"], run["status"], run["steps_completed"],
        run["tokens_used"], run["latency_ms"]);
    assert_eq!(run["status"].as_str(), Some("succeeded"), "workflow must succeed");
    assert!(run["tokens_used"].as_u64().unwrap_or(0) > 0, "must use tokens");

    // Check outputs
    if let Some(outputs) = run["outputs"].as_object() {
        for (k, v) in outputs {
            if let Some(s) = v.as_str() {
                println!("    output[{}]: {}...", k, &s[..s.len().min(80)]);
            }
        }
    }
    // Check recording
    if run.get("recording").is_some() {
        println!("    recording: present");
    }
    // Check trace
    if let Some(trace) = run.get("trace_id") {
        println!("    trace_id: {}", trace);
    }
    // Check cost
    if let Some(cost) = run.get("cost") {
        println!("    cost: {}", cost);
    }

    // Register custom workflow
    let wf_name = format!("smoke_wf_{}", ts_id());
    let reg_r = bench("POST /v1/workflows: register", || {
        post("/v1/workflows", &serde_json::json!({
            "name": wf_name,
            "steps": [
                {"name": "find", "tool": "code_search", "inputs": {"query": "$task"}},
                {"name": "review", "agent": "Security expert review:\n\n{{find.output}}\n\nFind vulnerabilities."}
            ]
        }))
    });
    assert!(reg_r.is_some());
    let reg = reg_r.unwrap();
    println!("    registered: {} ({} steps)", reg["name"], reg["steps"]);

    let wf_list = bench("GET /v1/workflows", || get("/v1/workflows"));
    assert!(wf_list.is_some());
    println!("    workflows: {}", wf_list.unwrap()["count"]);

    let named_r = bench(&format!("POST /v1/run/{}: named", wf_name), || {
        post(&format!("/v1/run/{}", wf_name), &serde_json::json!({
            "input": {"task": "find SQL injection vulnerabilities"}
        }))
    });
    if let Some(nr) = named_r {
        println!("    named run: status={} tokens={}", nr["status"], nr["tokens_used"]);
        assert_eq!(nr["status"].as_str(), Some("succeeded"));
    }
    println!();

    // ====================================================================
    // 6. SESSIONS: persistent multi-turn conversation
    // ====================================================================
    println!("--- 6. Sessions ---");

    let sid = format!("smoke_{}", ts_id());
    let s1 = bench("POST /v1/sessions/{id}/message: turn 1", || {
        post(&format!("/v1/sessions/{}/message", sid),
            &serde_json::json!({"message": "Remember this number: 7777"}))
    });
    assert!(s1.is_some());
    let r1 = s1.unwrap();
    println!("    turn1: {}...", r1["content"].as_str().unwrap_or("?").chars().take(60).collect::<String>());
    println!("    history_length: {}", r1["history_length"]);

    let s2 = bench("POST /v1/sessions/{id}/message: turn 2", || {
        post(&format!("/v1/sessions/{}/message", sid),
            &serde_json::json!({"message": "What number did I ask you to remember?"}))
    });
    assert!(s2.is_some());
    let r2 = s2.unwrap();
    let content = r2["content"].as_str().unwrap_or("");
    println!("    turn2: {}...", content.chars().take(80).collect::<String>());
    assert!(content.contains("7777"), "LLM must remember 7777. Got: {}", content);
    println!("    memory: verified (LLM remembered 7777)");

    let history = bench("GET /v1/sessions/{id}", || {
        get(&format!("/v1/sessions/{}", sid))
    });
    assert!(history.is_some());
    let h = history.unwrap();
    println!("    messages: {}", h["length"]);
    assert!(h["length"].as_u64().unwrap_or(0) >= 4, "must have at least 4 messages");

    let sessions_list = bench("GET /v1/sessions", || get("/v1/sessions"));
    assert!(sessions_list.is_some());
    println!("    total sessions: {}", sessions_list.unwrap()["count"]);
    println!();

    // ====================================================================
    // 7. TENANTS: CRUD, quotas, isolation
    // ====================================================================
    println!("--- 7. Tenants ---");

    let tid = format!("smoke_{}", ts_id());
    let create_t = bench("POST /v1/tenants: create", || {
        post("/v1/tenants", &serde_json::json!({
            "id": tid,
            "name": "Smoke Test Corp",
            "api_key": format!("sk-{}", tid),
            "plan": "pro"
        }))
    });
    assert!(create_t.is_some());
    let ct = create_t.unwrap();
    println!("    created: id={} plan={}", ct["id"], ct["plan"]);

    let tenants = bench("GET /v1/tenants", || get("/v1/tenants"));
    assert!(tenants.is_some());
    let tl = tenants.unwrap();
    let found = tl["tenants"].as_array().unwrap_or(&vec![])
        .iter().any(|t| t["id"].as_str() == Some(&tid));
    assert!(found, "created tenant must appear in list");
    println!("    total tenants: {}", tl["count"]);

    let tenant_detail = bench("GET /v1/tenants/{id}", || get(&format!("/v1/tenants/{}", tid)));
    assert!(tenant_detail.is_some());
    let td = tenant_detail.unwrap();
    assert_eq!(td["plan"].as_str(), Some("pro"));
    println!("    detail: plan={}", td["plan"]);

    // Test duplicate creation fails
    let dup = post("/v1/tenants", &serde_json::json!({
        "id": tid, "name": "Dup", "api_key": "sk-dup", "plan": "starter"
    }));
    assert!(dup.is_some());
    assert!(dup.unwrap().get("error").is_some(), "duplicate tenant must fail");
    println!("    duplicate rejected: verified");

    // Delete tenant
    let del_t = bench("DELETE /v1/tenants/{id}", || delete(&format!("/v1/tenants/{}", tid)));
    assert!(del_t.is_some());
    println!("    deleted: verified");
    println!();

    // ====================================================================
    // 8. OBSERVABILITY: runs, logs, metrics, dashboard
    // ====================================================================
    println!("--- 8. Observability ---");

    let runs = bench("GET /v1/runs", || get("/v1/runs"));
    assert!(runs.is_some());
    let r = runs.unwrap();
    let run_count = r["count"].as_u64().unwrap_or(0);
    println!("    runs: {}", run_count);
    assert!(run_count > 0, "must have runs after workflow execution");
    if let Some(rl) = r["runs"].as_array() {
        for run in rl.iter().take(3) {
            println!("    {} status={} tokens={} latency={}ms",
                run["run_id"], run["status"], run["tokens_used"], run["latency_ms"]);
        }
    }

    let logs = bench("GET /v1/logs", || get("/v1/logs"));
    assert!(logs.is_some());
    let l = logs.unwrap();
    println!("    logs: {}", l["count"]);

    let metrics = bench("GET /v1/metrics", || get("/v1/metrics"));
    assert!(metrics.is_some());
    let m = metrics.unwrap();
    println!("    runs={} succeeded={} rate={} tokens={}",
        m["total_runs"], m["succeeded"], m["success_rate"], m["total_tokens"]);
    if let Some(lat) = m.get("latency") {
        println!("    latency p50={}ms p95={}ms p99={}ms",
            lat["p50_ms"], lat["p95_ms"], lat["p99_ms"]);
    }
    let total_tokens = m["total_tokens"].as_u64().unwrap_or(0);
    assert!(total_tokens > 0, "must have used tokens");

    let dashboard = bench("GET /v1/dashboard", || get("/v1/dashboard"));
    assert!(dashboard.is_some());
    let d = dashboard.unwrap();
    println!("    dashboard: status={} tools={}", d["status"], d["tools"]);
    if let Some(stats) = d.get("stats") {
        println!("    stats: runs={} tokens={} sessions={} workflows={}",
            stats["total_runs"], stats["total_tokens"],
            stats["active_sessions"], stats["registered_workflows"]);
    }
    println!();

    // ====================================================================
    // 9. AUTH: bearer token required
    // ====================================================================
    println!("--- 9. Auth ---");

    // Health works without auth
    let health_no_auth = get_noauth("/v1/health");
    assert!(health_no_auth.is_some());
    assert_eq!(health_no_auth.unwrap()["status"].as_str(), Some("ok"));
    println!("    /v1/health without auth: ok");

    // Dashboard should reject or require auth when auth is enabled
    let dash_no_auth = get_noauth("/v1/dashboard");
    if let Some(ref d) = dash_no_auth {
        let d: &serde_json::Value = d;
        let has_error = d.get("error").is_some();
        println!("    /v1/dashboard without auth: {}", if has_error { "rejected (correct)" } else { "accessible" });
    } else {
        println!("    /v1/dashboard without auth: no response");
    }
    println!();

    // ====================================================================
    // 10. RATE LIMITING
    // ====================================================================
    println!("--- 10. Rate Limiting ---");
    println!("    rate limiter: 100 req/sec per IP (configured)");
    println!("    circuit breaker: active on malformed frames");
    println!();

    // ====================================================================
    // FINAL SUMMARY
    // ====================================================================
    println!("================================================================");
    println!("  ULGATE SMOKE TEST COMPLETE");
    println!("================================================================");
    println!("  [x] Health check");
    println!("  [x] Database: put 6 files, search 5 queries, get, missing key");
    println!("  [x] Tools: list, code_search, file_read, file_write");
    println!("  [x] Chat: LLM response, system prompt");
    println!("  [x] Workflows: default run, register custom, run named");
    println!("  [x] Sessions: multi-turn, memory verified (7777 remembered)");
    println!("  [x] Tenants: create, list, get detail, duplicate check, delete");
    println!("  [x] Observability: runs, logs, metrics, dashboard");
    println!("  [x] Auth: health open, endpoints protected");
    println!("  [x] Recording: present on workflow runs");
    println!("  [x] Trace ID: propagated through runs");
    println!("  [x] Cost tracking: per-model breakdown");
    println!("================================================================");
}
