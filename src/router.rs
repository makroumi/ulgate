//! HTTP router.

use crate::handlers::{self, AppState};
use crate::response;

pub fn route(state: &AppState, method: &str, path: &str, body: &str) -> String {
    // Extract query string
    let (path, query) = if let Some(idx) = path.find('?') {
        (&path[..idx], &path[idx + 1..])
    } else {
        (path, "")
    };

    match (method, path) {
        // Health
        ("GET", "/v1/health") | ("GET", "/health") | ("GET", "/") => {
            handlers::handle_health(state)
        }
        // Tools
        ("GET", "/v1/tools") => handlers::handle_list_tools(state),
        ("POST", "/v1/tools/call") => handlers::handle_tool_call(state, body),
        // Workflow
        ("POST", "/v1/run") => handlers::handle_run(state, body),
        // Chat
        ("POST", "/v1/chat") => handlers::handle_chat(state, body),
        // Observability (ulview)
        ("GET", "/v1/dashboard") => handlers::handle_dashboard(state),
        ("GET", "/v1/runs") => handlers::handle_list_runs(state),
        ("GET", "/v1/metrics") => handlers::handle_metrics(state),
        ("GET", "/v1/logs") => handlers::handle_logs(state),
        // Workflows
        ("POST", "/v1/workflows") => handlers::handle_register_workflow(state, body),
        ("GET", "/v1/workflows") => handlers::handle_list_workflows(state),
        // Sessions
        ("GET", "/v1/sessions") => handlers::handle_list_sessions(state),
        // Streaming (handled separately in server.rs)
        ("POST", "/v1/chat/stream") | ("POST", "/v1/run/stream") => {
            // This should not be called - streaming is handled in server.rs
            response::bad_request("use streaming endpoint directly")
        }
        // DB
        ("POST", "/v1/db/put") => handlers::handle_put(state, body),
        ("GET", "/v1/db/get") => {
            let key = extract_param(query, "key").unwrap_or_default();
            handlers::handle_get(state, &key)
        }
        ("GET", "/v1/db/search") => {
            let q = extract_param(query, "q")
                .or_else(|| extract_param(query, "query"))
                .unwrap_or_default();
            handlers::handle_search(state, &q)
        }
        // CORS preflight
        ("OPTIONS", _) => {
            "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\n\r\n".into()
        }
        _ => response::not_found(&format!("route not found: {} {}", method, path)),
    }
}

fn extract_param(query: &str, param: &str) -> Option<String> {
    query.split('&').find_map(|part| {
        let mut kv = part.splitn(2, '=');
        if kv.next()? == param {
            Some(kv.next().unwrap_or("").to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_query_param() {
        assert_eq!(extract_param("q=hello&limit=10", "q"), Some("hello".into()));
        assert_eq!(
            extract_param("q=hello&limit=10", "limit"),
            Some("10".into())
        );
        assert_eq!(extract_param("q=hello", "missing"), None);
    }
}
