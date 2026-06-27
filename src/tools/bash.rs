//! `bash` tool — execute a shell command in the working directory.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::tool::{AgentTool, ToolResult};
use crate::tools::truncate::{format_size, truncate_tail, DEFAULT_MAX_BYTES};

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // pid < 0 means "send to process group -pid".
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    unsafe {
        kill(-(pid as i32), SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(pid: u32) {
    let _ = pid;
}

pub struct BashTool {
    pub cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct BashInput {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
}

fn shell_program() -> (String, Vec<&'static str>) {
    #[cfg(unix)]
    {
        // SHELL may be any path; use it with -c. Returned owned (not leaked):
        // the previous `Box::leak` freed nothing, so every bash call in a long
        // session accumulated another copy of `$SHELL` permanently.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        (shell, vec!["-c"])
    }
    #[cfg(not(unix))]
    {
        ("cmd.exe".to_string(), vec!["/C"])
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to the last 2000 lines or 50KB (whichever is hit first); when truncated the full output is saved to a temp file. Optionally provide a timeout in seconds."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, cancel: CancellationToken) -> anyhow::Result<ToolResult> {
        let input: BashInput = serde_json::from_value(args)?;
        let command = input
            .command
            .ok_or_else(|| anyhow::anyhow!("bash requires a 'command' parameter"))?;

        if !self.cwd.exists() {
            anyhow::bail!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                self.cwd.display()
            );
        }

        let (program, args_vec) = shell_program();
        let mut cmd = Command::new(program);
        cmd.args(&args_vec).arg(&command);
        cmd.current_dir(&self.cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        #[cfg(unix)]
        {
            // tokio's Command exposes `process_group` natively on Unix.
            cmd.process_group(0);
        }
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn()?;

        let pid = child.id();
        // Stream stdout + stderr into a shared buffer.
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let buf1 = buffer.clone();
        let buf2 = buffer.clone();
        let stdout_task = tokio::spawn(async move {
            if let Some(out) = stdout.as_mut() {
                let mut tmp = [0u8; 8192];
                loop {
                    match out.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf1.lock().await.extend_from_slice(&tmp[..n]),
                    }
                }
            }
        });
        let stderr_task = tokio::spawn(async move {
            if let Some(err) = stderr.as_mut() {
                let mut tmp = [0u8; 8192];
                loop {
                    match err.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf2.lock().await.extend_from_slice(&tmp[..n]),
                    }
                }
            }
        });

        let timeout_secs = input.timeout.filter(|&t| t > 0);
        let wait = child.wait();

        let exit_status = tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(pid) = pid { kill_process_group(pid); }
                let _ = child.wait().await;
                return Err(anyhow::anyhow!("Command aborted"));
            }
            _ = async {
                if let Some(secs) = timeout_secs {
                    let _ = tokio::time::sleep(Duration::from_secs(secs)).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                if let Some(pid) = pid { kill_process_group(pid); }
                let _ = child.wait().await;
                // Await the stdout/stderr readers BEFORE draining. After the
                // kill the writers' pipe ends are closed, but the readers may
                // still be copying the last bytes out of the OS pipe buffer
                // into `buffer`. Draining first (the old order) took whatever
                // was accumulated so far and let the readers' final reads land
                // in the now-emptied buffer — silently dropping the output's
                // tail. The normal-completion path below already
                // awaits-then-drains; match it here so a timed-out command
                // surfaces everything it produced before being killed.
                stdout_task.await.ok();
                stderr_task.await.ok();
                let output = drain(buffer).await;
                let (text, _details) = format_output(&output, true);
                return Err(anyhow::anyhow!(
                    "{text}\n\nCommand timed out after {secs} seconds",
                    secs = timeout_secs.unwrap_or(0)
                ));
            }
            status = wait => status,
        };

        stdout_task.await.ok();
        stderr_task.await.ok();
        let output = drain(buffer).await;

        let status = exit_status?;
        let code = status.code();
        if !matches!(code, Some(0) | None) {
            let (text, _) = format_output(&output, true);
            return Err(anyhow::anyhow!(
                "{text}\n\nCommand exited with code {}",
                code.unwrap_or(-1)
            ));
        }

        let (text, details) = format_output(&output, false);
        Ok(ToolResult {
            content: vec![crate::ai::types::ToolResultContent::Text { text }],
            details,
            terminate: false,
        })
    }
}

async fn drain(buffer: Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    std::mem::take(&mut *buffer.lock().await)
}

/// Apply tail-truncation and, when truncated, persist the full output to a
/// temp file. Returns `(text, details)`.
fn format_output(output: &[u8], _truncated_by_limit: bool) -> (String, Value) {
    let content = String::from_utf8_lossy(output).to_string();
    if content.trim().is_empty() {
        return ("(no output)".to_string(), Value::Null);
    }
    let trunc = truncate_tail(&content, None, None);
    if trunc.truncated {
        let temp_path = std::env::temp_dir().join(format!("pi-bash-{}.log", uuid_brief()));
        let _ = std::fs::write(&temp_path, output);
        let start = trunc.total_lines.saturating_sub(trunc.output_lines) + 1;
        let end = trunc.total_lines;
        let notice = match trunc.truncated_by.as_deref() {
            Some("lines") => format!(
                "\n\n[Showing lines {start}-{end} of {}. Full output: {}]",
                trunc.total_lines,
                temp_path.display()
            ),
            _ => format!(
                "\n\n[Showing lines {start}-{end} of {} ({} limit). Full output: {}]",
                trunc.total_lines,
                format_size(DEFAULT_MAX_BYTES),
                temp_path.display()
            ),
        };
        let details = json!({
            "truncated": true,
            "fullOutputPath": temp_path.to_string_lossy(),
        });
        (format!("{}{notice}", trunc.content), details)
    } else {
        (content, Value::Null)
    }
}

fn uuid_brief() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

// Tests invoke unix shell commands (`printf`, `sleep`) via `/bin/sh -c`; on
// Windows the tool shells out to `cmd.exe`, where these don't exist. Gate the
// whole module to unix so `cargo test` stays green on Windows CI.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::tool::AgentTool;
    use tokio_util::sync::CancellationToken;

    fn result_text(res: ToolResult) -> String {
        match res.content.into_iter().next() {
            Some(crate::ai::types::ToolResultContent::Text { text }) => text,
            _ => panic!("expected text tool result"),
        }
    }

    #[tokio::test]
    async fn runs_a_simple_command_and_captures_stdout() {
        // Basic happy path: the tool returns the command's stdout as a result.
        let cwd = std::env::temp_dir().to_path_buf();
        let tool = BashTool { cwd };
        let args = serde_json::json!({ "command": "printf 'pi-bash-ok\\n'" });
        let res = tool.execute(args, CancellationToken::new()).await.unwrap();
        assert!(result_text(res).contains("pi-bash-ok"));
    }

    #[tokio::test]
    async fn timeout_kills_and_surfaces_output_produced_before_the_kill() {
        // A command prints a marker immediately, then blocks past the timeout.
        // The timeout path must kill it AND surface the output it produced.
        //
        // The deeper correctness property this guards: on timeout we now await
        // the stdout/stderr readers *before* draining the accumulated buffer
        // (matching the normal-completion path). The previous drain-before-await
        // ordering could drop output that the readers had not yet copied out of
        // the OS pipe buffer when the kill landed. That race is timing-dependent
        // and not reliably reproducible in a single run, so this test asserts
        // the always-true contract — the marker is captured and the timeout is
        // reported as an error — while the ordering fix itself is verified by
        // inspection against the normal path directly below the `select!`.
        let cwd = std::env::temp_dir().to_path_buf();
        let tool = BashTool { cwd };
        let args = serde_json::json!({
            "command": "printf 'before-timeout-marker\\n'; sleep 30",
            "timeout": 1
        });
        let err = tool
            .execute(args, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("before-timeout-marker"),
            "output produced before the kill must be surfaced: {err}"
        );
        assert!(
            err.contains("Command timed out after 1 second"),
            "timeout must be reported: {err}"
        );
    }

    #[tokio::test]
    async fn cancellation_aborts_the_command() {
        let cwd = std::env::temp_dir().to_path_buf();
        let tool = BashTool { cwd };
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        // Cancel shortly after starting a long-running command.
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel2.cancel();
        });
        let args = serde_json::json!({ "command": "sleep 30" });
        let err = tool
            .execute(args, cancel)
            .await
            .unwrap_err()
            .to_string();
        canceller.await.ok();
        assert!(err.contains("aborted"), "cancel must abort: {err}");
    }
}
