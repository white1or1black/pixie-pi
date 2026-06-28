//! pixie-pi — an AI coding agent.
//!
//! Two usage forms:
//! - **command-line**: `pi -p "prompt"` (or `pi "prompt"` piped / non-TTY) —
//!   one-shot, streams the answer, exits.
//! - **interactive**: `pi` (with a TTY) — a conversational REPL with slash
//!   commands, streaming responses, and tool-call display.
//!
//! This is a thin binary: the reusable core lives in the `pixie_pi` library
//! crate. Here we only own CLI parsing (`cli`), session/mode dispatch (`app`,
//! `modes`), and terminal rendering (`render`).

// The binary's own `render` module exposes a full ANSI palette; not every
// helper is exercised on every code path, so silence the lone unused-palette
// warning rather than carve up a coherent set of color functions. (The library
// crate deliberately does NOT carry this allow.)
#![allow(dead_code)]

mod app;
mod cli;
mod modes;
mod render;

use std::io::IsTerminal;

use anyhow::Result;
use clap::Parser;

use crate::cli::{AppMode, Args};
use crate::modes::interactive::run_interactive;
use crate::modes::print::run_print;
use crate::modes::stream_json::{run_stream_json_oneshot, run_stream_json_persistent};

#[tokio::main]
async fn main() -> Result<()> {
    // Structured logging (env-controlled; warns only by default).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let cwd = std::env::current_dir()?;

    // Separate @file references from the rest before clap parsing.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let (files, rest): (Vec<String>, Vec<String>) = raw
        .into_iter()
        .partition(|a| a.starts_with('@') && !a.starts_with("@@") && a.len() > 1);

    // parse_from treats the first element as the binary name, so prepend one.
    let mut clap_args: Vec<String> = vec!["pi".to_string()];
    clap_args.extend(rest);
    let args = Args::parse_from(clap_args);

    // Inline @file contents into the initial message.
    let mut file_contents: Vec<String> = Vec::new();
    for f in &files {
        let path = &f[1..];
        match std::fs::read_to_string(path) {
            Ok(content) => file_contents.push(content),
            Err(e) => {
                eprintln!("warning: could not read @file {path}: {e}");
            }
        }
    }

    let stdin_is_tty = std::io::stdin().is_terminal();
    let app_mode = app::resolve_app_mode(&args, stdin_is_tty);
    let initial_message = app::build_initial_messages(&args.messages, &file_contents);

    match app_mode {
        AppMode::Print(output) => {
            // No positional prompt and stdin is piped (not a TTY): treat the
            // piped bytes as the prompt, e.g. `echo "explain this" | pi`.
            let initial_message = match initial_message {
                Some(m) => Some(m),
                None if !stdin_is_tty => app::read_stdin_prompt(),
                None => None,
            };
            if initial_message.is_none() {
                eprintln!(
                    "error: no prompt provided. Pass a message, pipe input, or run `pi` with no args for interactive mode."
                );
                std::process::exit(1);
            }
            let mut session = app::build_session(&args, &cwd, Vec::new())?;
            let code = run_print(&mut session, initial_message, output, args.verbose).await?;
            std::process::exit(code);
        }
        AppMode::Interactive => {
            let session = app::build_session(&args, &cwd, Vec::new())?;
            let code = run_interactive(session, initial_message, args.verbose).await?;
            std::process::exit(code);
        }
        AppMode::StreamJsonOneShot => {
            // `--output-format stream-json`: one shot. Prompt resolution mirrors
            // print (positional/@file, else piped stdin as a text prompt).
            let prompt = match initial_message {
                Some(m) => Some(m),
                None if !stdin_is_tty => app::read_stdin_prompt(),
                None => None,
            };
            let Some(prompt) = prompt else {
                eprintln!(
                    "error: no prompt provided. Pass a message or pipe input \
                     (stream-json output needs a prompt)."
                );
                std::process::exit(1);
            };
            let mut session = app::build_session(&args, &cwd, Vec::new())?;
            let sid = session_id(&args);
            let code =
                run_stream_json_oneshot(&mut session, prompt, &sid, &args.permission_mode_label())
                    .await?;
            std::process::exit(code);
        }
        AppMode::StreamJsonPersistent => {
            // `--input-format stream-json`: persistent multi-turn. stdin is the
            // JSONL turn stream, so only an argv seed (`-p`/positional) may start
            // turn one — never read stdin as a text prompt here.
            let mut session = app::build_session(&args, &cwd, Vec::new())?;
            let sid = session_id(&args);
            let code = run_stream_json_persistent(
                &mut session,
                initial_message,
                &sid,
                &args.permission_mode_label(),
            )
            .await?;
            std::process::exit(code);
        }
    }
}

/// Stable id for the `system`/`result` NDJSON lines: the explicit `--session-id`,
/// else `--resume <id>`, else a freshly generated uuid so every run still has a
/// stable identifier even with no session selected.
fn session_id(args: &Args) -> String {
    args.session_id
        .clone()
        .or_else(|| args.resume_id().map(str::to_string))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}
