//! Claude Code `stream-json` wire drivers — the binary side of `pixie_pi::compat`.
//!
//! Two entry points share one `MapCtx` and the same emit pattern:
//! - [`run_stream_json_oneshot`]: `--output-format stream-json` with a single
//!   text prompt → emit `system` init, run **one** turn, map its events to
//!   Claude NDJSON, exit.
//! - [`run_stream_json_persistent`]: `--input-format stream-json` → emit
//!   `system` init once, then loop reading `{"type":"user",...}` JSONL turns
//!   from stdin, driving each to a `result` line (multi-turn over one process,
//!   like Pixie's persistent `claude` session).
//!
//! Both reuse [`super::drive`] unchanged for Ctrl-C → cancellation, per-turn
//! compaction, usage accumulation, and `session.save()` — so the only thing
//! this module owns is wiring stdin/stdout around the pure mapper in
//! [`pixie_pi::compat`].
//!
//! Turn sequencing is strictly sequential (read the next stdin line only after
//! the prior turn's `result`), matching Pixie's `read_persistent_turn`. Bypass
//! mode never blocks mid-turn for permission, so no concurrency is needed.

use std::time::Instant;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};

use pixie_pi::agent::context::AgentEvent;
use pixie_pi::compat::{map_events, system_init, to_ndjson, MapCtx};
use pixie_pi::session::AgentSession;
use pixie_pi::{Message, UserMessage};

use crate::modes::{drive, RunOutcome};

/// Write one NDJSON line to stdout and flush. NDJSON must be line-buffered: the
/// consuming process (Pixie) reads one record at a time, so each line must be
/// visible the instant it is produced — a block-buffered pipe would deadlock it.
fn emit(line: &str) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{line}");
    let _ = handle.flush();
}

/// Drive a single turn, mapping every [`AgentEvent`] to Claude NDJSON on stdout.
/// Returns the run outcome (carrying `had_error` for the exit code). The turn
/// duration is stamped onto `ctx` from `start` the instant `AgentEnd` arrives,
/// *before* the `result` line is mapped, so the terminator reports real elapsed
/// time rather than zero.
async fn drive_turn(
    session: &mut AgentSession,
    ctx: &mut MapCtx,
    prompts: Vec<Message>,
    start: Instant,
) -> RunOutcome {
    drive(session, prompts, |ev: &AgentEvent| -> bool {
        let is_end = matches!(ev, AgentEvent::AgentEnd { .. });
        if is_end {
            ctx.set_duration_ms(start.elapsed().as_millis() as u64);
        }
        for line in map_events(ev, ctx) {
            emit(&to_ndjson(&line));
        }
        is_end // AgentEnd ends consumption (it is the stream's last event).
    })
    .await
}

/// Build the run context from the session. `model`/`cwd` come from the session;
/// `session_id` and `permission_mode` are argv-derived (see `main::session_id`).
fn make_ctx(session: &AgentSession, session_id: &str, permission_mode: &str) -> MapCtx {
    MapCtx::new(
        session_id,
        session.model.id.clone(),
        session.cwd.display().to_string(),
        permission_mode,
    )
}

/// `--output-format stream-json` (one shot): emit `system` init, run one turn,
/// emit its events as Claude NDJSON, and return the exit code.
pub async fn run_stream_json_oneshot(
    session: &mut AgentSession,
    prompt: Message,
    session_id: &str,
    permission_mode: &str,
) -> Result<i32> {
    let mut ctx = make_ctx(session, session_id, permission_mode);
    emit(&to_ndjson(&system_init(&ctx, session.tool_names())));

    let start = Instant::now();
    let outcome = drive_turn(session, &mut ctx, vec![prompt], start).await;
    Ok(if outcome.had_error { 1 } else { 0 })
}

/// `--input-format stream-json` (persistent): emit `system` init once, then loop
/// over stdin JSONL turns. An optional `initial_prompt` from argv seeds turn one
/// before the first stdin read. The process exits on stdin EOF (or a
/// `{"type":"close"}` / `{"type":"end"}` line).
pub async fn run_stream_json_persistent(
    session: &mut AgentSession,
    initial_prompt: Option<Message>,
    session_id: &str,
    permission_mode: &str,
) -> Result<i32> {
    let mut ctx = make_ctx(session, session_id, permission_mode);
    emit(&to_ndjson(&system_init(&ctx, session.tool_names())));

    let mut had_error = false;

    if let Some(prompt) = initial_prompt {
        let start = Instant::now();
        let outcome = drive_turn(session, &mut ctx, vec![prompt], start).await;
        if outcome.had_error {
            had_error = true;
        }
    }

    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // EOF → end the persistent process.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // ignore malformed lines; never error on argv/stdin
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("close") | Some("end") => break,
            _ => {}
        }
        let Some(text) = extract_user_text(&value) else {
            continue; // a non-user line (or empty text) — wait for the next turn
        };

        let start = Instant::now();
        let outcome = drive_turn(session, &mut ctx, vec![Message::User(UserMessage::text(text))], start).await;
        if outcome.had_error {
            had_error = true;
        }
    }

    Ok(if had_error { 1 } else { 0 })
}

/// Pull the prompt text out of a Claude `{"type":"user","message":{...}}` line.
/// Supports both the block form
/// (`content:[{type:"text",text:"..."}]`) and the shorthand string form
/// (`content:"..."`). Returns `None` when there is no text to send.
fn extract_user_text(value: &serde_json::Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return (!s.trim().is_empty()).then(|| s.to_string());
    }
    let blocks = content.as_array()?;
    let mut texts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                texts.push(t.to_string());
            }
        }
    }
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::extract_user_text;
    use serde_json::json;

    #[test]
    fn extracts_text_from_block_content() {
        let v = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "hello" }]
            }
        });
        assert_eq!(extract_user_text(&v).as_deref(), Some("hello"));
    }

    #[test]
    fn joins_multiple_text_blocks() {
        let v = json!({
            "type": "user",
            "message": { "content": [
                { "type": "text", "text": "a" },
                { "type": "image", "source": {} },
                { "type": "text", "text": "b" }
            ]}
        });
        assert_eq!(extract_user_text(&v).as_deref(), Some("a\nb"));
    }

    #[test]
    fn accepts_shorthand_string_content() {
        let v = json!({ "message": { "content": "just a string" } });
        assert_eq!(extract_user_text(&v).as_deref(), Some("just a string"));
    }

    #[test]
    fn ignores_lines_without_user_text() {
        assert_eq!(extract_user_text(&json!({"type": "system"})), None);
        assert_eq!(
            extract_user_text(&json!({"message": {"content": [{ "type": "image" }]}})),
            None
        );
        assert_eq!(extract_user_text(&json!({"message": {"content": "   "}})), None);
    }
}
