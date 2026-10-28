use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolFuture, ToolOutput, str_arg};
use crate::api;

/// Replaces one exact occurrence of a string in a file.
pub struct EditFile;

impl Tool for EditFile {
    fn definition(&self) -> api::Tool {
        api::Tool {
            name: "edit_file".into(),
            description: "Replace `old_string` with `new_string` in a file in the working folder. \
                          `old_string` must match the file exactly, whitespace included, and must \
                          occur exactly once; include surrounding lines to make it unique. To create \
                          a file, or fill an empty one, give an empty `old_string`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to the working folder" },
                    "old_string": { "type": "string", "description": "Exact text to replace" },
                    "new_string": { "type": "string", "description": "Text to put in its place" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            cache_control: None,
        }
    }

    fn call<'a>(&'a self, input: &'a Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move { edit(input, ctx).unwrap_or_else(ToolOutput::error) })
    }

    fn needs_approval(&self) -> bool {
        true
    }

    /// The path, and the file it resolves to when that differs, then the old
    /// text as `- ` lines and the new as `+ ` lines.
    fn approval_preview(&self, input: &Value, ctx: &ToolContext) -> String {
        let field = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
        let path = field("path");
        let mut out = match ctx.resolve(path) {
            Ok(resolved) if ctx.display(&resolved) != path => {
                format!("{path} -> {}", ctx.display(&resolved))
            }
            _ => path.to_string(),
        };
        if field("old_string").is_empty() {
            out.push_str("\n(new or empty file)");
        }
        for line in field("old_string").lines() {
            out.push_str(&format!("\n- {line}"));
        }
        for line in field("new_string").lines() {
            out.push_str(&format!("\n+ {line}"));
        }
        out
    }

    fn approval_key(&self, input: &Value) -> String {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| input.to_string())
    }
}

fn edit(input: &Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
    let path = ctx.resolve_file(str_arg(input, "path")?)?;
    let old = str_arg(input, "old_string")?;
    let new = str_arg(input, "new_string")?;
    let shown = ctx.display(&path);

    if old.is_empty() {
        let has_content = std::fs::metadata(&path).is_ok_and(|m| m.len() > 0);
        if has_content {
            return Err(format!(
                "{shown}: exists and is not empty; give the text to replace in `old_string`"
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{shown}: {e}"))?;
        }
        std::fs::write(&path, new).map_err(|e| format!("{shown}: {e}"))?;
        return Ok(ToolOutput::ok(format!(
            "Wrote {shown} ({} lines)",
            new.lines().count()
        )));
    }

    let content = std::fs::read_to_string(&path).map_err(|e| format!("{shown}: {e}"))?;
    let (old, new) = line_endings_of(&content, old, new);
    let (old, new) = (old.as_str(), new.as_str());
    let starts: Vec<usize> = content.match_indices(old).map(|(i, _)| i).collect();
    match starts.len() {
        0 => Err(format!("{shown}: `old_string` not found")),
        1 => {
            let updated = content.replacen(old, new, 1);
            std::fs::write(&path, updated).map_err(|e| format!("{shown}: {e}"))?;
            Ok(ToolOutput::ok(format!(
                "Edited {shown} ({} lines -> {} lines)",
                old.lines().count(),
                new.lines().count()
            )))
        }
        n => {
            let lines: Vec<String> = starts
                .iter()
                .take(2)
                .map(|&i| format!("line {}", content[..i].matches('\n').count() + 1))
                .collect();
            Err(format!(
                "{shown}: `old_string` has {n} matches (at {}{}); add surrounding lines to make it unique",
                lines.join(", "),
                if n > 2 { ", ..." } else { "" }
            ))
        }
    }
}

/// `old` and `new` with `\n` line ends turned into `\r\n` when `old` only
/// matches that way: the file uses CRLF and `old` was written with LF.
fn line_endings_of(content: &str, old: &str, new: &str) -> (String, String) {
    let crlf = |text: &str| text.replace("\r\n", "\n").replace('\n', "\r\n");
    if !content.contains(old) && old.contains('\n') && content.contains(&crlf(old)) {
        (crlf(old), crlf(new))
    } else {
        (old.to_string(), new.to_string())
    }
}
