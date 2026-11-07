//! `just-harness`: a terminal REPL around the agent loop.

use std::fs::DirBuilder;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use just_harness::agent::{
    Agent, Approval, ApprovalRequest, Approver, DEFAULT_MAX_ITERATIONS, Observer, TurnEnd,
};
use just_harness::api::{
    ApiError, Client, ClientConfig, ContentBlock, Message, MessageParam, Role, StopReason,
    Transport, Usage,
};
use just_harness::clearing::{ClearPolicy, RemovedBlock, clear_before, plan};
use just_harness::compaction::{
    CompactGate, CompactGuard, CompactPolicy, SHRINK_ATTEMPTS, TouchedFiles, compacted,
    last_resort_summary_request, put_back, shrunk_summary_request, summary, summary_request,
};
use just_harness::memory::{
    ContextFile, INDEX_MAX_BYTES, INDEX_MAX_LINES, Settings, context_update, load_context,
    memory_dir,
};
use just_harness::session::{
    FORMAT_VERSION, Prompt as SessionPrompt, SessionLog, SessionMeta, find_by_id, latest, load,
    new_id, now_secs, sessions_dir, sessions_root,
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
    /// Blocks taken out in this turn.
    removed: Vec<RemovedBlock>,
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

    fn on_block_removed(&mut self, removed: &RemovedBlock) {
        self.log.record_block_removed(removed);
        self.removed.push(removed.clone());
        println!("\n{}", one_line(&removed.text, 160));
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

/// The message telling the model that a session that ran in `from` now
/// runs in `cwd`, with the memory folder this run has, if any.
fn folder_note(from: &str, cwd: &Path, memory: Option<&Path>) -> MessageParam {
    let memory = match memory {
        Some(dir) => format!(
            "Your memory folder is {dir}; read and write memory there.",
            dir = dir.display()
        ),
        None => "There is no memory folder in this run.".into(),
    };
    MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(format!(
            "<working-folder>\nThis session now continues in the working folder {cwd}, not {from} \
             where it ran before. Paths are relative to {cwd}, and the file tools work there, \
             not in {from}. {memory}\n</working-folder>",
            cwd = cwd.display(),
        ))],
    }
}

/// Makes `agent` send `prompt`; its tool definitions replace the tool
/// set's only when they differ from `current`'s.
fn use_prompt<T: Transport>(agent: &mut Agent<T>, prompt: &SessionPrompt, current: &SessionPrompt) {
    agent.system = prompt.system.clone();
    agent.tool_definitions = (prompt.tools != current.tools).then(|| prompt.tools.clone());
}

/// The system prompt: the working folder, and how to keep memory in
/// `memory` when there is a memory folder.
fn system_prompt(cwd: &Path, memory: Option<&Path>) -> String {
    let mut prompt = format!(
        "You are a coding agent working in the folder {}. Use the tools to search, read and \
         edit files there and to run commands. Paths are relative to that folder.",
        cwd.display()
    );
    if let Some(memory) = memory {
        prompt += &format!(
            "\n\nYou have a memory folder that lasts across sessions: {memory}. Keep in it what \
             a later session would need and cannot find in the code, its history or the \
             instruction files: the user's role and preferences, corrections and approaches the \
             user confirmed (with why), ongoing work and decisions, and where outside \
             information lives. Keep one fact per file, named for its subject, and one line \
             pointing to each file in {memory}/MEMORY.md; the start of each session shows you \
             its first {lines} lines, up to {bytes} bytes. Update a file rather than writing a \
             second one, and delete what turns out wrong. Read and write these files with \
             read_file and edit_file, by their absolute paths.",
            memory = memory.display(),
            lines = INDEX_MAX_LINES,
            bytes = INDEX_MAX_BYTES,
        );
    }
    prompt
}

/// How a compaction ended.
enum Compacted {
    /// The history is now the summary.
    Done,
    /// The history is unchanged: the request failed or gave no summary.
    Kept,
    /// Ctrl-C; the line typed is not sent.
    Cancelled,
    /// The history holds nothing to summarize.
    Nothing,
}

struct Compaction {
    end: Compacted,
    /// The summary request's usage, with when it ended.
    usages: Vec<(Usage, Instant)>,
}

/// Sends [`shrunk_summary_request`]s until one gives a summary, then
/// [`last_resort_summary_request`]; `None` when `history` holds nothing to
/// summarize.
async fn send_shrunk<T: Transport>(
    agent: &Agent<T>,
    history: &[MessageParam],
    terminal: &mut Terminal<'_>,
) -> Option<Result<Message, ApiError>> {
    shrunk_summary_request(history, 0)?;
    for attempt in 0..SHRINK_ATTEMPTS {
        let Some(messages) = shrunk_summary_request(history, attempt) else {
            break;
        };
        if attempt == 1 {
            println!("[too long still; the oldest messages are left out of the summary]");
        }
        match agent.send(messages, terminal).await {
            Err(error)
                if !matches!(
                    error,
                    ApiError::Status {
                        status: 400 | 413,
                        ..
                    }
                ) =>
            {
                return Some(Err(error));
            }
            Ok(message) if summary(&message).is_some() => return Some(Ok(message)),
            _ => {}
        }
    }
    println!("[too long still; asking for a summary of nothing, so the conversation is lost]");
    Some(agent.send(last_resort_summary_request(), terminal).await)
}

/// Asks for a summary of `history` and, when one comes back, replaces the
/// history with it and records that in the session file. With `tokens`, the
/// size of the last request, it sends the history as it is; without, the
/// history was refused as too long, so it sends it shrunk.
async fn compact<T: Transport>(
    agent: &Agent<T>,
    history: &mut Vec<MessageParam>,
    log: &mut Log,
    tokens: Option<u64>,
) -> Compaction {
    match tokens {
        Some(tokens) => println!(
            "[compacting {} message(s), about {tokens} tokens, into a summary]",
            history.len()
        ),
        None => println!("[the API refused the request as too long; trying to compact]"),
    }
    let mut terminal = Terminal {
        log,
        removed: Vec::new(),
        usages: Vec::new(),
    };
    let result = tokio::select! {
        result = async {
            match tokens {
                Some(_) => Some(agent.send(summary_request(history), &mut terminal).await),
                None => send_shrunk(agent, history, &mut terminal).await,
            }
        } => Some(result),
        _ = tokio::signal::ctrl_c() => None,
    };
    let usages = std::mem::take(&mut terminal.usages);
    println!();
    let end = match result {
        None => {
            println!("[compaction cancelled]");
            Compacted::Cancelled
        }
        Some(None) => Compacted::Nothing,
        Some(Some(Err(error))) => {
            println!("[compaction failed: {error}; the conversation is kept whole]");
            Compacted::Kept
        }
        Some(Some(Ok(message))) => match summary(&message) {
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
    /// Not created yet; `prompt` is recorded right after `meta`.
    Pending {
        dir: PathBuf,
        meta: SessionMeta,
        prompt: SessionPrompt,
    },
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

    fn record_cwd(&mut self, cwd: &str) {
        self.write(|file| file.record_cwd(cwd));
    }

    fn record_prompt(&mut self, prompt: &SessionPrompt) {
        self.write(|file| file.record_prompt(prompt));
    }

    fn record_compact(&mut self, summary: &str) {
        self.write(|file| file.record_compact(summary));
    }

    fn record_context(&mut self, files: &[ContextFile]) {
        self.write(|file| file.record_context(files));
    }

    fn record_clear(&mut self, before: usize) {
        self.write(|file| file.record_clear(before));
    }

    fn record_block_removed(&mut self, removed: &RemovedBlock) {
        self.write(|file| file.record_block_removed(removed));
    }

    fn write(&mut self, op: impl FnOnce(&mut SessionLog) -> std::io::Result<()>) {
        if let Log::Pending { dir, meta, prompt } = self {
            *self = match SessionLog::create(dir, meta).and_then(|mut file| {
                file.record_prompt(prompt)?;
                Ok(file)
            }) {
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

/// Says which files were found and left out of the project context, and why.
fn report_skipped(skipped: &[(PathBuf, String)]) {
    for (path, why) in skipped {
        println!(
            "[not loaded {}: {}]",
            printable(&path.display().to_string()),
            printable(why)
        );
    }
}

/// What [`open_session`] opened.
struct OpenedSession {
    /// The session to write to.
    log: Log,
    /// The history to start from.
    history: Vec<MessageParam>,
    /// The prompt its requests carry: the one recorded in a resumed session,
    /// else `current`.
    prompt: SessionPrompt,
    /// The folder a resumed session last ran in, when that is not `cwd`.
    moved_from: Option<String>,
    /// The files a resumed session touched.
    touched: TouchedFiles,
    /// The files a resumed session's project context last carried, when recorded.
    context: Option<Vec<ContextFile>>,
}

/// Opens the session `start` names, or a new one.
fn open_session(
    start: &Start,
    root: &Path,
    dir: &Path,
    cwd: &Path,
    current: &SessionPrompt,
) -> Result<OpenedSession, String> {
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
            return Ok(OpenedSession {
                log: Log::Pending {
                    dir: dir.to_path_buf(),
                    meta,
                    prompt: current.clone(),
                },
                history: Vec::new(),
                prompt: current.clone(),
                moved_from: None,
                touched: TouchedFiles::default(),
                context: None,
            });
        }
        Start::Continue => latest(root, &cwd_text)
            .map_err(|e| format!("{}: {}", shown(root), printable(&e.to_string())))?
            .ok_or_else(|| {
                format!(
                    "no session to continue for this folder (looked under {})",
                    shown(root)
                )
            })?,
        Start::Resume(target) if target.contains('/') || target.ends_with(".jsonl") => {
            PathBuf::from(target)
        }
        Start::Resume(id) => find_by_id(root, dir, id).map_err(|e| printable(&e))?,
    };
    let fail = |e: std::io::Error| printable(&e.to_string());
    let loaded = load(&path).map_err(fail)?;
    if loaded.skipped > 0 {
        println!(
            "[{} unreadable line(s) in the session file were skipped]",
            loaded.skipped
        );
    }
    let moved_from = (loaded.cwd != cwd_text).then(|| loaded.cwd.clone());
    if let Some(from) = &moved_from {
        println!(
            "[this session ran in {}; tools now work in {}, and the model is told so]",
            printable(from),
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
    let prompt = match loaded.prompt {
        Some(recorded) => {
            if &recorded != current {
                println!(
                    "[this session keeps the system prompt and tools it started with; this build's take effect after the next compaction]"
                );
            }
            recorded
        }
        None => {
            println!(
                "[this session has no readable record of its system prompt; it resumes with this build's, and a request carrying earlier thinking may be refused]"
            );
            file.record_prompt(current).map_err(fail)?;
            current.clone()
        }
    };
    println!(
        "session {} resumed with {} message(s)",
        printable(&loaded.meta.id),
        loaded.history.len()
    );
    Ok(OpenedSession {
        log: Log::Open(file),
        history: loaded.history,
        prompt,
        moved_from,
        touched: loaded.touched,
        context: loaded.context,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = args()?;
    let config =
        ClientConfig::from_lookup(setting).map_err(|e| format!("{e} (environment or ./.env)"))?;
    let home = PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set")?);
    let settings = Settings::load(&home.join(".just-harness").join("settings.json"))?;
    let context = ToolContext::new(".")?;
    let cwd = context.root().to_path_buf();
    let dir = memory_dir(&home, &cwd);
    let (context, memory) = match DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .and_then(|()| context.clone().allow(&dir))
    {
        Ok(context) => (context, Some(dir)),
        Err(e) => {
            println!(
                "[no memory folder at {}: {}; running without memory]",
                printable(&dir.display().to_string()),
                printable(&e.to_string())
            );
            (context, None)
        }
    };
    let mut agent = Agent::new(Client::new(config)?, MODEL, builtin(), context);
    agent.max_iterations = args.max_iterations;
    agent.prompt_cache = args.cache;
    // This build's prompt; a resumed session may carry an earlier one.
    let current = SessionPrompt {
        system: vec![ContentBlock::text(system_prompt(&cwd, memory.as_deref()))],
        tools: agent.tools.definitions(),
    };

    println!("just-harness · {MODEL} · {}", cwd.display());
    println!(
        "Type a request. Ctrl-C cancels a running request; Ctrl-C or Ctrl-D at the prompt quits."
    );
    let OpenedSession {
        mut log,
        mut history,
        prompt,
        mut moved_from,
        mut touched,
        context: earlier_context,
    } = open_session(
        &args.start,
        &sessions_root(&home),
        &sessions_dir(&home, &cwd),
        &cwd,
        &current,
    )?;
    use_prompt(&mut agent, &prompt, &current);
    // Whether the next turn starts by carrying the instruction files and the
    // memory index: in a new session, and after a compaction.
    let mut needs_context = history.is_empty();
    // Whether the next turn also brings back recent files: after a compaction.
    let mut needs_files = false;
    // Whether the next turn compares the instruction files and the memory
    // index with their earlier copies: in a resumed session.
    let mut needs_update = !needs_context;
    let mut allowed = HashSet::new();
    let mut cached: Option<(Usage, Instant)> = None;
    let clearing = ClearPolicy::default();
    let compacting = CompactPolicy::default();
    let mut compact_guard = CompactGuard::default();
    // The prompt and reply of the last request of the last turn kept, in tokens.
    let mut last_prompt: Option<u64> = None;
    let mut lines = input_lines();
    // A line refused as too long, to send again after the compaction.
    let mut resend: Option<String> = None;
    loop {
        let retrying = resend.is_some();
        let line = match resend.take() {
            Some(line) => line,
            None => {
                print!("\n> ");
                std::io::stdout().flush()?;
                let line = tokio::select! {
                    line = lines.recv() => line,
                    _ = tokio::signal::ctrl_c() => None,
                };
                let Some(line) = line else {
                    break;
                };
                line
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match compact_guard.gate(&history, &compacting, last_prompt) {
            CompactGate::Attempt { tokens } => {
                let compaction = compact(&agent, &mut history, &mut log, Some(tokens)).await;
                report_usage(&compaction.usages, &mut cached, args.cache);
                cached = None;
                match compaction.end {
                    Compacted::Done => {
                        compact_guard.compacted();
                        last_prompt = None;
                        needs_context = true;
                        needs_files = true;
                        use_prompt(&mut agent, &current, &current);
                        log.record_prompt(&current);
                    }
                    Compacted::Kept | Compacted::Nothing => compact_guard.attempted(),
                    Compacted::Cancelled => {
                        println!("[your line was not sent]");
                        continue;
                    }
                }
            }
            CompactGate::JustStopped => println!(
                "[automatic compaction stopped: {} attempts in a row made no room, because a summary failed or the conversation was {} tokens or more right after one. \
                 Requests over 100,000 tokens cost more. \
                 It starts again once a turn ends below that size, or after quitting and resuming]",
                compacting.max_attempts, compacting.trigger_tokens
            ),
            CompactGate::Wait | CompactGate::Stopped => {}
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
        if needs_context {
            needs_context = false;
            let loaded = load_context(&home, &cwd, memory.as_deref(), settings.instruction_files);
            for path in &loaded.read {
                println!("[loaded {}]", printable(&path.display().to_string()));
            }
            report_skipped(&loaded.skipped);
            if let Some(message) = loaded.message {
                log.append(&message);
                history.push(message);
            }
            log.record_context(&loaded.files);
            if std::mem::take(&mut needs_files) {
                let back = put_back(&touched, &agent.context, &loaded.read);
                for path in &back.read {
                    println!("[put back {}]", printable(&path.display().to_string()));
                }
                for path in &back.referenced {
                    println!(
                        "[put back the path of {}, too large for its contents]",
                        printable(&path.display().to_string())
                    );
                }
                if let Some(message) = back.message {
                    log.append(&message);
                    history.push(message);
                }
            }
        }
        if let Some(from) = moved_from.take() {
            let note = folder_note(&from, &cwd, memory.as_deref());
            log.append(&note);
            history.push(note);
            log.record_cwd(&cwd.display().to_string());
        }
        if std::mem::take(&mut needs_update) {
            let loaded = load_context(&home, &cwd, memory.as_deref(), settings.instruction_files);
            let update =
                context_update(earlier_context.as_deref(), &loaded.files, memory.as_deref());
            for path in &update.changed {
                println!("[loaded {}]", printable(path));
            }
            for path in &update.gone {
                println!("[no longer present {}]", printable(path));
            }
            report_skipped(&loaded.skipped);
            if let Some(message) = update.message {
                log.append(&message);
                history.push(message);
            }
            if earlier_context.as_deref() != Some(loaded.files.as_slice()) {
                log.record_context(&loaded.files);
            }
        }
        let before = history.len();
        let message = MessageParam {
            role: Role::User,
            content: vec![ContentBlock::text(line.clone())],
        };
        log.append(&message);
        history.push(message);
        let mut terminal = Terminal {
            log: &mut log,
            removed: Vec::new(),
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
        let removed = std::mem::take(&mut terminal.removed);
        for block in removed.iter().filter_map(|r| r.old.as_ref()) {
            touched.refuse(block);
        }
        last_prompt = usages.last().map(|(usage, _)| usage.history_tokens());
        if let Some((usage, _)) = usages.first() {
            compact_guard.first_request(usage.prompt_tokens(), &compacting);
        }
        for message in &history[before..] {
            touched.observe(message, &cwd);
        }
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
        let too_long = matches!(&result, Err(error) if error.is_prompt_too_long());
        let ran_tools = history.len() > before + 1;
        match result {
            Ok(outcome) => match outcome.end {
                TurnEnd::StoppedByUser => {
                    println!(
                        "\n[stopped by you after {} requests]",
                        outcome.iterations + outcome.refused
                    )
                }
                TurnEnd::IterationLimit => {
                    println!(
                        "\n[stopped after {} requests: the iteration limit]",
                        outcome.iterations + outcome.refused
                    )
                }
                TurnEnd::Stopped(reason) => {
                    println!(
                        "\n[{reason:?} · {} requests]",
                        outcome.iterations + outcome.refused
                    )
                }
            },
            Err(error) => {
                last_prompt = None;
                history.truncate(before);
                log.drop_to(before);
                if !too_long {
                    println!("\n[error: {error}; this request was dropped from the conversation]");
                }
            }
        }
        report_usage(&usages, &mut cached, args.cache);
        if failed || !removed.is_empty() {
            cached = None;
        }
        if too_long {
            let fate = if ran_tools {
                "[its tools had already run, so do not send your line as it was; send what is left of it]"
            } else {
                "[your line was not sent]"
            };
            if retrying {
                println!("[still too long right after a compaction]");
                println!("{fate}");
                continue;
            }
            let compaction = compact(&agent, &mut history, &mut log, None).await;
            report_usage(&compaction.usages, &mut cached, args.cache);
            cached = None;
            match compaction.end {
                Compacted::Done => {
                    compact_guard.compacted();
                    needs_context = true;
                    needs_files = true;
                    use_prompt(&mut agent, &current, &current);
                    log.record_prompt(&current);
                    if ran_tools {
                        println!("{fate}");
                    } else {
                        resend = Some(line);
                    }
                }
                Compacted::Nothing => {
                    history.clear();
                    log.drop_to(0);
                    needs_context = true;
                    needs_files = true;
                    use_prompt(&mut agent, &current, &current);
                    log.record_prompt(&current);
                    println!(
                        "[too long, with nothing in the conversation to summarize; the instruction files are read again with your next line]"
                    );
                    println!("{fate}");
                }
                Compacted::Kept => {
                    compact_guard.attempted();
                    println!("{fate}");
                }
                Compacted::Cancelled => println!("{fate}"),
            }
        }
    }
    Ok(())
}
