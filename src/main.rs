//! `just-harness`: a terminal REPL around the agent loop.

use std::io::Write;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use just_harness::agent::{
    Agent, Approval, ApprovalRequest, Approver, DEFAULT_MAX_ITERATIONS, Observer, TurnEnd,
};
use just_harness::api::{
    Client, ClientConfig, ContentBlock, MessageParam, Role, StopReason, Transport, Usage,
};
use just_harness::clearing::{ClearPolicy, clear_before, plan};
use just_harness::compaction::{
    CompactPolicy, compacted, should_compact, summary, summary_request,
};
use just_harness::session::{
    FORMAT_VERSION, SessionLog, SessionMeta, latest, load, new_id, now_secs, sessions_dir,
};
use just_harness::tools::{ToolContext, ToolOutput, builtin};
use serde_json::Value;

const MODEL: &str = "claude-haiku-5-5";
const USAGE: &str =
    "usage: just-harness [--max-iterations N] [--no-cache] [--continue | --resume <id|path>]";

/// Prints the reply as it streams and one line per tool call and result, and
/// appends each message the loop adds to the session log.
struct Terminal<'a> {
    log: &'a mut Log,
    /// Each request's usage in this turn, with when it ended.
    usages: Vec<(Usage, Instant)>,
}

impl Observer for Terminal<'_> {
    fn on_message(&mut self, message: &MessageParam) {
        self.log.append(message);
    }

    fn on_usage(&mut self, usage: &Usage) {
        self.usages.push((usage.clone(), Instant::now()));
    }

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

/// How long a cache entry lives after the request that wrote or read it.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Prints a turn's token usage, and the requests that read less from the
/// cache than the request before them left there. `cached` is the last
/// request whose cache entry can still be read.
fn report_usage(usages: &[(Usage, Instant)], cached: &mut Option<(Usage, Instant)>, caching: bool) {
    let mut total = Usage::default();
    let mut misses = 0;
    let mut missed_tokens = 0;
    for (usage, ended) in usages {
        total.add(usage);
        if let Some((previous, at)) = cached.as_ref()
            && ended.duration_since(*at) < CACHE_TTL
        {
            let shortfall = usage.cache_shortfall(previous);
            if shortfall > 0 {
                misses += 1;
                missed_tokens += shortfall;
            }
        }
        *cached = Some((usage.clone(), *ended));
    }
    if usages.is_empty() {
        return;
    }
    println!(
        "[tokens: {} uncached + {} from cache + {} written to cache in · {} out]",
        total.input_tokens,
        total.cache_read_input_tokens.unwrap_or(0),
        total.cache_creation_input_tokens.unwrap_or(0),
        total.output_tokens
    );
    if caching && misses > 0 {
        println!(
            "[{misses} of {} request(s) read {missed_tokens} fewer tokens from the cache than the request before left there: the prompt prefix changed]",
            usages.len()
        );
    }
}

/// How a compaction ended.
enum Compacted {
    /// The history is now the summary.
    Done,
    /// The history is unchanged: the request failed or gave no summary.
    Kept,
    /// Ctrl-C; the line typed is not sent.
    Cancelled,
}

struct Compaction {
    end: Compacted,
    /// The summary request's usage, with when it ended.
    usages: Vec<(Usage, Instant)>,
}

/// Asks for a summary of `history` and, when one comes back, replaces the
/// history with it and records that in the session file.
async fn compact<T: Transport>(
    agent: &Agent<T>,
    history: &mut Vec<MessageParam>,
    log: &mut Log,
    tokens: u64,
) -> Compaction {
    println!(
        "[compacting {} message(s), about {tokens} tokens, into a summary]",
        history.len()
    );
    let mut terminal = Terminal {
        log,
        usages: Vec::new(),
    };
    let result = tokio::select! {
        result = agent.send(summary_request(history), &mut terminal) => Some(result),
        _ = tokio::signal::ctrl_c() => None,
    };
    let usages = std::mem::take(&mut terminal.usages);
    println!();
    let end = match result {
        None => {
            println!("[compaction cancelled; your line was not sent]");
            Compacted::Cancelled
        }
        Some(Err(error)) => {
            println!("[compaction failed: {error}; the conversation is kept whole]");
            Compacted::Kept
        }
        Some(Ok(message)) => match summary(&message) {
            Some(text) => {
                if message.stop_reason == Some(StopReason::MaxTokens) {
                    println!("[the summary was cut at max_tokens]");
                }
                *history = compacted(&text);
                log.record_compact(&text);
                println!("[compacted into a summary of {} bytes]", text.len());
                Compacted::Done
            }
            None => {
                println!(
                    "[compaction gave no summary (stop reason {:?}); the conversation is kept whole]",
                    message.stop_reason
                );
                Compacted::Kept
            }
        },
    };
    Compaction { end, usages }
}

/// The session file: created with the first message, and given up after
/// the first failed write so the file never has gaps.
enum Log {
    Pending { dir: PathBuf, meta: SessionMeta },
    Open(SessionLog),
    Failed,
}

impl Log {
    fn append(&mut self, message: &MessageParam) {
        self.write(|file| file.append(message));
    }

    fn drop_to(&mut self, keep: usize) {
        self.write(|file| file.drop_to(keep));
    }

    fn record_compact(&mut self, summary: &str) {
        self.write(|file| file.record_compact(summary));
    }

    fn record_clear(&mut self, before: usize) {
        self.write(|file| file.record_clear(before));
    }

    fn write(&mut self, op: impl FnOnce(&mut SessionLog) -> std::io::Result<()>) {
        if let Log::Pending { dir, meta } = self {
            *self = match SessionLog::create(dir, meta) {
                Ok(file) => Log::Open(file),
                Err(e) => {
                    println!("\n[session not saved: {}]", printable(&e.to_string()));
                    Log::Failed
                }
            };
        }
        if let Log::Open(file) = self
            && let Err(e) = op(file)
        {
            println!(
                "\n[session file {}: {}; nothing more is written to it]",
                printable(&file.path().display().to_string()),
                printable(&e.to_string())
            );
            *self = Log::Failed;
        }
    }
}

enum Start {
    New,
    Continue,
    Resume(String),
}

struct Args {
    max_iterations: u32,
    start: Start,
    cache: bool,
}

fn args() -> Result<Args, String> {
    let mut parsed = Args {
        max_iterations: DEFAULT_MAX_ITERATIONS,
        start: Start::New,
        cache: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--max-iterations" => {
                parsed.max_iterations = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|&n: &u32| n > 0)
                    .ok_or_else(|| format!("--max-iterations needs a positive number\n{USAGE}"))?;
            }
            "--continue" if matches!(parsed.start, Start::New) => parsed.start = Start::Continue,
            "--no-cache" => parsed.cache = false,
            "--resume" if matches!(parsed.start, Start::New) => {
                let target = args
                    .next()
                    .ok_or_else(|| format!("--resume needs an id or a path\n{USAGE}"))?;
                parsed.start = Start::Resume(target);
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

/// The session to write to and the history to start from.
fn open_session(start: &Start, dir: &Path, cwd: &Path) -> Result<(Log, Vec<MessageParam>), String> {
    let cwd_text = cwd.display().to_string();
    let shown = |path: &Path| printable(&path.display().to_string());
    let path = match start {
        Start::New => {
            let meta = SessionMeta {
                version: FORMAT_VERSION,
                id: new_id(),
                created: now_secs(),
                cwd: cwd_text,
                model: MODEL.into(),
            };
            println!(
                "session {} (saved to {} from the first message)",
                meta.id,
                shown(dir)
            );
            return Ok((
                Log::Pending {
                    dir: dir.to_path_buf(),
                    meta,
                },
                Vec::new(),
            ));
        }
        Start::Continue => latest(dir, &cwd_text)
            .map_err(|e| format!("{}: {}", shown(dir), printable(&e.to_string())))?
            .ok_or_else(|| {
                format!(
                    "no session to continue for this folder (looked in {})",
                    shown(dir)
                )
            })?,
        Start::Resume(target) if target.contains('/') || target.ends_with(".jsonl") => {
            PathBuf::from(target)
        }
        Start::Resume(id) => dir.join(format!("{id}.jsonl")),
    };
    let fail = |e: std::io::Error| printable(&e.to_string());
    let loaded = load(&path).map_err(fail)?;
    if loaded.skipped > 0 {
        println!(
            "[{} unreadable line(s) in the session file were skipped]",
            loaded.skipped
        );
    }
    if loaded.meta.cwd != cwd_text {
        println!(
            "[this session ran in {}; tools now work in {}]",
            printable(&loaded.meta.cwd),
            printable(&cwd_text)
        );
    }
    let mut file = SessionLog::open(&path).map_err(fail)?;
    if let Some(at) = loaded.cut_at {
        println!(
            "[a tool call at message {} has no recorded result; the history is cut there]",
            at + 1
        );
        file.drop_to(at).map_err(fail)?;
    }
    if let Some(repair) = &loaded.repair {
        println!("[the session ended during a tool call; it is answered as having no result]");
        file.append(repair).map_err(fail)?;
    }
    println!(
        "session {} resumed with {} message(s)",
        printable(&loaded.meta.id),
        loaded.history.len()
    );
    Ok((Log::Open(file), loaded.history))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = args()?;
    let config =
        ClientConfig::from_lookup(setting).map_err(|e| format!("{e} (environment or ./.env)"))?;
    let context = ToolContext::new(".")?;
    let mut agent = Agent::new(Client::new(config)?, MODEL, builtin(), context);
    agent.max_iterations = args.max_iterations;
    agent.prompt_cache = args.cache;
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
    let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
    let cwd = agent.context.root().to_path_buf();
    let (mut log, mut history) =
        open_session(&args.start, &sessions_dir(Path::new(&home), &cwd), &cwd)?;
    let mut allowed = HashSet::new();
    let mut cached: Option<(Usage, Instant)> = None;
    let clearing = ClearPolicy::default();
    let compacting = CompactPolicy::default();
    // The prompt and reply of the last request of the last turn kept, in tokens.
    let mut last_prompt: Option<u64> = None;
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
        if let Some(tokens) = last_prompt
            && should_compact(&history, &compacting, tokens)
        {
            let compaction = compact(&agent, &mut history, &mut log, tokens).await;
            report_usage(&compaction.usages, &mut cached, args.cache);
            cached = None;
            match compaction.end {
                Compacted::Done => last_prompt = None,
                Compacted::Kept => {}
                Compacted::Cancelled => continue,
            }
        }
        if let Some(boundary) = last_prompt.and_then(|tokens| plan(&history, &clearing, tokens)) {
            let cleared = clear_before(&mut history, boundary);
            log.record_clear(boundary);
            println!(
                "[cleared {} old tool result(s), {} bytes, and {} thinking block(s) after them]",
                cleared.results, cleared.bytes, cleared.thinking
            );
            cached = None;
        }
        let before = history.len();
        let message = MessageParam {
            role: Role::User,
            content: vec![ContentBlock::text(line)],
        };
        log.append(&message);
        history.push(message);
        let mut terminal = Terminal {
            log: &mut log,
            usages: Vec::new(),
        };
        let mut approver = Prompt {
            lines: &mut lines,
            session: &mut allowed,
        };
        let result = tokio::select! {
            result = agent.run_turn(&mut history, &mut terminal, &mut approver) => Some(result),
            _ = tokio::signal::ctrl_c() => None,
        };
        let usages = std::mem::take(&mut terminal.usages);
        last_prompt = usages.last().map(|(usage, _)| usage.history_tokens());
        let Some(result) = result else {
            last_prompt = None;
            history.truncate(before);
            log.drop_to(before);
            println!("\n[cancelled; this request was dropped from the conversation]");
            report_usage(&usages, &mut cached, args.cache);
            cached = None;
            continue;
        };
        let failed = result.is_err();
        match result {
            Ok(outcome) => match outcome.end {
                TurnEnd::StoppedByUser => {
                    println!("\n[stopped by you after {} requests]", outcome.iterations)
                }
                TurnEnd::IterationLimit => {
                    println!(
                        "\n[stopped after {} requests: the iteration limit]",
                        outcome.iterations
                    )
                }
                TurnEnd::Stopped(reason) => {
                    println!("\n[{reason:?} · {} requests]", outcome.iterations)
                }
            },
            Err(error) => {
                last_prompt = None;
                history.truncate(before);
                log.drop_to(before);
                println!("\n[error: {error}; this request was dropped from the conversation]");
            }
        }
        report_usage(&usages, &mut cached, args.cache);
        if failed {
            cached = None;
        }
    }
    Ok(())
}
