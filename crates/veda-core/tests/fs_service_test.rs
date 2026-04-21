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
        .write_file("ws1", "/hello.txt", "hello world")
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
    svc.write_file("ws1", "/a/b/c.txt", "deep").await.unwrap();

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
    let r1 = svc.write_file("ws1", "/f.txt", "same").await.unwrap();
    assert!(!r1.content_unchanged);
    assert_eq!(r1.revision, 1);

    let r2 = svc.write_file("ws1", "/f.txt", "same").await.unwrap();
    assert!(r2.content_unchanged);
    assert_eq!(r2.revision, 1);
}

#[tokio::test]
async fn overwrite_bumps_revision() {
    let (svc, _) = make_service();
    let r1 = svc.write_file("ws1", "/f.txt", "v1").await.unwrap();
    assert_eq!(r1.revision, 1);

    let r2 = svc.write_file("ws1", "/f.txt", "v2").await.unwrap();
    assert!(!r2.content_unchanged);
    assert_eq!(r2.revision, 2);

    let content = svc.read_file("ws1", "/f.txt").await.unwrap();
    assert_eq!(content, "v2");
}

#[tokio::test]
async fn overwrite_enqueues_outbox() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/f.txt", "v1").await.unwrap();
    svc.write_file("ws1", "/f.txt", "v2").await.unwrap();

    let st = state.lock().unwrap();
    let sync_events: Vec<_> = st
        .outbox
        .iter()
        .filter(|e| e.event_type == OutboxEventType::ChunkSync)
        .collect();
    assert_eq!(sync_events.len(), 2);
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
    let result = svc.write_file("ws1", "/mydir", "oops").await;
    assert!(matches!(result, Err(VedaError::AlreadyExists(_))));
}

#[tokio::test]
async fn delete_file() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/del.txt", "gone").await.unwrap();
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
    svc.write_file("ws1", "/docs/a.txt", "a").await.unwrap();
    svc.write_file("ws1", "/docs/b.txt", "b").await.unwrap();

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
    svc.write_file("ws1", "/s.txt", "stat me").await.unwrap();

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
    svc.write_file("ws1", "/orig.txt", "shared").await.unwrap();
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
    svc.write_file("ws1", "/old.txt", "move me").await.unwrap();
    svc.rename("ws1", "/old.txt", "/new.txt").await.unwrap();

    let result = svc.read_file("ws1", "/old.txt").await;
    assert!(matches!(result, Err(VedaError::NotFound(_))));

    let content = svc.read_file("ws1", "/new.txt").await.unwrap();
    assert_eq!(content, "move me");
}

#[tokio::test]
async fn rename_to_existing_fails() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/a.txt", "a").await.unwrap();
    svc.write_file("ws1", "/b.txt", "b").await.unwrap();
    let result = svc.rename("ws1", "/a.txt", "/b.txt").await;
    assert!(matches!(result, Err(VedaError::AlreadyExists(_))));
}

#[tokio::test]
async fn read_lines() {
    let (svc, _) = make_service();
    let content = "line1\nline2\nline3\nline4\nline5\n";
    svc.write_file("ws1", "/lines.txt", content).await.unwrap();

    let lines = svc
        .read_file_lines("ws1", "/lines.txt", 2, 4)
        .await
        .unwrap();
    assert_eq!(lines, "line2\nline3\nline4");
}

#[tokio::test]
async fn fs_events_emitted() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/ev.txt", "hi").await.unwrap();
    svc.delete("ws1", "/ev.txt").await.unwrap();

    let st = state.lock().unwrap();
    let types: Vec<FsEventType> = st.fs_events.iter().map(|e| e.event_type).collect();
    assert!(types.contains(&FsEventType::Create));
    assert!(types.contains(&FsEventType::Delete));
}

#[tokio::test]
async fn workspace_isolation() {
    let (svc, _) = make_service();
    svc.write_file("ws1", "/secret.txt", "ws1 data")
        .await
        .unwrap();

    let result = svc.read_file("ws2", "/secret.txt").await;
    assert!(matches!(result, Err(VedaError::NotFound(_))));
}

#[tokio::test]
async fn delete_dir_cleans_up_child_files() {
    let (svc, state) = make_service();
    svc.write_file("ws1", "/docs/a.txt", "aaa").await.unwrap();
    svc.write_file("ws1", "/docs/b.txt", "bbb").await.unwrap();

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
    svc.write_file("ws1", "/orig.txt", "hello").await.unwrap();
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
    svc.write_file("ws1", "/log.txt", "line1\n").await.unwrap();
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
    let result = svc.write_file("ws1", "/big.txt", &big).await;
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
    svc.write_file("ws1", "/a.txt", "a").await.unwrap();
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
    svc.write_file("ws1", "/a.txt", "content_a").await.unwrap();
    svc.write_file("ws1", "/b.txt", "content_b").await.unwrap();

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
