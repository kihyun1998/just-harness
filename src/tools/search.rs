use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolFuture, ToolOutput};
use crate::api;

/// The most result lines one search returns.
pub const MAX_RESULTS: usize = 500;
/// Matching lines longer than this are cut.
pub const MAX_LINE_CHARS: usize = 300;

/// Searches file contents by regex, file paths by glob, or both, skipping
/// hidden files and what `.gitignore` excludes.
pub struct Search;

impl Tool for Search {
    fn definition(&self) -> api::Tool {
        api::Tool {
            name: "search".into(),
            description: format!(
                "Search the working folder. With `pattern` (a regex), returns matching lines as \
                 `path:line:text`. With only `glob`, lists matching files. With both, searches \
                 only files matching `glob`. A glob without `/` matches file names anywhere; \
                 with `/` it matches the path from the working folder, e.g. `src/**/*.rs`. \
                 Hidden files and paths in `.gitignore` are skipped. At most {MAX_RESULTS} results."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex to find in file contents" },
                    "glob": { "type": "string", "description": "Glob that file paths must match" },
                    "path": { "type": "string", "description": "Folder to search, relative to the working folder; defaults to all of it" }
                }
            }),
            cache_control: None,
        }
    }

    fn call<'a>(&'a self, input: &'a Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move { search(input, ctx).unwrap_or_else(ToolOutput::error) })
    }

    fn needs_approval(&self) -> bool {
        false
    }
}

fn search(input: &Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
    let arg = |key: &str| input.get(key).and_then(Value::as_str);
    let pattern = arg("pattern")
        .map(|p| Regex::new(p).map_err(|e| format!("invalid regex: {e}")))
        .transpose()?;
    let glob = arg("glob").map(compile_glob).transpose()?;
    if pattern.is_none() && glob.is_none() {
        return Err("give `pattern` or `glob`, or both".into());
    }
    let base = ctx.resolve(arg("path").unwrap_or("."))?;

    let mut walker = ignore::WalkBuilder::new(&base);
    walker.require_git(false).sort_by_file_path(Path::cmp);
    let mut results = Vec::new();
    for entry in walker.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let relative = ctx.display(entry.path());
        if let Some(glob) = &glob
            && !glob.matches(&relative)
        {
            continue;
        }
        match &pattern {
            None => results.push(relative),
            Some(pattern) => {
                let Ok(text) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                for (n, line) in text.lines().enumerate() {
                    if pattern.is_match(line) {
                        results.push(format!("{relative}:{}:{}", n + 1, cut(line)));
                        if results.len() > MAX_RESULTS {
                            break;
                        }
                    }
                }
            }
        }
        if results.len() > MAX_RESULTS {
            results.truncate(MAX_RESULTS);
            results.push(format!("[stopped at {MAX_RESULTS} results]"));
            break;
        }
    }
    if results.is_empty() {
        return Ok(ToolOutput::ok(
            "no matches (hidden files and paths in .gitignore are not searched)",
        ));
    }
    Ok(ToolOutput::ok(results.join("\n")))
}

/// A glob matched against the file name when it has no `/`, and against the
/// path from the working folder when it has one.
struct PathGlob {
    matcher: GlobMatcher,
    whole_path: bool,
}

impl PathGlob {
    fn matches(&self, relative: &str) -> bool {
        if self.whole_path {
            self.matcher.is_match(relative)
        } else {
            let name = relative.rsplit('/').next().unwrap_or(relative);
            self.matcher.is_match(name)
        }
    }
}

fn compile_glob(glob: &str) -> Result<PathGlob, String> {
    let matcher = GlobBuilder::new(glob)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("invalid glob: {e}"))?
        .compile_matcher();
    Ok(PathGlob {
        matcher,
        whole_path: glob.contains('/'),
    })
}

fn cut(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((i, _)) => format!("{}...", &line[..i]),
        None => line.to_string(),
    }
}
