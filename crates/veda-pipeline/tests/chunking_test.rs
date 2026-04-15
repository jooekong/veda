use veda_pipeline::chunking::{semantic_chunk, storage_chunk, DEFAULT_SEMANTIC_MAX_TOKENS};

#[test]
fn semantic_chunk_splits_on_markdown_headings() {
    let text = "intro line\n\n## Section A\n\nbody a\n\n### Nested\n\nbody nested\n\n## Section B\n\nbody b";
    let chunks = semantic_chunk(text, DEFAULT_SEMANTIC_MAX_TOKENS);

    assert!(chunks.len() >= 3);
    assert_eq!(chunks[0].index, 0);
    assert!(chunks[0].content.contains("intro line"));
    assert!(!chunks[0].content.contains("Section B"));

    let joined: String = chunks.iter().map(|c| c.content.as_str()).collect::<Vec<_>>().join("\n---\n");
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
fn storage_chunk_aligns_to_newlines() {
    let line = format!("{}\n", "x".repeat(80));
    let mut body = String::new();
    for _ in 0..30 {
        body.push_str(&line);
    }
    let chunk_size = 250;
    let chunks = storage_chunk(&body, chunk_size);

    assert!(chunks.len() >= 2);
    for c in chunks.iter().take(chunks.len().saturating_sub(1)) {
        assert!(
            c.content.ends_with('\n'),
            "non-terminal chunks should end on a newline boundary, got chunk_index={}",
            c.chunk_index
        );
    }
}

#[test]
fn storage_chunk_start_line_tracking() {
    let content = "AAAAAAAA\nBBBBBBBB\n";
    let chunks = storage_chunk(content, 9);

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].start_line, 1);
    assert_eq!(chunks[0].content, "AAAAAAAA\n");
    assert_eq!(chunks[1].start_line, 2);
    assert_eq!(chunks[1].content, "BBBBBBBB\n");
}

#[test]
fn semantic_chunk_empty_input() {
    assert!(semantic_chunk("", DEFAULT_SEMANTIC_MAX_TOKENS).is_empty());
}

#[test]
fn storage_chunk_empty_input() {
    assert!(storage_chunk("", 256).is_empty());
}
