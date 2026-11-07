//! Jupyter notebooks as text and image blocks: each cell's source, then its
//! outputs.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::Value;

use super::image;
use crate::api::ContentBlock;

/// The most bytes of cell and output text a notebook comes to.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// The most bytes of text one cell's outputs keep.
const MAX_OUTPUT_BYTES: usize = 10_000;

const NOT_A_NOTEBOOK: &str = "Notebook file is not a valid Jupyter notebook (top-level \"cells\" \
    must be an array of cell objects, each with a string or string-array \"source\").";

/// Text pieces joined by a line end, and the blocks they became, in order.
#[derive(Default)]
struct Blocks {
    blocks: Vec<ContentBlock>,
    pieces: Vec<String>,
    text_bytes: usize,
}

impl Blocks {
    fn text(&mut self, piece: String) {
        self.text_bytes += piece.len();
        self.pieces.push(piece);
    }

    fn block(&mut self, block: ContentBlock) {
        self.flush();
        self.blocks.push(block);
    }

    fn flush(&mut self) {
        if !self.pieces.is_empty() {
            self.blocks.push(ContentBlock::text(self.pieces.join("\n")));
            self.pieces.clear();
        }
    }
}

/// The blocks a notebook's `bytes` come to, or why it cannot be read.
pub fn render(bytes: &[u8]) -> Result<Vec<ContentBlock>, String> {
    let notebook: Value = serde_json::from_slice(bytes).map_err(|e| {
        format!(
            "Notebook file is not valid JSON (it may be truncated, corrupted, or still being \
             written): {e}"
        )
    })?;
    let cells = notebook["cells"]
        .as_array()
        .ok_or_else(|| NOT_A_NOTEBOOK.to_string())?;
    let language = language(&notebook);

    let mut out = Blocks::default();
    for (index, cell) in cells.iter().enumerate() {
        render_cell(cell, index, language, &mut out)?;
    }
    out.flush();
    if out.text_bytes > MAX_TEXT_BYTES {
        return Err(format!(
            "Notebook content ({}KB) exceeds maximum allowed size (256KB). Use a shell command \
             with jq to read specific portions:\n  \
             cat <notebook_path> | jq '.cells[:20]' # First 20 cells\n  \
             cat <notebook_path> | jq '.cells[100:120]' # Cells 100-120\n  \
             cat <notebook_path> | jq '.cells | length' # Count total cells\n  \
             cat <notebook_path> | jq '.cells[] | select(.cell_type==\"code\") | .source' # All \
             code sources",
            out.text_bytes / 1024
        ));
    }
    Ok(out.blocks)
}

fn language(notebook: &Value) -> &str {
    notebook["metadata"]["language_info"]["name"]
        .as_str()
        .unwrap_or("python")
}

fn render_cell(cell: &Value, index: usize, language: &str, out: &mut Blocks) -> Result<(), String> {
    let source = lines(&cell["source"]).ok_or_else(|| NOT_A_NOTEBOOK.to_string())?;
    let id = cell["id"]
        .as_str()
        .map_or_else(|| format!("cell-{index}"), String::from);
    let kind = cell["cell_type"].as_str().unwrap_or("code");
    let tag = match kind {
        "code" if language == "python" => String::new(),
        "code" => format!("<language>{language}</language>"),
        other => format!("<cell_type>{other}</cell_type>"),
    };
    out.text(format!(
        "<cell id=\"{id}\">{tag}{source}</cell id=\"{id}\">"
    ));

    let outputs = cell["outputs"].as_array().map_or(&[][..], Vec::as_slice);
    let output_bytes: usize = outputs
        .iter()
        .filter_map(output_text)
        .map(|text| text.len())
        .sum();
    if output_bytes > MAX_OUTPUT_BYTES {
        out.text(format!(
            "\nOutputs are too large to include. Use a shell command: cat <notebook_path> | jq \
             '.cells[{index}].outputs'"
        ));
        return Ok(());
    }
    for output in outputs {
        if let Some(text) = output_text(output) {
            out.text(format!("\n{text}"));
        }
        if let Some(data) = output_image(output) {
            match image_block(data) {
                Some(block) => out.block(block),
                None => out.text("\n[an image output could not be read]".to_string()),
            }
        }
    }
    Ok(())
}

/// The text of a source or output field: one string, or an array of strings.
fn lines(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => parts
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.concat()),
        _ => None,
    }
}

fn output_text(output: &Value) -> Option<String> {
    match output["output_type"].as_str()? {
        "stream" => lines(&output["text"]),
        "execute_result" | "display_data" => lines(&output["data"]["text/plain"]),
        "error" => {
            let trace = output["traceback"]
                .as_array()
                .map_or_else(Vec::new, |lines| {
                    lines.iter().filter_map(Value::as_str).collect()
                });
            Some(format!(
                "{}: {}\n{}",
                output["ename"].as_str().unwrap_or(""),
                output["evalue"].as_str().unwrap_or(""),
                trace.join("\n")
            ))
        }
        _ => None,
    }
}

fn output_image(output: &Value) -> Option<String> {
    ["image/png", "image/jpeg"]
        .iter()
        .find_map(|kind| lines(&output["data"][*kind]))
}

/// The block for an output's base64 `data`, which may hold line ends.
fn image_block(data: String) -> Option<ContentBlock> {
    let data: String = data.split_whitespace().collect();
    let bytes = BASE64.decode(data).ok()?;
    let format = image::sniff(&bytes)?;
    image::prepare(&bytes, format).ok().map(|image| image.block)
}
