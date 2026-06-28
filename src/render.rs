//! Terminal rendering: ANSI color helpers + a streaming event renderer that
//! prints assistant text, tool calls, and errors as the loop runs.

use serde_json::Value;

use pixie_pi::agent::context::AgentEvent;
use pixie_pi::ai::stream::AssistantMessageEvent;

/// Whether color output is enabled (respects `NO_COLOR` and non-TTY).
pub fn colors_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    atty_stdout()
}

#[cfg(unix)]
fn atty_stdout() -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(1) != 0 }
}

#[cfg(not(unix))]
fn atty_stdout() -> bool {
    // Conservative default on non-Unix.
    false
}

fn wrap(code: &str, s: &str) -> String {
    if colors_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    wrap("2", s)
}
pub fn bold(s: &str) -> String {
    wrap("1", s)
}
pub fn green(s: &str) -> String {
    wrap("32", s)
}
pub fn red(s: &str) -> String {
    wrap("31", s)
}
pub fn yellow(s: &str) -> String {
    wrap("33", s)
}
pub fn blue(s: &str) -> String {
    // ANSI 34 = blue. (36 is cyan — the function name is the contract, so emit
    // the color it promises rather than the look-alike it had drifted to.)
    wrap("34", s)
}
pub fn magenta(s: &str) -> String {
    wrap("35", s)
}

/// A one-line summary of a tool invocation, derived from its arguments.
pub fn tool_call_summary(name: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let path = || {
        let p = s("path");
        if !p.is_empty() {
            p
        } else {
            s("file_path")
        }
    };
    let summary = match name {
        "read" => path().to_string(),
        "write" => path().to_string(),
        "edit" => {
            let p = path();
            let n = args.get("edits").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(1);
            format!("{p} ({n} edit{})", if n == 1 { "" } else { "s" })
        }
        "bash" => s("command").to_string(),
        "grep" => {
            let pattern = s("pattern");
            let path = s("path");
            if path.is_empty() {
                format!("/{pattern}/")
            } else {
                format!("/{pattern}/ in {path}")
            }
        }
        "find" => {
            let pattern = s("pattern");
            let path = s("path");
            if path.is_empty() {
                pattern.to_string()
            } else {
                format!("{pattern} in {path}")
            }
        }
        "ls" => {
            let path = s("path");
            if path.is_empty() { ".".to_string() } else { path.to_string() }
        }
        _ => {
            // Generic: show first string-valued arg.
            args.as_object()
                .and_then(|o| {
                    o.values().find_map(|v| v.as_str().map(|s| s.to_string()))
                })
                .unwrap_or_default()
        }
    };
    if summary.is_empty() {
        String::new()
    } else {
        format!(" {summary}")
    }
}

/// Renders agent events to stdout/stderr as they arrive.
pub struct EventRenderer {
    show_thinking: bool,
    text_open: bool,
}

impl EventRenderer {
    pub fn new(show_thinking: bool) -> Self {
        Self {
            show_thinking,
            text_open: false,
        }
    }

    /// Handle one event. Returns `true` when the run has ended.
    pub fn handle(&mut self, ev: &AgentEvent) -> bool {
        match ev {
            AgentEvent::MessageUpdate { event } => self.handle_stream_event(event),
            AgentEvent::MessageStart(pixie_pi::ai::types::Message::Assistant(_)) => {
                self.text_open = true;
            }
            AgentEvent::MessageEnd(pixie_pi::ai::types::Message::Assistant(_)) => {
                if self.text_open {
                    println!();
                    self.text_open = false;
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                if self.text_open {
                    println!();
                    self.text_open = false;
                }
                let summary = tool_call_summary(tool_name, args);
                eprintln!("{} {}", blue(&format!("⏺ {tool_name}")), dim(&summary));
            }
            AgentEvent::ToolExecutionEnd { is_error, .. } => {
                if *is_error {
                    eprintln!("{}", red("  ⎿  (error)"));
                }
            }
            AgentEvent::TurnEnd { message, .. } => {
                // Provider/stream failures are embedded in the assistant message
                // (they don't surface as AgentEvent::Error), so surface them here.
                if let Some(err) = &message.error_message {
                    if self.text_open {
                        println!();
                        self.text_open = false;
                    }
                    eprintln!("{}", red(&format!("error: {err}")));
                }
            }
            AgentEvent::Error(msg) => {
                eprintln!("{}", red(&format!("error: {msg}")));
            }
            AgentEvent::AgentEnd { .. } => return true,
            _ => {}
        }
        false
    }

    fn handle_stream_event(&mut self, event: &AssistantMessageEvent) {
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                print!("{delta}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } if self.show_thinking => {
                eprint!("{}", dim(delta));
                use std::io::Write;
                let _ = std::io::stderr().flush();
            }
            _ => {}
        }
    }
}
