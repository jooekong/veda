use veda_types::SemanticChunk;

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
