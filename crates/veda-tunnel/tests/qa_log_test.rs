//! QA-log store against real MySQL (config/test.toml → veda_it).
//! Isolation: every run uses a fresh uuid bot_id and filters stats/list by it,
//! so accumulated rows from earlier runs never affect assertions.

use veda_tunnel::qa_log::{QaLogEntry, QaLogFilter, QaLogStore, FEEDBACK_DOWN, FEEDBACK_UP};

fn test_db_url() -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("config/test.toml");
    let raw = std::fs::read_to_string(&path).expect("config/test.toml");
    let v: toml::Value = toml::from_str(&raw).unwrap();
    v["mysql"]["database_url"].as_str().unwrap().to_string()
}

fn entry(bot: &str, outcome: &'static str, fid: &str) -> QaLogEntry {
    QaLogEntry {
        bot_id: bot.to_string(),
        chat_type: "group".to_string(),
        chat_key: "chat-1".to_string(),
        user_id: "user-a".to_string(),
        query: "绑定主库的流程是什么".to_string(),
        outcome,
        hit_count: if outcome == "answered" { 8 } else { 0 },
        citation_count: if outcome == "answered" { 3 } else { 0 },
        latency_ms: 4200,
        answer_text: "按 [1] 配置 maxConnectionAge=290…".to_string(),
        feedback_id: fid.to_string(),
        tool_trace: None,
    }
}

#[tokio::test]
async fn qa_log_roundtrip() {
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&test_db_url())
        .await
        .expect("mysql connect");
    // Bootstrap twice — must be idempotent.
    let store = QaLogStore::new(pool.clone()).await.expect("bootstrap 1");
    drop(store);
    let store = QaLogStore::new(pool).await.expect("bootstrap 2");

    let bot = format!("qa-test-{}", uuid::Uuid::new_v4().simple());
    let fid_a = uuid::Uuid::new_v4().to_string();
    let fid_b = uuid::Uuid::new_v4().to_string();

    store.log(&entry(&bot, "answered", &fid_a)).await.unwrap();
    store.log(&entry(&bot, "no_context", &fid_b)).await.unwrap();

    // user-a votes up, then changes to down (replace, not a second row);
    // user-b adds an independent down with a reason.
    store.upsert_feedback(&fid_a, "user-a", FEEDBACK_UP, None).await.unwrap();
    store
        .upsert_feedback(&fid_a, "user-a", FEEDBACK_DOWN, Some(2))
        .await
        .unwrap();
    store
        .upsert_feedback(&fid_a, "user-b", FEEDBACK_DOWN, Some(1))
        .await
        .unwrap();
    // Feedback for a reply whose qa_log row is missing must still store.
    store
        .upsert_feedback("orphan-fid", "user-c", FEEDBACK_UP, None)
        .await
        .unwrap();

    let stats = store.stats(7, Some(&bot)).await.unwrap();
    assert_eq!(stats.total, 2, "{stats:?}");
    assert_eq!(stats.outcomes["answered"], 1);
    assert_eq!(stats.outcomes["no_context"], 1);
    assert_eq!(stats.feedback_up, 0, "up was replaced by down");
    assert_eq!(stats.feedback_down, 2, "two distinct users voted down");

    // Full list, newest first.
    let all = store
        .list(&QaLogFilter {
            bot_id: Some(bot.clone()),
            page: 1,
            size: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].outcome, "no_context", "newest first");
    assert_eq!(all[1].down_count, 2);
    assert_eq!(all[1].up_count, 0);
    assert_eq!(
        all[1].answer_text.as_deref(),
        Some("按 [1] 配置 maxConnectionAge=290…"),
        "answer text stored verbatim"
    );

    // Outcome filter narrows to the missing-docs backlog.
    let gaps = store
        .list(&QaLogFilter {
            bot_id: Some(bot.clone()),
            outcome: Some("no_context".to_string()),
            page: 1,
            size: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(gaps.len(), 1);

    // down_voted filter → only the down-voted row.
    let bad = store
        .list(&QaLogFilter {
            bot_id: Some(bot.clone()),
            down_voted: true,
            page: 1,
            size: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].outcome, "answered");
}
