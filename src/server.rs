//! HTTP server. Single-threaded accept loop, spawns threads per connection.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

use crate::auth::ApiKeyStore;
use crate::handlers::{self, AppState};
use crate::ratelimit::HttpRateLimiter;
use crate::response;
use crate::router;

pub fn run(
    addr: &str,
    state: Arc<AppState>,
    auth: Arc<ApiKeyStore>,
    rate_limiter: Arc<HttpRateLimiter>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    println!("ulgate listening on http://{}", addr);
    println!();
    println!("  Endpoints:");
    println!("    GET  /v1/health");
    println!("    GET  /v1/tools");
    println!("    POST /v1/tools/call");
    println!("    POST /v1/run");
    println!("    POST /v1/chat");
    println!("    POST /v1/db/put");
    println!("    GET  /v1/db/get?key=...");
    println!("    GET  /v1/db/search?q=...");

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

    // Read headers
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

    // Rate limit check
    let peer_ip = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".into());
    if !rate_limiter.allow(&peer_ip) {
        let resp = response::http_429("rate limit exceeded");
        stream.write_all(resp.as_bytes())?;
        return Ok(());
    }

    // Auth check (skip for health and OPTIONS)
    if auth.is_enabled()
        && path != "/v1/health"
        && path != "/health"
        && path != "/"
        && method != "OPTIONS"
    {
        let authorized = headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .and_then(|(_, v)| ApiKeyStore::extract_bearer(v))
            .map(|token| auth.verify(token))
            .unwrap_or(false);

        if !authorized {
            let resp = response::http_401("invalid or missing API key");
            stream.write_all(resp.as_bytes())?;
            return Ok(());
        }
    }

    // Read body
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        use std::io::Read;
        reader.read_exact(&mut body)?;
    }
    let body_str = String::from_utf8_lossy(&body).to_string();

    eprintln!("[ulgate] {} {}", method, path);

    // Handle streaming endpoints directly (write to stream, not buffered response)
    let clean_path = if let Some(idx) = path.find('?') {
        &path[..idx]
    } else {
        path
    };
    if clean_path == "/v1/chat/stream" && method == "POST" {
        return handlers::handle_chat_stream(state, &body_str, &mut stream);
    }
    if clean_path == "/v1/run/stream" && method == "POST" {
        return handlers::handle_run_stream(state, &body_str, &mut stream);
    }

    let response = router::route(state, method, path, &body_str);
    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    Ok(())
}
