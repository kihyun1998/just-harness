//! `just-harness`: a terminal REPL around the agent loop.

use std::io::Write;

use std::collections::HashSet;

use just_harness::agent::{
    Agent, Approval, ApprovalRequest, Approver, DEFAULT_MAX_ITERATIONS, Observer, TurnEnd,
};
use just_harness::api::{Client, ClientConfig, ContentBlock, MessageParam, Role};
use just_harness::tools::{ToolContext, ToolOutput, builtin};
use serde_json::Value;

const MODEL: &str = "claude-haiku-5-5";
const USAGE: &str = "usage: just-harness [--max-iterations N]";

/// Prints the reply as it streams and one line per tool call and result.
struct Terminal;

impl Observer for Terminal {
    fn on_event(&mut self, event: &Value) {
        if event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta" {
            print!(
                "{}",
                printable(event["delta"]["text"].as_str().unwrap_or(""))
            );
            let _ = std::io::stdout().flush();
        }
    }

    fn on_tool_call(&mut self, name: &str, input: &Value) {
        println!("\n[{name}] {}", one_line(&input.to_string(), 160));
    }

    fn on_tool_result(&mut self, _name: &str, output: &ToolOutput) {
        let mark = if output.is_error { "error" } else { "ok" };
        println!("  -> {mark}: {}", one_line(&output.content, 160));
    }
}

/// `text` with the characters of [`hidden`] shown as `�`.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if hidden(c) { '\u{FFFD}' } else { c })
        .collect()
}

/// Control characters other than newline and tab, and invisible format
/// characters that change how text reads: bidi controls, zero-width
/// characters, the soft hyphen and the byte-order mark.
fn hidden(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

fn one_line(text: &str, max: usize) -> String {
    let flat = printable(text).replace('\n', " ⏎ ");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &flat[..i]),
        None => flat,
    }
}

/// Asks at the terminal before a tool that changes something runs.
struct Prompt<'a> {
    lines: &'a mut tokio::sync::mpsc::UnboundedReceiver<String>,
    /// Calls answered `a`: the tool and its command or file.
    session: &'a mut HashSet<(String, String)>,
}

/// How many preview lines are shown before the rest is counted.
const PREVIEW_LINES: usize = 40;

impl Approver for Prompt<'_> {
    async fn approve(&mut self, request: &ApprovalRequest<'_>) -> Approval {
        let mut dropped = 0;
        while self.lines.try_recv().is_ok() {
            dropped += 1;
        }
        if dropped > 0 {
            println!("  [{dropped} line(s) typed ahead were dropped]");
        }
        let key = (request.name.to_string(), request.key.clone());
        if self.session.contains(&key) {
            println!("  (allowed for this session)");
            return Approval::Allow;
        }
        let lines: Vec<&str> = request.preview.lines().collect();
        for line in lines.iter().take(PREVIEW_LINES) {
            println!("  {}", printable(line));
        }
        if lines.len() > PREVIEW_LINES {
            println!("  ... {} more lines", lines.len() - PREVIEW_LINES);
        }
        loop {
            print!("  allow? [y]es / [n]o [reason] / [a]lways this session / [q]uit turn: ");
            let _ = std::io::stdout().flush();
            let Some(answer) = self.lines.recv().await else {
                return Approval::Stop;
            };
            let answer = answer.trim();
            let (word, rest) = answer
                .split_once(char::is_whitespace)
                .unwrap_or((answer, ""));
            let reason = Some(rest.trim().to_string()).filter(|r| !r.is_empty());
            match (word, &reason) {
                ("y" | "yes", None) => return Approval::Allow,
                ("a", None) => {
                    self.session.insert(key);
                    return Approval::Allow;
                }
                ("q", None) => return Approval::Stop,
                ("n" | "no", _) => return Approval::Deny(reason),
                ("y" | "yes" | "a" | "q", Some(_)) => {
                    println!("  only n takes a reason; answer y, a or q on its own")
                }
                _ => {}
            }
        }
    }
}

/// Lines typed on stdin, read on a thread of their own.
fn input_lines() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// `key` from the environment, else from `./.env` (`KEY=value` lines).
fn setting(key: &str) -> Option<String> {
    if let Some(value) = std::env::var(key).ok().filter(|v| !v.is_empty()) {
        return Some(value);
    }
    let file = std::fs::read_to_string(".env").ok()?;
    file.lines()
        .filter_map(|line| line.trim().split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

fn max_iterations() -> Result<u32, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => Ok(DEFAULT_MAX_ITERATIONS),
        [flag, n] if flag == "--max-iterations" => n
            .parse()
            .ok()
            .filter(|&n: &u32| n > 0)
            .ok_or_else(|| format!("--max-iterations needs a positive number\n{USAGE}")),
        _ => Err(USAGE.into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let max_iterations = max_iterations()?;
    let config =
        ClientConfig::from_lookup(setting).map_err(|e| format!("{e} (environment or ./.env)"))?;
    let context = ToolContext::new(".")?;
    let mut agent = Agent::new(Client::new(config)?, MODEL, builtin(), context);
    agent.max_iterations = max_iterations;
    agent.system = vec![ContentBlock::text(format!(
        "You are a coding agent working in the folder {}. Use the tools to search, read and edit \
         files there and to run commands. Paths are relative to that folder.",
        agent.context.root().display()
    ))];

    println!(
        "just-harness · {MODEL} · {}",
        agent.context.root().display()
    );
    println!(
        "Type a request. Ctrl-C cancels a running request; Ctrl-C or Ctrl-D at the prompt quits."
    );
    let mut history: Vec<MessageParam> = Vec::new();
    let mut session = HashSet::new();
    let mut lines = input_lines();
    loop {
        print!("\n> ");
        std::io::stdout().flush()?;
        let line = tokio::select! {
            line = lines.recv() => line,
            _ = tokio::signal::ctrl_c() => None,
        };
        let Some(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let before = history.len();
        history.push(MessageParam {
            role: Role::User,
            content: vec![ContentBlock::text(line)],
        });
        let mut terminal = Terminal;
        let mut approver = Prompt {
            lines: &mut lines,
            session: &mut session,
        };
        let result = tokio::select! {
            result = agent.run_turn(&mut history, &mut terminal, &mut approver) => Some(result),
            _ = tokio::signal::ctrl_c() => None,
        };
        let Some(result) = result else {
            history.truncate(before);
            println!("\n[cancelled; this request was dropped from the conversation]");
            continue;
        };
        match result {
            Ok(outcome) => {
                let usage = &outcome.last_message.usage;
                match outcome.end {
                    TurnEnd::StoppedByUser => {
                        println!("\n[stopped by you after {} requests]", outcome.iterations)
                    }
                    TurnEnd::IterationLimit => {
                        println!(
                            "\n[stopped after {} requests: the iteration limit]",
                            outcome.iterations
                        )
                    }
                    TurnEnd::Stopped(reason) => println!(
                        "\n[{reason:?} · {} requests · last: {} in / {} out tokens]",
                        outcome.iterations, usage.input_tokens, usage.output_tokens
                    ),
                }
            }
            Err(error) => {
                history.truncate(before);
                println!("\n[error: {error}; this request was dropped from the conversation]");
            }
        }
    }
    Ok(())
}
