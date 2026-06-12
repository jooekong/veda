use veda_types::SemanticChunk;

/// Default `max_tokens` for [`semantic_chunk`] when callers want ~512-token sections.
pub const DEFAULT_SEMANTIC_MAX_TOKENS: usize = 512;

/// Binary payloads smuggled in as valid UTF-8 (e.g. an image whose bytes
/// happen to decode — in the wild: NUL-flooded PNG attachments synced into
/// an fs workspace). They are garbage to embed and tokenizers price control
/// bytes at ~1 token/char, so an "ASCII" window of NULs still blows the
/// upstream 8192-token/input cap. NUL never appears in legitimate text.
pub fn is_binary_content(text: &str) -> bool {
    text.contains('\0')
}

/// Per-char weight in quarter-tokens (no tokenizer in-tree). ASCII ≈ 4
/// chars/token — the BPE English rule of thumb; everything else (CJK,
/// emoji, …) is budgeted at 1 token/char. This is a worst-case ESTIMATE,
/// not a count: repetitive CJK compresses far below it, while rare CJK
/// chars can cost 2-3 tokens each — callers must keep headroom to the
/// upstream hard cap (the worker asks for 2048 against text-embedding-v4's
/// 8192-token/input limit, a 4× margin). The old flat ×4 chars-per-token
/// budget handed dense CJK 8192-char windows ≈ 8192+ real tokens, which the
/// upstream rejected with HTTP 400 — files then retried into dead letters.
fn char_quarters(c: char) -> usize {
    if c.is_ascii() {
        1
    } else {
        4
    }
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

fn sliding_windows(section: &str, budget_quarters: usize, overlap_quarters: usize) -> Vec<String> {
    let chars: Vec<char> = section.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    // prefix[i] = quarter-token weight of chars[..i]; strictly increasing.
    let mut prefix = Vec::with_capacity(chars.len() + 1);
    prefix.push(0usize);
    for &c in &chars {
        prefix.push(prefix[prefix.len() - 1] + char_quarters(c));
    }
    if *prefix.last().unwrap() <= budget_quarters {
        return vec![section.to_string()];
    }

    let overlap = overlap_quarters.min(budget_quarters.saturating_sub(1));
    let step_quarters = budget_quarters - overlap; // >= 1
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        // Furthest boundary keeping weight(start..end) <= budget. The clamp
        // guarantees >= 1 char of progress even if a single char outweighs
        // the whole budget.
        let end = prefix
            .partition_point(|&p| p <= prefix[start] + budget_quarters)
            .saturating_sub(1)
            .clamp(start + 1, chars.len());
        out.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        // First boundary with weight(start..i) >= step — the old fixed
        // `start += max_chars - overlap` stride, expressed by weight so the
        // overlap stays ~20% of the budget in any script.
        let next = prefix
            .partition_point(|&p| p < prefix[start] + step_quarters)
            .clamp(start + 1, chars.len());
        if next >= chars.len() {
            break;
        }
        start = next;
    }
    out
}

/// Split by markdown headings (`#` .. `######` at line start), then apply a
/// char-weighted sliding window when a section exceeds the approximate token
/// budget (ASCII ¼ token/char, everything else 1 — see [`char_quarters`]).
/// For pure-ASCII text the windows are byte-for-byte identical to the old
/// flat `max_tokens × 4` character budget.
pub fn semantic_chunk(text: &str, max_tokens: usize) -> Vec<SemanticChunk> {
    if text.is_empty() {
        return Vec::new();
    }

    let budget_quarters = max_tokens.saturating_mul(4).max(1);
    let overlap_quarters = (budget_quarters / 5).max(16);

    let mut out = Vec::new();
    let mut index: i32 = 0;

    for section in split_by_headings(text) {
        for piece in sliding_windows(&section, budget_quarters, overlap_quarters) {
            out.push(SemanticChunk {
                index,
                content: piece,
            });
            index += 1;
        }
    }

    out
}
