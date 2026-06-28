//! Run modes (`packages/coding-agent/modes`): `print` (command-line, one-shot),
//! `interactive` (REPL), and `stream_json` (Claude Code `stream-json` wire,
//! one-shot or persistent stdin multi-turn).

pub mod interactive;
pub mod print;
pub mod stream_json;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use pixie_pi::agent::context::AgentEvent;
use pixie_pi::session::AgentSession;

/// Outcome of draining a run's event stream.
pub struct RunOutcome {
    pub final_messages: Option<Vec<pixie_pi::ai::Message>>,
    pub last_usage: Option<pixie_pi::ai::Usage>,
    pub had_error: bool,
}

/// Drive a run to completion. `on_event` is called for every event (returning
/// `true` from it stops consumption). This is the shared core used by both
/// modes; it wires up Ctrl-C → cancellation and finalizes the session.
pub async fn drive<F>(session: &mut AgentSession, prompts: Vec<pixie_pi::ai::Message>, mut on_event: F) -> RunOutcome
where
    F: FnMut(&AgentEvent) -> bool,
{
    let cancel = CancellationToken::new();
    let cc = cancel.clone();
    let ctrl_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cc.cancel();
        }
    });

    // Auto-compact when the transcript is near the context limit, so long
    // sessions don't silently overflow. Summarizes the oldest messages and
    // keeps the recent tail (falls back to a plain drop if the model is
    // unavailable). Runs before the turn so the upcoming request fits.
    if session.should_compact() {
        let dropped = session.compact().await;
        if dropped > 0 {
            eprintln!(
                "{}",
                crate::render::dim(&format!(
                    "  ↳ context near limit: compacted {dropped} message(s)"
                ))
            );
        }
    }

    let mut stream = session.run(prompts, cancel);
    let mut outcome = RunOutcome {
        final_messages: None,
        last_usage: None,
        had_error: false,
    };

    while let Some(ev) = stream.next().await {
        match &ev {
            // Each assistant turn carries only that turn's tokens (the Anthropic
            // API reports per-request usage, not a running total), so we must
            // add every turn here. The previous code stashed only `last_usage`
            // and added it once at the end — so any run with more than one turn
            // (the normal agentic case: tool calls then a final answer) silently
            // dropped every turn's usage except the last, undercounting cost and
            // context usage.
            AgentEvent::Usage(u) => record_turn_usage(session, &mut outcome.last_usage, u),
            AgentEvent::AgentEnd { messages } => outcome.final_messages = Some(messages.clone()),
            AgentEvent::Error(_) => outcome.had_error = true,
            // A provider failure surfaces as a finished assistant turn with an
            // error/aborted stop reason (not an AgentEvent::Error), so detect it
            // here to get a non-zero exit code.
            AgentEvent::TurnEnd { message, .. } => {
                use pixie_pi::ai::types::StopReason;
                if matches!(
                    message.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) || message.error_message.is_some()
                {
                    outcome.had_error = true;
                }
            }
            _ => {}
        }
        let stop = on_event(&ev);
        if stop {
            break;
        }
    }

    ctrl_task.abort();
    if let Some(msgs) = outcome.final_messages.take() {
        session.messages = msgs;
    }
    let _ = session.save();
    outcome
}

/// Fold a turn's usage into the session's running total and remember it as the
/// most recent turn. Called once per `AgentEvent::Usage` so multi-turn runs sum
/// every turn rather than only the last.
fn record_turn_usage(session: &mut AgentSession, last: &mut Option<pixie_pi::ai::Usage>, u: &pixie_pi::ai::Usage) {
    session.add_usage(u);
    *last = Some(u.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixie_pi::ai::types::Usage;

    fn test_session() -> AgentSession {
        let model = pixie_pi::ai::builtin_models()[0].clone();
        AgentSession::new(
            std::path::PathBuf::from("."),
            "sys".into(),
            model,
            pixie_pi::ai::ThinkingLevel::Off,
            vec![],
            reqwest::Client::new(),
        )
    }

    #[test]
    fn record_turn_usage_sums_every_turn_not_just_the_last() {
        // Each assistant turn reports only its own tokens, so the running total
        // must accumulate across turns. Before the fix `drive` kept only the
        // latest turn's usage and added it once at the end, so a 3-turn run
        // reported turn 3's tokens and dropped turns 1 and 2.
        let mut session = test_session();
        let mut last = None;
        let turn = |input, output| Usage {
            input,
            output,
            ..Usage::default()
        };
        record_turn_usage(&mut session, &mut last, &turn(100, 50));
        record_turn_usage(&mut session, &mut last, &turn(200, 10));
        record_turn_usage(&mut session, &mut last, &turn(0, 5));

        assert_eq!(session.total_usage.input, 300, "input must sum all turns");
        assert_eq!(session.total_usage.output, 65, "output must sum all turns");
        assert_eq!(
            last.as_ref().unwrap().output,
            5,
            "`last` still remembers the most recent turn"
        );
    }
}
