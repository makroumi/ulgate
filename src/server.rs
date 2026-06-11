//! HTTP server. Single-threaded accept loop, spawns threads per connection.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

use crate::handlers::AppState;
use crate::router;

pub fn run(addr: &str, state: Arc<AppState>) -> std::io::Result<()> {
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
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &state) {
                eprintln!("[ulgate] connection error: {}", e);
            }
        });
    }
    Ok(())
}

fn handle_connection(
    mut stream: std::net::TcpStream,
    state: &AppState,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let parts: Vec<&str> = request_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 { return Ok(()); }
    let method = parts[0];
    let path = parts[1];

    // Read headers
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() { break; }
        let low = line.to_lowercase();
        if low.starts_with("content-length:") {
            content_length = low.trim_start_matches("content-length:")
                .trim().parse().unwrap_or(0);
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

    let response = router::route(state, method, path, &body_str);
    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    Ok(())
}
