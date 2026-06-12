//! Bridge between HTTP JSON and ulmen-native storage.
//!
//! External input arrives as JSON via the HTTP API.
//! Internally everything is stored as ulmen AgentPayloads via uldb::agent_store.
//! This module handles the conversion.

use ulmen_core::{AgentHeader, AgentPayload, AgentRecord, FieldValue, MetaFields, RecordType};

/// Build a Msg record from a chat message.
pub fn msg_record(
    id: &str,
    thread_id: &str,
    step: i64,
    role: &str,
    content: &str,
    tokens: i64,
) -> AgentRecord {
    AgentRecord {
        record_type: RecordType::Msg,
        id: id.into(),
        thread_id: thread_id.into(),
        step,
        fields: vec![
            FieldValue::Str(role.into()),
            FieldValue::Int(step),
            FieldValue::Str(content.into()),
            FieldValue::Int(tokens),
            FieldValue::Bool(false),
        ],
        meta: MetaFields::default(),
    }
}

/// Build a Tool record from a tool call.
pub fn tool_record(
    id: &str,
    thread_id: &str,
    step: i64,
    name: &str,
    args: &str,
    status: &str,
) -> AgentRecord {
    AgentRecord {
        record_type: RecordType::Tool,
        id: id.into(),
        thread_id: thread_id.into(),
        step,
        fields: vec![
            FieldValue::Str(name.into()),
            FieldValue::Str(args.into()),
            FieldValue::Str(status.into()),
        ],
        meta: MetaFields::default(),
    }
}

/// Build a Res record from a tool response.
pub fn res_record(
    id: &str,
    thread_id: &str,
    step: i64,
    name: &str,
    data: &str,
    status: &str,
    latency_ms: i64,
) -> AgentRecord {
    AgentRecord {
        record_type: RecordType::Res,
        id: id.into(),
        thread_id: thread_id.into(),
        step,
        fields: vec![
            FieldValue::Str(name.into()),
            FieldValue::Str(data.into()),
            FieldValue::Str(status.into()),
            FieldValue::Int(latency_ms),
        ],
        meta: MetaFields::default(),
    }
}

/// Build an Obs record from a workflow step result.
pub fn obs_record(
    id: &str,
    thread_id: &str,
    step: i64,
    source: &str,
    content: &str,
    confidence: f64,
) -> AgentRecord {
    AgentRecord {
        record_type: RecordType::Obs,
        id: id.into(),
        thread_id: thread_id.into(),
        step,
        fields: vec![
            FieldValue::Str(source.into()),
            FieldValue::Str(content.into()),
            FieldValue::Float(confidence),
        ],
        meta: MetaFields::default(),
    }
}

/// Build a Mem record from a memory entry.
pub fn mem_record(
    id: &str,
    thread_id: &str,
    step: i64,
    key: &str,
    value: &str,
    confidence: f64,
    ttl: Option<i64>,
) -> AgentRecord {
    AgentRecord {
        record_type: RecordType::Mem,
        id: id.into(),
        thread_id: thread_id.into(),
        step,
        fields: vec![
            FieldValue::Str(key.into()),
            FieldValue::Str(value.into()),
            FieldValue::Float(confidence),
            ttl.map(FieldValue::Int).unwrap_or(FieldValue::Null),
        ],
        meta: MetaFields::default(),
    }
}

/// Build an Err record.
pub fn err_record(
    id: &str,
    thread_id: &str,
    step: i64,
    code: &str,
    message: &str,
    source: &str,
    recoverable: bool,
) -> AgentRecord {
    AgentRecord {
        record_type: RecordType::Err,
        id: id.into(),
        thread_id: thread_id.into(),
        step,
        fields: vec![
            FieldValue::Str(code.into()),
            FieldValue::Str(message.into()),
            FieldValue::Str(source.into()),
            FieldValue::Bool(recoverable),
        ],
        meta: MetaFields::default(),
    }
}

/// Create a new empty payload for a session/run.
pub fn new_payload(thread_id: &str, session_id: Option<&str>) -> AgentPayload {
    AgentPayload {
        header: AgentHeader {
            thread_id: Some(thread_id.into()),
            session_id: session_id.map(|s| s.into()),
            record_count: 0,
            ..Default::default()
        },
        records: Vec::new(),
    }
}


/// Extract string from a FieldValue reference.
fn field_str(f: &FieldValue) -> Option<&str> {
    match f {
        FieldValue::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Extract the content string from a Msg record.
pub fn msg_content(record: &AgentRecord) -> Option<&str> {
    if record.record_type != RecordType::Msg {
        return None;
    }
    // content is field index 2
    record.fields.get(2).and_then(|f| field_str(f))
}

/// Extract the role string from a Msg record.
pub fn msg_role(record: &AgentRecord) -> Option<&str> {
    if record.record_type != RecordType::Msg {
        return None;
    }
    // role is field index 0
    record.fields.get(0).and_then(|f| field_str(f))
}

/// Extract the source from an Obs record.
pub fn obs_source(record: &AgentRecord) -> Option<&str> {
    if record.record_type != RecordType::Obs {
        return None;
    }
    record.fields.get(0).and_then(|f| field_str(f))
}

/// Extract the content from an Obs record.
pub fn obs_content(record: &AgentRecord) -> Option<&str> {
    if record.record_type != RecordType::Obs {
        return None;
    }
    record.fields.get(1).and_then(|f| field_str(f))
}

/// Convert an AgentPayload's Msg records to JSON for HTTP response.
/// This is the only place we go ulmen -> JSON (for the external API).
pub fn payload_to_chat_json(payload: &AgentPayload) -> Vec<serde_json::Value> {
    payload
        .records
        .iter()
        .filter(|r| r.record_type == RecordType::Msg)
        .map(|r| {
            let role = msg_role(r).unwrap_or("unknown");
            let content = msg_content(r).unwrap_or("");
            serde_json::json!({
                "role": role,
                "content": content,
            })
        })
        .collect()
}

/// Convert an AgentPayload's Obs records to JSON for HTTP response.
pub fn payload_to_run_json(payload: &AgentPayload) -> Vec<serde_json::Value> {
    payload
        .records
        .iter()
        .filter(|r| r.record_type == RecordType::Obs)
        .map(|r| {
            let source = obs_source(r).unwrap_or("unknown");
            let content = obs_content(r).unwrap_or("");
            serde_json::json!({
                "source": source,
                "content": content,
            })
        })
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Generate a unique record ID.
pub fn gen_id(prefix: &str) -> String {
    format!("{}_{}", prefix, now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_record_roundtrip() {
        let rec = msg_record("m1", "t1", 1, "user", "hello world", 5);
        assert_eq!(rec.record_type, RecordType::Msg);
        assert_eq!(msg_role(&rec), Some("user"));
        assert_eq!(msg_content(&rec), Some("hello world"));
    }

    #[test]
    fn tool_record_fields() {
        let rec = tool_record("t1", "t1", 2, "code_search", "{\"query\":\"auth\"}", "success");
        assert_eq!(rec.record_type, RecordType::Tool);
        assert_eq!(field_str(&rec.fields[0]), Some("code_search"));
    }

    #[test]
    fn obs_record_fields() {
        let rec = obs_record("o1", "t1", 3, "search_step", "found 3 results", 0.95);
        assert_eq!(obs_source(&rec), Some("search_step"));
        assert_eq!(obs_content(&rec), Some("found 3 results"));
    }

    #[test]
    fn mem_record_with_ttl() {
        let rec = mem_record("m1", "t1", 1, "auth:token", "valid", 0.9, Some(3600));
        assert_eq!(rec.record_type, RecordType::Mem);
        assert_eq!(rec.fields[3], FieldValue::Int(3600));
    }

    #[test]
    fn mem_record_without_ttl() {
        let rec = mem_record("m1", "t1", 1, "auth:token", "valid", 0.9, None);
        assert_eq!(rec.fields[3], FieldValue::Null);
    }

    #[test]
    fn err_record_fields() {
        let rec = err_record("e1", "t1", 1, "TOOL_FAIL", "not found", "search", true);
        assert_eq!(rec.record_type, RecordType::Err);
        assert_eq!(rec.fields[3], FieldValue::Bool(true));
    }

    #[test]
    fn new_payload_creates_empty() {
        let p = new_payload("thread_1", Some("session_1"));
        assert_eq!(p.header.thread_id.as_deref(), Some("thread_1"));
        assert_eq!(p.header.session_id.as_deref(), Some("session_1"));
        assert_eq!(p.records.len(), 0);
    }

    #[test]
    fn payload_to_chat_json_converts() {
        let mut p = new_payload("t1", None);
        p.records.push(msg_record("m1", "t1", 1, "user", "hello", 3));
        p.records.push(msg_record("m2", "t1", 2, "assistant", "hi there", 5));
        p.header.record_count = 2;

        let json = payload_to_chat_json(&p);
        assert_eq!(json.len(), 2);
        assert_eq!(json[0]["role"], "user");
        assert_eq!(json[1]["content"], "hi there");
    }

    #[test]
    fn payload_to_run_json_converts() {
        let mut p = new_payload("t1", None);
        p.records.push(obs_record("o1", "t1", 1, "search", "found stuff", 0.9));
        p.header.record_count = 1;

        let json = payload_to_run_json(&p);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["source"], "search");
    }

    #[test]
    fn ulmen_encode_decode_roundtrip() {
        let mut p = new_payload("t1", Some("sess_1"));
        p.records.push(msg_record("m1", "t1", 1, "user", "test message", 5));
        p.records.push(obs_record("o1", "t1", 2, "step_a", "output data", 0.85));
        p.header.record_count = 2;

        let encoded = p.encode();
        let decoded = AgentPayload::decode(&encoded).unwrap();
        assert_eq!(decoded.records.len(), 2);
        assert_eq!(msg_content(&decoded.records[0]), Some("test message"));
        assert_eq!(obs_content(&decoded.records[1]), Some("output data"));
    }
}

/// Parse structured run metadata from an Obs record content string.
/// Content format: "status=X steps=Y tokens=Z latency=Nms"
pub fn parse_run_summary(content: &str) -> RunSummary {
    let mut summary = RunSummary::default();
    for part in content.split_whitespace() {
        if let Some((k, v)) = part.split_once('=') {
            match k {
                "status" => summary.status = v.to_string(),
                "steps" => summary.steps = v.parse().unwrap_or(0),
                "tokens" => summary.tokens = v.parse().unwrap_or(0),
                "latency" => {
                    summary.latency_ms = v.trim_end_matches("ms").parse().unwrap_or(0)
                }
                "tenant" => summary.tenant = Some(v.to_string()),
                _ => {}
            }
        }
    }
    summary
}

#[derive(Debug, Default, Clone)]
pub struct RunSummary {
    pub status: String,
    pub steps: u64,
    pub tokens: u64,
    pub latency_ms: u64,
    pub tenant: Option<String>,
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    #[test]
    fn parse_run_summary_full() {
        let s = parse_run_summary("status=succeeded steps=3 tokens=500 latency=120ms tenant=acme");
        assert_eq!(s.status, "succeeded");
        assert_eq!(s.steps, 3);
        assert_eq!(s.tokens, 500);
        assert_eq!(s.latency_ms, 120);
        assert_eq!(s.tenant.as_deref(), Some("acme"));
    }

    #[test]
    fn parse_run_summary_partial() {
        let s = parse_run_summary("status=failed tokens=0");
        assert_eq!(s.status, "failed");
        assert_eq!(s.tokens, 0);
        assert_eq!(s.latency_ms, 0);
    }
}
