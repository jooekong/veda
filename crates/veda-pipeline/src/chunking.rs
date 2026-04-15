use veda_types::{FileChunk, SemanticChunk};

/// Default `max_tokens` for [`semantic_chunk`] when callers want ~512-token sections.
pub const DEFAULT_SEMANTIC_MAX_TOKENS: usize = 512;

/// Rough UTF-8 character budget from a token limit (no tokenizer in-tree).
fn approx_max_chars(max_tokens: usize) -> usize {
    max_tokens.saturating_mul(4).max(1)
}

fn is_markdown_heading_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let bytes = trimmed.as_bytes();
    let mut hashes = 0usize;
    while hashes < bytes.len() && hashes < 6 && bytes[hashes] == b'#' {
        hashes += 1;
    }
    if hashes == 0 || hashes > 6 {
        return false;
    }
    match bytes.get(hashes) {
        Some(b' ') | Some(b'\t') => true,
        None => true,
        _ => false,
    }
}

fn split_by_headings(text: &str) -> Vec<String> {
    fn flush(buf: &mut Vec<&str>, out: &mut Vec<String>) {
        if buf.is_empty() {
            return;
        }
        out.push(buf.join("\n"));
        buf.clear();
    }

    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    let mut sections: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();

    for line in lines {
        if is_markdown_heading_line(line) {
            flush(&mut current, &mut sections);
        }
        current.push(line);
    }
    flush(&mut current, &mut sections);

    sections
}

fn sliding_windows(section: &str, max_chars: usize, overlap_chars: usize) -> Vec<String> {
    let chars: Vec<char> = section.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    if chars.len() <= max_chars {
        return vec![section.to_string()];
    }

    let overlap = overlap_chars.min(max_chars.saturating_sub(1));
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + max_chars).min(chars.len());
        out.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        let step = max_chars.saturating_sub(overlap).max(1);
        start = start.saturating_add(step);
        if start >= chars.len() {
            break;
        }
    }
    out
}

/// Split by markdown headings (`#` .. `######` at line start), then apply a char-based sliding
/// window when a section exceeds the approximate token budget.
pub fn semantic_chunk(text: &str, max_tokens: usize) -> Vec<SemanticChunk> {
    if text.is_empty() {
        return Vec::new();
    }

    let max_chars = approx_max_chars(max_tokens);
    let overlap_chars = (max_chars / 5).max(16);

    let mut out = Vec::new();
    let mut index: i32 = 0;

    for section in split_by_headings(text) {
        for piece in sliding_windows(&section, max_chars, overlap_chars) {
            out.push(SemanticChunk {
                index,
                content: piece,
            });
            index += 1;
        }
    }

    out
}

/// Count logical lines covered by a chunk (1-based line numbering for the next chunk).
fn lines_spanned_by_chunk(chunk: &str) -> i32 {
    if chunk.is_empty() {
        return 0;
    }
    let newlines = chunk.bytes().filter(|&b| b == b'\n').count() as i32;
    if chunk.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// Split large text at byte boundaries near `chunk_size`, backing up to the last newline when possible.
/// `start_line` is 1-based for the first line in each chunk.
///
/// `file_id` is empty; callers can map chunks and set `file_id` when persisting.
pub fn storage_chunk(content: &str, chunk_size: usize) -> Vec<FileChunk> {
    if content.is_empty() {
        return Vec::new();
    }

    let bytes = content.as_bytes();
    let mut offset = 0usize;
    let mut chunk_index: i32 = 0;
    let mut current_line: i32 = 1;
    let chunk_size = chunk_size.max(1);

    let mut chunks = Vec::new();

    while offset < bytes.len() {
        let mut end = (offset + chunk_size).min(bytes.len());
        if end < bytes.len() {
            if let Some(rel_nl) = bytes[offset..end].iter().rposition(|&b| b == b'\n') {
                end = offset + rel_nl + 1;
            }
        }

        let chunk_content = String::from_utf8_lossy(&bytes[offset..end]).into_owned();
        let line_span = lines_spanned_by_chunk(&chunk_content);

        chunks.push(FileChunk {
            file_id: String::new(),
            chunk_index,
            start_line: current_line,
            content: chunk_content,
        });

        current_line += line_span;
        chunk_index += 1;
        offset = end;
    }

    chunks
}
