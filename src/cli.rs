//! CLI argument parsing (`cli/args.ts`). Mirrors the subset of pi flags that
//! the two supported modes (command-line / interactive) need.

use clap::Parser;

/// pixie-pi — an AI coding agent with read/write/edit/bash/grep/find/ls tools.
///
/// Run with no prompt to enter the interactive REPL, or pass a prompt (or
/// `-p`/`--print`) for a one-shot command-line run.
#[derive(Parser, Debug)]
#[command(
    name = "pixie-pi",
    version,
    about = "pixie-pi — AI coding agent (command-line and interactive modes)"
)]
pub struct Args {
    /// Prompt message(s). Omit to start the interactive REPL.
    #[arg(value_name = "MESSAGE")]
    pub messages: Vec<String>,

    /// Non-interactive: process the prompt and exit (command-line mode).
    #[arg(short = 'p', long = "print")]
    pub print: bool,

    /// Output mode for command-line runs: text (default) or json (NDJSON events).
    #[arg(long, value_name = "text|json", default_value = "text")]
    pub mode: String,

    /// Output format: `text` (default) or `stream-json` (Claude Code NDJSON).
    /// `stream-json` emits `system`/`assistant`/`user`/`result` lines so callers
    /// that spawn `claude` (e.g. Pixie) can drive pixie-pi as a drop-in.
    #[arg(long = "output-format", value_name = "text|stream-json", default_value = "text")]
    pub output_format: String,

    /// Input format: `text` (default) or `stream-json`. `stream-json` starts a
    /// persistent process that reads `{"type":"user",...}` JSONL turns from
    /// stdin and writes Claude `stream-json` to stdout (multi-turn over one
    /// process, like Pixie's persistent claude session).
    #[arg(long = "input-format", value_name = "text|stream-json", default_value = "text")]
    pub input_format: String,

    /// Model id or `provider/id` (optionally suffixed `:thinking`).
    #[arg(long)]
    pub model: Option<String>,

    /// Provider name (default: anthropic).
    #[arg(long)]
    pub provider: Option<String>,

    /// API key (defaults to ANTHROPIC_API_KEY).
    #[arg(long = "api-key")]
    pub api_key: Option<String>,

    /// Replace the system prompt entirely.
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    /// Append text to the system prompt (repeatable).
    #[arg(long = "append-system-prompt", value_name = "TEXT")]
    pub append_system_prompt: Vec<String>,

    /// Thinking level: off, minimal, low, medium, high, xhigh.
    #[arg(long)]
    pub thinking: Option<String>,

    /// Continue the most recent session for this project.
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a previous session. Bare `-r`/`--resume` resumes the most recent
    /// session in this project; `--resume <id>` resumes a specific one
    /// (Claude-compatible). `None` ⇒ not requested.
    #[arg(short = 'r', long = "resume", num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,

    /// Use a specific session file or id prefix.
    #[arg(long)]
    pub session: Option<String>,

    /// Claude-compatible session id. Maps to
    /// `<project session dir>/<id>.jsonl` (loaded if present, created
    /// otherwise). Used by `--input-format stream-json` for cross-restart
    /// continuity.
    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,

    /// Permission mode (Claude-compatible, e.g. `bypassPermissions`). All modes
    /// currently resolve to allow-all (bypass); interactive permission
    /// brokering is a documented future extension, so unknown modes are
    /// accepted and treated as bypass rather than rejected.
    #[arg(long = "permission-mode", value_name = "MODE")]
    pub permission_mode: Option<String>,

    /// Skip all permission prompts (Claude-compatible). Equivalent to
    /// `--permission-mode bypassPermissions`.
    #[arg(long = "dangerously-skip-permissions")]
    pub dangerously_skip_permissions: bool,

    /// Don't persist the session (ephemeral).
    #[arg(long = "no-session")]
    pub no_session: bool,

    /// Comma-separated allowlist of tool names to enable.
    #[arg(short = 't', long = "tools", value_delimiter = ',')]
    pub tools: Vec<String>,

    /// Comma-separated denylist of tool names to disable.
    #[arg(long = "exclude-tools", value_delimiter = ',')]
    pub exclude_tools: Vec<String>,

    /// Disable all tools.
    #[arg(short = 'n', long = "no-tools")]
    pub no_tools: bool,

    /// Disable built-in tools.
    #[arg(long = "no-builtin-tools")]
    pub no_builtin_tools: bool,

    /// Disable Anthropic prompt caching.
    #[arg(long = "no-cache")]
    pub no_cache: bool,

    /// Override the model's maximum output tokens.
    #[arg(long = "max-tokens")]
    pub max_tokens: Option<usize>,

    /// Show model thinking output while streaming. Under `--output-format
    /// stream-json` this flag is accepted (Claude requires it there); thinking
    /// is then carried in the NDJSON stream regardless.
    #[arg(long)]
    pub verbose: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
}

impl OutputMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Interactive,
    Print(OutputMode),
    /// `--output-format stream-json`: one shot — emit `system`, run a single
    /// turn, map its events to Claude NDJSON, exit.
    StreamJsonOneShot,
    /// `--input-format stream-json`: persistent — read JSONL turns from stdin
    /// in a loop, emitting Claude NDJSON for each until EOF.
    StreamJsonPersistent,
}

/// `--output-format` value. Unknown strings degrade to `Text` so Pixie's argv
/// never errors on an unrecognized format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    StreamJson,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "stream-json" => Self::StreamJson,
            _ => Self::Text,
        }
    }
}

/// `--input-format` value. Unknown strings degrade to `Text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputFormat {
    #[default]
    Text,
    StreamJson,
}

impl InputFormat {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "stream-json" => Self::StreamJson,
            _ => Self::Text,
        }
    }
}

impl Args {
    /// The requested output format (lenient: unknown ⇒ `Text`).
    pub fn output_format(&self) -> OutputFormat {
        OutputFormat::parse(&self.output_format)
    }

    /// The requested input format (lenient: unknown ⇒ `Text`).
    pub fn input_format(&self) -> InputFormat {
        InputFormat::parse(&self.input_format)
    }

    /// Whether `--resume` was requested at all (with or without an id).
    pub fn resume_requested(&self) -> bool {
        self.resume.is_some()
    }

    /// The id passed to `--resume <id>`, if any (bare `--resume` ⇒ `None`).
    pub fn resume_id(&self) -> Option<&str> {
        self.resume
            .as_deref()
            .filter(|s| !s.is_empty())
    }

    /// Effective permission-mode label for the `system.init` line. Defaults to
    /// `bypassPermissions`; `--dangerously-skip-permissions` forces it.
    pub fn permission_mode_label(&self) -> String {
        if self.dangerously_skip_permissions {
            return "bypassPermissions".to_string();
        }
        self.permission_mode
            .clone()
            .unwrap_or_else(|| "bypassPermissions".to_string())
    }
}
