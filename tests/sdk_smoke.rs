//! SDK smoke test — proves pixie-pi is consumable as a library dependency.
//!
//! An external crate (this integration test *is* one) constructs an
//! [`AgentSession`] through the public API and reuses the pure `compat` mapper,
//! with **no real API call**. If this compiles and passes, downstream programs
//! can depend on pixie-pi and drive the agent programmatically.

use std::path::PathBuf;

use pixie_pi::compat::{map_events, system_init, to_ndjson, MapCtx};
use pixie_pi::{AgentEvent, AgentSession, ThinkingLevel};

#[test]
fn construct_a_session_via_the_public_api() {
    // Exactly what a downstream program does: pick a model, build tools,
    // construct a session, set credentials — all through the public surface.
    let model = pixie_pi::ai::builtin_models()[0].clone();
    let tools = pixie_pi::tools::coding_tools(PathBuf::from("."));

    let mut session = AgentSession::new(
        PathBuf::from("."),
        "you are a helpful coding agent".to_string(),
        model,
        ThinkingLevel::Off,
        tools,
        reqwest::Client::new(),
    );
    session.api_key = Some("sk-test-not-a-real-key".into());

    // Reachable public helpers a consumer would call.
    assert!(!session.tool_names().is_empty(), "coding tools registered");
    assert_eq!(session.estimated_tokens(), 0, "empty transcript");
    let _ = session.context_usage();
    let _ = pixie_pi::config::session_file(std::path::Path::new("."));
}

#[test]
fn the_compat_mapper_is_reusable_from_sdk_consumers() {
    // The pure Claude stream-json mapper is part of the public API; an SDK
    // consumer can drive it directly (e.g. to emit its own stream-json output).
    let mut ctx =
        MapCtx::new("sess-smoke", "claude-sonnet-4-6", "/tmp/proj", "bypassPermissions");

    // The one-time system init line.
    let init = system_init(&ctx, vec!["read".into(), "bash".into()]);
    let init_v: serde_json::Value = serde_json::from_str(&to_ndjson(&init)).unwrap();
    assert_eq!(init_v["type"], "system");
    assert_eq!(init_v["subtype"], "init");
    assert_eq!(init_v["session_id"], "sess-smoke");

    // A turn terminator maps to exactly one `result` line.
    let lines = map_events(
        &AgentEvent::AgentEnd { messages: vec![] },
        &mut ctx,
    );
    assert_eq!(lines.len(), 1);
    let result_v: serde_json::Value = serde_json::from_str(&to_ndjson(&lines[0])).unwrap();
    assert_eq!(result_v["type"], "result");
    assert_eq!(result_v["session_id"], "sess-smoke");
}
