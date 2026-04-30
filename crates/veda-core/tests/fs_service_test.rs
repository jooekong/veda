mod mock_store;

use std::sync::Arc;
use veda_core::service::fs::FsService;
use veda_types::*;

fn make_service() -> (FsService, Arc<std::sync::Mutex<mock_store::MockState>>) {
    let store = mock_store::MockMetadataStore::new();
    let state = Arc::clone(&store.state);
    let svc = FsService::new(Arc::new(store));
    (svc, state)
}

#[tokio::test]
async fn write_and_read() {
    let (svc, _state) = make_service();
    let resp = svc
        .write_file("ws1", "/hello.txt", "hello world", None, None)
        .await
        .unwrap();
    assert!(!resp.content_unchanged);
    assert_eq!(resp.revision, 1);

    let content = svc.read_file("ws1", "/hello.txt").await.unwrap();
    assert_eq!(content, "hello world");
}

#[tokio::test]
async fn write_creates_parent_dirs() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/a/b/c.txt", "deep", None, None)
        .await
        .unwrap();

    let st = state.lock().unwrap();
    let dirs: Vec<&str> = st
        .dentries
        .iter()
        .filter(|d| d.is_dir)
        .map(|d| d.path.as_str())
        .collect();
    assert!(dirs.contains(&"/a"));
    assert!(dirs.contains(&"/a/b"));
}

#[tokio::test]
async fn dedup_same_content() {
    let (svc, _) = make_service();
    let r1 = svc
        .write_file("ws1", "/f.txt", "same", None, None)
        .await
        .unwrap();
    assert!(!r1.content_unchanged);
    assert_eq!(r1.revision, 1);

    let r2 = svc
        .write_file("ws1", "/f.txt", "same", None, None)
        .await
        .unwrap();
    assert!(r2.content_unchanged);
    assert_eq!(r2.revision, 1);
}

#[tokio::test]
async fn overwrite_bumps_revision() {
    let (svc, _) = make_service();
    let r1 = svc
        .write_file("ws1", "/f.txt", "v1", None, None)
        .await
        .unwrap();
    assert_eq!(r1.revision, 1);

    let r2 = svc
        .write_file("ws1", "/f.txt", "v2", None, None)
        .await
        .unwrap();
    assert!(!r2.content_unchanged);
    assert_eq!(r2.revision, 2);

    let content = svc.read_file("ws1", "/f.txt").await.unwrap();
    assert_eq!(content, "v2");
}

#[tokio::test]
async fn rapid_overwrite_dedupes_pending_chunksync() {
    // While a ChunkSync is still pending (worker hasn't completed it yet),
    // additional writes to the same file_id MUST NOT enqueue duplicate
    // ChunkSync events — the eventual single embed run will already see the
    // latest content.
    let (svc, state) = make_service();
    for v in ["v1", "v2", "v3", "v4", "v5"] {
        svc.write_file("ws1", "/f.txt", v, None, None)
            .await
            .unwrap();
    }

    let st = state.lock().unwrap();
    let sync_events: Vec<_> = st
        .outbox
        .iter()
        .filter(|e| e.event_type == OutboxEventType::ChunkSync)
        .collect();
    assert_eq!(
        sync_events.len(),
        1,
        "5 rapid writes should produce exactly 1 pending ChunkSync (got {})",
        sync_events.len()
    );
}

#[tokio::test]
async fn read_nonexistent_returns_not_found() {
    let (svc, _) = make_service();
    let result = svc.read_file("ws1", "/nope.txt").await;
    assert!(matches!(result, Err(VedaError::NotFound(_))));
}

#[tokio::test]
async fn write_to_dir_path_fails() {
    let (svc, _) = make_service();
    svc.mkdir("ws1", "/mydir").await.unwrap();
    let result = svc.write_file("ws1", "/mydir", "oops", None, None).await;
    assert!(matches!(result, Err(VedaError::AlreadyExists(_))));
}

#[tokio::test]
async fn delete_file() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/del.txt", "gone", None, None)
        .await
        .unwrap();
    svc.delete("ws1", "/del.txt").await.unwrap();

    let result = svc.read_file("ws1", "/del.txt").await;
    assert!(matches!(result, Err(VedaError::NotFound(_))));

    let st = state.lock().unwrap();
    let delete_events: Vec<_> = st
        .outbox
        .iter()
        .filter(|e| e.event_type == OutboxEventType::ChunkDelete)
        .collect();
    assert_eq!(delete_events.len(), 1);
}

#[tokio::test]
async fn delete_root_fails() {
    let (svc, _) = make_service();
    for path in ["/", "", "/.", "///"] {
        let result = svc.delete("ws1", path).await;
        match &result {
            Err(VedaError::InvalidPath(msg)) => {
                assert!(
                    msg.contains("cannot delete root"),
                    "path={path:?} msg={msg}"
                );
            }
            other => panic!("expected InvalidPath for {path:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn mkdir_and_list() {
    let (svc, _) = make_service();
    svc.mkdir("ws1", "/docs").await.unwrap();
    svc.write_file("ws1", "/docs/a.txt", "a", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/b.txt", "b", None, None)
        .await
        .unwrap();

    let entries = svc.list_dir("ws1", "/docs").await.unwrap();
    assert_eq!(entries.len(), 2);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
}

#[tokio::test]
async fn mkdir_idempotent() {
    let (svc, _) = make_service();
    svc.mkdir("ws1", "/foo").await.unwrap();
    svc.mkdir("ws1", "/foo").await.unwrap();
}

#[tokio::test]
async fn stat_file() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/s.txt", "stat me", None, None)
        .await
        .unwrap();

    let info = svc.stat("ws1", "/s.txt").await.unwrap();
    assert!(!info.is_dir);
    assert_eq!(info.path, "/s.txt");
    assert!(info.file_id.is_some());
    assert_eq!(info.size_bytes, Some(7));
    assert_eq!(info.revision, Some(1));
}

#[tokio::test]
async fn stat_dir() {
    let (svc, _) = make_service();
    svc.mkdir("ws1", "/mydir").await.unwrap();

    let info = svc.stat("ws1", "/mydir").await.unwrap();
    assert!(info.is_dir);
    assert!(info.file_id.is_none());
}

#[tokio::test]
async fn stat_root_virtual() {
    // Root has no dentry row; stat must still succeed and report a directory.
    // Regression: vfuse startup and root getattr 404'd before this.
    let (svc, _) = make_service();
    for path in ["/", "", "/.", "///"] {
        let info = svc.stat("ws1", path).await.unwrap();
        assert_eq!(info.path, "/", "input {path:?}");
        assert!(info.is_dir, "input {path:?}");
        assert!(info.file_id.is_none());
        assert!(info.size_bytes.is_none());
    }
}

#[tokio::test]
async fn copy_file_cow() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/orig.txt", "shared", None, None)
        .await
        .unwrap();
    let resp = svc
        .copy_file("ws1", "/orig.txt", "/copy.txt")
        .await
        .unwrap();
    assert!(resp.content_unchanged);

    let c1 = svc.read_file("ws1", "/orig.txt").await.unwrap();
    let c2 = svc.read_file("ws1", "/copy.txt").await.unwrap();
    assert_eq!(c1, c2);

    let st = state.lock().unwrap();
    let file = st.files.iter().find(|f| f.id == resp.file_id).unwrap();
    assert_eq!(file.ref_count, 2);
}

#[tokio::test]
async fn rename_file() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/old.txt", "move me", None, None)
        .await
        .unwrap();
    svc.rename("ws1", "/old.txt", "/new.txt").await.unwrap();

    let result = svc.read_file("ws1", "/old.txt").await;
    assert!(matches!(result, Err(VedaError::NotFound(_))));

    let content = svc.read_file("ws1", "/new.txt").await.unwrap();
    assert_eq!(content, "move me");
}

#[tokio::test]
async fn rename_to_existing_fails() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/a.txt", "a", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/b.txt", "b", None, None)
        .await
        .unwrap();
    let result = svc.rename("ws1", "/a.txt", "/b.txt").await;
    assert!(matches!(result, Err(VedaError::AlreadyExists(_))));
}

#[tokio::test]
async fn read_lines() {
    let (svc, _) = make_service();
    let content = "line1\nline2\nline3\nline4\nline5\n";
    svc.write_file("ws1", "/lines.txt", content, None, None)
        .await
        .unwrap();

    let lines = svc
        .read_file_lines("ws1", "/lines.txt", 2, 4)
        .await
        .unwrap();
    assert_eq!(lines, "line2\nline3\nline4");
}

#[tokio::test]
async fn read_lines_whole_file() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/f.txt", "a\nb\nc", None, None)
        .await
        .unwrap();

    let lines = svc.read_file_lines("ws1", "/f.txt", 1, 3).await.unwrap();
    assert_eq!(lines, "a\nb\nc");
}

#[tokio::test]
async fn read_lines_past_eof_returns_empty() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/f.txt", "a\nb\nc", None, None)
        .await
        .unwrap();

    let lines = svc.read_file_lines("ws1", "/f.txt", 10, 20).await.unwrap();
    assert_eq!(lines, "");
}

#[tokio::test]
async fn read_lines_clamps_end_to_eof() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/f.txt", "a\nb\nc", None, None)
        .await
        .unwrap();

    // end=100 is beyond EOF; should return through last line without error
    let lines = svc.read_file_lines("ws1", "/f.txt", 2, 100).await.unwrap();
    assert_eq!(lines, "b\nc");
}

#[tokio::test]
async fn read_lines_invalid_range_rejected() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/f.txt", "a\nb", None, None)
        .await
        .unwrap();

    assert!(matches!(
        svc.read_file_lines("ws1", "/f.txt", 0, 1).await,
        Err(VedaError::InvalidInput(_))
    ));
    assert!(matches!(
        svc.read_file_lines("ws1", "/f.txt", 5, 2).await,
        Err(VedaError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn read_lines_range_too_large_rejected() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/f.txt", "a", None, None)
        .await
        .unwrap();

    // 100_001 lines requested > MAX_LINE_RANGE (100_000)
    assert!(matches!(
        svc.read_file_lines("ws1", "/f.txt", 1, 100_001).await,
        Err(VedaError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn read_lines_on_directory_rejected() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/dir/f.txt", "x", None, None)
        .await
        .unwrap();

    assert!(matches!(
        svc.read_file_lines("ws1", "/dir", 1, 1).await,
        Err(VedaError::InvalidPath(_))
    ));
}

#[tokio::test]
async fn read_lines_nonexistent_rejected() {
    let (svc, _) = make_service();
    assert!(matches!(
        svc.read_file_lines("ws1", "/nope.txt", 1, 1).await,
        Err(VedaError::NotFound(_))
    ));
}

#[tokio::test]
async fn read_lines_chunked_across_chunks() {
    // Force chunked storage by exceeding INLINE_THRESHOLD (256 KB).
    // Each line is 100 bytes including '\n' → 3000 lines ≈ 300 KB → multiple chunks.
    let (svc, state) = make_service();
    let line_body = "x".repeat(99);
    let content: String = (0..3000)
        .map(|i| format!("{:04}{}\n", i, &line_body[4..]))
        .collect();
    svc.write_file("ws1", "/big.txt", &content, None, None)
        .await
        .unwrap();

    // verify storage is actually chunked
    {
        let st = state.lock().unwrap();
        let file = st.files.iter().find(|f| f.workspace_id == "ws1").unwrap();
        assert!(matches!(file.storage_type, StorageType::Chunked));
        // should have at least 2 chunks
        let chunk_count = st
            .file_chunks
            .iter()
            .filter(|c| c.file_id == file.id)
            .count();
        assert!(
            chunk_count >= 2,
            "expected multiple chunks, got {chunk_count}"
        );
    }

    // read 3 lines near the end, which must span into a later chunk
    let out = svc
        .read_file_lines("ws1", "/big.txt", 2800, 2802)
        .await
        .unwrap();
    let expected: String = (2799..2802)
        .map(|i| format!("{:04}{}", i, &line_body[4..]))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(out, expected);

    // read starting from the very first line
    let head = svc.read_file_lines("ws1", "/big.txt", 1, 2).await.unwrap();
    let expected_head: String = (0..2)
        .map(|i| format!("{:04}{}", i, &line_body[4..]))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(head, expected_head);
}

#[tokio::test]
async fn read_lines_chunked_oversized_single_line() {
    // A single line larger than CHUNK_SIZE (256 KB) must still be readable in full,
    // and must not break the `start_line` uniqueness relied on by the SQL optimizer.
    let (svc, state) = make_service();
    let long_line = "z".repeat(300 * 1024); // 300 KB, no '\n' inside
    let content = format!("{long_line}\nshort\n");
    svc.write_file("ws1", "/oversized.txt", &content, None, None)
        .await
        .unwrap();

    // verify storage went chunked and start_line values are unique across chunks
    {
        let st = state.lock().unwrap();
        let file = st.files.iter().find(|f| f.workspace_id == "ws1").unwrap();
        assert!(matches!(file.storage_type, StorageType::Chunked));
        let starts: Vec<i32> = st
            .file_chunks
            .iter()
            .filter(|c| c.file_id == file.id)
            .map(|c| c.start_line)
            .collect();
        let mut uniq = starts.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            starts.len(),
            uniq.len(),
            "chunk start_lines must be unique, got {starts:?}"
        );
    }

    // line 1 is the 300 KB line — must be returned fully, not a fragment
    let line1 = svc
        .read_file_lines("ws1", "/oversized.txt", 1, 1)
        .await
        .unwrap();
    assert_eq!(line1, long_line);

    // line 2 is "short"
    let line2 = svc
        .read_file_lines("ws1", "/oversized.txt", 2, 2)
        .await
        .unwrap();
    assert_eq!(line2, "short");
}

#[tokio::test]
async fn read_lines_chunked_fetches_only_overlapping_chunks() {
    // Verifies the SQL-semantics fix: requesting lines deep in the file should
    // only return the chunk containing them, not every chunk from index 0.
    let (svc, state) = make_service();
    let line_body = "y".repeat(99);
    let content: String = (0..3000)
        .map(|i| format!("{:04}{}\n", i, &line_body[4..]))
        .collect();
    svc.write_file("ws1", "/big.txt", &content, None, None)
        .await
        .unwrap();

    let file_id = {
        let st = state.lock().unwrap();
        st.files
            .iter()
            .find(|f| f.workspace_id == "ws1")
            .unwrap()
            .id
            .clone()
    };

    // directly probe the store with Some(start), Some(end) near the end
    let store = {
        // borrow the same state by wrapping a fresh store over it
        let shared = state.clone();
        mock_store::MockMetadataStore { state: shared }
    };
    use veda_core::store::MetadataStore;
    let all = store.get_file_chunks(&file_id, None, None).await.unwrap();
    let sliced = store
        .get_file_chunks(&file_id, Some(2800), Some(2802))
        .await
        .unwrap();
    assert!(
        sliced.len() < all.len(),
        "expected overlap-filter to prune chunks; sliced={}, all={}",
        sliced.len(),
        all.len()
    );
    // the first sliced chunk must cover line 2800
    let first = sliced.first().unwrap();
    assert!(first.start_line <= 2800);
}

#[tokio::test]
async fn fs_events_emitted() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/ev.txt", "hi", None, None)
        .await
        .unwrap();
    svc.delete("ws1", "/ev.txt").await.unwrap();

    let st = state.lock().unwrap();
    let types: Vec<FsEventType> = st.fs_events.iter().map(|e| e.event_type).collect();
    assert!(types.contains(&FsEventType::Create));
    assert!(types.contains(&FsEventType::Delete));
}

#[tokio::test]
async fn workspace_isolation() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/secret.txt", "ws1 data", None, None)
        .await
        .unwrap();

    let result = svc.read_file("ws2", "/secret.txt").await;
    assert!(matches!(result, Err(VedaError::NotFound(_))));
}

#[tokio::test]
async fn delete_dir_cleans_up_child_files() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/docs/a.txt", "aaa", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/b.txt", "bbb", None, None)
        .await
        .unwrap();

    svc.delete("ws1", "/docs").await.unwrap();

    let st = state.lock().unwrap();
    assert!(st.files.is_empty(), "child files should be cleaned up");
    assert!(
        st.file_contents.is_empty(),
        "child file contents should be cleaned up"
    );
    let delete_events: Vec<_> = st
        .outbox
        .iter()
        .filter(|e| e.event_type == OutboxEventType::ChunkDelete)
        .collect();
    assert_eq!(
        delete_events.len(),
        2,
        "should emit ChunkDelete for each child file"
    );
}

#[tokio::test]
async fn append_file_cow_isolation() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/orig.txt", "hello", None, None)
        .await
        .unwrap();
    svc.copy_file("ws1", "/orig.txt", "/copy.txt")
        .await
        .unwrap();

    // Append to one side should NOT affect the other
    svc.append_file("ws1", "/orig.txt", " world").await.unwrap();

    let orig = svc.read_file("ws1", "/orig.txt").await.unwrap();
    let copy = svc.read_file("ws1", "/copy.txt").await.unwrap();
    assert_eq!(orig, "hello world");
    assert_eq!(
        copy, "hello",
        "copy should be unchanged after appending to orig"
    );
}

#[tokio::test]
async fn append_creates_new_file() {
    let (svc, _) = make_service();
    let resp = svc
        .append_file("ws1", "/new.txt", "appended")
        .await
        .unwrap();
    assert_eq!(resp.revision, 1);
    assert!(!resp.content_unchanged);

    let content = svc.read_file("ws1", "/new.txt").await.unwrap();
    assert_eq!(content, "appended");
}

#[tokio::test]
async fn append_to_existing_file() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/log.txt", "line1\n", None, None)
        .await
        .unwrap();
    let resp = svc.append_file("ws1", "/log.txt", "line2\n").await.unwrap();
    assert_eq!(resp.revision, 2);
    assert!(!resp.content_unchanged);

    let content = svc.read_file("ws1", "/log.txt").await.unwrap();
    assert_eq!(content, "line1\nline2\n");
}

#[tokio::test]
async fn write_file_size_limit() {
    let (svc, _) = make_service();
    let big = "x".repeat(51 * 1024 * 1024);
    let result = svc.write_file("ws1", "/big.txt", &big, None, None).await;
    match &result {
        Err(VedaError::QuotaExceeded(msg)) => {
            assert!(msg.contains("50MB"), "error should mention limit: {msg}");
        }
        other => panic!("expected QuotaExceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn list_dir_root() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/a.txt", "a", None, None)
        .await
        .unwrap();
    svc.mkdir("ws1", "/subdir").await.unwrap();

    for path in ["/", "", "/.", "///"] {
        let entries = svc.list_dir("ws1", path).await.unwrap();
        assert_eq!(entries.len(), 2, "path={path:?}");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"), "path={path:?}");
        assert!(names.contains(&"subdir"), "path={path:?}");
    }
}

#[tokio::test]
async fn copy_overwrite_decrements_old_ref_count() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/a.txt", "content_a", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/b.txt", "content_b", None, None)
        .await
        .unwrap();

    let old_file_id = {
        let st = state.lock().unwrap();
        st.dentries
            .iter()
            .find(|d| d.path == "/b.txt")
            .unwrap()
            .file_id
            .clone()
            .unwrap()
    };

    svc.copy_file("ws1", "/a.txt", "/b.txt").await.unwrap();

    let st = state.lock().unwrap();
    assert!(
        !st.files.iter().any(|f| f.id == old_file_id),
        "old file should be cleaned up when ref_count reaches 0"
    );
}

#[tokio::test]
async fn read_file_range_returns_partial_content() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/range.txt", "Hello, World!", None, None)
        .await
        .unwrap();

    let (data, total) = svc
        .read_file_range("ws1", "/range.txt", 0, 5)
        .await
        .unwrap();
    assert_eq!(total, 13);
    assert_eq!(data, b"Hello");

    let (data, _) = svc
        .read_file_range("ws1", "/range.txt", 7, 6)
        .await
        .unwrap();
    assert_eq!(data, b"World!");

    // offset beyond file size returns empty
    let (data, _) = svc
        .read_file_range("ws1", "/range.txt", 100, 10)
        .await
        .unwrap();
    assert!(data.is_empty());
}

#[tokio::test]
async fn if_none_match_skips_rewrite() {
    // When the client pre-hashes the body and the digest matches the server's
    // stored checksum, the upload short-circuits with content_unchanged=true
    // and does NOT advance the revision.
    let (svc, state) = make_service();
    svc.write_file("ws1", "/x.txt", "hello", None, None)
        .await
        .unwrap();
    let stored_sha = {
        let st = state.lock().unwrap();
        st.files[0].checksum_sha256.clone()
    };

    let resp = svc
        .write_file("ws1", "/x.txt", "hello", None, Some(&stored_sha))
        .await
        .unwrap();
    assert!(resp.content_unchanged, "matching sha must short-circuit");
    assert_eq!(resp.revision, 1, "revision must not bump");
}

#[tokio::test]
async fn if_none_match_does_not_fire_on_different_path() {
    // Header applies to the target path only — a hash that matches some other
    // file must not bypass the write.
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/a.txt", "hello", None, None)
        .await
        .unwrap();

    // Using sha256("hello") against a path that doesn't exist yet must not
    // short-circuit — a new file must be created.
    let sha_hello = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    let resp = svc
        .write_file("ws1", "/b.txt", "hello", None, Some(sha_hello))
        .await
        .unwrap();
    assert!(!resp.content_unchanged);
    assert_eq!(resp.revision, 1);
}

#[tokio::test]
async fn incremental_append_preserves_prefix_chunks() {
    // For a chunked file, incremental append must rewrite only the last
    // chunk (+ any new chunks) and leave earlier chunks byte-identical —
    // which is the whole point of the incremental path.
    let (svc, state) = make_service();
    // 3 chunks of ~100 KB each — big enough to chunk, small enough for tests
    let line = "x".repeat(99);
    let block: String = (0..1000).map(|_| format!("{line}\n")).collect();
    let content = block.repeat(3); // ≈ 300 KB → chunked
    svc.write_file("ws1", "/big.txt", &content, None, None)
        .await
        .unwrap();

    // Snapshot chunk_sha256 for all chunks except the last one.
    let (file_id, prefix_before) = {
        let st = state.lock().unwrap();
        let fid = st.files[0].id.clone();
        let mut chunks: Vec<FileChunk> = st
            .file_chunks
            .iter()
            .filter(|c| c.file_id == fid)
            .cloned()
            .collect();
        chunks.sort_by_key(|c| c.chunk_index);
        let last_idx = chunks.last().unwrap().chunk_index;
        let prefix: Vec<(i32, String)> = chunks
            .iter()
            .filter(|c| c.chunk_index < last_idx)
            .map(|c| (c.chunk_index, c.chunk_sha256.clone()))
            .collect();
        (fid, prefix)
    };
    assert!(
        prefix_before.len() >= 1,
        "test needs at least 2 chunks to exercise the prefix"
    );

    // Append a small amount of new content.
    svc.append_file("ws1", "/big.txt", "TAIL\n").await.unwrap();

    // Prefix chunks must still match — same chunk_index, same chunk_sha256.
    let prefix_after: Vec<(i32, String)> = {
        let st = state.lock().unwrap();
        let mut chunks: Vec<FileChunk> = st
            .file_chunks
            .iter()
            .filter(|c| c.file_id == file_id && c.chunk_index < prefix_before.len() as i32)
            .cloned()
            .collect();
        chunks.sort_by_key(|c| c.chunk_index);
        chunks
            .into_iter()
            .map(|c| (c.chunk_index, c.chunk_sha256))
            .collect()
    };
    assert_eq!(
        prefix_before, prefix_after,
        "prefix chunks must be untouched by incremental append"
    );

    // And a round-trip read still returns the correct content.
    let roundtrip = svc.read_file("ws1", "/big.txt").await.unwrap();
    let mut expected = content.clone();
    expected.push_str("TAIL\n");
    assert_eq!(roundtrip, expected);
}

#[tokio::test]
async fn read_file_range_chunked_returns_correct_slice_from_middle() {
    // 3000 lines × 100 bytes = 300 KB → forced into chunked storage. Read a
    // 1KB byte range from the middle and validate it matches the original
    // content slice. This exercises the cumulative-byte-offset overlap walk.
    let (svc, state) = make_service();
    let line_body = "x".repeat(99);
    let content: String = (0..3000)
        .map(|i| format!("{:04}{}\n", i, &line_body[4..]))
        .collect();
    svc.write_file("ws1", "/big.txt", &content, None, None)
        .await
        .unwrap();
    {
        let st = state.lock().unwrap();
        let file = st.files.iter().find(|f| f.workspace_id == "ws1").unwrap();
        assert!(matches!(file.storage_type, StorageType::Chunked));
    }
    let total = content.len() as u64;
    let offset = total / 2;
    let length: u64 = 1024;
    let (data, reported_total) = svc
        .read_file_range("ws1", "/big.txt", offset, length)
        .await
        .unwrap();
    assert_eq!(reported_total, total);
    let expected = &content.as_bytes()[offset as usize..(offset + length) as usize];
    assert_eq!(data.as_slice(), expected);
}

#[tokio::test]
async fn read_file_range_chunked_handles_offset_past_eof() {
    let (svc, _state) = make_service();
    let line_body = "x".repeat(99);
    let content: String = (0..3000)
        .map(|i| format!("{:04}{}\n", i, &line_body[4..]))
        .collect();
    svc.write_file("ws1", "/big.txt", &content, None, None)
        .await
        .unwrap();
    let total = content.len() as u64;
    let (data, reported_total) = svc
        .read_file_range("ws1", "/big.txt", total + 100, 64)
        .await
        .unwrap();
    assert!(data.is_empty(), "past EOF must yield empty");
    assert_eq!(reported_total, total);
}

#[tokio::test]
async fn read_file_range_chunked_clamps_length_to_eof() {
    let (svc, _state) = make_service();
    let line_body = "x".repeat(99);
    let content: String = (0..3000)
        .map(|i| format!("{:04}{}\n", i, &line_body[4..]))
        .collect();
    svc.write_file("ws1", "/big.txt", &content, None, None)
        .await
        .unwrap();
    let total = content.len() as u64;
    // Request more bytes than the tail can provide — must clamp, not panic.
    let (data, reported_total) = svc
        .read_file_range("ws1", "/big.txt", total - 50, 1024)
        .await
        .unwrap();
    assert_eq!(reported_total, total);
    assert_eq!(data.as_slice(), &content.as_bytes()[(total - 50) as usize..]);
}

#[tokio::test]
async fn stat_uses_file_updated_at_not_dentry_updated_at_after_overwrite() {
    // Regression: dentry.updated_at only advances on rename/relink, but FUSE
    // mtime needs to bump on every overwrite. stat() now sources updated_at
    // from the file row (schema's ON UPDATE CURRENT_TIMESTAMP keeps it fresh).
    let (svc, state) = make_service();
    svc.write_file("ws1", "/m.txt", "v1", None, None)
        .await
        .unwrap();
    let info_before = svc.stat("ws1", "/m.txt").await.unwrap();
    // Stash dentry.updated_at and pin it forward of where the file's update
    // will land — this proves stat() doesn't trust dentry.updated_at.
    let pinned_dentry_time = info_before.updated_at + chrono::Duration::seconds(3600);
    {
        let mut st = state.lock().unwrap();
        let de = st
            .dentries
            .iter_mut()
            .find(|d| d.workspace_id == "ws1" && d.path == "/m.txt")
            .unwrap();
        de.updated_at = pinned_dentry_time;
    }
    // Now overwrite. The mock bumps file.updated_at to "now"; the dentry row
    // is intentionally pinned 1h in the future. If stat() returned dentry's
    // time we'd see pinned_dentry_time; we want the file's fresh time.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    svc.write_file("ws1", "/m.txt", "v2", Some(1), None)
        .await
        .unwrap();
    let info_after = svc.stat("ws1", "/m.txt").await.unwrap();
    assert!(
        info_after.updated_at < pinned_dentry_time,
        "stat must source updated_at from file (got dentry's pinned future time)"
    );
    assert!(
        info_after.updated_at >= info_before.updated_at,
        "stat updated_at must advance after content overwrite"
    );
}

#[tokio::test]
async fn read_lines_chunked_no_trailing_newline_returns_last_line() {
    // Regression for the W4.2 line-count off-by-one: when the file has no
    // trailing newline, the final logical line satisfies
    // `start_line + line_count == last_line`. Using `>` would drop it; we
    // use `>=` so this read returns the actual last line.
    let (svc, state) = make_service();
    let body = "x".repeat(95);
    let mut content: String = (0..3000)
        .map(|i| format!("{:04}{}\n", i, &body))
        .collect();
    // Force "no trailing newline" — strip the final '\n'.
    assert_eq!(content.pop(), Some('\n'));
    svc.write_file("ws1", "/big.txt", &content, None, None)
        .await
        .unwrap();
    {
        let st = state.lock().unwrap();
        let file = st.files.iter().find(|f| f.workspace_id == "ws1").unwrap();
        assert!(matches!(file.storage_type, StorageType::Chunked));
    }
    // 3000 lines total. Read line 3000 (the final, un-newline-terminated line).
    let last_line = svc
        .read_file_lines("ws1", "/big.txt", 3000, 3000)
        .await
        .unwrap();
    let expected = format!("2999{}", &body);
    assert_eq!(last_line, expected, "last line must round-trip");
}

#[tokio::test]
async fn get_file_chunks_returns_empty_when_range_exceeds_eof() {
    // Regression for W4.2 SQL boundary: the old implementation returned the
    // last chunk for any start_line past EOF. Mock now mirrors that fix.
    use veda_core::store::MetadataStore;
    let (svc, state) = make_service();
    let line_body = "x".repeat(99);
    let content: String = (0..3000)
        .map(|i| format!("{:04}{}\n", i, &line_body[4..]))
        .collect();
    svc.write_file("ws1", "/big.txt", &content, None, None)
        .await
        .unwrap();
    let (store, file_id) = {
        let st = state.lock().unwrap();
        let file = st.files.iter().find(|f| f.workspace_id == "ws1").unwrap();
        (Arc::clone(&state), file.id.clone())
    };
    let mock = mock_store::MockMetadataStore { state: store };
    let chunks = mock
        .get_file_chunks(&file_id, Some(10_000), Some(10_100))
        .await
        .unwrap();
    assert!(
        chunks.is_empty(),
        "querying past EOF must return empty, got {} chunks",
        chunks.len()
    );
}

#[tokio::test]
async fn events_min_id_returns_none_for_empty_workspace() {
    let (svc, _state) = make_service();
    let v = svc.events_min_id("nope").await.unwrap();
    assert_eq!(v, None);
}

#[tokio::test]
async fn events_min_id_after_writes() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/a.txt", "1", None, None).await.unwrap();
    svc.write_file("ws1", "/b.txt", "2", None, None).await.unwrap();
    let min = svc.events_min_id("ws1").await.unwrap();
    assert!(min.is_some(), "writes must produce events");
    let events = svc.query_events("ws1", 0, 100).await.unwrap();
    assert_eq!(min, events.iter().map(|e| e.id).min());
}

#[tokio::test]
async fn prune_events_older_than_clears_old_rows() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/a.txt", "1", None, None).await.unwrap();
    // Backdate the inserted event so it falls outside the retention window.
    {
        let mut st = state.lock().unwrap();
        for e in st.fs_events.iter_mut() {
            e.created_at = chrono::Utc::now() - chrono::Duration::days(30);
        }
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    let n = svc.prune_events_older_than(cutoff).await.unwrap();
    assert!(n > 0, "expected at least one deletion, got {n}");
    assert!(svc.query_events("ws1", 0, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn query_events_filtered_by_path_prefix() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/docs/a.md", "1", None, None).await.unwrap();
    svc.write_file("ws1", "/src/b.rs", "2", None, None).await.unwrap();
    svc.write_file("ws1", "/docs/c.md", "3", None, None).await.unwrap();
    let events = svc
        .query_events_filtered("ws1", 0, Some("/docs"), 100)
        .await
        .unwrap();
    assert_eq!(events.len(), 2, "only /docs/* events should match");
    assert!(events.iter().all(|e| e.path.starts_with("/docs")));
}

#[tokio::test]
async fn query_events_filtered_does_not_leak_into_sibling_dirs() {
    // Subtree boundary: a `/docs` prefix must not match `/docs_alt/*`.
    // The naive `LIKE 'prefix%'` shape fails this; the fix uses
    // `path = prefix OR path LIKE 'prefix/%'`.
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/docs/a.md", "1", None, None).await.unwrap();
    svc.write_file("ws1", "/docs_alt/b.md", "2", None, None).await.unwrap();
    svc.write_file("ws1", "/docs/c.md", "3", None, None).await.unwrap();
    let events = svc
        .query_events_filtered("ws1", 0, Some("/docs"), 100)
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        2,
        "/docs prefix must not match /docs_alt/*; got: {:?}",
        events.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
    assert!(events.iter().all(|e| e.path.starts_with("/docs/")));

    // Trailing slash variant should be canonicalized to the same result.
    let events_with_slash = svc
        .query_events_filtered("ws1", 0, Some("/docs/"), 100)
        .await
        .unwrap();
    assert_eq!(events_with_slash.len(), 2);
}
