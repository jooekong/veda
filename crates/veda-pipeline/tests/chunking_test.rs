use veda_pipeline::chunking::{is_binary_content, semantic_chunk, DEFAULT_SEMANTIC_MAX_TOKENS};

#[test]
fn binary_content_detection() {
    assert!(is_binary_content("\0\0\0\0"));
    assert!(is_binary_content("prefix\0suffix"));
    assert!(!is_binary_content("正常中文文本 with ASCII and émojis 🙂"));
    assert!(!is_binary_content(""));
}

#[test]
fn semantic_chunk_splits_on_markdown_headings() {
    let text = "intro line\n\n## Section A\n\nbody a\n\n### Nested\n\nbody nested\n\n## Section B\n\nbody b";
    let chunks = semantic_chunk(text, DEFAULT_SEMANTIC_MAX_TOKENS);

    assert!(chunks.len() >= 3);
    assert_eq!(chunks[0].index, 0);
    assert!(chunks[0].content.contains("intro line"));
    assert!(!chunks[0].content.contains("Section B"));

    let joined: String = chunks
        .iter()
        .map(|c| c.content.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n");
    assert!(joined.contains("Section A") || chunks.iter().any(|c| c.content.contains("Section A")));
    assert!(chunks.iter().any(|c| c.content.contains("Section B")));
}

#[test]
fn semantic_chunk_sliding_window_for_long_section() {
    let max_tokens = 32;
    let max_chars = max_tokens * 4;
    // Vary characters so consecutive windows are not accidentally identical strings.
    let filler: String = (0..(max_chars * 3))
        .map(|i| std::char::from_u32(b'a' as u32 + (i % 26) as u32).unwrap())
        .collect();
    let text = format!("## Only Section\n\n{filler}");
    let chunks = semantic_chunk(&text, max_tokens);

    assert!(
        chunks.len() > 1,
        "expected multiple windows for oversized section, got {}",
        chunks.len()
    );
    for w in chunks.windows(2) {
        assert_ne!(w[0].content, w[1].content);
    }
}

#[test]
fn semantic_chunk_empty_input() {
    assert!(semantic_chunk("", DEFAULT_SEMANTIC_MAX_TOKENS).is_empty());
}

/// Worst-case token estimate mirroring what the embed upstream faces:
/// ASCII ≈ 4 chars/token (BPE English rule of thumb), CJK and other
/// non-ASCII ≈ 1 token/char. Counted in quarter-tokens to stay integral.
fn estimated_tokens(s: &str) -> usize {
    let quarters: usize = s.chars().map(|c| if c.is_ascii() { 1 } else { 4 }).sum();
    quarters.div_ceil(4)
}

/// Reproduces the .89 production failure: the worker asks for ~2048-token
/// windows, but a flat ×4 chars-per-token budget hands dense CJK text
/// 8192-char windows ≈ 8192+ real tokens — past the upstream per-input cap
/// (text-embedding-v4 rejects >8192 tokens). Every window must stay within
/// the requested budget under the worst-case estimate.
#[test]
fn semantic_chunk_cjk_windows_respect_token_budget() {
    // ~20k dense CJK chars, no headings, no newlines — one giant section.
    let text: String = "鲜花水果生鲜配送优惠每日特价新品上架会员专享满减活动持续放送"
        .repeat(700);
    let chunks = semantic_chunk(&text, 2048);

    assert!(!chunks.is_empty());
    for c in &chunks {
        let est = estimated_tokens(&c.content);
        assert!(
            est <= 2048,
            "window estimated at {est} tokens exceeds the 2048 budget ({} chars)",
            c.content.chars().count()
        );
    }
}

/// ASCII behavior must not change with the weighted budget: 1 ASCII char =
/// ¼ token, so a 32-token budget still cuts 128-char windows exactly like
/// the old flat ×4 logic.
#[test]
fn semantic_chunk_ascii_window_size_unchanged() {
    let max_tokens = 32;
    let filler: String = (0..1000)
        .map(|i| std::char::from_u32(b'a' as u32 + (i % 26) as u32).unwrap())
        .collect();
    let chunks = semantic_chunk(&filler, max_tokens);

    assert!(chunks.len() > 1);
    assert_eq!(
        chunks[0].content.chars().count(),
        max_tokens * 4,
        "first ASCII window must still be max_tokens×4 chars"
    );
    for c in &chunks {
        assert!(estimated_tokens(&c.content) <= max_tokens);
    }
}
