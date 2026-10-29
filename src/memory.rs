//! Project memory: the instruction files a person writes and the memory
//! index the model keeps, read at the start of a session and handed to the
//! model as one user message.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::api::{ContentBlock, MessageParam, Role};
use crate::session::folder_slug;

/// The memory index's file name in the memory folder.
pub const INDEX: &str = "MEMORY.md";
/// How much of the index is read: its first lines, up to this many bytes.
pub const INDEX_MAX_LINES: usize = 200;
pub const INDEX_MAX_BYTES: usize = 25_000;
/// Instruction files larger than this are skipped.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Which instruction files are read, under the names Claude Code gives
/// its "Project instructions" setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstructionFiles {
    /// The `CLAUDE.md` files, or the `AGENTS.md` files where a folder has none.
    #[default]
    ClaudeMdOrAgentsMd,
    /// Both, each folder's `CLAUDE.md` files first.
    ClaudeMdAndAgentsMd,
    /// The `CLAUDE.md` files only.
    ClaudeMd,
}

impl InstructionFiles {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude-md-or-agents-md" => Some(Self::ClaudeMdOrAgentsMd),
            "claude-md-and-agents-md" => Some(Self::ClaudeMdAndAgentsMd),
            "claude-md" => Some(Self::ClaudeMd),
            _ => None,
        }
    }
}

/// The user's settings, from `~/.just-harness/settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// `"instructionFiles"`.
    pub instruction_files: InstructionFiles,
}

impl Settings {
    /// The settings in `path`; the defaults when the file does not exist.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let json: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut settings = Self::default();
        if let Some(value) = json.get("instructionFiles") {
            settings.instruction_files = value
                .as_str()
                .and_then(InstructionFiles::parse)
                .ok_or_else(|| {
                    format!(
                        "{}: instructionFiles is {value}; expected \"claude-md-or-agents-md\", \
                         \"claude-md-and-agents-md\" or \"claude-md\"",
                        path.display()
                    )
                })?;
        }
        Ok(settings)
    }
}

const PROJECT_CLAUDE: [&str; 3] = ["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"];
const PROJECT_AGENTS: [&str; 2] = ["AGENTS.md", ".claude/AGENTS.md"];

/// The instruction files of the working folder `dir` that `mode` reads, in
/// the order they are read.
pub fn instruction_files(dir: &Path, mode: InstructionFiles) -> Vec<PathBuf> {
    select(dir, &PROJECT_CLAUDE, &PROJECT_AGENTS, mode)
}

/// The user's own instruction files in `~/.just-harness/`.
fn user_files(home: &Path, mode: InstructionFiles) -> Vec<PathBuf> {
    select(
        &home.join(".just-harness"),
        &["CLAUDE.md"],
        &["AGENTS.md"],
        mode,
    )
}

fn select(dir: &Path, claude: &[&str], agents: &[&str], mode: InstructionFiles) -> Vec<PathBuf> {
    let existing = |names: &[&str]| -> Vec<PathBuf> {
        names
            .iter()
            .map(|name| dir.join(name))
            .filter(|path| path.is_file())
            .collect()
    };
    let mut files = existing(claude);
    let read_agents = match mode {
        InstructionFiles::ClaudeMdOrAgentsMd => files.is_empty(),
        InstructionFiles::ClaudeMdAndAgentsMd => true,
        InstructionFiles::ClaudeMd => false,
    };
    if read_agents {
        files.extend(existing(agents));
    }
    files
}

/// The folder the model keeps its memory in for the working folder `cwd`:
/// `~/.just-harness/projects/<project with / as ->/memory`, where the
/// project is the git repository `cwd` is in ([`project_root`]).
pub fn memory_dir(home: &Path, cwd: &Path) -> PathBuf {
    home.join(".just-harness")
        .join("projects")
        .join(folder_slug(&project_root(cwd)))
        .join("memory")
}

/// The top folder of the git repository `cwd` is in, the main one for a
/// worktree; `cwd` itself outside a repository.
pub fn project_root(cwd: &Path) -> PathBuf {
    for dir in cwd.ancestors() {
        let git = dir.join(".git");
        if git.is_dir() {
            return dir.to_path_buf();
        }
        if git.is_file()
            && let Some(root) = worktree_main(&git)
        {
            return root;
        }
    }
    cwd.to_path_buf()
}

/// The main repository's top folder for a worktree's `.git` file, which
/// names the worktree's git folder; that folder's `commondir` names the
/// main `.git` folder.
fn worktree_main(git_file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git_file).ok()?;
    let gitdir = git_file
        .parent()?
        .join(text.trim().strip_prefix("gitdir:")?.trim());
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = gitdir.join(common.trim()).canonicalize().ok()?;
    Some(common.parent()?.to_path_buf())
}

/// The part of the index that is read: the first [`INDEX_MAX_LINES`] lines,
/// cut further at [`INDEX_MAX_BYTES`] on a character boundary.
pub fn index_excerpt(text: &str) -> &str {
    let mut end = text.len();
    if let Some((i, _)) = text.match_indices('\n').nth(INDEX_MAX_LINES - 1) {
        end = i + 1;
    }
    if end > INDEX_MAX_BYTES {
        end = INDEX_MAX_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
    }
    &text[..end]
}

/// The user message that carries `files` (each path with its contents) and
/// the memory index excerpt read from `memory` (its folder and the text).
/// `None` when there is nothing to carry.
pub fn project_context(
    files: &[(PathBuf, String)],
    memory: Option<(&Path, &str)>,
) -> Option<MessageParam> {
    let mut parts = Vec::new();
    for (path, contents) in files {
        parts.push(format!(
            "Contents of {}:\n\n{}",
            path.display(),
            contents.trim_end()
        ));
    }
    if let Some((dir, index)) = memory
        && !index.trim().is_empty()
    {
        parts.push(format!(
            "Contents of {} (your memory index; the files it names are in {}):\n\n{}",
            dir.join(INDEX).display(),
            dir.display(),
            index.trim_end()
        ));
    }
    if parts.is_empty() {
        return None;
    }
    let text = format!(
        "<project-context>\nInstructions and memory carried over from earlier sessions. \
         Follow the instructions; they are context for the work, not the user's request.\n\n{}\n\
         </project-context>",
        parts.join("\n\n")
    );
    Some(MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    })
}

/// What [`load_context`] read.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Loaded {
    /// The [`project_context`] message, when there was anything to carry.
    pub message: Option<MessageParam>,
    /// The files read, the memory index included.
    pub read: Vec<PathBuf>,
    /// The files found and left out, each with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Reads the user's and the working folder's instruction files, and the
/// memory index in `memory` when there is a memory folder, into
/// [`project_context`]. A file is left out, with the reason, when it cannot
/// be read, is over [`MAX_FILE_BYTES`], is a `.env` file, or leads through
/// a symlink out of the folder it was found in; a file reached under two
/// names is read once.
pub fn load_context(
    home: &Path,
    cwd: &Path,
    memory: Option<&Path>,
    mode: InstructionFiles,
) -> Loaded {
    let mut loaded = Loaded::default();
    let mut seen = Vec::new();
    let mut files = Vec::new();
    let user_dir = home.join(".just-harness");
    let candidates = user_files(home, mode)
        .into_iter()
        .map(|p| (p, user_dir.as_path()))
        .chain(instruction_files(cwd, mode).into_iter().map(|p| (p, cwd)));
    for (path, dir) in candidates {
        match read_confined(&path, dir) {
            Ok((real, text)) => {
                if !seen.contains(&real) {
                    seen.push(real);
                    loaded.read.push(path.clone());
                    files.push((path, text));
                }
            }
            Err(why) => loaded.skipped.push((path, why)),
        }
    }
    let mut index = String::new();
    if let Some(dir) = memory {
        let path = dir.join(INDEX);
        if path.symlink_metadata().is_ok() {
            match read_confined(&path, dir) {
                Ok((_, text)) => {
                    loaded.read.push(path);
                    index = text;
                }
                Err(why) => loaded.skipped.push((path, why)),
            }
        }
    }
    loaded.message = project_context(&files, memory.map(|dir| (dir, index_excerpt(&index))));
    loaded
}

/// The file at `path`, with its real path, when it resolves inside `dir`
/// to a file not named `.env` and is at most [`MAX_FILE_BYTES`]; otherwise why not.
fn read_confined(path: &Path, dir: &Path) -> Result<(PathBuf, String), String> {
    let real = path.canonicalize().map_err(|e| e.to_string())?;
    let dir = dir.canonicalize().map_err(|e| e.to_string())?;
    if !real.starts_with(&dir) {
        return Err(format!("it leads outside {}", dir.display()));
    }
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if (name == ".env" || name.starts_with(".env.")) && name != ".env.example" {
        return Err("`.env` files may hold secrets".into());
    }
    let size = std::fs::metadata(&real).map_err(|e| e.to_string())?.len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "{size} bytes, over the {MAX_FILE_BYTES}-byte limit"
        ));
    }
    let bytes = std::fs::read(&real).map_err(|e| e.to_string())?;
    Ok((real, String::from_utf8_lossy(&bytes).into_owned()))
}
