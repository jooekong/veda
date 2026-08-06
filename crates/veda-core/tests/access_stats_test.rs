mod mock_store;

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use veda_core::service::access_stats::AccessRecorder;
use veda_core::service::fs::FsService;

fn at(y: i32, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
}

fn setup() -> (AccessRecorder, Arc<std::sync::Mutex<mock_store::MockState>>) {
    let store = mock_store::MockMetadataStore::new();
    let state = store.state.clone();
    (AccessRecorder::new(Arc::new(store), 8, true), state)
}

#[tokio::test]
async fn merges_counts_and_flushes_once() {
    let (r, state) = setup();
    let now = at(2026, 8, 5, 6);
    r.record_read_at("ws", "d1", now);
    r.record_read_at("ws", "d1", now);
    r.record_search_hits_at("ws", &["d1".into(), "d2".into()], now);

    assert_eq!(r.flush().await.unwrap(), 2);
    let rows = state.lock().unwrap().doc_access_rows.clone();
    let d1 = rows.iter().find(|r| r.dentry_id == "d1").unwrap();
    assert_eq!((d1.reads, d1.search_hits), (2, 1));
    let d2 = rows.iter().find(|r| r.dentry_id == "d2").unwrap();
    assert_eq!((d2.reads, d2.search_hits), (0, 1));
    // buffer drained: second flush is a no-op
    assert_eq!(r.flush().await.unwrap(), 0);
}

#[tokio::test]
async fn day_boundary_follows_fixed_offset_not_utc() {
    let (r, state) = setup();
    // 2026-08-05 20:00 UTC == 2026-08-06 04:00 at +08:00 — must bucket into
    // the 6th, not the UTC 5th.
    r.record_read_at("ws", "d1", at(2026, 8, 5, 20));
    r.record_read_at("ws", "d1", at(2026, 8, 5, 6)); // 14:00 local, the 5th
    r.flush().await.unwrap();
    let rows = state.lock().unwrap().doc_access_rows.clone();
    assert_eq!(rows.len(), 2, "one row per local day");
    let days: Vec<NaiveDate> = rows.iter().map(|r| r.day).collect();
    assert!(days.contains(&NaiveDate::from_ymd_opt(2026, 8, 5).unwrap()));
    assert!(days.contains(&NaiveDate::from_ymd_opt(2026, 8, 6).unwrap()));
}

#[tokio::test]
async fn failed_flush_drops_window_no_retry_double_count() {
    let (r, state) = setup();
    r.record_read_at("ws", "d1", at(2026, 8, 5, 6));
    state.lock().unwrap().fail_doc_access_upsert = true;
    assert!(r.flush().await.is_err());
    state.lock().unwrap().fail_doc_access_upsert = false;
    // window was dropped, not merged back: nothing left to flush
    assert_eq!(r.flush().await.unwrap(), 0);
    assert!(state.lock().unwrap().doc_access_rows.is_empty());
}

#[tokio::test]
async fn disabled_recorder_is_a_no_op() {
    let store = mock_store::MockMetadataStore::new();
    let state = store.state.clone();
    let r = AccessRecorder::disabled(Arc::new(store));
    r.record_read_at("ws", "d1", at(2026, 8, 5, 6));
    r.record_search_hits_at("ws", &["d1".into()], at(2026, 8, 5, 6));
    assert_eq!(r.flush().await.unwrap(), 0);
    assert!(state.lock().unwrap().doc_access_rows.is_empty());
}

// ── FsService instrumentation boundaries (review 2026-08-05 must-tests) ──

/// FsService wired to an ENABLED recorder plus a handle on the captured
/// flush rows. Reads through the service bump counters; flush lands them
/// in `MockState::doc_access_rows`.
fn counting_fs() -> (
    FsService,
    Arc<AccessRecorder>,
    Arc<std::sync::Mutex<mock_store::MockState>>,
) {
    let store = Arc::new(mock_store::MockMetadataStore::new());
    let state = store.state.clone();
    let recorder = Arc::new(AccessRecorder::new(store.clone(), 8, true));
    let svc = FsService::with_stats(store, recorder.clone());
    (svc, recorder, state)
}

fn total_reads(state: &Arc<std::sync::Mutex<mock_store::MockState>>) -> u64 {
    state
        .lock()
        .unwrap()
        .doc_access_rows
        .iter()
        .map(|r| r.reads)
        .sum()
}

#[tokio::test]
async fn grep_scans_do_not_count_as_reads() {
    let (svc, recorder, state) = counting_fs();
    for i in 0..5 {
        svc.write_file("ws", &format!("/docs/f{i}.md"), "needle here", None, None)
            .await
            .unwrap();
    }
    let hits = svc.grep("ws", "needle", None, false, 100).await.unwrap();
    assert_eq!(hits.len(), 5, "grep itself must still work");
    recorder.flush().await.unwrap();
    assert_eq!(
        total_reads(&state),
        0,
        "a grep sweep over 5 files must record zero reads"
    );

    // ...while a real read on the SAME service instance does count.
    svc.read_file("ws", "/docs/f0.md").await.unwrap();
    recorder.flush().await.unwrap();
    assert_eq!(total_reads(&state), 1);
}

#[tokio::test]
async fn preview_counts_exactly_once() {
    let (svc, recorder, state) = counting_fs();
    svc.write_file("ws", "/a.md", "text body", None, None)
        .await
        .unwrap();
    let p = svc.read_file_preview("ws", "/a.md", 1024).await.unwrap();
    assert!(!p.is_binary);
    recorder.flush().await.unwrap();
    assert_eq!(
        total_reads(&state),
        1,
        "text preview delegates to the range core internally and must not double-count"
    );
}

#[tokio::test]
async fn unsupported_binary_preview_is_not_a_read() {
    let (svc, recorder, state) = counting_fs();
    // ZIP magic — stored as blob, no extraction, preview returns the
    // localized "unsupported" placeholder without fetching content.
    let data = b"PK\x03\x04\x00\x01\xff\xfe\0zip\0bytes".to_vec();
    svc.write_blob("ws", "/x.zip", data, None).await.unwrap();
    let p = svc.read_file_preview("ws", "/x.zip", 1024).await.unwrap();
    assert!(p.is_binary);
    recorder.flush().await.unwrap();
    assert_eq!(total_reads(&state), 0);
}

#[tokio::test]
async fn negative_day_offset_buckets_west_of_utc() {
    // Western offsets must work too: 2026-08-06 03:00 UTC at -5 is still
    // the evening of the 5th.
    let store = mock_store::MockMetadataStore::new();
    let state = store.state.clone();
    let r = AccessRecorder::new(Arc::new(store), -5, true);
    r.record_read_at("ws", "d1", at(2026, 8, 6, 3));
    r.flush().await.unwrap();
    let rows = state.lock().unwrap().doc_access_rows.clone();
    assert_eq!(rows[0].day, NaiveDate::from_ymd_opt(2026, 8, 5).unwrap());
}
