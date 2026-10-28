//! Tools the model can call, and the folder they work in.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use futures_util::FutureExt;
use serde_json::Value;

use crate::api;

mod edit;
mod read;
mod search;
mod shell;

pub use edit::EditFile;
pub use read::ReadFile;
pub use search::Search;
pub use shell::Shell;

/// What a tool call returns: the text for the model, and whether it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;

pub trait Tool: Send + Sync {
    /// The definition sent to the API in `tools`.
    fn definition(&self) -> api::Tool;

    fn call<'a>(&'a self, input: &'a Value, ctx: &'a ToolContext) -> ToolFuture<'a>;
}

/// The working folder. Paths given to file tools resolve against it and may
/// not leave it.
#[derive(Debug, Clone)]
pub struct ToolContext {
    root: PathBuf,
}

impl ToolContext {
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            root: root.as_ref().canonicalize()?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` as an absolute path inside the working folder, or the reason it
    /// is refused. Symlinks are followed for the part of the path that exists.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let mut folded = PathBuf::new();
        for component in self.root.join(path).components() {
            match component {
                Component::ParentDir => {
                    folded.pop();
                }
                Component::CurDir => {}
                other => folded.push(other),
            }
        }
        let mut existing = folded.as_path();
        let mut rest = Vec::new();
        while existing.symlink_metadata().is_err() {
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_owned());
                    existing = parent;
                }
                _ => return Err(format!("{path}: cannot be resolved")),
            }
        }
        let mut resolved = existing
            .canonicalize()
            .map_err(|e| format!("{path}: {e}"))?;
        for name in rest.iter().rev() {
            resolved.push(name);
        }
        if !resolved.starts_with(&self.root) {
            return Err(format!(
                "{path}: outside the working folder {}",
                self.root.display()
            ));
        }
        Ok(resolved)
    }

    /// [`resolve`](Self::resolve) for a file tool that reads or writes the
    /// file's contents: also refuses `.env` and `.env.*` other than
    /// `.env.example`.
    pub fn resolve_file(&self, path: &str) -> Result<PathBuf, String> {
        let resolved = self.resolve(path)?;
        let name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if (name == ".env" || name.starts_with(".env.")) && name != ".env.example" {
            return Err(format!("{path}: refused, `.env` files may hold secrets"));
        }
        Ok(resolved)
    }

    /// `path` relative to the working folder, for messages to the model.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// The tools of one agent, looked up by name.
#[derive(Default)]
pub struct ToolSet {
    tools: Vec<(String, Box<dyn Tool>)>,
}

impl ToolSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push((tool.definition().name, Box::new(tool)));
        self
    }

    pub fn definitions(&self) -> Vec<api::Tool> {
        self.tools.iter().map(|(_, t)| t.definition()).collect()
    }

    /// Runs the tool called `name`. An unknown name, and a tool that
    /// panics, give an error result.
    pub async fn call(&self, name: &str, input: &Value, ctx: &ToolContext) -> ToolOutput {
        match self.tools.iter().find(|(n, _)| n == name) {
            Some((_, tool)) => match AssertUnwindSafe(tool.call(input, ctx)).catch_unwind().await {
                Ok(output) => output,
                Err(panic) => {
                    let reason = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown cause".into());
                    ToolOutput::error(format!("Error: tool '{name}' panicked: {reason}"))
                }
            },
            None => ToolOutput::error(format!("Error: Tool '{name}' not found")),
        }
    }
}

/// The four built-in tools.
pub fn builtin() -> ToolSet {
    ToolSet::new()
        .with(ReadFile)
        .with(EditFile)
        .with(Shell)
        .with(Search)
}

/// The string argument `key`, or why it is missing.
fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument `{key}`"))
}

/// The optional non-negative integer argument `key`.
fn u64_arg(input: &Value, key: &str) -> Result<Option<u64>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("argument `{key}` must be a non-negative integer")),
    }
}
