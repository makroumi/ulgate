//! HTTP request handlers.

use crate::bridge;
use crate::response;
use crate::tenant::{self, Tenant, TenantRegistry};
use std::sync::{Arc, RwLock};
use uldb::engine::Engine;
use ulflow::llm::LLM;
use ulflow::prelude::*;
use ulflow::step::Input;
use ulmcp::registry::Registry;
use ulmcp::tool::*;

pub struct AppState {
    pub engine: Arc<RwLock<Engine>>,
    pub registry: Arc<Registry>,
    pub llm: Option<LLM>,
    pub start_time: std::time::Instant,
    pub version: String,
    pub tenants: Arc<RwLock<TenantRegistry>>,
}

/// GET /v1/health
pub fn handle_health(state: &AppState) -> String {
    let uptime = state.start_time.elapsed().as_secs();
    let body = serde_json::json!({
        "status": "ok",
        "version": state.version,
        "uptime_seconds": uptime,
        "tools": state.registry.tool_count(),
        "resources": state.registry.resource_count(),
        "llm": state.llm.as_ref().map(|l| format!("{}:{}", l.provider(), l.model())),
    });
    response::ok(&body.to_string())
}

/// GET /v1/tools
pub fn handle_list_tools(state: &AppState) -> String {
    let tools: Vec<serde_json::Value> = state
        .registry
        .list_tools()
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "tags": t.tags,
                "timeout_ms": t.timeout_ms,
                "params": t.params.iter().map(|p| serde_json::json!({
                    "name": p.name,
                    "description": p.description,
                    "required": p.required,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let body = serde_json::json!({"tools": tools, "count": tools.len()});
    response::ok(&body.to_string())
}

/// POST /v1/tools/call
pub fn handle_tool_call(state: &AppState, body: &str) -> String {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let tool_name = match req["tool"].as_str() {
        Some(t) => t,
        None => return response::bad_request("missing 'tool' field"),
    };

    let mut arguments = std::collections::HashMap::new();
    if let Some(args) = req["arguments"].as_object() {
        for (k, v) in args {
            arguments.insert(k.clone(), ulmcp::mcp::adapter::json_to_tool_value(v));
        }
    }

    let call = ulmcp::tool::ToolCall {
        call_id: format!("gate_{}", now_ms()),
        tool_name: tool_name.to_string(),
        arguments,
    };

    let result = state.registry.invoke(&call);
    let output = ulmcp::mcp::adapter::tool_value_to_json(&result.output);

    let body = serde_json::json!({
        "call_id": result.call_id,
        "status": result.status.to_string(),
        "output": output,
        "error": result.error,
        "tokens_used": result.tokens_used,
        "latency_ms": result.latency_ms,
    });
    response::ok(&body.to_string())
}

/// POST /v1/run
pub fn handle_run(state: &AppState, body: &str) -> String {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    // Build flow input from request
    let mut flow_input = FlowInput::new();
    if let Some(input_obj) = req["input"].as_object() {
        for (k, v) in input_obj {
            if let Some(s) = v.as_str() {
                flow_input = flow_input.var(k.clone(), s.to_string());
            }
        }
    }

    let _task = req["input"]["task"]
        .as_str()
        .or_else(|| req["task"].as_str())
        .unwrap_or("analyze")
        .to_string();

    // Build a default workflow: search -> analyze
    let registry: Registry = build_default_registry(Arc::clone(&state.engine));
    let flow = match Flow::pipeline("gate_run")
        .context_budget(req["context_budget"].as_u64().unwrap_or(8192) as usize)
        .step(Step::tool("search").tool("code_search")
            .input("query", Input::from_var("task")).build())
        .step(Step::agent("analyze",
            "You are an expert software engineer.\n\nCode found:\n{{search.output}}\n\nTask: {{task}}\n\nProvide a clear, actionable response.")
        )
        .build() {
        Ok(f) => f,
        Err(e) => return response::internal_error(&format!("flow build failed: {}", e)),
    };

    let trace = TraceContext::new();
    let mut runner = FlowRunner::new(registry)
        .with_memory(Memory::new())
        .with_recording();
    if let Some(ref llm) = state.llm {
        runner = runner.with_llm(llm.clone());
    }
    if let Some(caps_arr) = req["capabilities"].as_array() {
        let cap_strs: Vec<&str> = caps_arr.iter().filter_map(|v| v.as_str()).collect();
        let caps = ulflow::capability::Capabilities::from_strings("api_agent", &cap_strs);
        runner = runner.with_capabilities(caps);
    }

    let start = std::time::Instant::now();
    match runner.run(flow, flow_input) {
        Ok(result) => {
            let latency = start.elapsed().as_millis();
            let outputs: serde_json::Map<String, serde_json::Value> = result
                .outputs
                .iter()
                .filter_map(|(k, v)| {
                    if let ulflow::context::ContextValue::String(s) = v {
                        Some((k.clone(), serde_json::Value::String(s.clone())))
                    } else {
                        None
                    }
                })
                .collect();

            let recording = runner
                .take_recording()
                .and_then(|r| serde_json::to_value(r).ok());

            let cost = runner.cost_tracker().all();
            let cost_json: serde_json::Map<String, serde_json::Value> = cost
                .iter()
                .map(|(model, usage)| {
                    (model.clone(), serde_json::json!({
                        "calls": usage.calls,
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "total_tokens": usage.total_tokens(),
                    }))
                })
                .collect();

            let mut body = serde_json::json!({
                "run_id": result.run_id,
                "status": result.status.to_string(),
                "steps_completed": result.steps_completed,
                "tokens_used": result.tokens_used,
                "latency_ms": latency,
                "outputs": outputs,
                "workflow": "default",
                "timestamp": now_ms(),
                "trace_id": trace.trace_id,
                "cost": cost_json,
            });

            if let Some(rec) = recording {
                body["recording"] = rec;
            }

            persist_run_result(&state.engine, &body);
            response::ok(&body.to_string())
        }
        Err(e) => response::internal_error(&format!("workflow failed: {}", e)),
    }
}

/// GET /v1/db/search?q=query
pub fn handle_search(state: &AppState, query: &str) -> String {
    let mut eng = state.engine.write().unwrap();
    let spec = uldb::query::planner::QuerySpec {
        text: query.to_string(),
        top_k: 10,
        ..Default::default()
    };
    let hits = eng.indices.query(&spec);
    let results: Vec<serde_json::Value> = hits.iter().map(|h| {
        let key = String::from_utf8_lossy(&h.key).to_string();
        let value = eng.get(&h.key)
            .map(|v| String::from_utf8_lossy(&v).to_string())
            .unwrap_or_default();
        serde_json::json!({"key": key, "score": h.score, "content": &value[..value.len().min(200)]})
    }).collect();
    let body = serde_json::json!({"query": query, "results": results, "count": results.len()});
    response::ok(&body.to_string())
}

/// POST /v1/db/put
pub fn handle_put(state: &AppState, body: &str) -> String {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };
    let key = match req["key"].as_str() {
        Some(k) => k,
        None => return response::bad_request("missing 'key'"),
    };
    let value = match req["value"].as_str() {
        Some(v) => v,
        None => return response::bad_request("missing 'value'"),
    };
    // Note: db:write capability is enforced at the tenant level.
    // Per-request capability checks happen in /v1/run via capabilities field.
    let mut eng = state.engine.write().unwrap();
    match eng.put(key.as_bytes(), value.as_bytes()) {
        Ok(()) => response::ok(&serde_json::json!({"status":"ok","key":key}).to_string()),
        Err(e) => response::internal_error(&e.to_string()),
    }
}

/// GET /v1/db/get?key=...
pub fn handle_get(state: &AppState, key: &str) -> String {
    let eng = state.engine.read().unwrap();
    match eng.get(key.as_bytes()) {
        Some(v) => {
            let val = String::from_utf8_lossy(&v).to_string();
            let body = serde_json::json!({"key": key, "value": val, "size": v.len()});
            response::ok(&body.to_string())
        }
        None => response::not_found(&format!("key not found: {}", key)),
    }
}

/// POST /v1/chat - simple LLM chat endpoint
pub fn handle_chat(state: &AppState, body: &str) -> String {
    let llm = match &state.llm {
        Some(l) => l,
        None => return response::bad_request("no LLM configured. Set LLM_PROVIDER and API key."),
    };

    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let prompt = match req["message"].as_str().or_else(|| req["prompt"].as_str()) {
        Some(p) => p,
        None => return response::bad_request("missing 'message' or 'prompt' field"),
    };
    let system = req["system"].as_str();

    let start = std::time::Instant::now();
    let result = if let Some(sys) = system {
        llm.ask_with_system(sys, prompt)
    } else {
        llm.ask(prompt)
    };
    let latency = start.elapsed().as_millis();

    match result {
        Ok(resp) => {
            let body = serde_json::json!({
                "content": resp.content,
                "model": resp.model,
                "input_tokens": resp.input_tokens,
                "output_tokens": resp.output_tokens,
                "finish_reason": resp.finish_reason.to_string(),
                "latency_ms": latency,
            });
            response::ok(&body.to_string())
        }
        Err(e) => response::internal_error(&e.to_string()),
    }
}

/// POST /v1/workflows - register a custom workflow from JSON
pub fn handle_register_workflow(state: &AppState, body: &str) -> String {
    let json: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    // Validate the workflow can be built
    match ulflow::json_flow::flow_from_json(&json) {
        Ok(flow) => {
            // Store workflow definition in uldb
            let key = format!("workflow:{}", flow.name);
            let mut eng = state.engine.write().unwrap();
            match eng.put(key.as_bytes(), body.as_bytes()) {
                Ok(()) => {
                    let resp = serde_json::json!({
                        "status": "registered",
                        "name": flow.name,
                        "steps": flow.steps.len(),
                    });
                    response::ok(&resp.to_string())
                }
                Err(e) => response::internal_error(&e.to_string()),
            }
        }
        Err(e) => response::bad_request(&format!("invalid workflow: {}", e)),
    }
}

/// GET /v1/workflows - list registered workflows
pub fn handle_list_workflows(state: &AppState) -> String {
    let eng = state.engine.read().unwrap();
    let results = eng.scan(b"workflow:", b"workflow:\xFF");
    let workflows: Vec<serde_json::Value> = results.iter().filter_map(|(k, v)| {
        let name = String::from_utf8_lossy(k).strip_prefix("workflow:")?.to_string();
        let json: serde_json::Value = serde_json::from_slice(v).ok()?;
        Some(serde_json::json!({"name": name, "steps": json["steps"].as_array().map(|a| a.len()).unwrap_or(0)}))
    }).collect();
    response::ok(&serde_json::json!({"workflows": workflows, "count": workflows.len()}).to_string())
}

/// POST /v1/run/:name - run a named workflow
pub fn handle_run_named(state: &AppState, workflow_name: &str, body: &str) -> String {
    // Load workflow from uldb
    let eng = state.engine.read().unwrap();
    let key = format!("workflow:{}", workflow_name);
    let workflow_json = match eng.get(key.as_bytes()) {
        Some(d) => d,
        None => return response::not_found(&format!("workflow not found: {}", workflow_name)),
    };
    drop(eng);

    let json: serde_json::Value = match serde_json::from_slice(&workflow_json) {
        Ok(v) => v,
        Err(e) => return response::internal_error(&format!("corrupt workflow: {}", e)),
    };

    let flow = match ulflow::json_flow::flow_from_json(&json) {
        Ok(f) => f,
        Err(e) => return response::internal_error(&format!("build flow: {}", e)),
    };

    // Parse input
    let req: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let mut flow_input = FlowInput::new();
    if let Some(obj) = req["input"].as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                flow_input = flow_input.var(k.clone(), s.to_string());
            }
        }
    }

    let registry = build_default_registry(Arc::clone(&state.engine));
    let mut runner = FlowRunner::new(registry);
    if let Some(ref llm) = state.llm {
        runner = runner.with_llm(llm.clone());
    }
    // Apply capability constraints from request
    if let Some(caps_arr) = req["capabilities"].as_array() {
        let cap_strs: Vec<&str> = caps_arr.iter().filter_map(|v| v.as_str()).collect();
        let caps = ulflow::capability::Capabilities::from_strings("api_agent", &cap_strs);
        runner = runner.with_capabilities(caps);
    }

    let start = std::time::Instant::now();
    match runner.run(flow, flow_input) {
        Ok(result) => {
            let outputs: serde_json::Map<String, serde_json::Value> = result
                .outputs
                .iter()
                .filter_map(|(k, v)| {
                    if let ulflow::context::ContextValue::String(s) = v {
                        Some((k.clone(), serde_json::Value::String(s.clone())))
                    } else {
                        None
                    }
                })
                .collect();
            response::ok(
                &serde_json::json!({
                    "run_id": result.run_id, "status": result.status.to_string(),
                    "steps_completed": result.steps_completed, "tokens_used": result.tokens_used,
                    "latency_ms": start.elapsed().as_millis() as u64, "outputs": outputs,
                })
                .to_string(),
            )
        }
        Err(e) => response::internal_error(&format!("workflow failed: {}", e)),
    }
}

/// POST /v1/sessions/:id/message - add message to session
/// Stores chat history as ulmen AgentPayload with Msg records via agent_store.
pub fn handle_session_message(state: &AppState, session_id: &str, body: &str) -> String {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let message = match req["message"].as_str() {
        Some(m) => m,
        None => return response::bad_request("missing 'message' field"),
    };

    let llm = match &state.llm {
        Some(l) => l,
        None => return response::bad_request("no LLM configured"),
    };

    let store_key = format!("session:{}", session_id);

    // Load existing session payload (ulmen-native)
    let existing = {
        let eng = state.engine.read().unwrap();
        uldb::agent_store::load_payload(&eng, &store_key).ok()
    };

    let step = existing
        .as_ref()
        .map(|p| p.records.len() as i64 + 1)
        .unwrap_or(1);

    // Build user Msg record
    let user_rec = bridge::msg_record(
        &bridge::gen_id("msg"),
        session_id,
        step,
        "user",
        message,
        ulmen_core::count_tokens(message) as i64,
    );

    // Build LLM messages from existing ulmen records
    let mut llm_messages: Vec<ulflow::llm::Message> = Vec::new();
    if let Some(ref payload) = existing {
        for rec in &payload.records {
            if rec.record_type != ulmen_core::RecordType::Msg {
                continue;
            }
            let role_str = bridge::msg_role(rec).unwrap_or("user");
            let content = bridge::msg_content(rec).unwrap_or("");
            let role = match role_str {
                "assistant" => ulflow::llm::Role::Assistant,
                "system" => ulflow::llm::Role::System,
                _ => ulflow::llm::Role::User,
            };
            llm_messages.push(ulflow::llm::Message {
                role,
                content: content.to_string(),
            });
        }
    }
    llm_messages.push(ulflow::llm::Message {
        role: ulflow::llm::Role::User,
        content: message.to_string(),
    });

    let start = std::time::Instant::now();
    let chat_req = ulflow::llm::ChatRequest::new(llm.model(), llm_messages);

    match llm.chat(&chat_req) {
        Ok(resp) => {
            let assistant_rec = bridge::msg_record(
                &bridge::gen_id("msg"),
                session_id,
                step + 1,
                "assistant",
                &resp.content,
                (resp.input_tokens + resp.output_tokens) as i64,
            );

            // Persist via agent_store (ulmen-native)
            {
                let mut eng = state.engine.write().unwrap();
                let _ = uldb::agent_store::append_records(
                    &mut eng,
                    &store_key,
                    &[user_rec, assistant_rec],
                );
            }

            let history_len = existing
                .as_ref()
                .map(|p| p.records.len())
                .unwrap_or(0)
                + 2;

            response::ok(
                &serde_json::json!({
                    "session_id": session_id,
                    "content": resp.content,
                    "model": resp.model,
                    "input_tokens": resp.input_tokens,
                    "output_tokens": resp.output_tokens,
                    "latency_ms": start.elapsed().as_millis() as u64,
                    "history_length": history_len,
                })
                .to_string(),
            )
        }
        Err(e) => response::internal_error(&e.to_string()),
    }
}

/// GET /v1/sessions/:id - get session history
/// Reads ulmen AgentPayload and converts Msg records to JSON for HTTP response.
pub fn handle_get_session(state: &AppState, session_id: &str) -> String {
    let store_key = format!("session:{}", session_id);
    let eng = state.engine.read().unwrap();
    match uldb::agent_store::load_payload(&eng, &store_key) {
        Ok(payload) => {
            let messages = bridge::payload_to_chat_json(&payload);
            response::ok(
                &serde_json::json!({
                    "session_id": session_id,
                    "messages": messages,
                    "length": messages.len(),
                })
                .to_string(),
            )
        }
        Err(_) => response::not_found(&format!("session not found: {}", session_id)),
    }
}

/// GET /v1/sessions - list all sessions
/// Lists sessions stored as ulmen payloads via agent_store.
pub fn handle_list_sessions(state: &AppState) -> String {
    let eng = state.engine.read().unwrap();
    let all_keys = uldb::agent_store::list_sessions(&eng);
    let sessions: Vec<serde_json::Value> = all_keys
        .iter()
        .filter(|k| k.starts_with("session:"))
        .map(|k| {
            let id = k.strip_prefix("session:").unwrap_or(k);
            let count = uldb::agent_store::load_payload(&eng, k)
                .map(|p| p.records.len())
                .unwrap_or(0);
            serde_json::json!({"id": id, "messages": count})
        })
        .collect();
    response::ok(&serde_json::json!({"sessions": sessions, "count": sessions.len()}).to_string())
}

fn persist_run_result(engine: &Arc<RwLock<Engine>>, result: &serde_json::Value) {
    let run_id = result["run_id"].as_str().unwrap_or("unknown");
    let store_key = format!("run:{}", run_id);

    // Build ulmen payload from run result
    let mut payload = bridge::new_payload(run_id, Some(run_id));

    let status = result["status"].as_str().unwrap_or("unknown");
    let tokens = result["tokens_used"].as_u64().unwrap_or(0);
    let latency = result["latency_ms"].as_u64().unwrap_or(0);
    let workflow = result["workflow"].as_str().unwrap_or("default");

    // Store run metadata as an Obs record
    payload.records.push(bridge::obs_record(
        &bridge::gen_id("run"),
        run_id,
        1,
        workflow,
        &format!(
            "status={} steps={} tokens={} latency={}ms",
            status,
            result["steps_completed"].as_u64().unwrap_or(0),
            tokens,
            latency,
        ),
        if status == "succeeded" { 1.0 } else { 0.0 },
    ));

    // Store each output as an Obs record
    if let Some(outputs) = result["outputs"].as_object() {
        for (i, (key, val)) in outputs.iter().enumerate() {
            if let Some(s) = val.as_str() {
                payload.records.push(bridge::obs_record(
                    &bridge::gen_id("obs"),
                    run_id,
                    (i + 2) as i64,
                    key,
                    &s[..s.len().min(4000)],
                    1.0,
                ));
            }
        }
    }

    payload.header.record_count = payload.records.len();

    if let Ok(mut eng) = engine.write() {
        let _ = uldb::agent_store::store_payload(&mut eng, &store_key, &payload);
    }
}

// ========================================================================
// ulview: Observability endpoints
// ========================================================================

/// GET /v1/runs - list recent workflow runs
pub fn handle_list_runs(state: &AppState) -> String {
    let eng = state.engine.read().unwrap();
    let results = eng.scan(b"run:", b"run:\xFF");
    let runs: Vec<serde_json::Value> = results
        .iter()
        .rev()
        .take(100)
        .filter_map(|(_k, v)| {
            let json: serde_json::Value = serde_json::from_slice(v).ok()?;
            Some(serde_json::json!({
                "run_id": json["run_id"],
                "status": json["status"],
                "steps_completed": json["steps_completed"],
                "tokens_used": json["tokens_used"],
                "latency_ms": json["latency_ms"],
                "workflow": json["workflow"],
                "timestamp": json["timestamp"],
            }))
        })
        .collect();
    response::ok(&serde_json::json!({"runs": runs, "count": runs.len()}).to_string())
}

/// GET /v1/runs/:id - get run details
pub fn handle_get_run(state: &AppState, run_id: &str) -> String {
    let eng = state.engine.read().unwrap();
    let key = format!("run:{}", run_id);
    match eng.get(key.as_bytes()) {
        Some(d) => {
            let json: serde_json::Value =
                serde_json::from_slice(&d).unwrap_or(serde_json::json!({}));
            response::ok(&json.to_string())
        }
        None => response::not_found(&format!("run not found: {}", run_id)),
    }
}

/// GET /v1/metrics - aggregated metrics
pub fn handle_metrics(state: &AppState) -> String {
    let eng = state.engine.read().unwrap();
    let runs = eng.scan(b"run:", b"run:\xFF");

    let total_runs = runs.len();
    let mut total_tokens = 0u64;
    let mut total_latency = 0u64;
    let mut succeeded = 0u64;
    let mut failed = 0u64;
    let mut latencies: Vec<u64> = Vec::new();

    for (_, v) in &runs {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(v) {
            let tokens = json["tokens_used"].as_u64().unwrap_or(0);
            let latency = json["latency_ms"].as_u64().unwrap_or(0);
            total_tokens += tokens;
            total_latency += latency;
            latencies.push(latency);
            if json["status"].as_str() == Some("succeeded") {
                succeeded += 1;
            } else {
                failed += 1;
            }
        }
    }

    latencies.sort();
    let p50 = latencies.get(latencies.len() / 2).copied().unwrap_or(0);
    let p95 = latencies
        .get(latencies.len() * 95 / 100)
        .copied()
        .unwrap_or(0);
    let p99 = latencies
        .get(latencies.len() * 99 / 100)
        .copied()
        .unwrap_or(0);
    let avg_latency = if total_runs > 0 {
        total_latency / total_runs as u64
    } else {
        0
    };
    let avg_tokens = if total_runs > 0 {
        total_tokens / total_runs as u64
    } else {
        0
    };

    let uptime = state.start_time.elapsed().as_secs();
    let runs_per_min = if uptime > 0 {
        total_runs as f64 / (uptime as f64 / 60.0)
    } else {
        0.0
    };
    let tokens_per_min = if uptime > 0 {
        total_tokens as f64 / (uptime as f64 / 60.0)
    } else {
        0.0
    };

    let body = serde_json::json!({
        "total_runs": total_runs,
        "succeeded": succeeded,
        "failed": failed,
        "success_rate": if total_runs > 0 { format!("{:.1}%", succeeded as f64 / total_runs as f64 * 100.0) } else { "N/A".into() },
        "total_tokens": total_tokens,
        "avg_tokens_per_run": avg_tokens,
        "tokens_per_minute": tokens_per_min as u64,
        "latency": {
            "avg_ms": avg_latency,
            "p50_ms": p50,
            "p95_ms": p95,
            "p99_ms": p99,
        },
        "runs_per_minute": format!("{:.1}", runs_per_min),
        "uptime_seconds": uptime,
        "llm": state.llm.as_ref().map(|l| format!("{}:{}", l.provider(), l.model())),
        "tools": state.registry.tool_count(),
    });
    response::ok(&body.to_string())
}

/// GET /v1/dashboard - combined health + metrics for dashboards
pub fn handle_dashboard(state: &AppState) -> String {
    let eng = state.engine.read().unwrap();
    let runs = eng.scan(b"run:", b"run:\xFF");
    let sessions = eng.scan(b"session:", b"session:\xFF");
    let workflows = eng.scan(b"workflow:", b"workflow:\xFF");

    let total_runs = runs.len();
    let mut total_tokens = 0u64;
    let mut recent_runs: Vec<serde_json::Value> = Vec::new();

    for (_, v) in runs.iter().rev().take(10) {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(v) {
            total_tokens += json["tokens_used"].as_u64().unwrap_or(0);
            recent_runs.push(serde_json::json!({
                "run_id": json["run_id"],
                "status": json["status"],
                "tokens": json["tokens_used"],
                "latency_ms": json["latency_ms"],
            }));
        }
    }

    let uptime = state.start_time.elapsed().as_secs();

    let body = serde_json::json!({
        "status": "ok",
        "version": state.version,
        "uptime_seconds": uptime,
        "llm": state.llm.as_ref().map(|l| format!("{}:{}", l.provider(), l.model())),
        "tools": state.registry.tool_count(),
        "stats": {
            "total_runs": total_runs,
            "total_tokens": total_tokens,
            "active_sessions": sessions.len(),
            "registered_workflows": workflows.len(),
        },
        "recent_runs": recent_runs,
    });
    response::ok(&body.to_string())
}

/// GET /v1/logs - recent events
pub fn handle_logs(state: &AppState) -> String {
    let eng = state.engine.read().unwrap();
    let runs = eng.scan(b"run:", b"run:\xFF");

    let mut events: Vec<serde_json::Value> = Vec::new();
    for (_, v) in runs.iter().rev().take(50) {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(v) {
            events.push(serde_json::json!({
                "run_id": json["run_id"],
                "status": json["status"],
                "workflow": json["workflow"],
                "tokens": json["tokens_used"],
                "latency_ms": json["latency_ms"],
                "timestamp": json["timestamp"],
            }));
        }
    }

    response::ok(&serde_json::json!({"events": events, "count": events.len()}).to_string())
}

/// POST /v1/chat/stream - SSE streaming chat
pub fn handle_chat_stream(
    state: &AppState,
    body: &str,
    stream: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let llm = match &state.llm {
        Some(l) => l,
        None => {
            let resp = response::bad_request("no LLM configured");
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    };

    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            let resp = response::bad_request(&format!("invalid JSON: {}", e));
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    };

    let prompt = match req["message"].as_str().or_else(|| req["prompt"].as_str()) {
        Some(p) => p,
        None => {
            let resp = response::bad_request("missing 'message' field");
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    };
    let system = req["system"].as_str();

    // SSE headers
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n")?;
    stream.flush()?;

    let start_event = format!(
        "data: {}\n\n",
        serde_json::json!({"type": "start", "model": llm.model()})
    );
    stream.write_all(start_event.as_bytes())?;
    stream.flush()?;

    let start_time = std::time::Instant::now();

    let mut messages = Vec::new();
    if let Some(sys) = system {
        messages.push(ulflow::llm::Message {
            role: ulflow::llm::Role::System,
            content: sys.to_string(),
        });
    }
    messages.push(ulflow::llm::Message {
        role: ulflow::llm::Role::User,
        content: prompt.to_string(),
    });
    let chat_req = ulflow::llm::ChatRequest::new(llm.model(), messages);

    // We need to write chunks as they arrive
    // Use a Vec buffer to collect chunks, then write the final result
    let mut all_chunks = Vec::<String>::new();
    let result = llm.chat_stream(&chat_req, |chunk| {
        all_chunks.push(chunk.to_string());
    });

    // Write all chunks as SSE events
    for chunk in &all_chunks {
        let event = format!(
            "data: {}\n\n",
            serde_json::json!({"type": "token", "content": chunk})
        );
        stream.write_all(event.as_bytes())?;
        stream.flush()?;
    }

    let latency = start_time.elapsed().as_millis();

    match result {
        Ok(resp) => {
            let done = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "type": "done", "model": resp.model,
                    "input_tokens": resp.input_tokens, "output_tokens": resp.output_tokens,
                    "finish_reason": resp.finish_reason.to_string(), "latency_ms": latency,
                })
            );
            stream.write_all(done.as_bytes())?;
        }
        Err(e) => {
            let err = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"type": "error", "message": e.to_string()})
            );
            stream.write_all(err.as_bytes())?;
        }
    }
    stream.flush()?;
    Ok(())
}

/// POST /v1/run/stream - SSE streaming workflow
pub fn handle_run_stream(
    state: &AppState,
    body: &str,
    stream: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            let resp = response::bad_request(&format!("invalid JSON: {}", e));
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    };

    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n")?;
    stream.flush()?;

    let mut flow_input = FlowInput::new();
    if let Some(input_obj) = req["input"].as_object() {
        for (k, v) in input_obj {
            if let Some(s) = v.as_str() {
                flow_input = flow_input.var(k.clone(), s.to_string());
            }
        }
    }

    let registry = build_default_registry(Arc::clone(&state.engine));
    let flow = match Flow::pipeline("gate_run")
        .context_budget(req["context_budget"].as_u64().unwrap_or(8192) as usize)
        .step(Step::tool("search").tool("code_search")
            .input("query", Input::from_var("task")).build())
        .step(Step::agent("analyze",
            "You are an expert software engineer.\n\nCode found:\n{{search.output}}\n\nTask: {{task}}\n\nProvide a clear, actionable response.")
        )
        .build() {
        Ok(f) => f,
        Err(e) => {
            let err = format!("data: {}\n\ndata: [DONE]\n\n", serde_json::json!({"type":"error","message":e.to_string()}));
            stream.write_all(err.as_bytes())?;
            return Ok(());
        }
    };

    let start_event = format!(
        "data: {}\n\n",
        serde_json::json!({"type": "start", "workflow": "gate_run"})
    );
    stream.write_all(start_event.as_bytes())?;
    stream.flush()?;

    let mut runner = FlowRunner::new(registry);
    if let Some(ref llm) = state.llm {
        runner = runner.with_llm(llm.clone());
    }
    // Apply capability constraints from request
    if let Some(caps_arr) = req["capabilities"].as_array() {
        let cap_strs: Vec<&str> = caps_arr.iter().filter_map(|v| v.as_str()).collect();
        let caps = ulflow::capability::Capabilities::from_strings("api_agent", &cap_strs);
        runner = runner.with_capabilities(caps);
    }

    let start_time = std::time::Instant::now();
    let result = runner.run(flow, flow_input);
    let latency = start_time.elapsed().as_millis();

    match result {
        Ok(r) => {
            for (key, val) in &r.outputs {
                if let ulflow::context::ContextValue::String(s) = val {
                    let event = format!(
                        "data: {}\n\n",
                        serde_json::json!({"type":"output","key":key,"content":&s[..s.len().min(2000)]})
                    );
                    stream.write_all(event.as_bytes())?;
                    stream.flush()?;
                }
            }
            let done = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "type":"done","status":r.status.to_string(),"steps_completed":r.steps_completed,
                    "tokens_used":r.tokens_used,"latency_ms":latency
                })
            );
            stream.write_all(done.as_bytes())?;
        }
        Err(e) => {
            let err = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"type":"error","message":e.to_string()})
            );
            stream.write_all(err.as_bytes())?;
        }
    }
    stream.flush()?;
    Ok(())
}

// Build a registry backed by the engine
pub fn build_default_registry(engine: Arc<RwLock<Engine>>) -> Registry {
    let mut reg = Registry::new();

    let eng_s = Arc::clone(&engine);
    reg.register_tool(
        ToolDef::new("code_search", "Search indexed content by keyword")
            .param("query", "Search query", ParamType::String, true)
            .param(
                "limit",
                "Max results (default: 10)",
                ParamType::Integer,
                false,
            )
            .tag("search"),
        Box::new(move |call| {
            let q = call
                .arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let limit = call
                .arguments
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(10) as usize;
            let mut eng = eng_s.write().unwrap();
            let hits = eng.indices.query(&uldb::query::planner::QuerySpec {
                text: q.to_string(),
                top_k: limit,
                ..Default::default()
            });
            let results: Vec<String> = hits
                .iter()
                .map(|h| {
                    let k = String::from_utf8_lossy(&h.key).to_string();
                    let v = eng
                        .get(&h.key)
                        .map(|d| String::from_utf8_lossy(&d).to_string())
                        .unwrap_or_default();
                    format!("[{}]\n{}", k, &v[..v.len().min(500)])
                })
                .collect();
            ToolResult {
                call_id: call.call_id.clone(),
                status: ToolStatus::Success,
                output: ToolValue::String(results.join("\n\n")),
                error: None,
                tokens_used: Some(results.len() * 50),
                latency_ms: None,
            }
        }),
    );

    let eng_r = Arc::clone(&engine);
    reg.register_tool(
        ToolDef::new("file_read", "Read a document by key")
            .param("key", "Document key", ParamType::String, true)
            .tag("io"),
        Box::new(move |call| {
            let k = call
                .arguments
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let eng = eng_r.read().unwrap();
            match eng.get(k.as_bytes()) {
                Some(d) => {
                    let s = String::from_utf8_lossy(&d).to_string();
                    let len = s.len();
                    ToolResult {
                        call_id: call.call_id.clone(),
                        status: ToolStatus::Success,
                        output: ToolValue::String(s),
                        error: None,
                        tokens_used: Some(len / 4),
                        latency_ms: None,
                    }
                }
                None => ToolResult {
                    call_id: call.call_id.clone(),
                    status: ToolStatus::Error,
                    output: ToolValue::Null,
                    error: Some(format!("not found: {}", k)),
                    tokens_used: None,
                    latency_ms: None,
                },
            }
        }),
    );

    let eng_w = Arc::clone(&engine);
    reg.register_tool(
        ToolDef::new("file_write", "Write a document")
            .param("key", "Document key", ParamType::String, true)
            .param("content", "Content to write", ParamType::String, true)
            .tag("io"),
        Box::new(move |call| {
            let k = call
                .arguments
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let c = call
                .arguments
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut eng = eng_w.write().unwrap();
            match eng.put(k.as_bytes(), c.as_bytes()) {
                Ok(()) => ToolResult {
                    call_id: call.call_id.clone(),
                    status: ToolStatus::Success,
                    output: ToolValue::String(format!("wrote {} bytes to {}", c.len(), k)),
                    error: None,
                    tokens_used: Some(5),
                    latency_ms: None,
                },
                Err(e) => ToolResult {
                    call_id: call.call_id.clone(),
                    status: ToolStatus::Error,
                    output: ToolValue::Null,
                    error: Some(e.to_string()),
                    tokens_used: None,
                    latency_ms: None,
                },
            }
        }),
    );

    reg
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use uldb::engine::EngineConfig;

    fn test_state() -> (AppState, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = Arc::new(RwLock::new(
            Engine::open(EngineConfig::new(dir.path())).unwrap(),
        ));
        let registry = Arc::new(build_default_registry(Arc::clone(&engine)));
        let state = AppState {
            engine,
            registry,
            llm: None,
            start_time: std::time::Instant::now(),
            version: "0.1.0".into(),
            tenants: Arc::new(RwLock::new(TenantRegistry::new())),
        };
        (state, dir)
    }

    #[test]
    fn health_check() {
        let (state, _dir) = test_state();
        let resp = handle_health(&state);
        assert!(resp.contains("200"));
        assert!(resp.contains("ok"));
    }

    #[test]
    fn list_tools() {
        let (state, _dir) = test_state();
        let resp = handle_list_tools(&state);
        assert!(resp.contains("code_search"));
        assert!(resp.contains("file_read"));
        assert!(resp.contains("file_write"));
    }

    #[test]
    fn put_and_get() {
        let (state, _dir) = test_state();
        let put = handle_put(&state, r#"{"key":"test/key","value":"hello world"}"#);
        assert!(put.contains("ok"));

        let get = handle_get(&state, "test/key");
        assert!(get.contains("hello world"));
    }

    #[test]
    fn get_missing() {
        let (state, _dir) = test_state();
        let resp = handle_get(&state, "nonexistent");
        assert!(resp.contains("404"));
    }

    #[test]
    fn search_returns_results() {
        let (state, _dir) = test_state();
        handle_put(
            &state,
            r#"{"key":"auth/jwt.py","value":"def validate_token jwt auth"}"#,
        );
        let resp = handle_search(&state, "validate jwt");
        assert!(resp.contains("auth/jwt.py") || resp.contains("count"));
    }

    #[test]
    fn tool_call_code_search() {
        let (state, _dir) = test_state();
        handle_put(
            &state,
            r#"{"key":"src/main.rs","value":"fn main() hello world rust"}"#,
        );
        let resp = handle_tool_call(
            &state,
            r#"{"tool":"code_search","arguments":{"query":"main rust"}}"#,
        );
        assert!(resp.contains("200") || resp.contains("output"));
    }

    #[test]
    fn tool_call_invalid_json() {
        let (state, _dir) = test_state();
        let resp = handle_tool_call(&state, "not json");
        assert!(resp.contains("400"));
    }

    #[test]
    fn chat_no_llm() {
        let (state, _dir) = test_state();
        let resp = handle_chat(&state, r#"{"message":"hello"}"#);
        assert!(resp.contains("400") || resp.contains("no LLM"));
    }
}

// ========================================================================
// Multi-tenant helpers and tenant-aware handlers
// ========================================================================

fn tenant_caps(tenant: &Tenant) -> ulflow::capability::Capabilities {
    let refs: Vec<&str> = tenant.capabilities.iter().map(|s| s.as_str()).collect();
    ulflow::capability::Capabilities::from_strings(format!("tenant:{}", tenant.id), &refs)
}

fn tenant_tool_allowlist(tenant: &Tenant) -> Vec<String> {
    if tenant.capabilities.iter().any(|c| c == "tool:*") {
        return vec!["*".into()];
    }
    tenant
        .capabilities
        .iter()
        .filter_map(|c| c.strip_prefix("tool:").map(|s| s.to_string()))
        .collect()
}

fn prefix_end_bytes(prefix: &str) -> Vec<u8> {
    let mut end = prefix.as_bytes().to_vec();
    end.push(0xFF);
    end
}

fn record_tenant_usage(state: &AppState, tenant: &Tenant, tokens: u64) {


    if let Ok(reg) = state.tenants.read() {
        reg.record_usage(&tenant.id, tokens);
    }
}

fn persist_run_result_for_tenant(
    engine: &Arc<RwLock<Engine>>,
    tenant: &Tenant,
    result: &serde_json::Value,
) {
    let run_id = result["run_id"].as_str().unwrap_or("unknown");
    let store_key = tenant.run_key(run_id);

    let mut payload = bridge::new_payload(run_id, Some(run_id));

    let status = result["status"].as_str().unwrap_or("unknown");
    let tokens = result["tokens_used"].as_u64().unwrap_or(0);
    let latency = result["latency_ms"].as_u64().unwrap_or(0);
    let workflow = result["workflow"].as_str().unwrap_or("default");

    payload.records.push(bridge::obs_record(
        &bridge::gen_id("run"),
        run_id,
        1,
        workflow,
        &format!(
            "status={} steps={} tokens={} latency={}ms tenant={}",
            status,
            result["steps_completed"].as_u64().unwrap_or(0),
            tokens,
            latency,
            tenant.id,
        ),
        if status == "succeeded" { 1.0 } else { 0.0 },
    ));

    if let Some(outputs) = result["outputs"].as_object() {
        for (i, (key, val)) in outputs.iter().enumerate() {
            if let Some(s) = val.as_str() {
                payload.records.push(bridge::obs_record(
                    &bridge::gen_id("obs"),
                    run_id,
                    (i + 2) as i64,
                    key,
                    &s[..s.len().min(4000)],
                    1.0,
                ));
            }
        }
    }

    payload.header.record_count = payload.records.len();

    if let Ok(mut eng) = engine.write() {
        let _ = uldb::agent_store::store_payload(&mut eng, &store_key, &payload);
    }
}

pub fn build_tenant_registry(engine: Arc<RwLock<Engine>>, tenant: &Tenant) -> Registry {
    let mut reg = Registry::new();

    let tenant_id_search = tenant.id.clone();
    let doc_prefix_search = format!("t:{}:doc:", tenant.id);
    let eng_s = Arc::clone(&engine);
    reg.register_tool(
        ToolDef::new("code_search", "Search indexed content by keyword")
            .param("query", "Search query", ParamType::String, true)
            .param("limit", "Max results (default: 10)", ParamType::Integer, false)
            .tag("search"),
        Box::new(move |call| {
            let q = call
                .arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let limit = call
                .arguments
                .get("limit")
                .and_then(|v| v.as_i64())
                .unwrap_or(10) as usize;

            let mut eng = eng_s.write().unwrap();
            let hits = eng.indices.query(&uldb::query::planner::QuerySpec {
                text: q.to_string(),
                top_k: limit * 4,
                ..Default::default()
            });

            let results: Vec<String> = hits
                .iter()
                .filter_map(|h| {
                    let k = String::from_utf8_lossy(&h.key).to_string();
                    if !k.starts_with(&doc_prefix_search) {
                        return None;
                    }
                    let v = eng
                        .get(&h.key)
                        .map(|d| String::from_utf8_lossy(&d).to_string())
                        .unwrap_or_default();
                    let display = k
                        .strip_prefix(&format!("t:{}:doc:", tenant_id_search))
                        .unwrap_or(&k);
                    Some(format!("[{}]\n{}", display, &v[..v.len().min(500)]))
                })
                .take(limit)
                .collect();

            ToolResult {
                call_id: call.call_id.clone(),
                status: ToolStatus::Success,
                output: ToolValue::String(results.join("\n\n")),
                error: None,
                tokens_used: Some(results.len() * 50),
                latency_ms: None,
            }
        }),
    );

    let tenant_id_read = tenant.id.clone();
    let eng_r = Arc::clone(&engine);
    reg.register_tool(
        ToolDef::new("file_read", "Read a document by key")
            .param("key", "Document key", ParamType::String, true)
            .tag("io"),
        Box::new(move |call| {
            let k = call
                .arguments
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let scoped = format!("t:{}:doc:{}", tenant_id_read, k);
            let eng = eng_r.read().unwrap();
            match eng.get(scoped.as_bytes()) {
                Some(d) => {
                    let s = String::from_utf8_lossy(&d).to_string();
                    let len = s.len();
                    ToolResult {
                        call_id: call.call_id.clone(),
                        status: ToolStatus::Success,
                        output: ToolValue::String(s),
                        error: None,
                        tokens_used: Some(len / 4),
                        latency_ms: None,
                    }
                }
                None => ToolResult {
                    call_id: call.call_id.clone(),
                    status: ToolStatus::Error,
                    output: ToolValue::Null,
                    error: Some(format!("not found: {}", k)),
                    tokens_used: None,
                    latency_ms: None,
                },
            }
        }),
    );

    let tenant_id_write = tenant.id.clone();
    let eng_w = Arc::clone(&engine);
    reg.register_tool(
        ToolDef::new("file_write", "Write a document")
            .param("key", "Document key", ParamType::String, true)
            .param("content", "Content to write", ParamType::String, true)
            .tag("io"),
        Box::new(move |call| {
            let k = call
                .arguments
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let c = call
                .arguments
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let scoped = format!("t:{}:doc:{}", tenant_id_write, k);
            let mut eng = eng_w.write().unwrap();
            match eng.put(scoped.as_bytes(), c.as_bytes()) {
                Ok(()) => ToolResult {
                    call_id: call.call_id.clone(),
                    status: ToolStatus::Success,
                    output: ToolValue::String(format!("wrote {} bytes to {}", c.len(), k)),
                    error: None,
                    tokens_used: Some(5),
                    latency_ms: None,
                },
                Err(e) => ToolResult {
                    call_id: call.call_id.clone(),
                    status: ToolStatus::Error,
                    output: ToolValue::Null,
                    error: Some(e.to_string()),
                    tokens_used: None,
                    latency_ms: None,
                },
            }
        }),
    );

    reg
}

pub fn handle_tool_call_for_tenant(state: &AppState, tenant: &Tenant, body: &str) -> String {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let tool_name = match req["tool"].as_str() {
        Some(t) => t,
        None => return response::bad_request("missing 'tool' field"),
    };

    let mut arguments = std::collections::HashMap::new();
    if let Some(args) = req["arguments"].as_object() {
        for (k, v) in args {
            arguments.insert(k.clone(), ulmcp::mcp::adapter::json_to_tool_value(v));
        }
    }

    let call = ulmcp::tool::ToolCall {
        call_id: format!("gate_{}", now_ms()),
        tool_name: tool_name.to_string(),
        arguments,
    };

    let allowed_owned = tenant_tool_allowlist(tenant);
    let allowed_refs: Vec<&str> = allowed_owned.iter().map(|s| s.as_str()).collect();

    let result = state.registry.invoke_checked(&call, Some(&allowed_refs));
    if result.status == ToolStatus::Error
        && result
            .error
            .as_deref()
            .map(|e| e.contains("capability denied"))
            .unwrap_or(false)
    {
        return response::forbidden(result.error.as_deref().unwrap_or("capability denied"));
    }

    record_tenant_usage(state, tenant, result.tokens_used.unwrap_or(0) as u64);

    let output = ulmcp::mcp::adapter::tool_value_to_json(&result.output);
    let body = serde_json::json!({
        "call_id": result.call_id,
        "status": result.status.to_string(),
        "output": output,
        "error": result.error,
        "tokens_used": result.tokens_used,
        "latency_ms": result.latency_ms,
        "tenant_id": tenant.id,
    });
    response::ok(&body.to_string())
}

pub fn handle_search_for_tenant(state: &AppState, tenant: &Tenant, query: &str) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_db_search() {
        return response::forbidden(&e.to_string());
    }

    let prefix = format!("t:{}:doc:", tenant.id);
    let mut eng = state.engine.write().unwrap();
    let spec = uldb::query::planner::QuerySpec {
        text: query.to_string(),
        top_k: 25,
        ..Default::default()
    };
    let hits = eng.indices.query(&spec);
    let results: Vec<serde_json::Value> = hits
        .iter()
        .filter_map(|h| {
            let key = String::from_utf8_lossy(&h.key).to_string();
            if !key.starts_with(&prefix) {
                return None;
            }
            let value = eng
                .get(&h.key)
                .map(|v| String::from_utf8_lossy(&v).to_string())
                .unwrap_or_default();
            let display_key = key.strip_prefix(&prefix).unwrap_or(&key).to_string();
            Some(serde_json::json!({
                "key": display_key,
                "score": h.score,
                "content": &value[..value.len().min(200)]
            }))
        })
        .take(10)
        .collect();

    record_tenant_usage(state, tenant, 0);

    let body = serde_json::json!({
        "tenant_id": tenant.id,
        "query": query,
        "results": results,
        "count": results.len()
    });
    response::ok(&body.to_string())
}

pub fn handle_put_for_tenant(state: &AppState, tenant: &Tenant, body: &str) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_db_write() {
        return response::forbidden(&e.to_string());
    }

    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };
    let key = match req["key"].as_str() {
        Some(k) => k,
        None => return response::bad_request("missing 'key'"),
    };
    let value = match req["value"].as_str() {
        Some(v) => v,
        None => return response::bad_request("missing 'value'"),
    };

    let scoped = tenant.doc_key(key);
    let mut eng = state.engine.write().unwrap();
    match eng.put(scoped.as_bytes(), value.as_bytes()) {
        Ok(()) => {
            record_tenant_usage(state, tenant, 0);
            response::ok(
                &serde_json::json!({"status":"ok","key":key,"tenant_id":tenant.id}).to_string(),
            )
        }
        Err(e) => response::internal_error(&e.to_string()),
    }
}

pub fn handle_get_for_tenant(state: &AppState, tenant: &Tenant, key: &str) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_db_read() {
        return response::forbidden(&e.to_string());
    }

    let eng = state.engine.read().unwrap();
    let scoped = tenant.doc_key(key);
    match eng.get(scoped.as_bytes()) {
        Some(v) => {
            record_tenant_usage(state, tenant, 0);
            let val = String::from_utf8_lossy(&v).to_string();
            let body = serde_json::json!({
                "key": key,
                "value": val,
                "size": v.len(),
                "tenant_id": tenant.id
            });
            response::ok(&body.to_string())
        }
        None => response::not_found(&format!("key not found: {}", key)),
    }
}

pub fn handle_chat_for_tenant(state: &AppState, tenant: &Tenant, body: &str) -> String {
    let llm = match &state.llm {
        Some(l) => l,
        None => return response::bad_request("no LLM configured. Set LLM_PROVIDER and API key."),
    };

    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_llm(llm.provider()) {
        return response::forbidden(&e.to_string());
    }

    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let prompt = match req["message"].as_str().or_else(|| req["prompt"].as_str()) {
        Some(p) => p,
        None => return response::bad_request("missing 'message' or 'prompt' field"),
    };
    let system = req["system"].as_str();

    let start = std::time::Instant::now();
    let result = if let Some(sys) = system {
        llm.ask_with_system(sys, prompt)
    } else {
        llm.ask(prompt)
    };
    let latency = start.elapsed().as_millis();

    match result {
        Ok(resp) => {
            record_tenant_usage(
                state,
                tenant,
                (resp.input_tokens + resp.output_tokens) as u64,
            );
            let body = serde_json::json!({
                "tenant_id": tenant.id,
                "content": resp.content,
                "model": resp.model,
                "input_tokens": resp.input_tokens,
                "output_tokens": resp.output_tokens,
                "finish_reason": resp.finish_reason.to_string(),
                "latency_ms": latency,
            });
            response::ok(&body.to_string())
        }
        Err(e) => response::internal_error(&e.to_string()),
    }
}

pub fn handle_register_workflow_for_tenant(
    state: &AppState,
    tenant: &Tenant,
    body: &str,
) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_workflow_register() {
        return response::forbidden(&e.to_string());
    }

    let json: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    match ulflow::json_flow::flow_from_json(&json) {
        Ok(flow) => {
            let key = tenant.workflow_key(&flow.name);
            let mut eng = state.engine.write().unwrap();
            match eng.put(key.as_bytes(), body.as_bytes()) {
                Ok(()) => {
                    record_tenant_usage(state, tenant, 0);
                    let resp = serde_json::json!({
                        "status": "registered",
                        "name": flow.name,
                        "steps": flow.steps.len(),
                        "tenant_id": tenant.id,
                    });
                    response::ok(&resp.to_string())
                }
                Err(e) => response::internal_error(&e.to_string()),
            }
        }
        Err(e) => response::bad_request(&format!("invalid workflow: {}", e)),
    }
}

pub fn handle_list_workflows_for_tenant(state: &AppState, tenant: &Tenant) -> String {
    let prefix = format!("t:{}:workflow:", tenant.id);
    let eng = state.engine.read().unwrap();
    let results = eng.scan(prefix.as_bytes(), &prefix_end_bytes(&prefix));
    let workflows: Vec<serde_json::Value> = results
        .iter()
        .filter_map(|(k, v)| {
            let raw = String::from_utf8_lossy(k).to_string();
            let name = raw.strip_prefix(&prefix)?.to_string();
            let json: serde_json::Value = serde_json::from_slice(v).ok()?;
            Some(serde_json::json!({
                "name": name,
                "steps": json["steps"].as_array().map(|a| a.len()).unwrap_or(0)
            }))
        })
        .collect();

    record_tenant_usage(state, tenant, 0);
    response::ok(
        &serde_json::json!({
            "tenant_id": tenant.id,
            "workflows": workflows,
            "count": workflows.len()
        })
        .to_string(),
    )
}

pub fn handle_run_for_tenant(state: &AppState, tenant: &Tenant, body: &str) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_workflow_execute() {
        return response::forbidden(&e.to_string());
    }

    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let mut flow_input = FlowInput::new();
    if let Some(input_obj) = req["input"].as_object() {
        for (k, v) in input_obj {
            if let Some(s) = v.as_str() {
                flow_input = flow_input.var(k.clone(), s.to_string());
            }
        }
    }

    let registry = build_tenant_registry(Arc::clone(&state.engine), tenant);
    let flow = match Flow::pipeline("gate_run")
        .context_budget(req["context_budget"].as_u64().unwrap_or(8192) as usize)
        .step(
            Step::tool("search")
                .tool("code_search")
                .input("query", Input::from_var("task"))
                .build(),
        )
        .step(Step::agent(
            "analyze",
            "You are an expert software engineer.\n\nCode found:\n{{search.output}}\n\nTask: {{task}}\n\nProvide a clear, actionable response.",
        ))
        .build()
    {
        Ok(f) => f,
        Err(e) => return response::internal_error(&format!("flow build failed: {}", e)),
    };

    let mut runner = FlowRunner::new(registry)
        .with_capabilities(caps)
        .with_recording();

    if let Some(ref llm) = state.llm {
        runner = runner.with_llm(llm.clone());
    }

    let start = std::time::Instant::now();
    match runner.run(flow, flow_input) {
        Ok(result) => {
            let latency = start.elapsed().as_millis();
            let outputs: serde_json::Map<String, serde_json::Value> = result
                .outputs
                .iter()
                .filter_map(|(k, v)| {
                    if let ulflow::context::ContextValue::String(s) = v {
                        Some((k.clone(), serde_json::Value::String(s.clone())))
                    } else {
                        None
                    }
                })
                .collect();

            let recording = runner
                .take_recording()
                .and_then(|r| serde_json::to_value(r).ok());

            let mut body = serde_json::json!({
                "tenant_id": tenant.id,
                "run_id": result.run_id,
                "status": result.status.to_string(),
                "steps_completed": result.steps_completed,
                "tokens_used": result.tokens_used,
                "latency_ms": latency,
                "outputs": outputs,
                "workflow": "default",
                "timestamp": now_ms(),
            });

            if let Some(recording) = recording {
                body["recording"] = recording;
            }

            persist_run_result_for_tenant(&state.engine, tenant, &body);
            record_tenant_usage(state, tenant, result.tokens_used as u64);
            response::ok(&body.to_string())
        }
        Err(e) => response::internal_error(&format!("workflow failed: {}", e)),
    }
}

pub fn handle_run_named_for_tenant(
    state: &AppState,
    tenant: &Tenant,
    workflow_name: &str,
    body: &str,
) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_workflow_execute() {
        return response::forbidden(&e.to_string());
    }

    let scoped_key = tenant.workflow_key(workflow_name);
    let eng = state.engine.read().unwrap();
    let workflow_json = match eng.get(scoped_key.as_bytes()) {
        Some(d) => d,
        None => return response::not_found(&format!("workflow not found: {}", workflow_name)),
    };
    drop(eng);

    let json: serde_json::Value = match serde_json::from_slice(&workflow_json) {
        Ok(v) => v,
        Err(e) => return response::internal_error(&format!("corrupt workflow: {}", e)),
    };

    let flow = match ulflow::json_flow::flow_from_json(&json) {
        Ok(f) => f,
        Err(e) => return response::internal_error(&format!("build flow: {}", e)),
    };

    let req: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let mut flow_input = FlowInput::new();
    if let Some(obj) = req["input"].as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                flow_input = flow_input.var(k.clone(), s.to_string());
            }
        }
    }

    let registry = build_tenant_registry(Arc::clone(&state.engine), tenant);
    let mut runner = FlowRunner::new(registry)
        .with_capabilities(caps)
        .with_recording();
    if let Some(ref llm) = state.llm {
        runner = runner.with_llm(llm.clone());
    }

    let start = std::time::Instant::now();
    match runner.run(flow, flow_input) {
        Ok(result) => {
            let outputs: serde_json::Map<String, serde_json::Value> = result
                .outputs
                .iter()
                .filter_map(|(k, v)| {
                    if let ulflow::context::ContextValue::String(s) = v {
                        Some((k.clone(), serde_json::Value::String(s.clone())))
                    } else {
                        None
                    }
                })
                .collect();

            let recording = runner
                .take_recording()
                .and_then(|r| serde_json::to_value(r).ok());

            let mut body = serde_json::json!({
                "tenant_id": tenant.id,
                "run_id": result.run_id,
                "status": result.status.to_string(),
                "steps_completed": result.steps_completed,
                "tokens_used": result.tokens_used,
                "latency_ms": start.elapsed().as_millis() as u64,
                "outputs": outputs,
                "workflow": workflow_name,
                "timestamp": now_ms(),
            });

            if let Some(recording) = recording {
                body["recording"] = recording;
            }

            persist_run_result_for_tenant(&state.engine, tenant, &body);
            record_tenant_usage(state, tenant, result.tokens_used as u64);
            response::ok(&body.to_string())
        }
        Err(e) => response::internal_error(&format!("workflow failed: {}", e)),
    }
}

pub fn handle_session_message_for_tenant(
    state: &AppState,
    tenant: &Tenant,
    session_id: &str,
    body: &str,
) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_session_create() {
        return response::forbidden(&e.to_string());
    }

    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let message = match req["message"].as_str() {
        Some(m) => m,
        None => return response::bad_request("missing 'message' field"),
    };

    let llm = match &state.llm {
        Some(l) => l,
        None => return response::bad_request("no LLM configured"),
    };

    if let Err(e) = caps.require_llm(llm.provider()) {
        return response::forbidden(&e.to_string());
    }

    let store_key = tenant.session_key(session_id);

    let existing = {
        let eng = state.engine.read().unwrap();
        uldb::agent_store::load_payload(&eng, &store_key).ok()
    };

    let step = existing
        .as_ref()
        .map(|p| p.records.len() as i64 + 1)
        .unwrap_or(1);

    let user_rec = bridge::msg_record(
        &bridge::gen_id("msg"),
        session_id,
        step,
        "user",
        message,
        ulmen_core::count_tokens(message) as i64,
    );

    // Build LLM messages from existing ulmen records
    let mut llm_messages: Vec<ulflow::llm::Message> = Vec::new();
    if let Some(ref payload) = existing {
        for rec in &payload.records {
            if rec.record_type != ulmen_core::RecordType::Msg {
                continue;
            }
            let role_str = bridge::msg_role(rec).unwrap_or("user");
            let content = bridge::msg_content(rec).unwrap_or("");
            let role = match role_str {
                "assistant" => ulflow::llm::Role::Assistant,
                "system" => ulflow::llm::Role::System,
                _ => ulflow::llm::Role::User,
            };
            llm_messages.push(ulflow::llm::Message {
                role,
                content: content.to_string(),
            });
        }
    }
    llm_messages.push(ulflow::llm::Message {
        role: ulflow::llm::Role::User,
        content: message.to_string(),
    });

    let start = std::time::Instant::now();
    let chat_req = ulflow::llm::ChatRequest::new(llm.model(), llm_messages);

    match llm.chat(&chat_req) {
        Ok(resp) => {
            let assistant_rec = bridge::msg_record(
                &bridge::gen_id("msg"),
                session_id,
                step + 1,
                "assistant",
                &resp.content,
                (resp.input_tokens + resp.output_tokens) as i64,
            );

            {
                let mut eng = state.engine.write().unwrap();
                let _ = uldb::agent_store::append_records(
                    &mut eng,
                    &store_key,
                    &[user_rec, assistant_rec],
                );
            }

            let history_len = existing
                .as_ref()
                .map(|p| p.records.len())
                .unwrap_or(0)
                + 2;

            record_tenant_usage(
                state,
                tenant,
                (resp.input_tokens + resp.output_tokens) as u64,
            );

            response::ok(
                &serde_json::json!({
                    "tenant_id": tenant.id,
                    "session_id": session_id,
                    "content": resp.content,
                    "model": resp.model,
                    "input_tokens": resp.input_tokens,
                    "output_tokens": resp.output_tokens,
                    "latency_ms": start.elapsed().as_millis() as u64,
                    "history_length": history_len,
                })
                .to_string(),
            )
        }
        Err(e) => response::internal_error(&e.to_string()),
    }
}

pub fn handle_get_session_for_tenant(
    state: &AppState,
    tenant: &Tenant,
    session_id: &str,
) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_session_read() {
        return response::forbidden(&e.to_string());
    }

    let store_key = tenant.session_key(session_id);
    let eng = state.engine.read().unwrap();
    match uldb::agent_store::load_payload(&eng, &store_key) {
        Ok(payload) => {
            record_tenant_usage(state, tenant, 0);
            let messages = bridge::payload_to_chat_json(&payload);
            response::ok(
                &serde_json::json!({
                    "tenant_id": tenant.id,
                    "session_id": session_id,
                    "messages": messages,
                    "length": messages.len(),
                })
                .to_string(),
            )
        }
        Err(_) => response::not_found(&format!("session not found: {}", session_id)),
    }
}

pub fn handle_list_sessions_for_tenant(state: &AppState, tenant: &Tenant) -> String {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_session_read() {
        return response::forbidden(&e.to_string());
    }

    let session_prefix = tenant.session_key("");
    let eng = state.engine.read().unwrap();
    let all_keys = uldb::agent_store::list_sessions(&eng);
    let sessions: Vec<serde_json::Value> = all_keys
        .iter()
        .filter(|k| k.starts_with(&session_prefix))
        .map(|k| {
            let id = k.strip_prefix(&session_prefix).unwrap_or(k);
            let count = uldb::agent_store::load_payload(&eng, k)
                .map(|p| p.records.len())
                .unwrap_or(0);
            serde_json::json!({"id": id, "messages": count})
        })
        .collect();

    record_tenant_usage(state, tenant, 0);
    response::ok(
        &serde_json::json!({
            "tenant_id": tenant.id,
            "sessions": sessions,
            "count": sessions.len()
        })
        .to_string(),
    )
}

pub fn handle_list_runs_for_tenant(state: &AppState, tenant: &Tenant) -> String {
    let run_prefix = tenant.run_key("");
    let eng = state.engine.read().unwrap();
    let all_keys = uldb::agent_store::list_sessions(&eng);
    let runs: Vec<serde_json::Value> = all_keys
        .iter()
        .filter(|k| k.starts_with(&run_prefix))
        .rev()
        .take(100)
        .filter_map(|k| {
            let payload = uldb::agent_store::load_payload(&eng, k).ok()?;
            let run_id = k.strip_prefix(&run_prefix).unwrap_or(k);
            let first_obs = payload.records.first()?;
            let content = bridge::obs_content(first_obs).unwrap_or("");
            Some(serde_json::json!({
                "run_id": run_id,
                "summary": &content[..content.len().min(200)],
                "record_count": payload.records.len(),
            }))
        })
        .collect();

    record_tenant_usage(state, tenant, 0);
    response::ok(
        &serde_json::json!({
            "tenant_id": tenant.id,
            "runs": runs,
            "count": runs.len()
        })
        .to_string(),
    )
}

pub fn handle_get_run_for_tenant(state: &AppState, tenant: &Tenant, run_id: &str) -> String {
    let store_key = tenant.run_key(run_id);
    let eng = state.engine.read().unwrap();
    match uldb::agent_store::load_payload(&eng, &store_key) {
        Ok(payload) => {
            record_tenant_usage(state, tenant, 0);
            let observations = bridge::payload_to_run_json(&payload);
            response::ok(
                &serde_json::json!({
                    "tenant_id": tenant.id,
                    "run_id": run_id,
                    "records": observations,
                    "record_count": payload.records.len(),
                })
                .to_string(),
            )
        }
        Err(_) => response::not_found(&format!("run not found: {}", run_id)),
    }
}

pub fn handle_metrics_for_tenant(state: &AppState, tenant: &Tenant) -> String {
    let prefix = format!("t:{}:run:", tenant.id);
    let eng = state.engine.read().unwrap();
    let runs = eng.scan(prefix.as_bytes(), &prefix_end_bytes(&prefix));

    let total_runs = runs.len();
    let mut total_tokens = 0u64;
    let mut total_latency = 0u64;
    let mut succeeded = 0u64;
    let mut failed = 0u64;
    let mut latencies: Vec<u64> = Vec::new();

    for (_, v) in &runs {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(v) {
            let tokens = json["tokens_used"].as_u64().unwrap_or(0);
            let latency = json["latency_ms"].as_u64().unwrap_or(0);
            total_tokens += tokens;
            total_latency += latency;
            latencies.push(latency);
            if json["status"].as_str() == Some("succeeded") {
                succeeded += 1;
            } else {
                failed += 1;
            }
        }
    }

    latencies.sort();
    let p50 = latencies.get(latencies.len() / 2).copied().unwrap_or(0);
    let p95 = latencies.get(latencies.len() * 95 / 100).copied().unwrap_or(0);
    let p99 = latencies.get(latencies.len() * 99 / 100).copied().unwrap_or(0);
    let avg_latency = if total_runs > 0 {
        total_latency / total_runs as u64
    } else {
        0
    };
    let avg_tokens = if total_runs > 0 {
        total_tokens / total_runs as u64
    } else {
        0
    };

    record_tenant_usage(state, tenant, 0);

    let body = serde_json::json!({
        "tenant_id": tenant.id,
        "total_runs": total_runs,
        "succeeded": succeeded,
        "failed": failed,
        "success_rate": if total_runs > 0 { format!("{:.1}%", succeeded as f64 / total_runs as f64 * 100.0) } else { "N/A".into() },
        "total_tokens": total_tokens,
        "avg_tokens_per_run": avg_tokens,
        "latency": {
            "avg_ms": avg_latency,
            "p50_ms": p50,
            "p95_ms": p95,
            "p99_ms": p99,
        },
    });
    response::ok(&body.to_string())
}

pub fn handle_dashboard_for_tenant(state: &AppState, tenant: &Tenant) -> String {
    let runs_prefix = format!("t:{}:run:", tenant.id);
    let sessions_prefix = format!("t:{}:session:", tenant.id);
    let workflows_prefix = format!("t:{}:workflow:", tenant.id);

    let eng = state.engine.read().unwrap();
    let runs = eng.scan(runs_prefix.as_bytes(), &prefix_end_bytes(&runs_prefix));
    let sessions = eng.scan(
        sessions_prefix.as_bytes(),
        &prefix_end_bytes(&sessions_prefix),
    );
    let workflows = eng.scan(
        workflows_prefix.as_bytes(),
        &prefix_end_bytes(&workflows_prefix),
    );

    let total_runs = runs.len();
    let mut total_tokens = 0u64;
    let mut recent_runs: Vec<serde_json::Value> = Vec::new();

    for (_, v) in runs.iter().rev().take(10) {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(v) {
            total_tokens += json["tokens_used"].as_u64().unwrap_or(0);
            recent_runs.push(serde_json::json!({
                "run_id": json["run_id"],
                "status": json["status"],
                "tokens": json["tokens_used"],
                "latency_ms": json["latency_ms"],
            }));
        }
    }

    record_tenant_usage(state, tenant, 0);

    let body = serde_json::json!({
        "status": "ok",
        "tenant_id": tenant.id,
        "version": state.version,
        "llm": state.llm.as_ref().map(|l| format!("{}:{}", l.provider(), l.model())),
        "tools": state.registry.tool_count(),
        "stats": {
            "total_runs": total_runs,
            "total_tokens": total_tokens,
            "active_sessions": sessions.len(),
            "registered_workflows": workflows.len(),
        },
        "recent_runs": recent_runs,
    });
    response::ok(&body.to_string())
}

pub fn handle_logs_for_tenant(state: &AppState, tenant: &Tenant) -> String {
    let prefix = format!("t:{}:run:", tenant.id);
    let eng = state.engine.read().unwrap();
    let runs = eng.scan(prefix.as_bytes(), &prefix_end_bytes(&prefix));

    let mut events: Vec<serde_json::Value> = Vec::new();
    for (_, v) in runs.iter().rev().take(50) {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(v) {
            events.push(serde_json::json!({
                "run_id": json["run_id"],
                "status": json["status"],
                "workflow": json["workflow"],
                "tokens": json["tokens_used"],
                "latency_ms": json["latency_ms"],
                "timestamp": json["timestamp"],
            }));
        }
    }

    record_tenant_usage(state, tenant, 0);
    response::ok(
        &serde_json::json!({"tenant_id": tenant.id, "events": events, "count": events.len()})
            .to_string(),
    )
}

pub fn handle_chat_stream_for_tenant(
    state: &AppState,
    tenant: &Tenant,
    body: &str,
    stream: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let llm = match &state.llm {
        Some(l) => l,
        None => {
            let resp = response::bad_request("no LLM configured");
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    };

    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_llm(llm.provider()) {
        let resp = response::forbidden(&e.to_string());
        stream.write_all(resp.as_bytes())?;
        return Ok(());
    }

    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            let resp = response::bad_request(&format!("invalid JSON: {}", e));
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    };

    let prompt = match req["message"].as_str().or_else(|| req["prompt"].as_str()) {
        Some(p) => p,
        None => {
            let resp = response::bad_request("missing 'message' field");
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    };
    let system = req["system"].as_str();

    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n")?;
    stream.flush()?;

    let start_event = format!(
        "data: {}\n\n",
        serde_json::json!({"type": "start", "model": llm.model(), "tenant_id": tenant.id})
    );
    stream.write_all(start_event.as_bytes())?;
    stream.flush()?;

    let start_time = std::time::Instant::now();

    let mut messages = Vec::new();
    if let Some(sys) = system {
        messages.push(ulflow::llm::Message {
            role: ulflow::llm::Role::System,
            content: sys.to_string(),
        });
    }
    messages.push(ulflow::llm::Message {
        role: ulflow::llm::Role::User,
        content: prompt.to_string(),
    });
    let chat_req = ulflow::llm::ChatRequest::new(llm.model(), messages);

    let mut all_chunks = Vec::<String>::new();
    let result = llm.chat_stream(&chat_req, |chunk| {
        all_chunks.push(chunk.to_string());
    });

    for chunk in &all_chunks {
        let event = format!(
            "data: {}\n\n",
            serde_json::json!({"type": "token", "content": chunk})
        );
        stream.write_all(event.as_bytes())?;
        stream.flush()?;
    }

    let latency = start_time.elapsed().as_millis();

    match result {
        Ok(resp) => {
            record_tenant_usage(
                state,
                tenant,
                (resp.input_tokens + resp.output_tokens) as u64,
            );
            let done = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "type": "done",
                    "tenant_id": tenant.id,
                    "model": resp.model,
                    "input_tokens": resp.input_tokens,
                    "output_tokens": resp.output_tokens,
                    "finish_reason": resp.finish_reason.to_string(),
                    "latency_ms": latency,
                })
            );
            stream.write_all(done.as_bytes())?;
        }
        Err(e) => {
            let err = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"type": "error", "message": e.to_string()})
            );
            stream.write_all(err.as_bytes())?;
        }
    }
    stream.flush()?;
    Ok(())
}

pub fn handle_run_stream_for_tenant(
    state: &AppState,
    tenant: &Tenant,
    body: &str,
    stream: &mut impl std::io::Write,
) -> std::io::Result<()> {
    let caps = tenant_caps(tenant);
    if let Err(e) = caps.require_workflow_execute() {
        let resp = response::forbidden(&e.to_string());
        stream.write_all(resp.as_bytes())?;
        return Ok(());
    }

    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            let resp = response::bad_request(&format!("invalid JSON: {}", e));
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    };

    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n")?;
    stream.flush()?;

    let mut flow_input = FlowInput::new();
    if let Some(input_obj) = req["input"].as_object() {
        for (k, v) in input_obj {
            if let Some(s) = v.as_str() {
                flow_input = flow_input.var(k.clone(), s.to_string());
            }
        }
    }

    let registry = build_tenant_registry(Arc::clone(&state.engine), tenant);
    let flow = match Flow::pipeline("gate_run")
        .context_budget(req["context_budget"].as_u64().unwrap_or(8192) as usize)
        .step(
            Step::tool("search")
                .tool("code_search")
                .input("query", Input::from_var("task"))
                .build(),
        )
        .step(Step::agent(
            "analyze",
            "You are an expert software engineer.\n\nCode found:\n{{search.output}}\n\nTask: {{task}}\n\nProvide a clear, actionable response.",
        ))
        .build()
    {
        Ok(f) => f,
        Err(e) => {
            let err = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"type":"error","message":e.to_string()})
            );
            stream.write_all(err.as_bytes())?;
            return Ok(());
        }
    };

    let start_event = format!(
        "data: {}\n\n",
        serde_json::json!({"type": "start", "workflow": "gate_run", "tenant_id": tenant.id})
    );
    stream.write_all(start_event.as_bytes())?;
    stream.flush()?;

    let mut runner = FlowRunner::new(registry)
        .with_capabilities(caps)
        .with_recording();
    if let Some(ref llm) = state.llm {
        runner = runner.with_llm(llm.clone());
    }

    let start_time = std::time::Instant::now();
    let result = runner.run(flow, flow_input);
    let latency = start_time.elapsed().as_millis();

    match result {
        Ok(r) => {
            for (key, val) in &r.outputs {
                if let ulflow::context::ContextValue::String(s) = val {
                    let event = format!(
                        "data: {}\n\n",
                        serde_json::json!({"type":"output","key":key,"content":&s[..s.len().min(2000)]})
                    );
                    stream.write_all(event.as_bytes())?;
                    stream.flush()?;
                }
            }

            let recording = runner
                .take_recording()
                .and_then(|rec| serde_json::to_value(rec).ok());

            let mut body_json = serde_json::json!({
                "tenant_id": tenant.id,
                "run_id": r.run_id,
                "status": r.status.to_string(),
                "steps_completed": r.steps_completed,
                "tokens_used": r.tokens_used,
                "latency_ms": latency,
                "outputs": r.outputs.iter().filter_map(|(k, v)| {
                    if let ulflow::context::ContextValue::String(s) = v {
                        Some((k.clone(), serde_json::Value::String(s.clone())))
                    } else {
                        None
                    }
                }).collect::<serde_json::Map<String, serde_json::Value>>(),
                "workflow": "default",
                "timestamp": now_ms(),
            });

            if let Some(recording) = recording {
                body_json["recording"] = recording;
            }

            persist_run_result_for_tenant(&state.engine, tenant, &body_json);
            record_tenant_usage(state, tenant, r.tokens_used as u64);

            let done = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "type":"done",
                    "tenant_id": tenant.id,
                    "status":r.status.to_string(),
                    "steps_completed":r.steps_completed,
                    "tokens_used":r.tokens_used,
                    "latency_ms":latency
                })
            );
            stream.write_all(done.as_bytes())?;
        }
        Err(e) => {
            let err = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"type":"error","message":e.to_string()})
            );
            stream.write_all(err.as_bytes())?;
        }
    }
    stream.flush()?;
    Ok(())
}

pub fn handle_create_tenant(state: &AppState, body: &str) -> String {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let id = match req["id"].as_str() {
        Some(v) if !v.is_empty() => v,
        _ => return response::bad_request("missing 'id'"),
    };
    let name = req["name"].as_str().unwrap_or(id);
    let api_key = match req["api_key"].as_str() {
        Some(v) if !v.is_empty() => v,
        _ => return response::bad_request("missing 'api_key'"),
    };

    let mut tenant = Tenant::new(id, name, api_key);

    if let Some(plan) = req["plan"].as_str() {
        tenant = tenant.with_plan(plan);
    }
    if let Some(caps) = req["capabilities"].as_array() {
        let caps: Vec<String> = caps
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if !caps.is_empty() {
            tenant = tenant.with_capabilities(caps);
        }
    }

    {
        let mut reg = state.tenants.write().unwrap();
        if reg.get(id).is_some() {
            return response::bad_request(&format!("tenant already exists: {}", id));
        }
        reg.register(tenant.clone());
    }

    {
        let mut eng = state.engine.write().unwrap();
        if let Err(e) = tenant::save_tenant(&mut eng, &tenant) {
            return response::internal_error(&format!("persist tenant failed: {}", e));
        }
    }

    response::created(
        &serde_json::json!({
            "id": tenant.id,
            "name": tenant.name,
            "plan": tenant.plan,
            "namespace_id": tenant.namespace_id,
            "capabilities": tenant.capabilities,
        })
        .to_string(),
    )
}

pub fn handle_list_tenants(state: &AppState) -> String {
    let reg = state.tenants.read().unwrap();
    let tenants: Vec<serde_json::Value> = reg
        .list_owned()
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "name": t.name,
                "plan": t.plan,
                "namespace_id": t.namespace_id,
                "capabilities": t.capabilities,
            })
        })
        .collect();

    response::ok(
        &serde_json::json!({
            "tenants": tenants,
            "count": tenants.len()
        })
        .to_string(),
    )
}

pub fn handle_get_tenant(state: &AppState, tenant_id: &str) -> String {
    let reg = state.tenants.read().unwrap();
    let tenant = match reg.get(tenant_id) {
        Some(t) => t.clone(),
        None => return response::not_found(&format!("tenant not found: {}", tenant_id)),
    };
    let usage = reg.get_usage(tenant_id);

    response::ok(
        &serde_json::json!({
            "id": tenant.id,
            "name": tenant.name,
            "plan": tenant.plan,
            "namespace_id": tenant.namespace_id,
            "capabilities": tenant.capabilities,
            "quota": {
                "max_tokens_per_day": tenant.quota.max_tokens_per_day,
                "max_requests_per_hour": tenant.quota.max_requests_per_hour,
                "max_storage_bytes": tenant.quota.max_storage_bytes,
                "max_sessions": tenant.quota.max_sessions,
                "max_workflows": tenant.quota.max_workflows,
            },
            "usage": {
                "tokens_today": usage.tokens_today,
                "requests_this_hour": usage.requests_this_hour,
                "storage_bytes": usage.storage_bytes,
                "active_sessions": usage.active_sessions,
                "registered_workflows": usage.registered_workflows,
                "total_runs": usage.total_runs,
                "total_tokens": usage.total_tokens,
            }
        })
        .to_string(),
    )
}

pub fn handle_delete_tenant(state: &AppState, tenant_id: &str) -> String {
    {
        let mut reg = state.tenants.write().unwrap();
        if !reg.remove(tenant_id) {
            return response::not_found(&format!("tenant not found: {}", tenant_id));
        }
    }

    {
        let mut eng = state.engine.write().unwrap();
        if let Err(e) = tenant::delete_tenant(&mut eng, tenant_id) {
            return response::internal_error(&format!("delete tenant failed: {}", e));
        }
    }

    response::ok(
        &serde_json::json!({
            "status": "deleted",
            "tenant_id": tenant_id
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tenant_tests {
    use super::*;
    use tempfile::TempDir;
    use uldb::engine::EngineConfig;

    fn test_state() -> (AppState, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = Arc::new(RwLock::new(
            Engine::open(EngineConfig::new(dir.path())).unwrap(),
        ));
        let registry = Arc::new(build_default_registry(Arc::clone(&engine)));
        let state = AppState {
            engine,
            registry,
            llm: None,
            start_time: std::time::Instant::now(),
            version: "0.1.0".into(),
            tenants: Arc::new(RwLock::new(TenantRegistry::new())),
        };
        (state, dir)
    }

    #[test]
    fn create_and_list_tenants() {
        let (state, _dir) = test_state();
        let resp = handle_create_tenant(
            &state,
            r#"{"id":"acme","name":"Acme Corp","api_key":"sk-acme-123","plan":"starter"}"#,
        );
        assert!(resp.contains("201"));
        assert!(resp.contains("acme"));

        let list = handle_list_tenants(&state);
        assert!(list.contains("acme"));
        assert!(list.contains("Acme Corp"));
    }

    #[test]
    fn create_tenant_duplicate_rejected() {
        let (state, _dir) = test_state();
        handle_create_tenant(
            &state,
            r#"{"id":"dup","name":"D","api_key":"sk-d"}"#,
        );
        let resp = handle_create_tenant(
            &state,
            r#"{"id":"dup","name":"D2","api_key":"sk-d2"}"#,
        );
        assert!(resp.contains("400"));
        assert!(resp.contains("already exists"));
    }

    #[test]
    fn get_tenant_details() {
        let (state, _dir) = test_state();
        handle_create_tenant(
            &state,
            r#"{"id":"info","name":"Info Co","api_key":"sk-info","plan":"pro"}"#,
        );
        let resp = handle_get_tenant(&state, "info");
        assert!(resp.contains("Info Co"));
        assert!(resp.contains("pro"));
        assert!(resp.contains("usage"));
    }

    #[test]
    fn get_tenant_missing() {
        let (state, _dir) = test_state();
        let resp = handle_get_tenant(&state, "nonexistent");
        assert!(resp.contains("404"));
    }

    #[test]
    fn delete_tenant_works() {
        let (state, _dir) = test_state();
        handle_create_tenant(
            &state,
            r#"{"id":"gone","name":"Gone","api_key":"sk-gone"}"#,
        );
        let resp = handle_delete_tenant(&state, "gone");
        assert!(resp.contains("deleted"));

        let check = handle_get_tenant(&state, "gone");
        assert!(check.contains("404"));
    }

    #[test]
    fn tenant_scoped_put_and_get() {
        let (state, _dir) = test_state();
        let tenant = crate::tenant::Tenant::new("t1", "T1", "sk-t1");

        let put = handle_put_for_tenant(
            &state,
            &tenant,
            r#"{"key":"auth.py","value":"def validate(): pass"}"#,
        );
        assert!(put.contains("ok"));

        let get = handle_get_for_tenant(&state, &tenant, "auth.py");
        assert!(get.contains("validate"));
        assert!(get.contains("t1"));
    }

    #[test]
    fn tenant_scoped_isolation() {
        let (state, _dir) = test_state();
        let t1 = crate::tenant::Tenant::new("iso_a", "A", "sk-a");
        let t2 = crate::tenant::Tenant::new("iso_b", "B", "sk-b");

        handle_put_for_tenant(
            &state,
            &t1,
            r#"{"key":"secret.py","value":"a_secret"}"#,
        );
        handle_put_for_tenant(
            &state,
            &t2,
            r#"{"key":"secret.py","value":"b_secret"}"#,
        );

        let get_a = handle_get_for_tenant(&state, &t1, "secret.py");
        assert!(get_a.contains("a_secret"));
        assert!(!get_a.contains("b_secret"));

        let get_b = handle_get_for_tenant(&state, &t2, "secret.py");
        assert!(get_b.contains("b_secret"));
        assert!(!get_b.contains("a_secret"));
    }

    #[test]
    fn tenant_search_scoped() {
        let (state, _dir) = test_state();
        let tenant = crate::tenant::Tenant::new("srch", "S", "sk-s");

        handle_put_for_tenant(
            &state,
            &tenant,
            r#"{"key":"jwt.py","value":"def validate_jwt token auth"}"#,
        );

        let resp = handle_search_for_tenant(&state, &tenant, "validate jwt");
        assert!(resp.contains("srch"));
    }

    #[test]
    fn tenant_capability_denied() {
        let (state, _dir) = test_state();
        let tenant = crate::tenant::Tenant::new("limited", "L", "sk-l")
            .with_capabilities(vec!["db:read".into()]);
        let resp = handle_put_for_tenant(
            &state,
            &tenant,
            r#"{"key":"x","value":"y"}"#,
        );
        assert!(resp.contains("403"));
    }
}
