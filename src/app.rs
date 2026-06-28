//! Application setup shared by both modes: model/credential resolution, tool
//! selection, system prompt, and session construction.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;

use pixie_pi::agent::tool::AgentTool;
use crate::cli::{AppMode, Args, InputFormat, OutputFormat, OutputMode};
use pixie_pi::prompt::build_system_prompt;
use pixie_pi::session::AgentSession;
use pixie_pi::tools;
use pixie_pi::ai::{self, resolve_model, Model, ThinkingLevel};

/// Decide which mode to run.
///
/// Machine-facing `stream-json` wire formats take precedence over the
/// text/interactive split — they are NDJSON protocols, never a terminal chat:
/// `--input-format stream-json` ⇒ persistent stdin multi-turn;
/// `--output-format stream-json` ⇒ one-shot Claude NDJSON. Otherwise this
/// mirrors pi's `resolveAppMode`: `--print` or non-TTY stdin ⇒ print mode, else
/// interactive.
pub fn resolve_app_mode(args: &Args, stdin_is_tty: bool) -> AppMode {
    if args.input_format() == InputFormat::StreamJson {
        return AppMode::StreamJsonPersistent;
    }
    if args.output_format() == OutputFormat::StreamJson {
        return AppMode::StreamJsonOneShot;
    }
    if args.print || !stdin_is_tty {
        let output = OutputMode::parse(&args.mode).unwrap_or(OutputMode::Text);
        AppMode::Print(output)
    } else {
        AppMode::Interactive
    }
}

/// Resolve credentials from CLI flags + environment.
pub fn resolve_credentials(args: &Args) -> (Option<String>, Option<String>) {
    let auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let api_key = args
        .api_key
        .clone()
        .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty()));
    (api_key, auth_token)
}

/// Build the tool set from CLI flags.
pub fn build_tools(cwd: &Path, args: &Args) -> Vec<Arc<dyn AgentTool>> {
    if args.no_tools || args.no_builtin_tools {
        return Vec::new();
    }
    let mut tools = if args.tools.is_empty() {
        tools::coding_tools(cwd.to_path_buf())
    } else {
        tools::select_tools(cwd.to_path_buf(), &args.tools)
    };
    if !args.exclude_tools.is_empty() {
        tools.retain(|t| !args.exclude_tools.iter().any(|n| n == t.name()));
    }
    tools
}

/// Resolve the model from the registry + flags.
pub fn resolve_selected_model(args: &Args) -> anyhow::Result<Model> {
    let registry = ai::builtin_models();
    let pattern = match (&args.provider, &args.model) {
        (Some(p), Some(m)) => format!("{p}/{m}"),
        (Some(p), None) => p.clone(),
        (None, Some(m)) => m.clone(),
        (None, None) => registry[0].id.clone(),
    };
    if let Some(m) = resolve_model(&registry, &pattern) {
        return Ok(m);
    }
    // If the user explicitly asked for a model we can't resolve, error.
    if args.model.is_some() || args.provider.is_some() {
        bail!(
            "Could not resolve model '{}'. Available: {}",
            pattern,
            registry.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    Ok(registry[0].clone())
}

/// Parse the requested thinking level (default Medium).
pub fn resolve_thinking(args: &Args) -> ThinkingLevel {
    args.thinking
        .as_deref()
        .and_then(ThinkingLevel::parse)
        .unwrap_or(ThinkingLevel::Medium)
}

/// Build a fully-configured [`AgentSession`].
pub fn build_session(args: &Args, cwd: &Path, messages: Vec<ai::Message>) -> anyhow::Result<AgentSession> {
    let model = resolve_selected_model(args)?;
    let thinking = resolve_thinking(args);
    let mut tools = build_tools(cwd, args);
    // Discover Claude Code–compatible skills and register the `skill` tool when
    // any exist (and tools aren't fully disabled / excluded). Project skills
    // (`.claude/skills`) shadow user skills (`~/.claude/skills`).
    let skills = std::sync::Arc::new(pixie_pi::skills::Skills::discover(cwd));
    let skills_enabled = !args.no_tools
        && !args.no_builtin_tools
        && !skills.skills.is_empty()
        && !args.exclude_tools.iter().any(|n| n == "skill");
    if skills_enabled {
        tools.push(std::sync::Arc::new(pixie_pi::tools::skill::SkillTool::new(
            cwd.to_path_buf(),
            skills.clone(),
        )));
    }
    let (api_key, auth_token) = resolve_credentials(args);

    if api_key.is_none() && auth_token.is_none() {
        bail!(
            "No Anthropic credentials found. Set the ANTHROPIC_API_KEY environment variable \
             (or ANTHROPIC_AUTH_TOKEN for a Bearer token), or pass --api-key <key>."
        );
    }

    let system_prompt = match &args.system_prompt {
        Some(s) => s.clone(),
        None => {
            let extra = if args.append_system_prompt.is_empty() {
                None
            } else {
                Some(args.append_system_prompt.join("\n\n"))
            };
            build_system_prompt(pixie_pi::prompt::PromptOptions {
                cwd,
                tool_names: &tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>(),
                extra: extra.as_deref(),
                skills: if skills_enabled { Some(skills.as_ref()) } else { None },
            })
        }
    };

    // Claude-style `--session-id <id>` / `--resume <id>` map to a per-project
    // session file `<project session dir>/<id>.jsonl` (loaded if it exists,
    // created on first save otherwise). This gives the persistent stream-json
    // driver cross-restart continuity like Pixie's continue/resume.
    let claude_session_id = args
        .session_id
        .clone()
        .or_else(|| args.resume_id().map(str::to_string));

    let session_file = if args.no_session {
        None
    } else if let Some(sess) = &args.session {
        Some(expand_session_path(sess, cwd))
    } else if let Some(id) = &claude_session_id {
        Some(pixie_pi::config::project_session_dir(cwd).join(format!("{id}.jsonl")))
    } else {
        Some(pixie_pi::config::session_file(cwd))
    };

    // Load an existing transcript for continue/resume.
    let want_resume = args.continue_session
        || args.resume_requested()
        || args.session.is_some()
        || claude_session_id.is_some();
    let loaded = match &session_file {
        Some(path) if want_resume => {
            if path.exists() {
                AgentSession::load(path).ok()
            } else if args.session.is_some() {
                // An explicit `--session <file>` must already exist.
                bail!("Session file not found: {}", path.display())
            } else {
                // `--continue`/`--resume`/`--session-id` for a not-yet-created
                // session: start fresh; the file is created on first save.
                None
            }
        }
        _ => None,
    };
    let messages = loaded.unwrap_or(messages);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;

    let mut session = AgentSession::new(
        cwd.to_path_buf(),
        system_prompt,
        model,
        thinking,
        tools,
        client,
    );
    session.messages = messages;
    session.api_key = api_key;
    session.auth_token = auth_token;
    session.session_file = session_file;
    session.max_tokens = args.max_tokens;
    session.cache_control = !args.no_cache;
    Ok(session)
}

fn expand_session_path(sess: &str, cwd: &Path) -> PathBuf {
    let expanded = pixie_pi::tools::util::expand_tilde(sess);
    if expanded.is_absolute() {
        expanded
    } else {
        // Treat as an id prefix under this project's session dir.
        pixie_pi::config::project_session_dir(cwd).join(format!("{sess}.jsonl"))
    }
}

/// Build the initial user message(s) from positional prompts + `@file`
/// contents. Returns `None` when there is nothing to send.
pub fn build_initial_messages(prompts: &[String], file_contents: &[String]) -> Option<ai::Message> {
    let mut parts: Vec<String> = Vec::new();
    let combined = prompts.join("\n\n");
    if !combined.trim().is_empty() {
        parts.push(combined);
    }
    for (i, content) in file_contents.iter().enumerate() {
        parts.push(format!("@file[{}]\n{}", i, content));
    }
    if parts.is_empty() {
        return None;
    }
    Some(ai::Message::User(ai::UserMessage::text(parts.join("\n\n"))))
}

/// Turn piped stdin into a prompt message. Empty / whitespace-only input yields
/// `None` (so the caller can emit its "no prompt" error). `content` is taken by
/// value to keep the pure decision in [`stdin_to_prompt`] unit-testable.
pub fn read_stdin_prompt() -> Option<ai::Message> {
    use std::io::Read;
    let mut content = String::new();
    if std::io::stdin().read_to_string(&mut content).is_err() {
        return None;
    }
    stdin_to_prompt(content)
}

/// Pure helper: map raw stdin bytes to an optional user message.
fn stdin_to_prompt(content: String) -> Option<ai::Message> {
    if content.trim().is_empty() {
        None
    } else {
        Some(ai::Message::User(ai::UserMessage::text(content)))
    }
}

#[cfg(test)]
mod tests {
    use super::stdin_to_prompt;
    use pixie_pi::ai::types::Message;

    #[test]
    fn stdin_to_prompt_ignores_blank_input() {
        assert!(stdin_to_prompt(String::new()).is_none());
        assert!(stdin_to_prompt("   \n\t ".into()).is_none());
    }

    #[test]
    fn stdin_to_prompt_wraps_nonempty_input_as_user_message() {
        let msg = stdin_to_prompt("explain this stack trace\n".into()).unwrap();
        match msg {
            Message::User(u) => assert_eq!(u.text_content(), "explain this stack trace\n"),
            _ => panic!("expected a user message"),
        }
    }

    #[test]
    fn build_initial_messages_combines_positional_and_files() {
        // Positional prompts come first, then @file contents, joined blank-line.
        let msg =
            super::build_initial_messages(&["hello".to_string()], &["FILE_BODY".to_string()])
                .unwrap();
        match msg {
            Message::User(u) => {
                let t = u.text_content();
                assert!(t.contains("hello"));
                assert!(t.contains("FILE_BODY"));
            }
            _ => panic!("expected a user message"),
        }
    }
}
