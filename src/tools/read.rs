use std::io::Read;
use std::path::Path;

use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolFuture, ToolOutput, str_arg, u64_arg};
use crate::api;

mod image;
mod notebook;

pub use notebook::render as render_notebook;

pub const DEFAULT_READ_LIMIT: usize = 2000;
/// The most bytes one read returns; lines past it are left for the next offset.
pub const MAX_READ_BYTES: usize = 50_000;
/// Lines longer than this many characters are cut.
pub const MAX_LINE_CHARS: usize = 2000;
/// The largest image file read.
pub const MAX_IMAGE_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// The largest notebook file read.
pub const MAX_NOTEBOOK_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// Extensions refused as binary formats.
const BINARY_EXTENSIONS: &[&str] = &[
    "7z", "a", "app", "avi", "bin", "bmp", "bz2", "class", "dat", "db", "deb", "dll", "dmg", "doc",
    "docx", "dylib", "exe", "flac", "gz", "heic", "heif", "ico", "iso", "jar", "m4a", "mkv", "mov",
    "mp3", "mp4", "o", "otf", "parquet", "ppt", "pptx", "pyc", "rar", "rpm", "so", "sqlite", "tar",
    "tgz", "tif", "tiff", "ttf", "wasm", "wav", "webm", "woff", "woff2", "xls", "xlsx", "xz",
    "zip",
];

/// What a file is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Text,
    Image,
    Notebook,
    Binary,
}

/// What `path` is read as: by its extension, and by its first bytes when it
/// has none.
pub fn file_kind(path: &Path) -> FileKind {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return if starts_as_image(path) {
            FileKind::Image
        } else {
            FileKind::Text
        };
    };
    let extension = extension.to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => FileKind::Image,
        "ipynb" => FileKind::Notebook,
        other if BINARY_EXTENSIONS.contains(&other) => FileKind::Binary,
        _ => FileKind::Text,
    }
}

fn starts_as_image(path: &Path) -> bool {
    if !std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    let mut start = [0u8; 12];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut filled = 0;
    while filled < start.len() {
        match file.read(&mut start[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    image::sniff(&start[..filled]).is_some()
}

/// Reads text files as numbered lines, images as image blocks, and notebooks
/// as their cells.
pub struct ReadFile;

impl Tool for ReadFile {
    fn definition(&self) -> api::Tool {
        api::Tool {
            name: "read_file".into(),
            description: format!(
                "Read a file in the working folder. Text comes back as lines numbered from 1, \
                 as `<number>\\t<line>`. Reads up to {DEFAULT_READ_LIMIT} lines unless \
                 `limit` says otherwise, and at most {MAX_READ_BYTES} bytes; use `offset` to \
                 continue. Lines over {MAX_LINE_CHARS} characters are cut. PNG, JPEG, GIF and \
                 WebP images come back as images you can see, large ones scaled down. \
                 Jupyter notebooks come back as their cells with their outputs."
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
    match file_kind(&path) {
        FileKind::Text => {}
        FileKind::Image => return read_image(&path, &shown),
        FileKind::Notebook => return read_notebook(&path, &shown),
        FileKind::Binary => return Err(binary_refusal(&path)),
    }
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

fn binary_refusal(path: &Path) -> String {
    let extension = path.extension().unwrap_or_default().to_string_lossy();
    format!(
        "This tool cannot read binary files. The file appears to be a binary .{extension} file. \
         Use a shell command or script that can read the format."
    )
}

fn read_image(path: &Path, shown: &str) -> Result<ToolOutput, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!(
            "Cannot read this as an image: {shown} is not a regular file."
        ));
    }
    if meta.len() > MAX_IMAGE_FILE_BYTES {
        return Err(format!(
            "File is too large to read as an image (the limit is 128MB): {shown}. If it is an \
             image, resize it or export a smaller copy, then read that."
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{shown}: {e}"))?;
    if bytes.is_empty() {
        return Err(format!("Image file is empty: {shown}"));
    }
    let Some(format) = image::sniff(&bytes) else {
        return Err(format!(
            "File has an image extension but its content is not a valid PNG/JPEG/GIF/WebP: \
             {shown}. This usually means a download saved an error or login page instead of the \
             image."
        ));
    };
    let prepared = image::prepare(&bytes, format).map_err(|e| format!("{shown}: {e}"))?;
    Ok(ToolOutput {
        content: format!("[image {shown}, {}x{}]", prepared.width, prepared.height),
        blocks: vec![prepared.block],
        after: prepared.note,
        is_error: false,
    })
}

fn read_notebook(path: &Path, shown: &str) -> Result<ToolOutput, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if meta.len() > MAX_NOTEBOOK_FILE_BYTES {
        return Err(format!(
            "Notebook file is too large to read (the limit is 100MB): {shown}. Read a portion \
             with a shell command instead, such as: cat <notebook_path> | jq '.cells[:20]'"
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{shown}: {e}"))?;
    let blocks = notebook::render(&bytes)?;
    Ok(ToolOutput {
        content: format!("[notebook {shown}]"),
        blocks,
        after: None,
        is_error: false,
    })
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
