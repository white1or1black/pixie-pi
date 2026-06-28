//! Claude Code `stream-json` envelope shapes + the pixie→Claude event mapper.
//!
//! Each line of Claude's `stream-json` output is a JSON object externally
//! tagged by `type`. Pixie (and other `claude` integrations) drive a turn by
//! reading NDJSON lines off stdout until a `result` line ends the turn. This
//! module maps pixie-pi's [`AgentEvent`]s onto those four envelope types:
//!
//! | Claude line  | Emitted from                                             |
//! |--------------|----------------------------------------------------------|
//! | `system`     | once at process start ([`system_init`]; not an event)    |
//! | `assistant`  | [`AgentEvent::MessageEnd`] of an assistant message       |
//! | `user`       | [`AgentEvent::MessageEnd`] of a tool-result message      |
//! | `result`     | [`AgentEvent::AgentEnd`] (exactly one per turn)          |
//!
//! [`AgentEvent`]: crate::agent::context::AgentEvent
//! [`AgentEvent::MessageEnd`]: crate::agent::context::AgentEvent::MessageEnd
//! [`AgentEvent::AgentEnd`]: crate::agent::context::AgentEvent::AgentEnd

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::agent::context::AgentEvent;
use crate::ai::types::{
    AssistantMessage, ContentBlock, Message, StopReason, ToolResultContent, ToolResultMessage,
    Usage,
};

/// Per-process / per-turn state the mapper accumulates as events flow through
/// [`map_events`]. The immutable fields (`session_id`, `model`, …) describe the
/// run; the rest are updated in place so the final `result` line can report
/// totals (usage, cost, turn count, duration, …).
#[derive(Debug, Clone)]
pub struct MapCtx {
    /// Stable id for this session (echoed in every line). Usually the
    /// `--session-id` value, else a freshly generated id.
    pub session_id: String,
    /// Model id driving the run (e.g. `claude-sonnet-4-6`).
    pub model: String,
    /// Working directory, surfaced in the `system.init` line.
    pub cwd: String,
    /// Permission mode string (e.g. `bypassPermissions`).
    pub permission_mode: String,

    // --- mutable accumulators (advanced by map_events) ---
    usage: Usage,
    num_turns: usize,
    final_text: String,
    last_stop_reason: Option<String>,
    error: Option<String>,
    /// Wall-clock duration of the turn, set by the driver from
    /// [`std::time::Instant`] right before the `result` line is mapped.
    pub duration_ms: u64,
}

impl MapCtx {
    /// Construct a context with zeroed accumulators.
    pub fn new(
        session_id: impl Into<String>,
        model: impl Into<String>,
        cwd: impl Into<String>,
        permission_mode: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            model: model.into(),
            cwd: cwd.into(),
            permission_mode: permission_mode.into(),
            usage: Usage::default(),
            num_turns: 0,
            final_text: String::new(),
            last_stop_reason: None,
            error: None,
            duration_ms: 0,
        }
    }

    /// Stamp the turn duration (called by the driver on `AgentEnd`).
    pub fn set_duration_ms(&mut self, ms: u64) {
        self.duration_ms = ms;
    }
}

/// One NDJSON line of Claude's `stream-json` output.
///
/// Serialized with an internal `type` tag so each variant renders as
/// `{"type":"<variant>", …}`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ClaudeLine {
    /// `system` init line — emitted once at process start (not from an event).
    #[serde(rename = "system")]
    System {
        subtype: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        session_id: String,
        model: String,
        #[serde(rename = "permissionMode", skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        tools: Vec<String>,
        /// `mcpServers` map (empty — pixie-pi has no MCP server surface yet).
        #[serde(rename = "mcpServers", skip_serializing_if = "Option::is_none")]
        mcp_servers: Option<Value>,
        version: String,
    },
    /// `assistant` line carrying a full assistant message.
    #[serde(rename = "assistant")]
    Assistant {
        message: Value,
        session_id: String,
    },
    /// `user` line carrying tool results back to the model's view.
    #[serde(rename = "user")]
    User {
        message: Value,
        session_id: String,
    },
    /// `result` line — the turn terminator (exactly one per turn).
    #[serde(rename = "result")]
    Result {
        subtype: String,
        is_error: bool,
        result: String,
        session_id: String,
        #[serde(rename = "total_cost_usd")]
        total_cost_usd: f64,
        duration_ms: u64,
        num_turns: u64,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        /// Token totals with Claude's snake_case keys.
        usage: Value,
        /// Per-model token totals with Claude's camelCase keys.
        #[serde(rename = "modelUsage")]
        model_usage: Value,
    },
}

impl ClaudeLine {
    /// Build a `system` init line from the run context + active tool names.
    fn system(ctx: &MapCtx, tools: Vec<String>) -> Self {
        ClaudeLine::System {
            subtype: "init".to_string(),
            cwd: Some(ctx.cwd.clone()),
            session_id: ctx.session_id.clone(),
            model: ctx.model.clone(),
            permission_mode: Some(ctx.permission_mode.clone()),
            tools,
            mcp_servers: Some(json!({})),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Build an `assistant` line from a pixie assistant message.
    fn assistant(a: &AssistantMessage, ctx: &MapCtx) -> Self {
        let content: Vec<Value> = a.content.iter().map(map_content_block).collect();
        let message = json!({
            "id": a.response_id.clone().unwrap_or_else(|| "msg_0".to_string()),
            "type": "message",
            "role": "assistant",
            "model": if a.model.is_empty() { ctx.model.clone() } else { a.model.clone() },
            "content": content,
            "stop_reason": map_stop_reason(a.stop_reason).unwrap_or("end_turn"),
            "stop_sequence": Value::Null,
            "usage": usage_tokens(&a.usage),
        });
        ClaudeLine::Assistant {
            message,
            session_id: ctx.session_id.clone(),
        }
    }

    /// Build a `user` line carrying a single tool result.
    fn user_tool_result(t: &ToolResultMessage, ctx: &MapCtx) -> Self {
        let text: String = t
            .content
            .iter()
            .filter_map(|c| match c {
                ToolResultContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let block = json!({
            "type": "tool_result",
            "tool_use_id": t.tool_call_id,
            "content": text,
            "is_error": t.is_error,
        });
        ClaudeLine::User {
            message: json!({ "role": "user", "content": [block] }),
            session_id: ctx.session_id.clone(),
        }
    }

    /// Build the turn-terminating `result` line from accumulated context.
    fn result(ctx: &MapCtx) -> Self {
        let is_error = ctx.error.is_some();
        let result_text = ctx
            .error
            .clone()
            .unwrap_or_else(|| ctx.final_text.clone());
        let model_usage = per_model_usage(&ctx.model, &ctx.usage);
        ClaudeLine::Result {
            subtype: if is_error {
                "error".to_string()
            } else {
                "success".to_string()
            },
            is_error,
            result: result_text,
            session_id: ctx.session_id.clone(),
            total_cost_usd: ctx.usage.cost.total,
            duration_ms: ctx.duration_ms,
            num_turns: ctx.num_turns as u64,
            model: ctx.model.clone(),
            stop_reason: ctx.last_stop_reason.clone(),
            usage: usage_tokens(&ctx.usage),
            model_usage,
        }
    }
}

/// Map one pixie [`AgentEvent`] to zero or more Claude [`ClaudeLine`]s, advancing
/// `ctx` in place (accumulating usage, turn count, final text, …).
///
/// Returns lines in emission order; the caller flushes each as NDJSON. Most
/// events produce no line — only assistant/tool-result message ends and the
/// agent-end boundary do.
pub fn map_events(ev: &AgentEvent, ctx: &mut MapCtx) -> Vec<ClaudeLine> {
    let mut out = Vec::new();
    match ev {
        // Per-turn usage arrives as its own event (after the assistant message
        // end); fold it into the running total for the final `result` line.
        AgentEvent::Usage(u) => ctx.usage.add(u),

        AgentEvent::MessageEnd(msg) => match msg {
            Message::Assistant(a) => {
                ctx.num_turns += 1;
                if let Some(err) = &a.error_message {
                    ctx.error = Some(err.clone());
                }
                ctx.last_stop_reason = map_stop_reason(a.stop_reason).map(str::to_string);
                let text = a.text_content();
                if !text.is_empty() {
                    ctx.final_text = text;
                }
                out.push(ClaudeLine::assistant(a, ctx));
            }
            Message::ToolResult(t) => out.push(ClaudeLine::user_tool_result(t, ctx)),
            // The user's own prompt is not echoed — real claude doesn't emit a
            // `user` line for it, and doing so would read as a tool_result.
            Message::User(_) => {}
        },

        // A fatal loop failure is folded into the single `result` terminator
        // below (is_error=true) so the turn still ends with exactly one line.
        AgentEvent::Error(msg) => ctx.error = Some(msg.clone()),

        AgentEvent::AgentEnd { messages } => {
            // Recover a final text if no assistant turn produced one.
            if ctx.final_text.is_empty() {
                if let Some(text) = messages.iter().rev().find_map(|m| match m {
                    Message::Assistant(a) => {
                        let t = a.text_content();
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    }
                    _ => None,
                }) {
                    ctx.final_text = text;
                }
            }
            out.push(ClaudeLine::result(ctx));
        }

        _ => {}
    }
    out
}

/// Build the one-time `system` init line (emitted at process start, before any
/// event). `tools` is the list of active tool names.
pub fn system_init(ctx: &MapCtx, tools: Vec<String>) -> ClaudeLine {
    ClaudeLine::system(ctx, tools)
}

/// Serialize a [`ClaudeLine`] to a single NDJSON line (no trailing newline).
pub fn to_ndjson(line: &ClaudeLine) -> String {
    serde_json::to_string(line).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// shape mappers
// ---------------------------------------------------------------------------

/// Map a pixie assistant content block onto Claude's block shapes:
/// - `Text`            → `{type:"text",text}`
/// - `Thinking`        → `{type:"thinking",thinking}` (signature/redacted dropped)
/// - `ToolCall`        → `{type:"tool_use",id,name,input}` (pixie's `arguments`
///   becomes Claude's `input`)
fn map_content_block(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { thinking, .. } => {
            json!({ "type": "thinking", "thinking": thinking })
        }
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": arguments,
        }),
    }
}

/// Map pixie's stop reason onto Claude's.
fn map_stop_reason(r: StopReason) -> Option<&'static str> {
    Some(match r {
        StopReason::Stop => "end_turn",
        StopReason::Length => "max_tokens",
        StopReason::ToolUse => "tool_use",
        // Provider/stream failures don't have a dedicated Claude stop reason;
        // surface them via the `result` line's `is_error` instead.
        StopReason::Error | StopReason::Aborted => "end_turn",
    })
}

/// Claude usage object with snake_case token keys. pixie's `cache_write` maps
/// to Anthropic's `cache_creation`.
fn usage_tokens(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input,
        "output_tokens": u.output,
        "cache_creation_input_tokens": u.cache_write,
        "cache_read_input_tokens": u.cache_read,
    })
}

/// Claude `modelUsage` map (camelCase keys) for the result line.
fn per_model_usage(model: &str, u: &Usage) -> Value {
    let mut m = Map::new();
    m.insert(
        model.to_string(),
        json!({
            "inputTokens": u.input,
            "outputTokens": u.output,
            "cacheReadInputTokens": u.cache_read,
            "cacheCreationInputTokens": u.cache_write,
        }),
    );
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::types::{AssistantMessage, ContentBlock, Message, ToolResultContent,
        ToolResultMessage, Usage};

    fn ctx() -> MapCtx {
        MapCtx::new("sess-1", "claude-sonnet-4-6", "/tmp/proj", "bypassPermissions")
    }

    fn assistant_text(text: &str, usage: Usage) -> AssistantMessage {
        let mut a = AssistantMessage::empty();
        a.content.push(ContentBlock::Text {
            text: text.to_string(),
        });
        a.usage = usage;
        a
    }

    #[test]
    fn system_init_has_init_subtype_and_model() {
        let line = system_init(&ctx(), vec!["read".into(), "bash".into()]);
        let v: Value = serde_json::from_str(&to_ndjson(&line)).unwrap();
        assert_eq!(v["type"], "system");
        assert_eq!(v["subtype"], "init");
        assert_eq!(v["model"], "claude-sonnet-4-6");
        assert_eq!(v["session_id"], "sess-1");
        assert_eq!(v["permissionMode"], "bypassPermissions");
        assert_eq!(v["cwd"], "/tmp/proj");
        assert_eq!(v["tools"][0], "read");
    }

    #[test]
    fn assistant_message_end_emits_assistant_line_with_text_block() {
        let mut c = ctx();
        let a = assistant_text("Hello", Usage::default());
        let lines = map_events(
            &AgentEvent::MessageEnd(Message::Assistant(a)),
            &mut c,
        );
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&to_ndjson(&lines[0])).unwrap();
        assert_eq!(v["type"], "assistant");
        assert_eq!(v["session_id"], "sess-1");
        assert_eq!(v["message"]["role"], "assistant");
        assert_eq!(v["message"]["content"][0]["type"], "text");
        assert_eq!(v["message"]["content"][0]["text"], "Hello");
    }

    #[test]
    fn tool_call_block_is_tool_use_with_input_not_arguments() {
        let mut c = ctx();
        let mut a = AssistantMessage::empty();
        a.content.push(ContentBlock::ToolCall {
            id: "tu_1".into(),
            name: "read".into(),
            arguments: json!({ "path": "/a" }),
        });
        let lines = map_events(&AgentEvent::MessageEnd(Message::Assistant(a)), &mut c);
        let block = serde_json::from_str::<Value>(&to_ndjson(&lines[0])).unwrap()["message"]
            ["content"][0]
            .clone();
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "tu_1");
        assert_eq!(block["name"], "read");
        // pixie's `arguments` is remapped to Claude's `input`.
        assert_eq!(block["input"]["path"], "/a");
        assert!(block.get("arguments").is_none());
    }

    #[test]
    fn tool_result_message_end_emits_user_line_with_tool_result_block() {
        let mut c = ctx();
        let t = ToolResultMessage {
            tool_call_id: "tu_1".into(),
            tool_name: "read".into(),
            content: vec![ToolResultContent::text("file body")],
            is_error: false,
            timestamp: 0,
        };
        let lines = map_events(&AgentEvent::MessageEnd(Message::ToolResult(t)), &mut c);
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&to_ndjson(&lines[0])).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["type"], "tool_result");
        assert_eq!(v["message"]["content"][0]["tool_use_id"], "tu_1");
        assert_eq!(v["message"]["content"][0]["content"], "file body");
        assert_eq!(v["message"]["content"][0]["is_error"], false);
    }

    #[test]
    fn user_prompt_is_not_echoed_as_a_user_line() {
        let mut c = ctx();
        let prompt = Message::User(crate::ai::types::UserMessage::text("ignored prompt"));
        let lines = map_events(&AgentEvent::MessageEnd(prompt), &mut c);
        assert!(lines.is_empty(), "injected user prompt must not be echoed");
    }

    #[test]
    fn usage_event_accumulates_into_context_and_emits_no_line() {
        let mut c = ctx();
        let a = assistant_text("Hi", Usage::default());
        map_events(&AgentEvent::MessageEnd(Message::Assistant(a)), &mut c);
        // The assistant message's own usage is in its line; the aggregate Usage
        // event folds into the context for the result line.
        let lines = map_events(
            &AgentEvent::Usage(Usage {
                input: 100,
                output: 40,
                cache_read: 5,
                cache_write: 2,
                ..Usage::default()
            }),
            &mut c,
        );
        assert!(lines.is_empty());
    }

    #[test]
    fn agent_end_emits_exactly_one_result_line_with_totals() {
        let mut c = ctx();
        let a = assistant_text("final answer", Usage::default());
        map_events(&AgentEvent::MessageEnd(Message::Assistant(a)), &mut c);
        map_events(
            &AgentEvent::Usage(Usage {
                input: 100,
                output: 40,
                cache_read: 5,
                cache_write: 2,
                cost: crate::ai::types::Cost {
                    total: 0.0123,
                    ..Default::default()
                },
                ..Usage::default()
            }),
            &mut c,
        );
        c.set_duration_ms(1500);

        let lines = map_events(
            &AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
            &mut c,
        );
        assert_eq!(lines.len(), 1, "AgentEnd ⇒ exactly one result line");
        let v: Value = serde_json::from_str(&to_ndjson(&lines[0])).unwrap();
        assert_eq!(v["type"], "result");
        assert_eq!(v["subtype"], "success");
        assert_eq!(v["is_error"], false);
        assert_eq!(v["result"], "final answer");
        assert_eq!(v["num_turns"], 1);
        assert_eq!(v["duration_ms"], 1500);
        assert_eq!(v["total_cost_usd"], 0.0123);
        assert_eq!(v["usage"]["input_tokens"], 100);
        assert_eq!(v["usage"]["output_tokens"], 40);
        // cache_write (2) surfaces as Anthropic's cache_creation.
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 2);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 5);
        // modelUsage mirrors the same totals under the model key, camelCase.
        assert_eq!(v["modelUsage"]["claude-sonnet-4-6"]["inputTokens"], 100);
        assert_eq!(v["modelUsage"]["claude-sonnet-4-6"]["outputTokens"], 40);
    }

    #[test]
    fn full_tool_use_turn_sequences_assistant_user_result() {
        // assistant(tool_use) → user(tool_result) → assistant(text) → result
        let mut c = ctx();

        let mut a1 = AssistantMessage::empty();
        a1.stop_reason = StopReason::ToolUse;
        a1.content.push(ContentBlock::ToolCall {
            id: "tu_1".into(),
            name: "read".into(),
            arguments: json!({ "path": "/a" }),
        });
        let l1 = map_events(&AgentEvent::MessageEnd(Message::Assistant(a1)), &mut c);

        let t = ToolResultMessage {
            tool_call_id: "tu_1".into(),
            tool_name: "read".into(),
            content: vec![ToolResultContent::text("ok")],
            is_error: false,
            timestamp: 0,
        };
        let l2 = map_events(&AgentEvent::MessageEnd(Message::ToolResult(t)), &mut c);

        let mut a2 = AssistantMessage::empty();
        a2.content.push(ContentBlock::Text {
            text: "done".into(),
        });
        let l3 = map_events(&AgentEvent::MessageEnd(Message::Assistant(a2)), &mut c);

        let l4 = map_events(
            &AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
            &mut c,
        );

        let line_of = |ls: Vec<ClaudeLine>| {
            serde_json::from_str::<Value>(&to_ndjson(&ls[0])).unwrap()["type"]
                .clone()
        };
        assert_eq!(line_of(l1), "assistant");
        assert_eq!(line_of(l2), "user");
        assert_eq!(line_of(l3), "assistant");
        assert_eq!(line_of(l4), "result");
        assert_eq!(c.num_turns, 2, "two assistant turns in a tool-use run");
    }

    #[test]
    fn fatal_error_surfaces_in_result_line_as_error() {
        let mut c = ctx();
        let a = assistant_text("partial", Usage::default());
        map_events(&AgentEvent::MessageEnd(Message::Assistant(a)), &mut c);
        map_events(&AgentEvent::Error("boom".into()), &mut c);
        let lines = map_events(
            &AgentEvent::AgentEnd {
                messages: Vec::new(),
            },
            &mut c,
        );
        let v: Value = serde_json::from_str(&to_ndjson(&lines[0])).unwrap();
        assert_eq!(v["type"], "result");
        assert_eq!(v["is_error"], true);
        assert_eq!(v["subtype"], "error");
        assert_eq!(v["result"], "boom", "error message becomes the result text");
    }

    #[test]
    fn to_ndjson_round_trips_through_serde() {
        let line = system_init(&ctx(), Vec::new());
        let s = to_ndjson(&line);
        assert!(!s.contains('\n'), "a line has no embedded newline");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "system");
    }
}
