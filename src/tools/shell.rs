use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{Tool, ToolContext, ToolFuture, ToolOutput, str_arg, u64_arg};
use crate::api;

pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 120;
pub const MAX_SHELL_TIMEOUT_SECS: u64 = 600;
/// Each of stdout and stderr is cut to this many lines and bytes.
pub const OUTPUT_LIMIT_LINES: usize = 2000;
pub const OUTPUT_LIMIT_BYTES: usize = 50_000;

/// Runs a command with `sh -c` in the working folder, in its own process
/// group, which is killed whole at the timeout.
pub struct Shell;

impl Tool for Shell {
    fn definition(&self) -> api::Tool {
        api::Tool {
            name: "shell".into(),
            description: format!(
                "Run a command with `sh -c` in the working folder and return its exit code, \
                 stdout and stderr. A non-zero exit code is reported, not treated as a failure. \
                 Each stream keeps its last {OUTPUT_LIMIT_LINES} lines or {OUTPUT_LIMIT_BYTES} bytes. \
                 The command is done when its process exits; a background job (`&`) keeps \
                 running and its later output is not returned. \
                 The command is killed after `timeout_secs` (default {DEFAULT_SHELL_TIMEOUT_SECS}, \
                 at most {MAX_SHELL_TIMEOUT_SECS}). There is no terminal and no stdin."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command line" },
                    "timeout_secs": { "type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_SECS }
                },
                "required": ["command"]
            }),
            cache_control: None,
        }
    }

    fn call<'a>(&'a self, input: &'a Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move { run(input, ctx).await.unwrap_or_else(ToolOutput::error) })
    }
}

async fn run(input: &Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
    let command = str_arg(input, "command")?;
    let timeout_secs = u64_arg(input, "timeout_secs")?
        .unwrap_or(DEFAULT_SHELL_TIMEOUT_SECS)
        .clamp(1, MAX_SHELL_TIMEOUT_SECS);
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(ctx.root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start `sh`: {e}"))?;
    let pid = child.id();
    let mut guard = KillGroupOnDrop(pid);
    let stdout = Arc::new(Mutex::new(Tail::default()));
    let stderr = Arc::new(Mutex::new(Tail::default()));
    let readers = [
        tokio::spawn(capture(child.stdout.take(), stdout.clone())),
        tokio::spawn(capture(child.stderr.take(), stderr.clone())),
    ];

    let waited = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
    guard.0 = None;
    if waited.is_err()
        && let Some(pid) = pid
    {
        kill_group(pid);
    }
    for reader in readers {
        let abort = reader.abort_handle();
        if tokio::time::timeout(OUTPUT_DRAIN, reader).await.is_err() {
            abort.abort();
        }
    }

    let (mut text, is_error) = match waited {
        Err(_) => (
            format!("command timed out after {timeout_secs} s and was killed"),
            true,
        ),
        Ok(status) => {
            let status = status.map_err(|e| format!("command failed to run: {e}"))?;
            let exit = match status.code() {
                Some(code) => code.to_string(),
                None => "none (killed by a signal)".into(),
            };
            (format!("exit code: {exit}"), false)
        }
    };
    for (name, tail) in [("stdout", stdout), ("stderr", stderr)] {
        if let Some(shown) = tail.lock().unwrap().render() {
            text.push_str(&format!("\n--- {name} ---\n{shown}"));
        }
    }
    Ok(ToolOutput {
        content: text,
        is_error,
    })
}

/// How long output is still read after the command exits or is killed.
const OUTPUT_DRAIN: Duration = Duration::from_millis(500);

/// The last `OUTPUT_LIMIT_BYTES` of a stream, and its full size.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    total_bytes: usize,
    newlines: usize,
    ends_with_newline: bool,
}

/// Reads `pipe` to its end into `tail`.
async fn capture(pipe: Option<impl AsyncRead + Unpin>, tail: Arc<Mutex<Tail>>) {
    let Some(mut pipe) = pipe else { return };
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            return;
        }
        tail.lock().unwrap().push(&buf[..n]);
    }
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.total_bytes += chunk.len();
        self.newlines += chunk.iter().filter(|&&b| b == b'\n').count();
        self.ends_with_newline = chunk.last() == Some(&b'\n');
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > 2 * OUTPUT_LIMIT_BYTES {
            let excess = self.bytes.len() - OUTPUT_LIMIT_BYTES;
            self.bytes.drain(..excess);
        }
    }

    /// The text to show, cut to its last lines within the limits with a note,
    /// or `None` when the stream was empty.
    fn render(&self) -> Option<String> {
        if self.total_bytes == 0 {
            return None;
        }
        let total_lines = self.newlines + usize::from(!self.ends_with_newline);
        let start = self.bytes.len().saturating_sub(OUTPUT_LIMIT_BYTES);
        let mut kept = &self.bytes[start..];
        let cut = start > 0 || self.bytes.len() < self.total_bytes;
        if cut
            && let Some(newline) = kept.iter().position(|&b| b == b'\n')
            && newline + 1 < kept.len()
        {
            kept = &kept[newline + 1..];
        }
        let text = String::from_utf8_lossy(kept);
        let lines: Vec<&str> = text.trim_end_matches('\n').lines().collect();
        if !cut && lines.len() <= OUTPUT_LIMIT_LINES {
            return Some(lines.join("\n"));
        }
        let shown = &lines[lines.len().saturating_sub(OUTPUT_LIMIT_LINES)..];
        Some(format!(
            "[output truncated: {total_lines} lines, {} bytes; showing the last {} lines]\n{}",
            self.total_bytes,
            shown.len(),
            shown.join("\n")
        ))
    }
}

/// Kills the process group when dropped while armed: a cancelled tool call
/// takes its command with it.
struct KillGroupOnDrop(Option<u32>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

/// Sends SIGKILL to the process group `pid` leads.
fn kill_group(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
