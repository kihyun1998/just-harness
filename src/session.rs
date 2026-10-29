//! A conversation kept on disk as JSONL, and read back to resume it.
//!
//! The first line is `{"type":"session", ...SessionMeta}`. Each later line is
//! `{"type":"message","message":<MessageParam>}` or
//! `{"type":"drop","keep":<n>}`, which cuts the history back to its first `n`
//! messages, or `{"type":"clear","before":<n>}`, which clears the tool results
//! before message `n` as [`clear_before`] does, or
//! `{"type":"compact","summary":<text>}`, which replaces the history with
//! [`compacted`]`(summary)`, or `{"type":"prompt",...Prompt}`, the system
//! prompt and tool definitions the requests carry from there on.

use std::fs::{DirBuilder, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{BlockKind, ContentBlock, MessageParam, Role};
use crate::clearing::clear_before;
use crate::compaction::compacted;

/// The format version written to new files; files with a higher one are refused.
pub const FORMAT_VERSION: u32 = 4;

/// The first line of a session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    pub id: String,
    /// Seconds since the Unix epoch.
    pub created: u64,
    /// The working folder the session ran in.
    pub cwd: String,
    pub model: String,
}

/// The system prompt and tool definitions a session's requests carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub system: Vec<ContentBlock>,
    pub tools: Vec<crate::api::Tool>,
}

/// An open session file that messages are appended to.
pub struct SessionLog {
    path: PathBuf,
    file: File,
}

impl SessionLog {
    /// Creates `<dir>/<id>.jsonl`, and `dir` if needed, and writes `meta` as
    /// its first line. Folders it creates are mode 0700 and the file 0600. An
    /// existing file is not overwritten.
    pub fn create(dir: &Path, meta: &SessionMeta) -> std::io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let path = dir.join(format!("{}.jsonl", meta.id));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let mut log = Self { path, file };
        let mut line = serde_json::to_value(meta)?;
        line["type"] = json!("session");
        log.write_line(&line)?;
        Ok(log)
    }

    /// Opens an existing session file to append to it. A file whose last
    /// line has no line end gets one first.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;
        let mut last = [0u8; 1];
        let cut = file.seek(SeekFrom::End(-1)).is_ok()
            && file.read_exact(&mut last).is_ok()
            && last[0] != b'\n';
        if cut {
            file.write_all(b"\n")?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one message.
    pub fn append(&mut self, message: &MessageParam) -> std::io::Result<()> {
        self.write_line(&json!({ "type": "message", "message": message }))
    }

    /// Records that the history was cut back to its first `keep` messages.
    pub fn drop_to(&mut self, keep: usize) -> std::io::Result<()> {
        self.write_line(&json!({ "type": "drop", "keep": keep }))
    }

    /// Records that the history's tool results before message `before`
    /// were cleared ([`clear_before`]).
    pub fn record_clear(&mut self, before: usize) -> std::io::Result<()> {
        self.write_line(&json!({ "type": "clear", "before": before }))
    }

    /// Records that the history was compacted into `summary` ([`compacted`]).
    pub fn record_compact(&mut self, summary: &str) -> std::io::Result<()> {
        self.write_line(&json!({ "type": "compact", "summary": summary }))
    }

    /// Records the prompt the session's requests carry from here on.
    pub fn record_prompt(&mut self, prompt: &Prompt) -> std::io::Result<()> {
        let mut line = serde_json::to_value(prompt)?;
        line["type"] = json!("prompt");
        self.write_line(&line)
    }

    /// Writes one line and syncs it to the disk.
    fn write_line(&mut self, line: &Value) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(line)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.sync_data()
    }
}

/// A session file read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub meta: SessionMeta,
    /// The history to continue from, `repair` included.
    pub history: Vec<MessageParam>,
    /// Lines that did not parse and were left out.
    pub skipped: usize,
    /// Where the history was cut because a tool call inside it has no result
    /// in the message after it; to record as a drop when the session continues.
    pub cut_at: Option<usize>,
    /// Results added for the tool calls of a history that ends on them, to
    /// append to the log when the session continues.
    pub repair: Option<MessageParam>,
    /// The last prompt recorded; `None` for a file that records none.
    pub prompt: Option<Prompt>,
}

/// Reads a session file. The first line must be the session's meta, of this
/// format version or older; a later line that does not parse is skipped and
/// counted.
pub fn load(path: &Path) -> std::io::Result<Loaded> {
    let mut lines = BufReader::new(File::open(path)?).lines();
    let meta = read_meta(path, lines.next().transpose()?)?;

    let mut history: Vec<MessageParam> = Vec::new();
    let mut prompt = None;
    let mut skipped = 0;
    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            skipped += 1;
            continue;
        };
        match record["type"].as_str() {
            Some("message") => match serde_json::from_value(record["message"].clone()) {
                Ok(message) => history.push(message),
                Err(_) => skipped += 1,
            },
            Some("drop") => match record["keep"].as_u64() {
                Some(keep) => history.truncate(keep as usize),
                None => skipped += 1,
            },
            Some("prompt") => match serde_json::from_value(record.clone()) {
                Ok(recorded) => prompt = Some(recorded),
                Err(_) => skipped += 1,
            },
            Some("compact") => match record["summary"].as_str() {
                Some(summary) => history = compacted(summary),
                None => skipped += 1,
            },
            Some("clear") => match record["before"].as_u64() {
                Some(before) => {
                    clear_before(&mut history, before as usize);
                }
                None => skipped += 1,
            },
            _ => skipped += 1,
        }
    }
    let cut_at = first_unanswered(&history);
    if let Some(at) = cut_at {
        history.truncate(at);
    }
    let repair = results_for_trailing_calls(&history);
    if let Some(repair) = &repair {
        history.push(repair.clone());
    }
    Ok(Loaded {
        meta,
        history,
        skipped,
        cut_at,
        repair,
        prompt,
    })
}

fn read_meta(path: &Path, first: Option<String>) -> std::io::Result<SessionMeta> {
    let first = first.ok_or_else(|| invalid(path, "empty file"))?;
    let first: Value = serde_json::from_str(&first).map_err(|e| invalid(path, &e.to_string()))?;
    if first["type"] != "session" {
        return Err(invalid(path, "the first line is not session meta"));
    }
    let meta: SessionMeta =
        serde_json::from_value(first).map_err(|e| invalid(path, &e.to_string()))?;
    if meta.version > FORMAT_VERSION {
        return Err(invalid(
            path,
            &format!(
                "written in format {}, newer than this build's {FORMAT_VERSION}",
                meta.version
            ),
        ));
    }
    Ok(meta)
}

/// The index of the first assistant message, before the last, whose tool
/// calls are not all answered in the message right after it.
fn first_unanswered(history: &[MessageParam]) -> Option<usize> {
    history
        .windows(2)
        .enumerate()
        .filter(|(_, pair)| pair[0].role == Role::Assistant)
        .find(|(_, pair)| {
            let answered: Vec<&str> = pair[1]
                .content
                .iter()
                .filter(|b| b.block_type() == "tool_result")
                .filter_map(|b| b.0.get("tool_use_id").and_then(Value::as_str))
                .collect();
            pair[0].content.iter().any(|b| match b.kind() {
                BlockKind::ToolUse { id, .. } => {
                    pair[1].role != Role::User || !answered.contains(&id)
                }
                _ => false,
            })
        })
        .map(|(i, _)| i)
}

/// Results for the tool calls of a history whose last message holds them.
fn results_for_trailing_calls(history: &[MessageParam]) -> Option<MessageParam> {
    let last = history.last().filter(|m| m.role == Role::Assistant)?;
    let results: Vec<ContentBlock> = last
        .content
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::ToolUse { id, .. } => Some(ContentBlock::tool_result(
                id,
                "No result: the session ended before this call's result was recorded, so it may or may not have run.",
                true,
            )),
            _ => None,
        })
        .collect();
    (!results.is_empty()).then_some(MessageParam {
        role: Role::User,
        content: results,
    })
}

fn invalid(path: &Path, why: &str) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::InvalidData,
        format!("{}: not a session file: {why}", path.display()),
    )
}

/// The folder holding the sessions of working folder `cwd`, under `home`:
/// `<home>/.just-harness/sessions/<cwd with each / as ->`.
pub fn sessions_dir(home: &Path, cwd: &Path) -> PathBuf {
    home.join(".just-harness")
        .join("sessions")
        .join(folder_slug(cwd))
}

/// `cwd` as one folder name: each `/` as `-`.
pub fn folder_slug(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}

/// The most recently modified session file in `dir` whose meta names `cwd`.
/// Entries that cannot be read, and files that are not sessions, are skipped.
pub fn latest(dir: &Path, cwd: &str) -> std::io::Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_some_and(|(t, _)| modified <= *t) {
            continue;
        }
        let first = File::open(&path)
            .ok()
            .and_then(|f| BufReader::new(f).lines().next())
            .and_then(Result::ok);
        if read_meta(&path, first).is_ok_and(|meta| meta.cwd == cwd) {
            newest = Some((modified, path));
        }
    }
    Ok(newest.map(|(_, path)| path))
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A new session id: the creation time in seconds and 8 random hex digits.
pub fn new_id() -> String {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("{}-{:08x}", now_secs(), hasher.finish() as u32)
}
