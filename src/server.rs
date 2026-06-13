//! HTTP server. Single-threaded accept loop, spawns threads per connection.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

use crate::auth::ApiKeyStore;
use crate::handlers::{self, AppState};
use crate::ratelimit::HttpRateLimiter;
use crate::response;
use crate::router;
use crate::tenant::Tenant;

pub fn run(
    addr: &str,
    state: Arc<AppState>,
    auth: Arc<ApiKeyStore>,
    rate_limiter: Arc<HttpRateLimiter>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    println!("ulgate listening on http://{}", addr);

    for stream in listener.incoming() {
        let stream = stream?;
        let state = Arc::clone(&state);
        let auth = Arc::clone(&auth);
        let rate_limiter = Arc::clone(&rate_limiter);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &state, &auth, &rate_limiter) {
                eprintln!("[ulgate] connection error: {}", e);
            }
        });
    }
    Ok(())
}

fn handle_connection(
    mut stream: std::net::TcpStream,
    state: &AppState,
    auth: &ApiKeyStore,
    rate_limiter: &HttpRateLimiter,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let parts: Vec<&str> = request_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0];
    let path = parts[1];

    let mut content_length = 0usize;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
        let low = line.to_lowercase();
        if low.starts_with("content-length:") {
            content_length = low
                .trim_start_matches("content-length:")
                .trim()
                .parse()
                .unwrap_or(0);
        }
        let trimmed = line.trim();
        if let Some((key, value)) = trimmed.split_once(':') {
            headers.push((key.trim().to_lowercase(), value.trim().to_string()));
        }
    }

    let peer_ip = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".into());
    if !rate_limiter.allow(&peer_ip) {
        let resp = response::http_429("rate limit exceeded");
        stream.write_all(resp.as_bytes())?;
        return Ok(());
    }

    let clean_path = if let Some(idx) = path.find('?') {
        &path[..idx]
    } else {
        path
    };

    // K8s probe endpoints (always open, no auth)
    match (method, clean_path) {
        ("GET", "/healthz") => {
            let resp = state.probes.liveness_response();
            let body = resp.to_json();
            let http = if resp.is_ok() {
                crate::response::ok(&body)
            } else {
                crate::response::internal_error(&body)
            };
            stream.write_all(http.as_bytes())?;
            return Ok(());
        }
        ("GET", "/readyz") => {
            let db_ok = state.engine.read().is_ok();
            let deg_ok = state.degradation.allows_reads();
            let resp = state.probes.readiness_response(db_ok, deg_ok);
            let body = resp.to_json();
            let http = if resp.is_ok() {
                crate::response::ok(&body)
            } else {
                crate::response::internal_error(&body)
            };
            stream.write_all(http.as_bytes())?;
            return Ok(());
        }
        ("GET", "/startupz") => {
            let resp = state.probes.startup_response();
            let body = resp.to_json();
            let http = if resp.is_ok() {
                crate::response::ok(&body)
            } else {
                crate::response::internal_error(&body)
            };
            stream.write_all(http.as_bytes())?;
            return Ok(());
        }
        _ => {}
    }

    // Degradation: reject if in emergency mode (except health/probes)
    if !state.degradation.allows_reads() && clean_path != "/v1/health" {
        let resp = crate::response::internal_error("system in emergency mode");
        stream.write_all(resp.as_bytes())?;
        return Ok(());
    }

    // Shutdown: reject new requests if draining
    if state.shutdown.should_reject_new() {
        let resp = crate::response::internal_error("server shutting down");
        stream.write_all(resp.as_bytes())?;
        return Ok(());
    }

    state.shutdown.request_start();
    state.probes.heartbeat();

    let skip_auth =
        clean_path == "/v1/health" || clean_path == "/health" || clean_path == "/" || method == "OPTIONS";

    let tenant_count = state.tenants.read().unwrap().count();
    let auth_required = auth.is_enabled() || tenant_count > 0;

    let mut tenant_ctx: Option<Tenant> = None;

    if auth_required && !skip_auth {
        let token = headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .and_then(|(_, v)| ApiKeyStore::extract_bearer(v));

        let token = match token {
            Some(t) => t,
            None => {
                let resp = response::http_401("missing bearer token");
                stream.write_all(resp.as_bytes())?;
                return Ok(());
            }
        };

        let admin_ok = auth.is_enabled() && auth.verify(token);
        let tenant_match = state
            .tenants
            .read()
            .unwrap()
            .authenticate(token)
            .cloned();

        // Try OAuth JWT validation if API key auth fails
        let oauth_ok = if !admin_ok && tenant_match.is_none() && state.oauth.has_providers() {
            match state.oauth.validate(token) {
                Ok(claims) => {
                    if let Ok(mut audit) = state.audit.lock() {
                        let _ = audit.record_auth(claims.iss.as_deref(), Some(&claims.sub), true);
                    }
                    true
                }
                Err(_) => false,
            }
        } else { false };

        if !admin_ok && tenant_match.is_none() && !oauth_ok {
            if let Ok(mut audit) = state.audit.lock() {
                let _ = audit.record_auth(None, None, false);
            }
            let resp = response::http_401("invalid API key");
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }

        tenant_ctx = tenant_match;

        if let Some(ref tenant) = tenant_ctx {
            let quota = state.tenants.read().unwrap().check_quota(&tenant.id);
            if let Err(e) = quota {
                let resp = response::http_429(&e.to_string());
                stream.write_all(resp.as_bytes())?;
                return Ok(());
            }
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        use std::io::Read;
        reader.read_exact(&mut body)?;
    }
    let body_str = String::from_utf8_lossy(&body).to_string();

    eprintln!(
        "[ulgate] {} {}{}",
        method,
        path,
        tenant_ctx
            .as_ref()
            .map(|t| format!(" tenant={}", t.id))
            .unwrap_or_default()
    );

    if clean_path == "/v1/chat/stream" && method == "POST" {
        return match tenant_ctx.as_ref() {
            Some(t) => handlers::handle_chat_stream_for_tenant(state, t, &body_str, &mut stream),
            None => handlers::handle_chat_stream(state, &body_str, &mut stream),
        };
    }

    if clean_path == "/v1/run/stream" && method == "POST" {
        return match tenant_ctx.as_ref() {
            Some(t) => handlers::handle_run_stream_for_tenant(state, t, &body_str, &mut stream),
            None => handlers::handle_run_stream(state, &body_str, &mut stream),
        };
    }

    if method == "GET" && clean_path.starts_with("/v1/runs/") {
        let suffix = &clean_path["/v1/runs/".len()..];
        if !suffix.is_empty() {
            let resp = match tenant_ctx.as_ref() {
                Some(t) => handlers::handle_get_run_for_tenant(state, t, suffix),
                None => handlers::handle_get_run(state, suffix),
            };
            stream.write_all(resp.as_bytes())?;
            stream.flush()?;
            return Ok(());
        }
    }

    if method == "POST"
        && clean_path.starts_with("/v1/sessions/")
        && clean_path.ends_with("/message")
    {
        let id = &clean_path["/v1/sessions/".len()..clean_path.len() - "/message".len()];
        let resp = match tenant_ctx.as_ref() {
            Some(t) => handlers::handle_session_message_for_tenant(state, t, id, &body_str),
            None => handlers::handle_session_message(state, id, &body_str),
        };
        stream.write_all(resp.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    if method == "GET" && clean_path.starts_with("/v1/sessions/") {
        let id = &clean_path["/v1/sessions/".len()..];
        if !id.is_empty() {
            let resp = match tenant_ctx.as_ref() {
                Some(t) => handlers::handle_get_session_for_tenant(state, t, id),
                None => handlers::handle_get_session(state, id),
            };
            stream.write_all(resp.as_bytes())?;
            stream.flush()?;
            return Ok(());
        }
    }

    if method == "POST" && clean_path.starts_with("/v1/run/") && clean_path != "/v1/run/stream" {
        let name = &clean_path["/v1/run/".len()..];
        if !name.is_empty() {
            let resp = match tenant_ctx.as_ref() {
                Some(t) => handlers::handle_run_named_for_tenant(state, t, name, &body_str),
                None => handlers::handle_run_named(state, name, &body_str),
            };
            stream.write_all(resp.as_bytes())?;
            stream.flush()?;
            return Ok(());
        }
    }

    if clean_path.starts_with("/v1/tenants/") {
        let tenant_id = &clean_path["/v1/tenants/".len()..];
        if !tenant_id.is_empty() {
            let resp = if tenant_ctx.is_some() {
                response::forbidden("tenant tokens cannot manage tenants")
            } else {
                match method {
                    "GET" => handlers::handle_get_tenant(state, tenant_id),
                    "DELETE" => handlers::handle_delete_tenant(state, tenant_id),
                    _ => response::method_not_allowed(),
                }
            };
            stream.write_all(resp.as_bytes())?;
            stream.flush()?;
            return Ok(());
        }
    }

    let req_start = std::time::Instant::now();
    let response = router::route_with_tenant(state, method, path, &body_str, tenant_ctx.as_ref());
    let latency_ms = req_start.elapsed().as_millis() as u64;

    // Determine if this was an error response
    let is_error = response.starts_with("HTTP/1.1 4") || response.starts_with("HTTP/1.1 5");
    let status_code: u16 = if response.starts_with("HTTP/1.1 ") {
        response[9..12].parse().unwrap_or(200)
    } else { 200 };

    // SLO tracking
    let slo_endpoint = if clean_path.starts_with("/v1/run") { "api/run" }
        else if clean_path.starts_with("/v1/chat") { "api/chat" }
        else { "" };
    if !slo_endpoint.is_empty() {
        state.slo.record(slo_endpoint, latency_ms, is_error);
    }

    // Traffic shadow
    state.shadow.record(
        method, clean_path, &body_str,
        status_code,
        &response[response.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0)..response.len().min(4096)],
        latency_ms,
        tenant_ctx.as_ref().map(|t| t.id.as_str()),
    );

    // Update degradation based on SLO metrics
    if let Some(report) = state.slo.report("api/run") {
        state.degradation.evaluate(
            report.error_rate_pct / 100.0,
            report.p99_ms,
        );
    }

    state.shutdown.request_end();

    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    Ok(())
}
