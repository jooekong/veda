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
async fn blob_roundtrip_lossless() {
    let (svc, state) = make_service();
    // ZIP/jar magic (PK\x03\x04) + NUL + high bytes: not valid UTF-8.
    let data = b"PK\x03\x04\x00\x01\xff\xfe\0jar\0bytes\xc0here".to_vec();
    let resp = svc
        .write_blob("ws1", "/app.jar", data.clone(), None)
        .await
        .unwrap();
    assert!(!resp.content_unchanged);

    // Bytes come back identical, with a non-text mime.
    let (bytes, mime) = svc.read_file_raw("ws1", "/app.jar").await.unwrap();
    assert_eq!(bytes, data);
    assert_ne!(mime, "text/plain");

    let st = state.lock().unwrap();
    let f = st.files.iter().find(|f| f.id == resp.file_id).unwrap();
    assert_eq!(f.storage_type, StorageType::Blob);
    assert_eq!(f.source_type, SourceType::Binary);
}

#[tokio::test]
async fn blob_pdf_detected_enqueues_extract() {
    let (svc, state) = make_service();
    let data = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\nbinary pdf body".to_vec();
    let resp = svc.write_blob("ws1", "/doc.pdf", data, None).await.unwrap();

    let st = state.lock().unwrap();
    let f = st.files.iter().find(|f| f.id == resp.file_id).unwrap();
    assert_eq!(f.mime_type, "application/pdf");
    assert_eq!(f.source_type, SourceType::Pdf);
    // PDF enqueues ExtractSync (text-extract → embed), not ChunkSync.
    assert!(st
        .outbox
        .iter()
        .any(|e| e.event_type == OutboxEventType::ExtractSync));
    assert!(!st
        .outbox
        .iter()
        .any(|e| e.event_type == OutboxEventType::ChunkSync));
}

/// Real .docx / .doc fixtures shared with veda-pipeline's extractor tests,
/// so the infer-based mime detection runs against genuine files.
const DOCX_BYTES: &[u8] =
    include_bytes!("../../veda-pipeline/tests/fixtures/veda_word_e2e.docx");
const DOC_BYTES: &[u8] = include_bytes!("../../veda-pipeline/tests/fixtures/veda_word_e2e.doc");

#[tokio::test]
async fn blob_word_detected_enqueues_extract() {
    for (path, data, mime) in [
        ("/a.docx", DOCX_BYTES, MIME_DOCX),
        // Genuine MS Word writer → infer's sub-type probe succeeds.
        (
            "/b.doc",
            include_bytes!("../../veda-pipeline/tests/fixtures/msword_sample.doc") as &[u8],
            MIME_DOC,
        ),
        // Spec-violating writer (macOS textutil): infer can't open the
        // container so it reports generic OLE — the WordDocument-stream
        // sniff still proves it Word and normalizes the mime.
        ("/c.doc", DOC_BYTES, MIME_DOC),
    ] {
        let (svc, state) = make_service();
        let resp = svc.write_blob("ws1", path, data.to_vec(), None).await.unwrap();
        let st = state.lock().unwrap();
        let f = st.files.iter().find(|f| f.id == resp.file_id).unwrap();
        assert_eq!(f.mime_type, mime, "{path}");
        assert_eq!(f.source_type, SourceType::Word, "{path}");
        assert!(st.outbox.iter().any(|e| e.event_type == OutboxEventType::ExtractSync));
        assert!(!st.outbox.iter().any(|e| e.event_type == OutboxEventType::ChunkSync));
    }
}

#[tokio::test]
async fn blob_non_word_ole_stays_binary() {
    // CFB magic + no "WordDocument" stream name anywhere: an OLE container
    // that is not a Word file (xls/ppt/msi). Must NOT be routed to the
    // extractor — stored unindexed under the generic OLE mime.
    let mut data = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    data.extend_from_slice(&[0u8; 1024]);
    let (svc, state) = make_service();
    let resp = svc.write_blob("ws1", "/book.xls", data, None).await.unwrap();
    let st = state.lock().unwrap();
    let f = st.files.iter().find(|f| f.id == resp.file_id).unwrap();
    assert_eq!(f.mime_type, MIME_OLE_STORAGE);
    assert_eq!(f.source_type, SourceType::Binary);
    assert!(!st.outbox.iter().any(|e| e.event_type == OutboxEventType::ExtractSync));
}

/// Insert a stored extract for `file_id`, keyed to the given source hash.
fn seed_extract(
    state: &Arc<std::sync::Mutex<mock_store::MockState>>,
    file_id: &str,
    content: &str,
    source_sha256: &str,
) {
    state.lock().unwrap().file_extracts.insert(
        file_id.to_string(),
        FileExtract {
            file_id: file_id.to_string(),
            content: content.to_string(),
            source_sha256: source_sha256.to_string(),
        },
    );
}

#[tokio::test]
async fn blob_word_read_serves_fresh_extract_only() {
    let (svc, state) = make_service();
    let resp = svc.write_blob("ws1", "/w.docx", DOCX_BYTES.to_vec(), None).await.unwrap();

    // No extract row yet → "extraction pending", not the generic binary error.
    let err = svc.read_file("ws1", "/w.docx").await.unwrap_err();
    assert!(matches!(err, VedaError::InvalidInput(ref m) if m.contains("提取")), "err: {err}");

    // Fresh extract (sha matches the blob) → served as text.
    let sha = {
        let st = state.lock().unwrap();
        st.files.iter().find(|f| f.id == resp.file_id).unwrap().checksum_sha256.clone()
    };
    seed_extract(&state, &resp.file_id, "extracted word text", &sha);
    assert_eq!(svc.read_file("ws1", "/w.docx").await.unwrap(), "extracted word text");

    // Stale extract (sha from an older blob revision) → treated as absent,
    // never served.
    seed_extract(&state, &resp.file_id, "stale text", "deadbeef");
    let err = svc.read_file("ws1", "/w.docx").await.unwrap_err();
    assert!(matches!(err, VedaError::InvalidInput(_)), "err: {err}");
}

#[tokio::test]
async fn blob_word_preview_serves_extract_as_text() {
    let (svc, state) = make_service();
    let resp = svc.write_blob("ws1", "/w.docx", DOCX_BYTES.to_vec(), None).await.unwrap();
    let sha = {
        let st = state.lock().unwrap();
        st.files.iter().find(|f| f.id == resp.file_id).unwrap().checksum_sha256.clone()
    };
    seed_extract(&state, &resp.file_id, "提取的中文文本内容", &sha);

    let p = svc.read_file_preview("ws1", "/w.docx", 1024).await.unwrap();
    assert!(!p.is_binary);
    assert_eq!(p.content, "提取的中文文本内容");
    assert!(!p.truncated);

    // Tiny max_bytes: truncates on a char boundary instead of panicking
    // mid-UTF-8.
    let p = svc.read_file_preview("ws1", "/w.docx", 4).await.unwrap();
    assert!(p.truncated);
    assert_eq!(p.content, "提");

    // Without a fresh extract the preview falls back to the binary notice.
    seed_extract(&state, &resp.file_id, "stale", "deadbeef");
    let p = svc.read_file_preview("ws1", "/w.docx", 1024).await.unwrap();
    assert!(p.is_binary);
    assert!(p.content.contains("Word"), "content: {}", p.content);
}

#[tokio::test]
async fn blob_word_line_reads_page_extracted_text() {
    let (svc, state) = make_service();
    let resp = svc.write_blob("ws1", "/w.docx", DOCX_BYTES.to_vec(), None).await.unwrap();

    // No extract yet → clear "pending" error, not the generic binary one.
    let err = svc.read_file_lines("ws1", "/w.docx", 1, 2).await.unwrap_err();
    assert!(matches!(err, VedaError::InvalidInput(ref m) if m.contains("提取")), "err: {err}");

    let sha = {
        let st = state.lock().unwrap();
        st.files.iter().find(|f| f.id == resp.file_id).unwrap().checksum_sha256.clone()
    };
    seed_extract(&state, &resp.file_id, "line one\nline two\nline three\nline four", &sha);

    // 1-indexed inclusive window over the extracted text.
    assert_eq!(svc.read_file_lines("ws1", "/w.docx", 2, 3).await.unwrap(), "line two\nline three");
    // End past EOF clamps; start past EOF yields empty.
    assert_eq!(svc.read_file_lines("ws1", "/w.docx", 4, 99).await.unwrap(), "line four");
    assert_eq!(svc.read_file_lines("ws1", "/w.docx", 50, 60).await.unwrap(), "");

    // Non-extractable blobs keep the original refusal.
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRpng".to_vec();
    svc.write_blob("ws1", "/p.png", png, None).await.unwrap();
    let err = svc.read_file_lines("ws1", "/p.png", 1, 2).await.unwrap_err();
    assert!(matches!(err, VedaError::InvalidInput(ref m) if m.contains("binary")), "err: {err}");
}

#[tokio::test]
async fn delete_word_blob_purges_extract() {
    let (svc, state) = make_service();
    let resp = svc.write_blob("ws1", "/w.docx", DOCX_BYTES.to_vec(), None).await.unwrap();
    seed_extract(&state, &resp.file_id, "text", "sha");
    svc.delete("ws1", "/w.docx").await.unwrap();
    assert!(state.lock().unwrap().file_extracts.is_empty(), "extract row must die with the file");
}

#[tokio::test]
async fn overwrite_word_blob_purges_stale_extract() {
    let (svc, state) = make_service();
    let resp = svc.write_blob("ws1", "/w.docx", DOCX_BYTES.to_vec(), None).await.unwrap();
    seed_extract(&state, &resp.file_id, "old text", "sha-old");

    // Rewrite the same path with different binary content (a PNG): the old
    // extract must be dropped in the same transaction, not survive as junk.
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRpng".to_vec();
    svc.write_blob("ws1", "/w.docx", png, None).await.unwrap();
    assert!(
        state.lock().unwrap().file_extracts.is_empty(),
        "rewrite must purge the previous revision's extract"
    );
}

#[tokio::test]
async fn blob_image_stored_not_indexed() {
    let (svc, state) = make_service();
    // PNG magic.
    let data = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRpng".to_vec();
    let resp = svc.write_blob("ws1", "/pic.png", data, None).await.unwrap();

    let st = state.lock().unwrap();
    let f = st.files.iter().find(|f| f.id == resp.file_id).unwrap();
    assert_eq!(f.source_type, SourceType::Image);
    assert!(f.mime_type.starts_with("image/"));
    // Images are stored but not indexed: no extract/chunk events.
    assert!(!st.outbox.iter().any(|e| matches!(
        e.event_type,
        OutboxEventType::ChunkSync | OutboxEventType::ExtractSync
    )));
}

#[tokio::test]
async fn blob_rejected_by_text_read() {
    let (svc, _state) = make_service();
    let data = b"\x89PNG\r\n\x1a\nbinary".to_vec();
    svc.write_blob("ws1", "/pic.png", data, None).await.unwrap();
    // The text read API refuses a binary file (callers must use read_file_raw).
    let err = svc.read_file("ws1", "/pic.png").await.unwrap_err();
    assert!(matches!(err, VedaError::InvalidInput(_)));
}

#[tokio::test]
async fn read_file_preview_text_vs_binary() {
    let (svc, _) = make_service();
    // Text file: real content, not flagged binary.
    svc.write_file("ws1", "/a.txt", "hello world", None, None)
        .await
        .unwrap();
    let p = svc
        .read_file_preview("ws1", "/a.txt", 256 * 1024)
        .await
        .unwrap();
    assert!(!p.is_binary);
    assert_eq!(p.content, "hello world");
    assert_eq!(p.size, 11);
    assert_eq!(p.mime_type, "text/plain");
    assert!(!p.truncated);

    // Binary (blob) file: empty content, is_binary=true, real mime/size —
    // no garbled UTF-8-lossy bytes.
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRpng".to_vec();
    let n = png.len() as u64;
    svc.write_blob("ws1", "/pic.png", png, None).await.unwrap();
    let pb = svc
        .read_file_preview("ws1", "/pic.png", 256 * 1024)
        .await
        .unwrap();
    assert!(pb.is_binary);
    assert!(pb.mime_type.starts_with("image/"));
    // content carries a localized unsupported-preview message with a friendly
    // kind ("图片" for images, not the raw mime).
    assert_eq!(pb.content, "暂不支持预览该格式（图片）");
    assert_eq!(pb.size, n);
    assert!(!pb.truncated);

    // Truncated text: content capped at max_bytes (tiny cap forces it).
    let pt = svc.read_file_preview("ws1", "/a.txt", 5).await.unwrap();
    assert!(pt.truncated);
    assert_eq!(pt.size, 11);
    assert_eq!(pt.content, "hello");
    assert!(!pt.is_binary);
}

#[tokio::test]
async fn text_overwritten_by_blob_purges_stale_index() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/f", "hello text", None, None)
        .await
        .unwrap();
    let data = b"\x89PNG\r\n\x1a\nbinary".to_vec();
    svc.write_blob("ws1", "/f", data.clone(), None)
        .await
        .unwrap();

    // Now reads back as the blob bytes with an image mime.
    let (bytes, mime) = svc.read_file_raw("ws1", "/f").await.unwrap();
    assert_eq!(bytes, data);
    assert!(mime.starts_with("image/"));

    let st = state.lock().unwrap();
    // The text→image type change enqueues a ChunkDelete to purge stale vectors.
    assert!(st
        .outbox
        .iter()
        .any(|e| e.event_type == OutboxEventType::ChunkDelete));
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
async fn list_dir_carries_mime_and_size() {
    let (svc, _) = make_service();
    svc.mkdir("ws1", "/docs").await.unwrap();
    svc.mkdir("ws1", "/docs/sub").await.unwrap();
    svc.write_file("ws1", "/docs/a.txt", "hello", None, None)
        .await
        .unwrap();

    let entries = svc.list_dir("ws1", "/docs").await.unwrap();
    // File entries now carry real metadata instead of null.
    let a = entries.iter().find(|e| e.name == "a.txt").unwrap();
    assert!(
        a.mime_type.is_some(),
        "mime_type should be populated, got {:?}",
        a.mime_type
    );
    assert_eq!(a.size_bytes, Some(5));
    // Directory entries have no file_id and stay null.
    let sub = entries.iter().find(|e| e.name == "sub").unwrap();
    assert!(sub.is_dir);
    assert_eq!(sub.mime_type, None);
    assert_eq!(sub.size_bytes, None);
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
async fn read_lines_open_ended_window_clamps_to_eof() {
    // The fs route turns an open-ended `start:` (CLI `--range "3:"`) into the
    // bounded window `start ..= start + MAX_LINE_RANGE - 1` (routes/fs.rs),
    // which the service then clamps to EOF — yielding start..end-of-file
    // without tripping the range-too-large cap. Pins that contract with the
    // real constant so the two stay in lockstep.
    use veda_core::service::fs::MAX_LINE_RANGE;
    let (svc, _) = make_service();
    svc.write_file("ws1", "/f.txt", "a\nb\nc\nd\ne", None, None)
        .await
        .unwrap();

    let end = 3i32.saturating_add(MAX_LINE_RANGE - 1);
    let lines = svc.read_file_lines("ws1", "/f.txt", 3, end).await.unwrap();
    assert_eq!(lines, "c\nd\ne");

    let end1 = 1i32.saturating_add(MAX_LINE_RANGE - 1);
    let all = svc.read_file_lines("ws1", "/f.txt", 1, end1).await.unwrap();
    assert_eq!(all, "a\nb\nc\nd\ne");
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
async fn append_missing_inline_content_row_fails() {
    let (svc, state) = make_service();
    let resp = svc
        .write_file("ws1", "/log.txt", "line1\n", None, None)
        .await
        .unwrap();
    // Corrupt state: file row survives, its inline content row is gone.
    // The append full-rewrite path must fail loudly instead of treating the
    // file as empty and replacing it with just the appended bytes.
    state.lock().unwrap().file_contents.remove(&resp.file_id);

    let err = svc
        .append_file("ws1", "/log.txt", "line2\n")
        .await
        .expect_err("append must not rewrite a file whose content row is missing");
    assert!(matches!(err, VedaError::NotFound(_)), "err: {err}");
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
    // `ensure_parents` now emits a Create event for /docs the first
    // time it's auto-created, so the prefix match returns the dir
    // itself + the two file events = 3.
    assert_eq!(events.len(), 3, "only /docs and /docs/* events should match");
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
    // `ensure_parents` emits a Create event for /docs (and separately
    // for /docs_alt); the filter must include /docs itself + /docs/a
    // + /docs/c, but never anything under /docs_alt.
    assert_eq!(
        events.len(),
        3,
        "/docs prefix must not match /docs_alt/*; got: {:?}",
        events.iter().map(|e| &e.path).collect::<Vec<_>>()
    );
    assert!(events.iter().all(|e| e.path == "/docs" || e.path.starts_with("/docs/")));

    // Trailing slash variant should be canonicalized to the same result.
    let events_with_slash = svc
        .query_events_filtered("ws1", 0, Some("/docs/"), 100)
        .await
        .unwrap();
    assert_eq!(events_with_slash.len(), 3);
}

/// Reserved-name sweep: every mutating call site that creates or moves
/// content into a path MUST reject the reserved sidecar basenames
/// (`.abstract`, `.overview`). The path-layer helper is unit-tested in
/// `crates/veda-core/src/path.rs`, but the *wiring* — that each public
/// FsService entry point actually calls it — only shows up here.
///
/// Drives both reserved names against all five mutating entry points so
/// a future contributor can't silently miss a call site by adding a new
/// reserved name (just extend `RESERVED` below) or adding a new mutator
/// (add it to the sweep here).
#[tokio::test]
async fn reserved_basename_rejected_at_every_mutating_call_site() {
    const RESERVED: &[&str] = &[".abstract", ".overview"];

    fn is_reserved_err(e: &VedaError, reserved: &str) -> bool {
        // The contract is `VedaError::InvalidPath` whose message names the
        // basename and the word "reserved". Asserting both keeps the test
        // honest if someone refactors the error variant.
        match e {
            VedaError::InvalidPath(msg) => msg.contains(reserved) && msg.contains("reserved"),
            _ => false,
        }
    }

    for reserved in RESERVED {
        let (svc, _state) = make_service();

        // ── write_file ──────────────────────────────────────────────
        let err = svc
            .write_file("ws1", &format!("/docs/{reserved}"), "x", None, None)
            .await
            .expect_err(&format!("write_file must reject /docs/{reserved}"));
        assert!(
            is_reserved_err(&err, reserved),
            "write_file: expected InvalidPath reserved error, got {err:?}"
        );

        // ── append_file ─────────────────────────────────────────────
        let err = svc
            .append_file("ws1", &format!("/docs/{reserved}"), "x")
            .await
            .expect_err(&format!("append_file must reject /docs/{reserved}"));
        assert!(
            is_reserved_err(&err, reserved),
            "append_file: expected InvalidPath reserved error, got {err:?}"
        );

        // ── mkdir ───────────────────────────────────────────────────
        let err = svc
            .mkdir("ws1", &format!("/docs/{reserved}"))
            .await
            .expect_err(&format!("mkdir must reject /docs/{reserved}"));
        assert!(
            is_reserved_err(&err, reserved),
            "mkdir: expected InvalidPath reserved error, got {err:?}"
        );

        // Setup a source for the copy/rename cases. The source name is
        // intentionally NOT reserved — copy_file / rename only reject the
        // *destination* (see fs.rs:919-923, fs.rs:1199-1200), matching
        // the documented rationale ("moving a legacy file out of a
        // reserved slot is allowed; moving into one is not").
        svc.write_file("ws1", "/docs/src.md", "src", None, None)
            .await
            .unwrap();

        // ── copy_file (dst) ─────────────────────────────────────────
        let err = svc
            .copy_file("ws1", "/docs/src.md", &format!("/docs/{reserved}"))
            .await
            .expect_err(&format!("copy_file must reject dst /docs/{reserved}"));
        assert!(
            is_reserved_err(&err, reserved),
            "copy_file dst: expected InvalidPath reserved error, got {err:?}"
        );

        // ── rename (dst) ────────────────────────────────────────────
        let err = svc
            .rename("ws1", "/docs/src.md", &format!("/docs/{reserved}"))
            .await
            .expect_err(&format!("rename must reject dst /docs/{reserved}"));
        assert!(
            is_reserved_err(&err, reserved),
            "rename dst: expected InvalidPath reserved error, got {err:?}"
        );
    }
}

#[tokio::test]
async fn list_dir_with_dir_sizes_aggregates_subtrees() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/docs/api/a.json", "0123456789", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/api/sub/b.json", "01234", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/biz/c.md", "0123456", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/top.md", "012", None, None)
        .await
        .unwrap();

    let entries = svc.list_dir_with_dir_sizes("ws1", "/docs").await.unwrap();
    let get = |name: &str| entries.iter().find(|e| e.name == name).unwrap();

    // Directory sizes are recursive subtree sums.
    assert_eq!(get("api").size_bytes, Some(15), "10 + 5 nested");
    assert_eq!(get("biz").size_bytes, Some(7));
    // Files keep their own size.
    assert_eq!(get("top.md").size_bytes, Some(3));

    // The plain listing still reports directories as size-less — hot
    // paths must not silently grow an O(subtree) aggregate.
    let plain = svc.list_dir("ws1", "/docs").await.unwrap();
    let api = plain.iter().find(|e| e.name == "api").unwrap();
    assert_eq!(api.size_bytes, None);
}

#[tokio::test]
async fn list_dir_with_dir_sizes_empty_dir_reports_zero() {
    let (svc, _state) = make_service();
    svc.mkdir("ws1", "/emptydir").await.unwrap();
    let entries = svc.list_dir_with_dir_sizes("ws1", "/").await.unwrap();
    let d = entries.iter().find(|e| e.name == "emptydir").unwrap();
    assert_eq!(d.size_bytes, Some(0), "empty directory shows 0, not null");
}

// ── path-scope family: a prefix may name a directory subtree, a single
//    file, or nothing (fix/path-scope-prefix-self) ─────────────────────

#[tokio::test]
async fn grep_with_file_path_scopes_to_that_single_file() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/docs/a.md", "needle here", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/b.md", "needle there", None, None)
        .await
        .unwrap();
    let hits = svc
        .grep("ws1", "needle", Some("/docs/a.md"), false, 100)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "/docs/a.md");
}

#[tokio::test]
async fn grep_with_dir_prefix_unchanged() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/docs/a.md", "needle a", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/sub/b.md", "needle b", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/other/c.md", "needle c", None, None)
        .await
        .unwrap();
    let hits = svc
        .grep("ws1", "needle", Some("/docs"), false, 100)
        .await
        .unwrap();
    let mut paths: Vec<_> = hits.iter().map(|h| h.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["/docs/a.md", "/docs/sub/b.md"]);
}

#[tokio::test]
async fn grep_root_scans_whole_workspace() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/a.md", "needle", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/b.md", "needle", None, None)
        .await
        .unwrap();
    let hits = svc.grep("ws1", "needle", None, false, 100).await.unwrap();
    assert_eq!(hits.len(), 2);
}

#[tokio::test]
async fn grep_nonexistent_path_returns_empty() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/a.md", "needle", None, None)
        .await
        .unwrap();
    let hits = svc
        .grep("ws1", "needle", Some("/nope"), false, 100)
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn grep_bare_prefix_equals_slashed() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/docs/a.md", "needle", None, None)
        .await
        .unwrap();
    let bare = svc
        .grep("ws1", "needle", Some("docs"), false, 100)
        .await
        .unwrap();
    let slashed = svc
        .grep("ws1", "needle", Some("/docs"), false, 100)
        .await
        .unwrap();
    assert_eq!(bare.len(), 1);
    assert_eq!(bare[0].path, slashed[0].path);
}

#[tokio::test]
async fn list_dir_recursive_on_file_errors() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/notes.md", "x", None, None)
        .await
        .unwrap();
    let err = svc
        .list_dir_recursive("ws1", "/notes.md", 100)
        .await
        .unwrap_err();
    assert!(matches!(err, VedaError::InvalidPath(_)));
}

#[tokio::test]
async fn list_dir_recursive_on_missing_path_errors() {
    let (svc, _state) = make_service();
    let err = svc.list_dir_recursive("ws1", "/nope", 100).await.unwrap_err();
    assert!(matches!(err, VedaError::NotFound(_)));
}

#[tokio::test]
async fn list_dir_recursive_root_ok() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/docs/a.md", "x", None, None)
        .await
        .unwrap();
    let entries = svc.list_dir_recursive("ws1", "/", 100).await.unwrap();
    assert!(entries.iter().any(|d| d.path == "/docs/a.md"));
}

#[tokio::test]
async fn glob_files_literal_pattern_matches_file_itself() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/notes.md", "x", None, None)
        .await
        .unwrap();
    let out = svc.glob_files("ws1", "/notes.md", 100).await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].path, "/notes.md");
}

#[tokio::test]
async fn glob_files_missing_prefix_returns_empty() {
    let (svc, _state) = make_service();
    let out = svc.glob_files("ws1", "/nope/*.md", 100).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn glob_files_children_pattern_under_file_returns_empty() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/notes.md", "x", None, None)
        .await
        .unwrap();
    let out = svc.glob_files("ws1", "/notes.md/*", 100).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn glob_files_root_pattern_ok() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/a.txt", "x", None, None)
        .await
        .unwrap();
    svc.write_file("ws1", "/docs/b.txt", "x", None, None)
        .await
        .unwrap();
    let out = svc.glob_files("ws1", "/*.txt", 100).await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].path, "/a.txt");
}

// ── root destination guard: root must never receive a dentry row ──────

#[tokio::test]
async fn rename_to_root_rejected() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/docs/a.md", "x", None, None)
        .await
        .unwrap();
    let err = svc.rename("ws1", "/docs", "/").await.unwrap_err();
    assert!(matches!(err, VedaError::InvalidPath(_)));
    // Empty destination normalizes to "/" and takes the same rejection.
    let err = svc.rename("ws1", "/docs", "").await.unwrap_err();
    assert!(matches!(err, VedaError::InvalidPath(_)));
}

#[tokio::test]
async fn copy_to_root_rejected() {
    let (svc, _state) = make_service();
    svc.write_file("ws1", "/a.md", "x", None, None)
        .await
        .unwrap();
    let err = svc.copy_file("ws1", "/a.md", "/").await.unwrap_err();
    assert!(matches!(err, VedaError::InvalidPath(_)));
}

#[tokio::test]
async fn write_to_root_rejected() {
    let (svc, _state) = make_service();
    let err = svc
        .write_file("ws1", "/", "x", None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, VedaError::InvalidPath(_)));
    // mkdir("/") stays an idempotent no-op.
    svc.mkdir("ws1", "/").await.unwrap();
}
