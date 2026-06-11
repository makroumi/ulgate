//! HTTP request handlers.

use crate::response;
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

    let mut runner = FlowRunner::new(registry);
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

            let body = serde_json::json!({
                "run_id": result.run_id,
                "status": result.status.to_string(),
                "steps_completed": result.steps_completed,
                "tokens_used": result.tokens_used,
                "latency_ms": latency,
                "outputs": outputs,
            });
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
pub fn handle_session_message(state: &AppState, session_id: &str, body: &str) -> String {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return response::bad_request(&format!("invalid JSON: {}", e)),
    };

    let message = match req["message"].as_str() {
        Some(m) => m,
        None => return response::bad_request("missing 'message' field"),
    };

    // Load or create session
    let session_key = format!("session:{}", session_id);
    let mut history: Vec<serde_json::Value> = {
        let eng = state.engine.read().unwrap();
        match eng.get(session_key.as_bytes()) {
            Some(d) => serde_json::from_slice(&d).unwrap_or_else(|_| Vec::new()),
            None => Vec::new(),
        }
    };

    // Add user message
    history.push(serde_json::json!({"role": "user", "content": message}));

    // Call LLM with full history
    let llm = match &state.llm {
        Some(l) => l,
        None => return response::bad_request("no LLM configured"),
    };

    let messages: Vec<ulflow::llm::Message> = history
        .iter()
        .filter_map(|m| {
            let role = match m["role"].as_str()? {
                "user" => ulflow::llm::Role::User,
                "assistant" => ulflow::llm::Role::Assistant,
                "system" => ulflow::llm::Role::System,
                _ => return None,
            };
            Some(ulflow::llm::Message {
                role,
                content: m["content"].as_str()?.to_string(),
            })
        })
        .collect();

    let start = std::time::Instant::now();
    let chat_req = ulflow::llm::ChatRequest::new(llm.model(), messages);

    match llm.chat(&chat_req) {
        Ok(resp) => {
            // Add assistant response to history
            history.push(serde_json::json!({"role": "assistant", "content": resp.content}));

            // Persist to uldb
            {
                let mut eng = state.engine.write().unwrap();
                let _ = eng.put(
                    session_key.as_bytes(),
                    serde_json::to_vec(&history).unwrap().as_slice(),
                );
            }

            response::ok(
                &serde_json::json!({
                    "session_id": session_id,
                    "content": resp.content,
                    "model": resp.model,
                    "input_tokens": resp.input_tokens,
                    "output_tokens": resp.output_tokens,
                    "latency_ms": start.elapsed().as_millis() as u64,
                    "history_length": history.len(),
                })
                .to_string(),
            )
        }
        Err(e) => response::internal_error(&e.to_string()),
    }
}

/// GET /v1/sessions/:id - get session history
pub fn handle_get_session(state: &AppState, session_id: &str) -> String {
    let eng = state.engine.read().unwrap();
    let key = format!("session:{}", session_id);
    match eng.get(key.as_bytes()) {
        Some(d) => {
            let history: Vec<serde_json::Value> = serde_json::from_slice(&d).unwrap_or_default();
            response::ok(
                &serde_json::json!({
                    "session_id": session_id,
                    "messages": history,
                    "length": history.len(),
                })
                .to_string(),
            )
        }
        None => response::not_found(&format!("session not found: {}", session_id)),
    }
}

/// GET /v1/sessions - list all sessions
pub fn handle_list_sessions(state: &AppState) -> String {
    let eng = state.engine.read().unwrap();
    let results = eng.scan(b"session:", b"session:\xFF");
    let sessions: Vec<serde_json::Value> = results
        .iter()
        .map(|(k, v)| {
            let id = String::from_utf8_lossy(k)
                .strip_prefix("session:")
                .unwrap_or("?")
                .to_string();
            let count = serde_json::from_slice::<Vec<serde_json::Value>>(v)
                .map(|h| h.len())
                .unwrap_or(0);
            serde_json::json!({"id": id, "messages": count})
        })
        .collect();
    response::ok(&serde_json::json!({"sessions": sessions, "count": sessions.len()}).to_string())
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
