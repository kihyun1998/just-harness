use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolFuture, ToolOutput, str_arg, u64_arg};
use crate::api;

pub const DEFAULT_READ_LIMIT: usize = 2000;
/// The most bytes one read returns; lines past it are left for the next offset.
pub const MAX_READ_BYTES: usize = 50_000;
/// Lines longer than this many characters are cut.
pub const MAX_LINE_CHARS: usize = 2000;

/// Reads a UTF-8 text file as numbered lines.
pub struct ReadFile;

impl Tool for ReadFile {
    fn definition(&self) -> api::Tool {
        api::Tool {
            name: "read_file".into(),
            description: format!(
                "Read a text file in the working folder. Lines come back numbered from 1, \
                 as `<number>\\t<line>`. Reads up to {DEFAULT_READ_LIMIT} lines unless \
                 `limit` says otherwise, and at most {MAX_READ_BYTES} bytes; use `offset` to \
                 continue. Lines over {MAX_LINE_CHARS} characters are cut."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to the working folder" },
                    "offset": { "type": "integer", "minimum": 1, "description": "First line to read, from 1" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Most lines to read" }
                },
                "required": ["path"]
            }),
            cache_control: None,
        }
    }

    fn call<'a>(&'a self, input: &'a Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move { read(input, ctx).unwrap_or_else(ToolOutput::error) })
    }

    fn needs_approval(&self) -> bool {
        false
    }
}

fn read(input: &Value, ctx: &ToolContext) -> Result<ToolOutput, String> {
    let path = ctx.resolve_file(str_arg(input, "path")?)?;
    let offset = u64_arg(input, "offset")?.unwrap_or(1).max(1) as usize;
    let limit = u64_arg(input, "limit")?
        .map(|l| l.max(1) as usize)
        .unwrap_or(DEFAULT_READ_LIMIT);
    let shown = ctx.display(&path);
    let bytes = std::fs::read(&path).map_err(|e| format!("{shown}: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown}: not UTF-8 text"))?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok(ToolOutput::ok(format!("[{shown} is empty]")));
    }
    if offset > total {
        return Err(format!(
            "{shown}: offset {offset} is past the last line, {total}"
        ));
    }
    let last = (offset - 1).saturating_add(limit).min(total);
    let mut out: Vec<String> = Vec::new();
    let mut bytes = 0;
    let mut end = offset - 1;
    for (i, line) in lines[offset - 1..last].iter().enumerate() {
        let numbered = format!("{:>6}\t{}", offset + i, cut(line));
        if bytes + numbered.len() + 1 > MAX_READ_BYTES && !out.is_empty() {
            break;
        }
        bytes += numbered.len() + 1;
        out.push(numbered);
        end = offset + i;
    }
    if offset > 1 || end < total {
        let more = if end < total {
            format!("; more from offset {}", end + 1)
        } else {
            String::new()
        };
        out.push(format!("[lines {offset}-{end} of {total}{more}]"));
    }
    Ok(ToolOutput::ok(out.join("\n")))
}

fn cut(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((i, _)) => format!(
            "{}... [line cut at {MAX_LINE_CHARS} of {} characters]",
            &line[..i],
            line.chars().count()
        ),
        None => line.to_string(),
    }
}
