//! ulgate: HTTP API gateway for the ULMEN ecosystem.
//!
//! Bring your model, watch magic happen.
//!
//! Single binary. Zero configuration required.
//! Speak JSON in, get results out.
//!
//! Quick start:
//!   ulgate serve --port 8080 --db ./data --llm groq:llama-3.3-70b-versatile
//!
//! Then:
//!   curl http://localhost:8080/v1/health
//!   curl http://localhost:8080/v1/tools
//!   curl -X POST http://localhost:8080/v1/run \
//!        -d '{"workflow":"analyze","input":{"task":"review auth code"}}'
//!
//! Copyright (c) 2026 El Mehdi Makroumi. All rights reserved.

pub mod auth;
pub mod probes;
pub mod degradation;
pub mod shadow;
pub mod slo;
pub mod oauth;
pub mod bridge;
pub mod config;
pub mod handlers;
pub mod ratelimit;
pub mod response;
pub mod router;
pub mod server;
pub mod tenant;
