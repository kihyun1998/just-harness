//! PDF files: whole as a `document` block, or pages as images.

use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;
use tokio::process::Command;

use super::image;
use crate::api::ContentBlock;
use crate::tools::ToolOutput;

/// The most pages a PDF is read whole with.
const MAX_WHOLE_PAGES: u64 = 10;
/// The most pages one read renders.
const MAX_RANGE_PAGES: u64 = 20;
/// The largest PDF sent whole, in bytes.
const MAX_WHOLE_BYTES: u64 = 90_000_000;
/// The resolution pages are rendered at.
const RENDER_DPI: &str = "100";

/// Reads the PDF at `path`, or the `pages` of it.
pub async fn read(path: &Path, shown: &str, pages: Option<&str>) -> Result<ToolOutput, String> {
    let range = pages.map(parse_pages).transpose()?;
    let meta = std::fs::metadata(path).map_err(|e| format!("{shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!(
            "Cannot read this as a PDF: {shown} is not a regular file."
        ));
    }
    if !starts_as_pdf(path) {
        return Err(format!(
            "File is not a valid PDF (missing %PDF- header): {shown}"
        ));
    }
    let size = size_text(meta.len());
    match range {
        Some(range) => read_pages(path, shown, &size, range).await,
        None => read_whole(path, shown, &size, meta.len()).await,
    }
}

fn starts_as_pdf(path: &Path) -> bool {
    use std::io::Read;
    let mut start = [0u8; 5];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut start))
        .is_ok()
        && &start == b"%PDF-"
}

fn parse_pages(pages: &str) -> Result<(u64, u64), String> {
    let number = |text: &str| text.parse::<u64>().ok().filter(|n| *n >= 1);
    let range = match pages.split_once('-') {
        Some((first, last)) => number(first).zip(number(last)),
        None => number(pages).map(|page| (page, page)),
    };
    let range = range.filter(|(first, last)| first <= last).ok_or_else(|| {
        format!(
            "Invalid pages parameter: \"{pages}\". Give one page (\"3\") or one range (\"1-5\"), \
             not a list. To read several pages or ranges, read each one separately. Pages are \
             1-indexed."
        )
    })?;
    if range.1 - range.0 + 1 > MAX_RANGE_PAGES {
        return Err(format!(
            "Page range \"{pages}\" exceeds maximum of {MAX_RANGE_PAGES} pages per request. \
             Please use a smaller range."
        ));
    }
    Ok(range)
}

async fn read_whole(
    path: &Path,
    shown: &str,
    size: &str,
    bytes: u64,
) -> Result<ToolOutput, String> {
    if let Some(count) = page_count(path).await
        && count > MAX_WHOLE_PAGES
    {
        return Err(format!(
            "This PDF has {count} pages, which is too many to read at once. Use the pages \
             parameter to read specific page ranges (e.g., pages: \"1-5\"). Maximum \
             {MAX_RANGE_PAGES} pages per request."
        ));
    }
    if bytes > MAX_WHOLE_BYTES {
        return Err(format!(
            "This PDF is too large to send whole ({size}): {shown}. Use the pages parameter to \
             read specific page ranges (e.g., pages: \"1-5\"), or a shell command that can \
             extract its text."
        ));
    }
    let data = std::fs::read(path).map_err(|e| format!("{shown}: {e}"))?;
    let text = format!("PDF file read: {shown} ({size})");
    let document = ContentBlock(json!({
        "type": "document",
        "source": {
            "type": "base64",
            "media_type": "application/pdf",
            "data": BASE64.encode(data),
        },
    }));
    Ok(ToolOutput {
        content: text.clone(),
        blocks: vec![ContentBlock::text(text), document],
        after: None,
        is_error: false,
    })
}

async fn read_pages(
    path: &Path,
    shown: &str,
    size: &str,
    (first, last): (u64, u64),
) -> Result<ToolOutput, String> {
    let mut last = last;
    if let Some(count) = page_count(path).await {
        if first > count {
            return Err(format!(
                "Requested pages {first}-{last} is outside the document (PDF has {count} pages). \
                 Use a range within 1-{count}, maximum {MAX_RANGE_PAGES} pages per request (e.g. \
                 pages: \"1-{}\").",
                count.min(MAX_RANGE_PAGES)
            ));
        }
        last = last.min(count);
    }
    let mut images = Vec::new();
    for page in first..=last {
        let jpeg = render_page(path, page).await?;
        let format = image::sniff(&jpeg)
            .ok_or_else(|| format!("{shown}: page {page} did not render as an image"))?;
        let prepared = image::prepare(&jpeg, format).map_err(|e| format!("{shown}: {e}"))?;
        images.push(prepared.block);
    }
    let text = format!(
        "PDF pages extracted: {} page(s) from {shown} ({size})",
        images.len()
    );
    let mut blocks = vec![ContentBlock::text(text.clone())];
    blocks.extend(images);
    Ok(ToolOutput {
        content: text,
        blocks,
        after: None,
        is_error: false,
    })
}

/// The page count `pdfinfo` reports, or `None` when it cannot tell.
async fn page_count(path: &Path) -> Option<u64> {
    let output = quiet(Command::new("pdfinfo").arg(path))
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Pages:")?.trim().parse().ok())
}

async fn render_page(path: &Path, page: u64) -> Result<Vec<u8>, String> {
    let page_text = page.to_string();
    let mut command = Command::new("pdftoppm");
    command
        .args(["-jpeg", "-singlefile", "-r", RENDER_DPI])
        .args(["-f", &page_text, "-l", &page_text])
        .arg(path);
    let output = quiet(&mut command).output().await.map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or \
             `apt-get install poppler-utils`) to enable PDF page rendering."
                .to_string()
        } else {
            format!("pdftoppm: {e}")
        }
    })?;
    if !output.status.success() {
        return Err(format!(
            "pdftoppm could not render page {page}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

/// `command` with no stdin, its output captured, and killed if dropped.
fn quiet(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
}

fn size_text(bytes: u64) -> String {
    let (value, unit) = if bytes < 1024 * 1024 {
        (bytes as f64 / 1024.0, "KB")
    } else {
        (bytes as f64 / (1024.0 * 1024.0), "MB")
    };
    let text = format!("{value:.1}");
    format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
}
