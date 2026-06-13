//! HTTP router.

use crate::handlers::{self, AppState};
use crate::response;
use crate::tenant::Tenant;

pub fn route(state: &AppState, method: &str, path: &str, body: &str) -> String {
    route_with_tenant(state, method, path, body, None)
}

pub fn route_with_tenant(
    state: &AppState,
    method: &str,
    path: &str,
    body: &str,
    tenant: Option<&Tenant>,
) -> String {
    let (path, query) = if let Some(idx) = path.find('?') {
        (&path[..idx], &path[idx + 1..])
    } else {
        (path, "")
    };

    match (method, path) {
        ("GET", "/v1/health") | ("GET", "/health") | ("GET", "/") => {
            handlers::handle_health(state)
        }

        ("GET", "/v1/tools") => handlers::handle_list_tools(state),
        ("POST", "/v1/tools/call") => match tenant {
            Some(t) => handlers::handle_tool_call_for_tenant(state, t, body),
            None => handlers::handle_tool_call(state, body),
        },

        ("POST", "/v1/run") => match tenant {
            Some(t) => handlers::handle_run_for_tenant(state, t, body),
            None => handlers::handle_run(state, body),
        },

        ("POST", "/v1/chat") => match tenant {
            Some(t) => handlers::handle_chat_for_tenant(state, t, body),
            None => handlers::handle_chat(state, body),
        },

        ("GET", "/v1/dashboard") => match tenant {
            Some(t) => handlers::handle_dashboard_for_tenant(state, t),
            None => handlers::handle_dashboard(state),
        },
        ("GET", "/v1/runs") => match tenant {
            Some(t) => handlers::handle_list_runs_for_tenant(state, t),
            None => handlers::handle_list_runs(state),
        },
        ("GET", "/v1/metrics") => match tenant {
            Some(t) => handlers::handle_metrics_for_tenant(state, t),
            None => handlers::handle_metrics(state),
        },
        ("GET", "/v1/logs") => match tenant {
            Some(t) => handlers::handle_logs_for_tenant(state, t),
            None => handlers::handle_logs(state),
        },

        ("POST", "/v1/workflows") => match tenant {
            Some(t) => handlers::handle_register_workflow_for_tenant(state, t, body),
            None => handlers::handle_register_workflow(state, body),
        },
        ("GET", "/v1/workflows") => match tenant {
            Some(t) => handlers::handle_list_workflows_for_tenant(state, t),
            None => handlers::handle_list_workflows(state),
        },

        ("GET", "/v1/sessions") => match tenant {
            Some(t) => handlers::handle_list_sessions_for_tenant(state, t),
            None => handlers::handle_list_sessions(state),
        },

        ("POST", "/v1/chat/stream") | ("POST", "/v1/run/stream") => {
            response::bad_request("use streaming endpoint directly")
        }

        ("POST", "/v1/db/put") => match tenant {
            Some(t) => handlers::handle_put_for_tenant(state, t, body),
            None => handlers::handle_put(state, body),
        },
        ("GET", "/v1/db/get") => {
            let key = extract_param(query, "key").unwrap_or_default();
            match tenant {
                Some(t) => handlers::handle_get_for_tenant(state, t, &key),
                None => handlers::handle_get(state, &key),
            }
        }
        ("GET", "/v1/db/search") => {
            let q = extract_param(query, "q")
                .or_else(|| extract_param(query, "query"))
                .unwrap_or_default();
            match tenant {
                Some(t) => handlers::handle_search_for_tenant(state, t, &q),
                None => handlers::handle_search(state, &q),
            }
        }

        ("POST", "/v1/tenants") => {
            if tenant.is_some() {
                response::forbidden("tenant tokens cannot create tenants")
            } else {
                handlers::handle_create_tenant(state, body)
            }
        }
        ("GET", "/v1/tenants") => {
            if tenant.is_some() {
                response::forbidden("tenant tokens cannot list tenants")
            } else {
                handlers::handle_list_tenants(state)
            }
        }

        // Enterprise endpoints
        ("GET", "/v1/slo") => handlers::handle_slo(state),
        ("GET", "/v1/degradation") => handlers::handle_degradation(state),
        ("POST", "/v1/degradation") => handlers::handle_set_degradation(state, body),
        ("GET", "/v1/shadow") => handlers::handle_shadow(state),
        ("GET", "/v1/shadow/errors") => handlers::handle_shadow_errors(state),
        ("GET", "/v1/probes") => handlers::handle_probes(state),
        ("GET", "/v1/system") => handlers::handle_system(state),
        ("GET", "/v1/audit") => handlers::handle_audit(state),
        ("GET", "/v1/audit/verify") => handlers::handle_audit_verify(state),
        ("POST", "/v1/gdpr/delete") => handlers::handle_gdpr_delete(state, body),
        ("GET", "/v1/gdpr/certificates") => handlers::handle_gdpr_certificates(state),

        ("OPTIONS", _) => {
            "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\n\r\n".into()
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
