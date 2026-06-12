//! HTTP response builders.

pub fn ok(body: &str) -> String {
    http_response(200, "application/json", body)
}

pub fn created(body: &str) -> String {
    http_response(201, "application/json", body)
}

pub fn bad_request(msg: &str) -> String {
    let body = serde_json::json!({"error": msg, "status": 400}).to_string();
    http_response(400, "application/json", &body)
}

pub fn forbidden(msg: &str) -> String {
    let body = serde_json::json!({"error": msg, "status": 403}).to_string();
    http_response(403, "application/json", &body)
}

pub fn not_found(msg: &str) -> String {
    let body = serde_json::json!({"error": msg, "status": 404}).to_string();
    http_response(404, "application/json", &body)
}

pub fn internal_error(msg: &str) -> String {
    let body = serde_json::json!({"error": msg, "status": 500}).to_string();
    http_response(500, "application/json", &body)
}

pub fn method_not_allowed() -> String {
    let body = serde_json::json!({"error": "method not allowed", "status": 405}).to_string();
    http_response(405, "application/json", &body)
}

pub fn http_401(msg: &str) -> String {
    let body = serde_json::json!({"error": msg, "status": 401}).to_string();
    http_response(401, "application/json", &body)
}

pub fn http_429(msg: &str) -> String {
    let body = serde_json::json!({"error": msg, "status": 429}).to_string();
    http_response(429, "application/json", &body)
}

fn http_response(status: u16, content_type: &str, body: &str) -> String {
    let status_text = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\n\r\n{}",
        status, status_text, content_type, body.len(), body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_response() {
        let r = ok("{\"status\":\"ok\"}");
        assert!(r.starts_with("HTTP/1.1 200 OK"));
        assert!(r.contains("application/json"));
        assert!(r.contains("{\"status\":\"ok\"}"));
    }

    #[test]
    fn error_response() {
        let r = bad_request("invalid input");
        assert!(r.contains("400"));
        assert!(r.contains("invalid input"));
    }

    #[test]
    fn cors_headers() {
        let r = ok("{}");
        assert!(r.contains("Access-Control-Allow-Origin: *"));
        assert!(r.contains("DELETE"));
    }

    #[test]
    fn forbidden_response() {
        let r = forbidden("nope");
        assert!(r.contains("403"));
        assert!(r.contains("nope"));
    }
}
