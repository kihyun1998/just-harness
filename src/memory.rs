//! Project memory: the instruction files a person writes and the memory
//! index the model keeps, read at the start of a session and handed to the
//! model as one user message, and what differs from them when a session
//! resumes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
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
    /// The `CLAUDE.md` files, or the `AGENTS.md` files when none of those
    /// carries anything.
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

/// The instruction files `mode` reads for the working folder `cwd`, from the
/// top folder down to `cwd`.
pub fn instruction_files(cwd: &Path, home: &Path, mode: InstructionFiles) -> Vec<PathBuf> {
    project_files(cwd, home, mode)
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

/// [`instruction_files`], each with the folder it was found in. Left out, and
/// not counting: `home`'s own `.claude` folder, and above a worktree inside
/// its repository, all but `CLAUDE.local.md`.
fn project_files(cwd: &Path, home: &Path, mode: InstructionFiles) -> Vec<(PathBuf, PathBuf)> {
    let home = home.canonicalize().ok();
    let home_claude = home
        .as_ref()
        .and_then(|h| h.join(".claude").canonicalize().ok());
    let checkout = checkout_above_worktree(cwd);
    let mut folders: Vec<&Path> = cwd.ancestors().collect();
    folders.reverse();
    let existing = |names: &[&str]| -> Vec<Vec<(PathBuf, PathBuf)>> {
        folders
            .iter()
            .map(|dir| {
                let real = dir.canonicalize().ok();
                let is_home = home.is_some() && real == home;
                let is_home_claude = home_claude.is_some() && real == home_claude;
                let above_worktree = match (&real, &checkout) {
                    (Some(real), Some((main, worktree))) => {
                        real.starts_with(main) && !real.starts_with(worktree)
                    }
                    _ => false,
                };
                names
                    .iter()
                    .filter(|_| !is_home_claude)
                    .filter(|name| !(is_home && name.starts_with(".claude/")))
                    .filter(|name| !above_worktree || **name == "CLAUDE.local.md")
                    .map(|name| (dir.join(name), dir.to_path_buf()))
                    .filter(|(path, _)| path.is_file())
                    .collect()
            })
            .collect()
    };
    let claude = existing(&PROJECT_CLAUDE);
    let any_claude = claude
        .iter()
        .flatten()
        .any(|(path, dir)| carries(path, dir));
    let agents = match mode {
        InstructionFiles::ClaudeMdOrAgentsMd if any_claude => None,
        InstructionFiles::ClaudeMd => None,
        _ => Some(existing(&PROJECT_AGENTS)),
    };
    claude
        .into_iter()
        .enumerate()
        .flat_map(|(i, mut files)| {
            if let Some(agents) = &agents {
                files.extend(agents[i].iter().cloned());
            }
            files
        })
        .collect()
}

/// Whether the file at `path`, found in `dir`, would be carried.
fn carries(path: &Path, dir: &Path) -> bool {
    read_confined(path, dir).is_ok_and(|(_, text)| !text.trim().is_empty())
}

/// The real paths of the main checkout and the worktree, when `cwd` is in a
/// worktree whose main repository has a checkout.
fn checkout_above_worktree(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let worktree = cwd.ancestors().find(|dir| dir.join(".git").exists())?;
    let main = worktree_main(&worktree.join(".git"))?;
    main.join(".git")
        .is_dir()
        .then_some((main, worktree.canonicalize().ok()?))
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
        InstructionFiles::ClaudeMdOrAgentsMd => !files.iter().any(|path| carries(path, dir)),
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
        parts.push(file_part(path, contents));
    }
    if let Some((dir, index)) = memory
        && !index.trim().is_empty()
    {
        parts.push(index_part(dir, index));
    }
    carrying(&parts)
}

/// The user message that carries `parts` as the project context; `None` when
/// there are none.
fn carrying(parts: &[String]) -> Option<MessageParam> {
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

fn file_part(path: &Path, contents: &str) -> String {
    format!("Contents of {}:\n\n{}", path.display(), contents.trim_end())
}

fn index_part(dir: &Path, index: &str) -> String {
    format!(
        "Contents of {} (your memory index; the files it names are in {}):\n\n{}",
        dir.join(INDEX).display(),
        dir.display(),
        index.trim_end()
    )
}

/// One file as the project context carried it: its path as shown to the
/// model, and an instruction file's text or the excerpt of the memory index,
/// with no blank end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFile {
    pub path: String,
    pub text: String,
}

/// What [`context_update`] found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContextUpdate {
    /// The user message to send; `None` when nothing differs.
    pub message: Option<MessageParam>,
    /// The files sent, in the order of `now`.
    pub changed: Vec<String>,
    /// The files no longer there, in the order of `earlier`.
    pub gone: Vec<String>,
}

/// What differs between the `earlier` copies, `None` when there are none to
/// compare with, and the files as they are `now`, path by path. With no
/// earlier files the changed ones come as a first project context. `memory`
/// is the memory folder of this run.
pub fn context_update(
    earlier: Option<&[ContextFile]>,
    now: &[ContextFile],
    memory: Option<&Path>,
) -> ContextUpdate {
    let earlier = earlier.unwrap_or_default();
    let changed: Vec<&ContextFile> = now.iter().filter(|file| !earlier.contains(file)).collect();
    let gone: Vec<String> = earlier
        .iter()
        .filter(|file| !now.iter().any(|current| current.path == file.path))
        .map(|file| file.path.clone())
        .collect();
    if changed.is_empty() && gone.is_empty() {
        return ContextUpdate::default();
    }
    let contents: Vec<String> = changed
        .iter()
        .map(|file| match memory {
            Some(dir) if file.path == dir.join(INDEX).display().to_string() => {
                index_part(dir, &file.text)
            }
            _ => file_part(Path::new(&file.path), &file.text),
        })
        .collect();
    let message = if earlier.is_empty() {
        carrying(&contents)
    } else {
        let mut parts = vec![if contents.is_empty() {
            "Instruction files were re-read when this session resumed.".to_string()
        } else {
            "Instruction files were re-read when this session resumed; these differ from their \
             earlier copies, each replaces its earlier copy, and the instructions carried above \
             still apply:"
                .to_string()
        }];
        parts.extend(contents);
        if !gone.is_empty() {
            parts.push(format!(
                "Instructions no longer present: {}",
                gone.join(", ")
            ));
        }
        Some(MessageParam {
            role: Role::User,
            content: vec![ContentBlock::text(format!(
                "<project-context-update>\n{}\n</project-context-update>",
                parts.join("\n\n")
            ))],
        })
    };
    ContextUpdate {
        message,
        changed: changed.iter().map(|file| file.path.clone()).collect(),
        gone,
    }
}

/// What [`load_context`] read.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Loaded {
    /// The [`project_context`] message, when there was anything to carry.
    pub message: Option<MessageParam>,
    /// What that message carries, file by file.
    pub files: Vec<ContextFile>,
    /// The files read, the memory index included.
    pub read: Vec<PathBuf>,
    /// The files found and left out, each with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Reads the user's instruction files, those of the folders from the top down
/// to the working folder, and the memory index in `memory` when there is a memory folder, into
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
        .map(|p| (p, user_dir.clone()))
        .chain(project_files(cwd, home, mode));
    for (path, dir) in candidates {
        match read_confined(&path, &dir) {
            Ok((real, text)) => {
                if !seen.contains(&real) {
                    seen.push(real);
                    loaded.read.push(path.clone());
                    if !text.trim().is_empty() {
                        files.push((path, text));
                    }
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
    let excerpt = index_excerpt(&index);
    loaded.files = files
        .iter()
        .map(|(path, text)| ContextFile {
            path: path.display().to_string(),
            text: text.trim_end().to_string(),
        })
        .collect();
    if let Some(dir) = memory
        && !excerpt.trim().is_empty()
    {
        loaded.files.push(ContextFile {
            path: dir.join(INDEX).display().to_string(),
            text: excerpt.trim_end().to_string(),
        });
    }
    loaded.message = project_context(&files, memory.map(|dir| (dir, excerpt)));
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
