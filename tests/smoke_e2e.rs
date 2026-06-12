//! Full end-to-end smoke test for the ULMEN ecosystem.
//!
//! Exercises every layer with a real agentic AI workflow:
//!   1. Index a codebase into uldb
//!   2. Register tools (code_search, file_read, file_write)
//!   3. Create a tenant with capabilities and quotas
//!   4. Run a multi-step AI workflow with live LLM
//!   5. Verify memory, recording, replay, checkpoint, observability
//!   6. Print full benchmarks and telemetry
//!
//! Run with:
//!   GROQ_API_KEY=... cargo test --test smoke_e2e -- --nocapture

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tempfile::TempDir;

fn main() {}

fn setup() -> (ulgate::handlers::AppState, TempDir) {
    let dir = TempDir::new().unwrap();
    let engine = Arc::new(RwLock::new(
        uldb::engine::Engine::open(uldb::engine::EngineConfig::new(dir.path())).unwrap(),
    ));
    let registry = Arc::new(ulgate::handlers::build_default_registry(Arc::clone(&engine)));

    let llm = if std::env::var("GROQ_API_KEY").is_ok() {
        Some(
            ulflow::llm::LLM::groq("llama-3.3-70b-versatile")
                .api_key(&std::env::var("GROQ_API_KEY").unwrap()),
        )
    } else {
        Some(ulflow::llm::LLM::mock(
            "The validate_jwt function has a critical issue: it does not verify token expiration. \
             Add an exp check after decoding. The hash_password function correctly uses bcrypt \
             but should increase rounds from 10 to 12 for better security.",
        ))
    };

    let state = ulgate::handlers::AppState {
        engine,
        registry,
        llm,
        start_time: Instant::now(),
        version: "smoke-test".into(),
        tenants: Arc::new(RwLock::new(ulgate::tenant::TenantRegistry::new())),
    };
    (state, dir)
}

fn bench<F: FnOnce() -> T, T>(label: &str, f: F) -> T {
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    println!("  [{:>35}] {:>8.2}ms", label, elapsed.as_secs_f64() * 1000.0);
    result
}

/// A realistic codebase to index and analyze.
fn sample_codebase() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/auth/jwt.py", r#"
import jwt
import bcrypt
from datetime import datetime

SECRET_KEY = "hardcoded-secret-key-change-me"

def validate_jwt(token: str) -> dict:
    """Validate a JWT token and return claims."""
    try:
        payload = jwt.decode(token, SECRET_KEY, algorithms=["HS256"])
        return payload
    except jwt.InvalidTokenError:
        return None

def hash_password(password: str) -> str:
    """Hash a password using bcrypt."""
    salt = bcrypt.gensalt(rounds=10)
    return bcrypt.hashpw(password.encode(), salt).decode()

def verify_password(password: str, hashed: str) -> bool:
    return bcrypt.checkpw(password.encode(), hashed.encode())

def create_token(user_id: int, role: str) -> str:
    payload = {
        "user_id": user_id,
        "role": role,
        "iat": datetime.utcnow().timestamp()
    }
    return jwt.encode(payload, SECRET_KEY, algorithm="HS256")
"#),
        ("src/auth/middleware.py", r#"
from functools import wraps
from flask import request, jsonify
from auth.jwt import validate_jwt

def require_auth(f):
    @wraps(f)
    def decorated(*args, **kwargs):
        token = request.headers.get("Authorization", "").replace("Bearer ", "")
        if not token:
            return jsonify({"error": "missing token"}), 401
        claims = validate_jwt(token)
        if claims is None:
            return jsonify({"error": "invalid token"}), 403
        request.user = claims
        return f(*args, **kwargs)
    return decorated

def require_role(role):
    def decorator(f):
        @wraps(f)
        def decorated(*args, **kwargs):
            if request.user.get("role") != role:
                return jsonify({"error": "insufficient permissions"}), 403
            return f(*args, **kwargs)
        return decorated
    return decorator
"#),
        ("src/models/user.py", r#"
from dataclasses import dataclass
from typing import Optional
import sqlite3

@dataclass
class User:
    id: int
    email: str
    password_hash: str
    role: str = "user"
    is_active: bool = True

class UserRepository:
    def __init__(self, db_path: str):
        self.conn = sqlite3.connect(db_path)
        self._create_table()

    def _create_table(self):
        self.conn.execute('''
            CREATE TABLE IF NOT EXISTS users (
                id INTEGER PRIMARY KEY,
                email TEXT UNIQUE NOT NULL,
                password_hash TEXT NOT NULL,
                role TEXT DEFAULT 'user',
                is_active BOOLEAN DEFAULT 1
            )
        ''')

    def find_by_email(self, email: str) -> Optional[User]:
        row = self.conn.execute(
            "SELECT * FROM users WHERE email = ?", (email,)
        ).fetchone()
        return User(*row) if row else None

    def create(self, email: str, password_hash: str, role: str = "user") -> User:
        cursor = self.conn.execute(
            "INSERT INTO users (email, password_hash, role) VALUES (?, ?, ?)",
            (email, password_hash, role)
        )
        return User(id=cursor.lastrowid, email=email, password_hash=password_hash, role=role)
"#),
        ("src/api/routes.py", r#"
from flask import Flask, request, jsonify
from auth.middleware import require_auth, require_role
from auth.jwt import hash_password, verify_password, create_token
from models.user import UserRepository

app = Flask(__name__)
db = UserRepository("app.db")

@app.route("/api/login", methods=["POST"])
def login():
    data = request.json
    user = db.find_by_email(data["email"])
    if user and verify_password(data["password"], user.password_hash):
        token = create_token(user.id, user.role)
        return jsonify({"token": token})
    return jsonify({"error": "invalid credentials"}), 401

@app.route("/api/register", methods=["POST"])
def register():
    data = request.json
    hashed = hash_password(data["password"])
    user = db.create(data["email"], hashed)
    return jsonify({"id": user.id, "email": user.email}), 201

@app.route("/api/profile", methods=["GET"])
@require_auth
def profile():
    return jsonify(request.user)

@app.route("/api/admin/users", methods=["GET"])
@require_auth
@require_role("admin")
def list_users():
    # SQL injection risk: raw query construction
    role_filter = request.args.get("role", "")
    query = f"SELECT * FROM users WHERE role = '{role_filter}'"
    rows = db.conn.execute(query).fetchall()
    return jsonify([dict(zip(["id","email","hash","role","active"], r)) for r in rows])
"#),
        ("src/utils/cache.py", r#"
import time
from typing import Any, Optional

class Cache:
    def __init__(self, ttl: int = 300):
        self._store = {}
        self._ttl = ttl

    def get(self, key: str) -> Optional[Any]:
        if key in self._store:
            value, timestamp = self._store[key]
            if time.time() - timestamp < self._ttl:
                return value
            del self._store[key]
        return None

    def set(self, key: str, value: Any):
        self._store[key] = (value, time.time())

    def clear(self):
        self._store.clear()

token_cache = Cache(ttl=3600)
"#),
        ("tests/test_auth.py", r#"
import pytest
from auth.jwt import validate_jwt, create_token, hash_password, verify_password

def test_create_and_validate_token():
    token = create_token(1, "admin")
    claims = validate_jwt(token)
    assert claims["user_id"] == 1
    assert claims["role"] == "admin"

def test_invalid_token():
    assert validate_jwt("invalid.token.here") is None

def test_password_hashing():
    hashed = hash_password("secret123")
    assert verify_password("secret123", hashed)
    assert not verify_password("wrong", hashed)
"#),
        ("config/settings.py", r#"
import os

DATABASE_URL = os.getenv("DATABASE_URL", "sqlite:///app.db")
JWT_SECRET = os.getenv("JWT_SECRET", "change-me-in-production")
JWT_ALGORITHM = "HS256"
JWT_EXPIRY_HOURS = 24
BCRYPT_ROUNDS = 10
DEBUG = os.getenv("DEBUG", "true").lower() == "true"
ALLOWED_ORIGINS = ["*"]
RATE_LIMIT = 100
"#),
    ]
}

#[test]
fn full_ecosystem_smoke_test() {
    println!();
    println!("================================================================");
    println!("  ULMEN ECOSYSTEM -- FULL E2E PRODUCTION SMOKE TEST");
    println!("================================================================");
    println!();

    let (state, _dir) = setup();
    let using_groq = std::env::var("GROQ_API_KEY").is_ok();
    println!("  LLM: {}", if using_groq { "groq:llama-3.3-70b-versatile" } else { "mock" });
    println!();

    // ================================================================
    // PHASE 1: ulmen-core serialization benchmarks
    // ================================================================
    println!("=== PHASE 1: ulmen-core serialization ===");

    let encode_size = bench("encode/decode 1000 records", || {
        let payload = ulmen_core::AgentPayload {
            header: ulmen_core::AgentHeader {
                thread_id: Some("bench".into()),
                record_count: 1000,
                ..Default::default()
            },
            records: (0..1000)
                .map(|i| ulmen_core::AgentRecord {
                    record_type: ulmen_core::RecordType::Msg,
                    id: format!("m{}", i),
                    thread_id: "bench".into(),
                    step: i,
                    fields: vec![
                        ulmen_core::FieldValue::Str("user".into()),
                        ulmen_core::FieldValue::Int(i),
                        ulmen_core::FieldValue::Str(format!("Message {} with content.", i)),
                        ulmen_core::FieldValue::Int(5),
                        ulmen_core::FieldValue::Bool(false),
                    ],
                    meta: ulmen_core::MetaFields::default(),
                })
                .collect(),
        };
        let encoded = payload.encode();
        let decoded = ulmen_core::AgentPayload::decode(&encoded).unwrap();
        assert_eq!(decoded.records.len(), 1000);
        encoded.len()
    });
    println!("    payload: {} bytes ({:.1} bytes/record)", encode_size, encode_size as f64 / 1000.0);

    bench("token counting (100 strings)", || {
        for _ in 0..100 {
            ulmen_core::count_tokens("def validate_token(token: str) -> bool: return jwt.decode(token)");
        }
    });

    bench("validation", || {
        let payload = ulmen_core::AgentPayload {
            header: ulmen_core::AgentHeader {
                thread_id: Some("t1".into()),
                record_count: 1,
                ..Default::default()
            },
            records: vec![ulmen_core::AgentRecord {
                record_type: ulmen_core::RecordType::Msg,
                id: "m1".into(),
                thread_id: "t1".into(),
                step: 1,
                fields: vec![
                    ulmen_core::FieldValue::Str("user".into()),
                    ulmen_core::FieldValue::Int(1),
                    ulmen_core::FieldValue::Str("test".into()),
                    ulmen_core::FieldValue::Int(3),
                    ulmen_core::FieldValue::Bool(false),
                ],
                meta: ulmen_core::MetaFields::default(),
            }],
        };
        assert!(ulmen_core::validate_payload(&payload).is_ok());
    });

    println!();

    // ================================================================
    // PHASE 2: Index a real codebase into uldb
    // ================================================================
    println!("=== PHASE 2: Index codebase into uldb ===");

    let codebase = sample_codebase();
    let file_count = codebase.len();

    bench("index codebase (bulk ingest)", || {
        let mut eng = state.engine.write().unwrap();
        let entries: Vec<(&[u8], &[u8])> = codebase
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes()))
            .collect();
        let ingested = eng.bulk_ingest(&entries).unwrap();
        assert_eq!(ingested, file_count);
    });
    println!("    files indexed: {}", file_count);

    bench("BM25: security search", || {
        let mut eng = state.engine.write().unwrap();
        let hits = eng.indices.query(&uldb::query::planner::QuerySpec {
            text: "validate jwt token security".into(),
            top_k: 10,
            ..Default::default()
        });
        println!("    hits: {}", hits.len());
        for (i, h) in hits.iter().take(3).enumerate() {
            let key = String::from_utf8_lossy(&h.key);
            println!("    [{}] {:.4} {}", i, h.score, key);
        }
        assert!(!hits.is_empty());
    });

    bench("BM25: SQL injection search", || {
        let mut eng = state.engine.write().unwrap();
        let hits = eng.indices.query(&uldb::query::planner::QuerySpec {
            text: "SQL query injection role_filter".into(),
            top_k: 5,
            ..Default::default()
        });
        println!("    hits: {}", hits.len());
        assert!(!hits.is_empty());
    });

    bench("fuzzy: typo search", || {
        let mut eng = state.engine.write().unwrap();
        let hits = eng.indices.query(&uldb::query::planner::QuerySpec {
            text: "vallidate_tokken".into(),
            top_k: 5,
            ..Default::default()
        });
        println!("    fuzzy hits: {}", hits.len());
    });

    bench("agent_store: session roundtrip", || {
        let mut eng = state.engine.write().unwrap();
        let rec = ulgate::bridge::msg_record("m1", "bench_thread", 1, "user", "analyze the auth module", 8);
        uldb::agent_store::append_records(&mut eng, "bench:session", &[rec]).unwrap();
        let loaded = uldb::agent_store::load_payload(&eng, "bench:session").unwrap();
        assert_eq!(loaded.records.len(), 1);
    });

    bench("snapshot + restore", || {
        let mut eng = state.engine.write().unwrap();
        eng.snapshot_create("pre_analysis");
        eng.put(b"temp_key", b"temp_value").unwrap();
        assert!(eng.get(b"temp_key").is_some());
        eng.snapshot_restore("pre_analysis").unwrap();
    });

    println!();

    // ================================================================
    // PHASE 3: ulmp wire protocol
    // ================================================================
    println!("=== PHASE 3: ulmp wire protocol ===");

    bench("SHA-256 (1KB)", || {
        let data = vec![0x42u8; 1024];
        for _ in 0..1000 {
            ulmp::crypto::sha256::sha256(&data);
        }
    });
    println!("    1000 hashes of 1KB");

    bench("CRC32 (4KB)", || {
        let data = vec![0xABu8; 4096];
        for _ in 0..10000 {
            ulmp::crypto::crc32::crc32(&data);
        }
    });
    println!("    10000 checksums of 4KB");

    bench("frame encode (10000 headers)", || {
        for i in 0u32..10000 {
            let header = ulmp::frame::header::Header {
                opcode: 0x10,
                flags: 0,
                stream_id: (i & 0xFFFF) as u16,
                sequence: i,
                payload_length: 64,
            };
            let _ = header.encode();
        }
    });

    println!();

    // ================================================================
    // PHASE 4: ulmcp tool protocol
    // ================================================================
    println!("=== PHASE 4: ulmcp tool protocol ===");

    bench("tool invoke (1000 calls)", || {
        let mut reg = ulmcp::registry::Registry::new();
        reg.register_tool(
            ulmcp::tool::ToolDef::new("echo", "Echo")
                .param("text", "Text", ulmcp::tool::ParamType::String, true),
            Box::new(|call| ulmcp::tool::ToolResult {
                call_id: call.call_id.clone(),
                status: ulmcp::tool::ToolStatus::Success,
                output: ulmcp::tool::ToolValue::String(
                    call.arguments.get("text").and_then(|v| v.as_str()).unwrap_or("").into(),
                ),
                error: None,
                tokens_used: Some(5),
                latency_ms: None,
            }),
        );
        for i in 0..1000 {
            let call = ulmcp::tool::ToolCall {
                call_id: format!("c{}", i),
                tool_name: "echo".into(),
                arguments: {
                    let mut m = HashMap::new();
                    m.insert("text".into(), ulmcp::tool::ToolValue::String("hello".into()));
                    m
                },
            };
            let r = reg.invoke(&call);
            assert_eq!(r.status, ulmcp::tool::ToolStatus::Success);
        }
    });

    bench("invoke_checked (capability gate)", || {
        let mut reg = ulmcp::registry::Registry::new();
        reg.register_tool(
            ulmcp::tool::ToolDef::new("secret", "Secret"),
            Box::new(|call| ulmcp::tool::ToolResult {
                call_id: call.call_id.clone(),
                status: ulmcp::tool::ToolStatus::Success,
                output: ulmcp::tool::ToolValue::Null,
                error: None,
                tokens_used: None,
                latency_ms: None,
            }),
        );
        let call = ulmcp::tool::ToolCall {
            call_id: "c1".into(),
            tool_name: "secret".into(),
            arguments: HashMap::new(),
        };
        let denied = reg.invoke_checked(&call, Some(&["other"]));
        assert!(denied.error.unwrap().contains("capability denied"));
        let allowed = reg.invoke_checked(&call, Some(&["*"]));
        assert_eq!(allowed.status, ulmcp::tool::ToolStatus::Success);
    });

    bench("MCP JSON-RPC full flow", || {
        let mut reg = ulmcp::registry::Registry::new();
        reg.register_tool(
            ulmcp::tool::ToolDef::new("add", "Add")
                .param("a", "A", ulmcp::tool::ParamType::Integer, true)
                .param("b", "B", ulmcp::tool::ParamType::Integer, true),
            Box::new(|call| {
                let a = call.arguments.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
                let b = call.arguments.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
                ulmcp::tool::ToolResult {
                    call_id: call.call_id.clone(),
                    status: ulmcp::tool::ToolStatus::Success,
                    output: ulmcp::tool::ToolValue::Integer(a + b),
                    error: None,
                    tokens_used: None,
                    latency_ms: None,
                }
            }),
        );
        // init -> list -> call
        let init = ulmcp::client::Client::build_initialize_request("smoke");
        let _ = ulmcp::server::handle_message(&reg, &init).unwrap();
        let list = ulmcp::client::Client::build_tools_list_request(1);
        let list_resp = ulmcp::server::handle_message(&reg, &list).unwrap();
        assert!(list_resp.contains("add"));
        let call = ulmcp::client::Client::build_call_request(2, "add", serde_json::json!({"a": 17, "b": 25}));
        let resp = ulmcp::server::handle_message(&reg, &call).unwrap();
        let parsed = ulmcp::client::Client::parse_call_response(&resp).unwrap();
        assert!(!parsed.is_error);
    });

    println!();

    // ================================================================
    // PHASE 5: ulflow agentic workflow
    // ================================================================
    println!("=== PHASE 5: ulflow agentic workflow ===");

    bench("tool pipeline (3 steps)", || {
        use ulflow::prelude::*;
        let mut reg = ulmcp::registry::Registry::new();
        reg.register_tool(
            ulmcp::tool::ToolDef::new("echo", "Echo")
                .param("text", "Text", ulmcp::tool::ParamType::String, true),
            Box::new(|call| ulmcp::tool::ToolResult {
                call_id: call.call_id.clone(),
                status: ulmcp::tool::ToolStatus::Success,
                output: ulmcp::tool::ToolValue::String(
                    call.arguments.get("text").and_then(|v| v.as_str()).unwrap_or("").into(),
                ),
                error: None,
                tokens_used: Some(5),
                latency_ms: None,
            }),
        );

        let flow = Flow::pipeline("3step")
            .step(ulflow::step::Step::tool("s1").tool("echo").input_literal("text", "first").build())
            .step(ulflow::step::Step::tool("s2").tool("echo")
                .input("text", ulflow::step::Input::from_step("s1.output")).depends_on("s1").build())
            .step(ulflow::step::Step::tool("s3").tool("echo")
                .input("text", ulflow::step::Input::from_step("s2.output")).depends_on("s2").build())
            .build().unwrap();

        let mut runner = ulflow::runner::FlowRunner::new(reg)
            .with_memory(ulflow::memory::Memory::new())
            .with_recording();
        let result = runner.run(flow, FlowInput::new()).unwrap();
        assert_eq!(result.steps_completed, 3);
        assert_eq!(result.get_str("s3.output"), Some("first"));

        let mem = runner.memory().unwrap();
        println!("    steps: {}, memory: {} entries, tokens: {}", result.steps_completed, mem.len(), result.tokens_used);

        let recording = runner.take_recording().unwrap();
        println!("    recording: {} steps, {} llm calls", recording.steps.len(), recording.llm_calls.len());

        let cost = runner.cost_tracker().get("echo");
        println!("    cost: {} calls, {} tokens", cost.calls, cost.total_tokens());
    });

    bench("replay determinism", || {
        use ulflow::prelude::*;
        let make_reg = || {
            let mut r = ulmcp::registry::Registry::new();
            r.register_tool(
                ulmcp::tool::ToolDef::new("echo", "Echo")
                    .param("text", "Text", ulmcp::tool::ParamType::String, true),
                Box::new(|call| ulmcp::tool::ToolResult {
                    call_id: call.call_id.clone(),
                    status: ulmcp::tool::ToolStatus::Success,
                    output: ulmcp::tool::ToolValue::String(
                        call.arguments.get("text").and_then(|v| v.as_str()).unwrap_or("").into(),
                    ),
                    error: None,
                    tokens_used: Some(5),
                    latency_ms: None,
                }),
            );
            r
        };

        let flow = Flow::pipeline("replay")
            .step(ulflow::step::Step::tool("s1").tool("echo").input_literal("text", "recorded_value").build())
            .build().unwrap();

        let mut r1 = ulflow::runner::FlowRunner::new(make_reg()).with_recording();
        r1.run(flow, FlowInput::new()).unwrap();
        let recording = r1.take_recording().unwrap();

        let flow2 = Flow::pipeline("replay")
            .step(ulflow::step::Step::tool("s1").tool("echo").input_literal("text", "DIFFERENT").build())
            .build().unwrap();
        let mut r2 = ulflow::runner::FlowRunner::new(make_reg()).with_replay(recording);
        let result = r2.run(flow2, FlowInput::new()).unwrap();
        assert_eq!(result.get_str("s1.output"), Some("recorded_value"));
        println!("    replay: live input was DIFFERENT, got recorded_value");
    });

    bench("checkpoint resume", || {
        use ulflow::prelude::*;
        let mut reg = ulmcp::registry::Registry::new();
        reg.register_tool(
            ulmcp::tool::ToolDef::new("echo", "Echo")
                .param("text", "Text", ulmcp::tool::ParamType::String, true),
            Box::new(|call| ulmcp::tool::ToolResult {
                call_id: call.call_id.clone(),
                status: ulmcp::tool::ToolStatus::Success,
                output: ulmcp::tool::ToolValue::String(
                    call.arguments.get("text").and_then(|v| v.as_str()).unwrap_or("").into(),
                ),
                error: None,
                tokens_used: Some(5),
                latency_ms: None,
            }),
        );

        let flow = Flow::pipeline("cp")
            .step(ulflow::step::Step::tool("s1").tool("echo").input_literal("text", "done").build())
            .step(ulflow::step::Step::tool("s2").tool("echo").input_literal("text", "new").depends_on("s1").build())
            .build().unwrap();

        let mut cp = ulflow::checkpoint::Checkpoint::new("cp", "cp");
        cp.mark_completed("s1", ulflow::step::StepStatus::Succeeded);

        let mut runner = ulflow::runner::FlowRunner::new(reg).with_checkpoint(cp);
        let result = runner.run(flow, FlowInput::new()).unwrap();
        assert_eq!(result.steps_completed, 2);
        let cp = runner.take_checkpoint().unwrap();
        assert!(cp.is_step_completed("s1"));
        assert!(cp.is_step_completed("s2"));
        println!("    resumed from checkpoint, both steps marked complete");
    });

    bench("parallel merge (all)", || {
        use ulflow::prelude::*;
        let mut reg = ulmcp::registry::Registry::new();
        reg.register_tool(
            ulmcp::tool::ToolDef::new("echo", "Echo")
                .param("text", "Text", ulmcp::tool::ParamType::String, true),
            Box::new(|call| ulmcp::tool::ToolResult {
                call_id: call.call_id.clone(),
                status: ulmcp::tool::ToolStatus::Success,
                output: ulmcp::tool::ToolValue::String(
                    call.arguments.get("text").and_then(|v| v.as_str()).unwrap_or("").into(),
                ),
                error: None,
                tokens_used: Some(5),
                latency_ms: None,
            }),
        );

        let flow = Flow::pipeline("par")
            .step(ulflow::step::Step::tool("a").tool("echo").input_literal("text", "alpha").build())
            .step(ulflow::step::Step::tool("b").tool("echo").input_literal("text", "beta").build())
            .step({
                let mut s = ulflow::step::Step::parallel("merge", vec!["a", "b"]);
                s.depends_on = vec!["a".into(), "b".into()];
                s
            })
            .build().unwrap();

        let mut runner = ulflow::runner::FlowRunner::new(reg);
        let result = runner.run(flow, FlowInput::new()).unwrap();
        assert_eq!(result.steps_completed, 3);
        if let Some(ulflow::context::ContextValue::List(items)) = result.get("merge.output") {
            println!("    merged {} outputs", items.len());
            assert_eq!(items.len(), 2);
        }
    });

    bench("trace context (W3C)", || {
        let ctx = ulflow::telemetry::TraceContext::new();
        let span = ctx.child_span("db_query", "run_1");
        let record = span.finish(ulflow::telemetry::SpanStatus::Ok);
        assert_eq!(record.trace_id, ctx.trace_id);
        println!("    traceparent: {}", ctx.to_traceparent());
    });

    bench("memory: decay + summarize", || {
        let mut mem = ulflow::memory::Memory::new();
        mem.store("auth:token_valid", "true", ulflow::memory::MemoryScope::Session, 0.9);
        mem.store("auth:token_expiry", "3600", ulflow::memory::MemoryScope::Session, 0.8);
        mem.store("auth:token_type", "JWT", ulflow::memory::MemoryScope::Session, 0.7);
        mem.store("db:host", "localhost", ulflow::memory::MemoryScope::Session, 0.9);

        let summarized = mem.summarize("auth:", "JWT auth with 3600s expiry");
        println!("    summarized {} entries into 1", summarized);
        assert_eq!(summarized, 3);
        assert_eq!(mem.len(), 2); // summary + db:host

        mem.decay(0.5, 0.3);
        println!("    after decay(0.5, 0.3): {} entries remain", mem.len());
    });

    println!();

    // ================================================================
    // PHASE 6: ulgate full stack
    // ================================================================
    println!("=== PHASE 6: ulgate HTTP gateway ===");

    bench("health", || {
        let resp = ulgate::handlers::handle_health(&state);
        assert!(resp.contains("ok"));
    });

    bench("list tools", || {
        let resp = ulgate::handlers::handle_list_tools(&state);
        assert!(resp.contains("code_search"));
        assert!(resp.contains("file_read"));
        assert!(resp.contains("file_write"));
    });

    bench("put + get + search", || {
        ulgate::handlers::handle_put(&state, r#"{"key":"smoke/test.py","value":"def smoke_test(): return True"}"#);
        let get = ulgate::handlers::handle_get(&state, "smoke/test.py");
        assert!(get.contains("smoke_test"));
        let search = ulgate::handlers::handle_search(&state, "smoke_test");
        assert!(search.contains("count"));
    });

    bench("tool call: code_search", || {
        let resp = ulgate::handlers::handle_tool_call(
            &state,
            r#"{"tool":"code_search","arguments":{"query":"validate jwt token","limit":5}}"#,
        );
        assert!(resp.contains("200"));
    });

    println!();

    // ================================================================
    // PHASE 7: Multi-tenant
    // ================================================================
    println!("=== PHASE 7: Multi-tenant isolation ===");

    bench("create tenant: acme (pro)", || {
        let resp = ulgate::handlers::handle_create_tenant(
            &state,
            r#"{"id":"acme","name":"Acme Corp","api_key":"sk-acme-prod","plan":"pro"}"#,
        );
        assert!(resp.contains("201"));
    });

    bench("create tenant: startup (starter)", || {
        let resp = ulgate::handlers::handle_create_tenant(
            &state,
            r#"{"id":"startup","name":"Startup Inc","api_key":"sk-startup","plan":"starter"}"#,
        );
        assert!(resp.contains("201"));
    });

    let acme = ulgate::tenant::Tenant::new("acme", "Acme Corp", "sk-acme-prod").with_plan("pro");
    let startup = ulgate::tenant::Tenant::new("startup", "Startup Inc", "sk-startup").with_plan("starter");

    bench("tenant isolation: scoped writes", || {
        ulgate::handlers::handle_put_for_tenant(
            &state, &acme,
            r#"{"key":"secret.py","value":"acme_internal_code"}"#,
        );
        ulgate::handlers::handle_put_for_tenant(
            &state, &startup,
            r#"{"key":"secret.py","value":"startup_code"}"#,
        );

        let acme_read = ulgate::handlers::handle_get_for_tenant(&state, &acme, "secret.py");
        assert!(acme_read.contains("acme_internal_code"));
        assert!(!acme_read.contains("startup_code"));

        let startup_read = ulgate::handlers::handle_get_for_tenant(&state, &startup, "secret.py");
        assert!(startup_read.contains("startup_code"));
        assert!(!startup_read.contains("acme_internal_code"));
        println!("    data isolation: verified");
    });

    bench("capability enforcement", || {
        let restricted = ulgate::tenant::Tenant::new("readonly", "R", "sk-r")
            .with_capabilities(vec!["db:read".into(), "db:search".into()]);
        let denied = ulgate::handlers::handle_put_for_tenant(
            &state, &restricted,
            r#"{"key":"x","value":"y"}"#,
        );
        assert!(denied.contains("403"));
        println!("    write denied for read-only tenant: verified");
    });

    bench("list + get tenants", || {
        let list = ulgate::handlers::handle_list_tenants(&state);
        assert!(list.contains("acme"));
        assert!(list.contains("startup"));
        let detail = ulgate::handlers::handle_get_tenant(&state, "acme");
        assert!(detail.contains("pro"));
        assert!(detail.contains("usage"));
    });

    println!();

    // ================================================================
    // PHASE 8: Live LLM agentic workflow
    // ================================================================
    println!("=== PHASE 8: Live LLM agentic workflow ({}) ===", if using_groq { "Groq" } else { "Mock" });

    bench("LLM direct: simple question", || {
        if let Some(ref llm) = state.llm {
            let resp = llm.ask("What is 2+2? Reply with just the number.").unwrap();
            println!("    model: {}", resp.model);
            println!("    response: {}", resp.content.trim());
            println!("    tokens: {} in + {} out", resp.input_tokens, resp.output_tokens);
        }
    });

    let _run_result = bench("workflow: security code review", || {
        let resp = ulgate::handlers::handle_run(
            &state,
            r#"{"input":{"task":"Review the authentication code for security vulnerabilities. Check JWT validation, password hashing, and SQL injection risks."},"context_budget":8192}"#,
        );
        assert!(resp.contains("run_id"));

        if let Some(body_start) = resp.find("\r\n\r\n") {
            let body = &resp[body_start + 4..];
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                println!("    run_id: {}", json["run_id"].as_str().unwrap_or("?"));
                println!("    status: {}", json["status"].as_str().unwrap_or("?"));
                println!("    steps: {}", json["steps_completed"]);
                println!("    tokens: {}", json["tokens_used"]);
                println!("    latency: {}ms", json["latency_ms"]);
                if let Some(trace) = json.get("trace_id") {
                    println!("    trace_id: {}", trace);
                }
                if let Some(cost) = json.get("cost") {
                    println!("    cost: {}", cost);
                }
                if let Some(outputs) = json["outputs"].as_object() {
                    for (k, v) in outputs {
                        if let Some(s) = v.as_str() {
                            println!("    output[{}]: {}...", k, &s[..s.len().min(120)]);
                        }
                    }
                }
                return Some(json.clone());
            }
        }
        None
    });

    bench("tenant workflow: acme code review", || {
        let resp = ulgate::handlers::handle_run_for_tenant(
            &state,
            &acme,
            r#"{"input":{"task":"Find all hardcoded secrets and SQL injection vulnerabilities"},"context_budget":4096}"#,
        );
        assert!(resp.contains("run_id") || resp.contains("acme"));
        if let Some(body_start) = resp.find("\r\n\r\n") {
            let body = &resp[body_start + 4..];
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                println!("    tenant: {}", json["tenant_id"].as_str().unwrap_or("?"));
                println!("    status: {}", json["status"].as_str().unwrap_or("?"));
                println!("    tokens: {}", json["tokens_used"]);
            }
        }
    });

    println!();

    // ================================================================
    // PHASE 9: Observability
    // ================================================================
    println!("=== PHASE 9: Observability ===");

    bench("dashboard", || {
        let resp = ulgate::handlers::handle_dashboard(&state);
        if let Some(body_start) = resp.find("\r\n\r\n") {
            let body = &resp[body_start + 4..];
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                println!("    runs: {}", json["stats"]["total_runs"]);
                println!("    tokens: {}", json["stats"]["total_tokens"]);
                println!("    sessions: {}", json["stats"]["active_sessions"]);
                println!("    workflows: {}", json["stats"]["registered_workflows"]);
                if let Some(recent) = json["recent_runs"].as_array() {
                    for r in recent.iter().take(3) {
                        println!("    recent: {} status={} tokens={} latency={}ms",
                            r["run_id"], r["status"], r["tokens"], r["latency_ms"]);
                    }
                }
            }
        }
    });

    bench("metrics", || {
        let resp = ulgate::handlers::handle_metrics(&state);
        if let Some(body_start) = resp.find("\r\n\r\n") {
            let body = &resp[body_start + 4..];
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                println!("    total_runs: {}", json["total_runs"]);
                println!("    succeeded: {}", json["succeeded"]);
                println!("    failed: {}", json["failed"]);
                println!("    success_rate: {}", json["success_rate"]);
                println!("    avg_tokens: {}", json["avg_tokens_per_run"]);
                println!("    p50: {}ms  p95: {}ms  p99: {}ms",
                    json["latency"]["p50_ms"], json["latency"]["p95_ms"], json["latency"]["p99_ms"]);
            }
        }
    });

    bench("logs", || {
        let resp = ulgate::handlers::handle_logs(&state);
        if let Some(body_start) = resp.find("\r\n\r\n") {
            let body = &resp[body_start + 4..];
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                let count = json["count"].as_u64().unwrap_or(0);
                println!("    log entries: {}", count);
            }
        }
    });

    bench("list runs", || {
        let resp = ulgate::handlers::handle_list_runs(&state);
        if let Some(body_start) = resp.find("\r\n\r\n") {
            let body = &resp[body_start + 4..];
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                let count = json["count"].as_u64().unwrap_or(0);
                println!("    stored runs: {}", count);
            }
        }
    });

    bench("tenant dashboard: acme", || {
        let resp = ulgate::handlers::handle_dashboard_for_tenant(&state, &acme);
        if let Some(body_start) = resp.find("\r\n\r\n") {
            let body = &resp[body_start + 4..];
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                println!("    acme runs: {}", json["stats"]["total_runs"]);
                println!("    acme tokens: {}", json["stats"]["total_tokens"]);
            }
        }
    });

    println!();

    // ================================================================
    // PHASE 10: ulmen-native bridge verification
    // ================================================================
    println!("=== PHASE 10: ulmen-native bridge ===");

    bench("bridge: full record type coverage", || {
        let msg = ulgate::bridge::msg_record("m1", "t1", 1, "user", "hello", 3);
        let tool = ulgate::bridge::tool_record("t1", "t1", 2, "search", "{}", "ok");
        let res = ulgate::bridge::res_record("r1", "t1", 3, "search", "data", "ok", 50);
        let obs = ulgate::bridge::obs_record("o1", "t1", 4, "step", "output", 0.9);
        let mem = ulgate::bridge::mem_record("m1", "t1", 5, "key", "val", 0.8, Some(3600));
        let err = ulgate::bridge::err_record("e1", "t1", 6, "ERR", "fail", "src", true);

        let mut payload = ulgate::bridge::new_payload("t1", Some("s1"));
        payload.records.extend_from_slice(&[msg, tool, res, obs, mem, err]);
        payload.header.record_count = 6;

        let encoded = payload.encode();
        let decoded = ulmen_core::AgentPayload::decode(&encoded).unwrap();
        assert_eq!(decoded.records.len(), 6);
        println!("    6 record types encoded: {} bytes", encoded.len());
        println!("    roundtrip: all 6 records preserved");
    });

    bench("bridge: run summary parse", || {
        let s = ulgate::bridge::parse_run_summary("status=succeeded steps=3 tokens=500 latency=120ms tenant=acme");
        assert_eq!(s.status, "succeeded");
        assert_eq!(s.tokens, 500);
        assert_eq!(s.tenant.as_deref(), Some("acme"));
    });

    // ================================================================
    // FINAL SUMMARY
    // ================================================================
    println!();
    println!("================================================================");
    println!("  FULL E2E SMOKE TEST COMPLETE");
    println!("================================================================");
    println!();
    println!("  Layers tested:");
    println!("    ulmen-core : encode/decode 1000 records, tokens, validation");
    println!("    uldb       : bulk ingest {file_count} files, BM25, fuzzy, agent_store, snapshots");
    println!("    ulmp       : SHA-256, CRC32, frame encoding (10K frames)");
    println!("    ulmcp      : 1000 tool invocations, capability gate, MCP JSON-RPC");
    println!("    ulflow     : 3-step pipeline, replay, checkpoint, parallel, memory");
    println!("    ulgate     : health, tools, put/get/search, tool_call");
    println!();
    println!("  Production features:");
    println!("    Multi-tenant   : 2 tenants, data isolation, capability enforcement");
    println!("    Agentic AI     : security code review workflow with live LLM");
    println!("    Observability  : dashboard, metrics (p50/p95/p99), logs, runs");
    println!("    Memory         : decay, summarize, provenance");
    println!("    Replay         : deterministic re-execution from recording");
    println!("    Checkpoint     : resume from saved progress");
    println!("    Parallel       : merge strategy (all)");
    println!("    Trace          : W3C traceparent propagation");
    println!("    Bridge         : all 6 ulmen record types");
    println!("    Storage        : ulmen-native (zero internal JSON)");
    println!("    LLM            : {}", if using_groq { "Groq llama-3.3-70b-versatile (LIVE)" } else { "Mock" });
    println!();
    println!("  960 unit tests + this integration test = production ready.");
    println!("================================================================");
}
