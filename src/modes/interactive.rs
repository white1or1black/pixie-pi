//! Interactive REPL mode — `pi` (optionally with an initial prompt).

use std::io::Write;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use pixie_pi::ai::{self, ThinkingLevel};
use crate::modes::drive;
use crate::render::{blue, dim, green, magenta, red, yellow, EventRenderer};
use pixie_pi::session::AgentSession;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY_FILE: &str = "history.txt";

enum SlashResult {
    Exit,
    Continue,
}

/// Run the interactive REPL. Returns the process exit code.
pub async fn run_interactive(
    mut session: AgentSession,
    initial: Option<ai::Message>,
    show_thinking: bool,
) -> Result<i32> {
    let mut rl = DefaultEditor::new()?;
    let hist_path = pixie_pi::config::agent_dir().join(HISTORY_FILE);
    let _ = rl.load_history(&hist_path);

    print_banner(&session);

    if let Some(msg) = initial {
        run_one(&mut session, msg, show_thinking).await;
    }

    let prompt = format!("{} ", dim("❯"));
    loop {
        match rl.readline(&prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(trimmed);
                if trimmed.starts_with('/') {
                    match handle_slash(&mut session, trimmed).await {
                        SlashResult::Exit => break,
                        SlashResult::Continue => {}
                    }
                } else {
                    // Plain prompt — send it to the model as a new user turn.
                    let msg = ai::Message::User(ai::UserMessage::text(trimmed));
                    run_one(&mut session, msg, show_thinking).await;
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ignore Ctrl-C at the prompt.
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("{}", red(&format!("input error: {e}")));
                break;
            }
        }
    }

    let _ = rl.save_history(&hist_path);
    Ok(0)
}

fn print_banner(session: &AgentSession) {
    let tools = session.tool_names().join(", ");
    let usage_pct = (session.context_usage() * 100.0).round() as u64;
    println!(
        "{} v{}  —  {} {}  —  {}",
        magenta("pixie-pi"),
        VERSION,
        dim("model"),
        green(&session.model.id),
        dim(&format!("({}%)", usage_pct))
    );
    println!("{} {}", dim("cwd"), session.cwd.display());
    println!("{} {}", dim("tools"), tools);
    println!(
        "  {}  {}  {}  {}",
        dim("/help for commands"),
        dim("/model to switch"),
        dim("/compact to trim"),
        dim("/exit to quit")
    );
    println!();
}

async fn run_one(session: &mut AgentSession, msg: ai::Message, show_thinking: bool) {
    let mut renderer = EventRenderer::new(show_thinking);
    drive(session, vec![msg], |ev| {
        renderer.handle(ev);
        matches!(ev, pixie_pi::agent::context::AgentEvent::AgentEnd { .. })
    })
    .await;
    // Print a compact cost/context line after each turn.
    let cost = session.total_usage.cost.total;
    let pct = (session.context_usage() * 100.0).round() as u64;
    eprintln!(
        "{}",
        dim(&format!(
            "  ↳ {} out, {} in, ${:.4}, ctx {}%",
            session.total_usage.output,
            session.total_usage.input,
            cost,
            pct
        ))
    );
    let _ = std::io::stderr().flush();
}

async fn handle_slash(session: &mut AgentSession, input: &str) -> SlashResult {
    let mut parts = input[1..].split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    match cmd {
        "exit" | "quit" | "q" => SlashResult::Exit,
        "help" | "h" | "?" => {
            println!("{}", blue("Slash commands:"));
            for (c, d) in [
                ("/help", "show this help"),
                ("/exit", "quit the session"),
                ("/clear", "clear the conversation"),
                ("/model <id>", "switch model"),
                ("/thinking <level>", "off|minimal|low|medium|high|xhigh"),
                ("/compact", "summarize old messages to fit the context"),
                ("/tools", "list available tools"),
                ("/context", "show token usage"),
                ("/cost", "show cumulative cost"),
                ("/system", "show the system prompt"),
            ] {
                println!("  {}   {}", yellow(c), dim(d));
            }
            SlashResult::Continue
        }
        "clear" => {
            session.messages.clear();
            println!("{}", green("Conversation cleared."));
            SlashResult::Continue
        }
        "model" => {
            let pattern = rest.join(" ");
            if pattern.is_empty() {
                println!("{} {}", dim("current model:"), session.model.id);
                return SlashResult::Continue;
            }
            match ai::resolve_model(&ai::builtin_models(), &pattern) {
                Some(m) => {
                    println!("{} {} → {}", dim("model"), session.model.id, green(&m.id));
                    session.model = m;
                }
                None => {
                    let avail = ai::builtin_models()
                        .iter()
                        .map(|m| m.id.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("{}", red(&format!("Unknown model. Available: {avail}")));
                }
            }
            SlashResult::Continue
        }
        "thinking" => {
            let level = rest.first().copied().unwrap_or("");
            match ThinkingLevel::parse(level) {
                Some(t) => {
                    session.thinking = t;
                    println!("{} thinking={:?}", dim("set"), t);
                }
                None => {
                    println!(
                        "{}",
                        red("Usage: /thinking off|minimal|low|medium|high|xhigh")
                    );
                }
            }
            SlashResult::Continue
        }
        "compact" => {
            let dropped = session.compact().await;
            let _ = session.save();
            println!("{}", green(&format!("Compacted: dropped {dropped} messages.")));
            SlashResult::Continue
        }
        "tools" => {
            let tools = session.tool_names().join(", ");
            println!("{} {}", dim("tools"), tools);
            SlashResult::Continue
        }
        "context" | "ctx" => {
            let pct = (session.context_usage() * 100.0).round() as u64;
            println!(
                "{} ~{} tokens / {} ({}%)",
                dim("context"),
                session.estimated_tokens(),
                session.model.context_window,
                pct
            );
            SlashResult::Continue
        }
        "cost" => {
            let u = &session.total_usage;
            println!(
                "{} in={} out={} cache_read={} cache_write={} cost=${:.6}",
                dim("usage"),
                u.input,
                u.output,
                u.cache_read,
                u.cache_write,
                u.cost.total
            );
            SlashResult::Continue
        }
        "system" => {
            let s = &session.system_prompt;
            let preview: String = s.chars().take(600).collect();
            println!("{preview}");
            if s.chars().count() > 600 {
                println!("{}", dim("…(truncated)"));
            }
            SlashResult::Continue
        }
        "" => SlashResult::Continue,
        other => {
            println!("{}", red(&format!("Unknown command: /{other} (try /help)")));
            SlashResult::Continue
        }
    }
}
