use serde_json;
use veda_types::errors::ApiResponse;
use veda_types::*;

#[test]
fn enum_serde_roundtrip() {
    let cases: Vec<(&str, SearchMode)> = vec![
        ("\"hybrid\"", SearchMode::Hybrid),
        ("\"semantic\"", SearchMode::Semantic),
        ("\"fulltext\"", SearchMode::Fulltext),
    ];
    for (json, expected) in cases {
        let de: SearchMode = serde_json::from_str(json).unwrap();
        assert_eq!(de, expected);
        let ser = serde_json::to_string(&expected).unwrap();
        assert_eq!(ser, json);
    }
}

#[test]
fn storage_type_serde() {
    assert_eq!(
        serde_json::to_string(&StorageType::Inline).unwrap(),
        "\"inline\""
    );
    assert_eq!(
        serde_json::to_string(&StorageType::Chunked).unwrap(),
        "\"chunked\""
    );
}

#[test]
fn outbox_event_type_serde() {
    let types = vec![
        (OutboxEventType::ChunkSync, "\"chunk_sync\""),
        (OutboxEventType::ChunkDelete, "\"chunk_delete\""),
        (OutboxEventType::CollectionSync, "\"collection_sync\""),
    ];
    for (val, expected) in types {
        assert_eq!(serde_json::to_string(&val).unwrap(), expected);
        let de: OutboxEventType = serde_json::from_str(expected).unwrap();
        assert_eq!(de, val);
    }
}

#[test]
fn search_mode_default_is_hybrid() {
    assert_eq!(SearchMode::default(), SearchMode::Hybrid);
}

#[test]
fn veda_error_display() {
    let err = VedaError::NotFound("file /foo".into());
    assert_eq!(err.to_string(), "not found: file /foo");

    let err = VedaError::AlreadyExists("/bar".into());
    assert_eq!(err.to_string(), "already exists: /bar");

    let err = VedaError::PermissionDenied;
    assert_eq!(err.to_string(), "permission denied");

    let err = VedaError::InvalidPath("..".into());
    assert_eq!(err.to_string(), "invalid path: ..");

    let err = VedaError::Storage("connection refused".into());
    assert_eq!(err.to_string(), "storage error: connection refused");
}

#[test]
fn veda_error_from_string() {
    let err: VedaError = "something broke".to_string().into();
    assert!(matches!(err, VedaError::Internal(msg) if msg == "something broke"));
}

#[test]
fn api_response_ok() {
    let resp = ApiResponse::ok(42u32);
    assert!(resp.success);
    assert_eq!(resp.data, Some(42));
    assert!(resp.error.is_none());

    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains("\"success\":true"));
    assert!(json.contains("\"data\":42"));
    assert!(!json.contains("\"error\""));
}

#[test]
fn api_response_err() {
    let resp = ApiResponse::<()>::err("bad request");
    assert!(!resp.success);
    assert!(resp.data.is_none());
    assert_eq!(resp.error.as_deref(), Some("bad request"));

    let json = serde_json::to_string(&resp).unwrap();
    assert!(!json.contains("\"data\""));
}

#[test]
fn account_password_hash_not_serialized() {
    let acct = Account {
        id: "a1".into(),
        name: "joe".into(),
        email: Some("joe@test.com".into()),
        password_hash: Some("secret_hash".into()),
        status: AccountStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let json = serde_json::to_string(&acct).unwrap();
    assert!(!json.contains("secret_hash"));
    assert!(!json.contains("password_hash"));
}

#[test]
fn dentry_serde_roundtrip() {
    let d = Dentry {
        id: "d1".into(),
        workspace_id: "ws1".into(),
        parent_path: "/".into(),
        name: "readme.md".into(),
        path: "/readme.md".into(),
        file_id: Some("f1".into()),
        is_dir: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let json = serde_json::to_string(&d).unwrap();
    let d2: Dentry = serde_json::from_str(&json).unwrap();
    assert_eq!(d2.id, "d1");
    assert_eq!(d2.path, "/readme.md");
    assert!(!d2.is_dir);
}

#[test]
fn file_record_serde() {
    let f = FileRecord {
        id: "f1".into(),
        workspace_id: "ws1".into(),
        size_bytes: 1024,
        mime_type: "text/plain".into(),
        storage_type: StorageType::Inline,
        source_type: SourceType::Text,
        line_count: Some(50),
        checksum_sha256: "abc123".into(),
        revision: 1,
        ref_count: 1,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let json = serde_json::to_string(&f).unwrap();
    assert!(json.contains("\"inline\""));
    assert!(json.contains("\"text\""));

    let f2: FileRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(f2.storage_type, StorageType::Inline);
    assert_eq!(f2.source_type, SourceType::Text);
    assert_eq!(f2.size_bytes, 1024);
}

#[test]
fn search_request_defaults() {
    let json = r#"{"workspace_id":"ws1","query":"hello"}"#;
    let req: SearchRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.mode, SearchMode::Hybrid);
    assert_eq!(req.limit, 10);
    assert!(req.path_prefix.is_none());
    assert!(req.query_vector.is_none());
}

#[test]
fn collection_schema_serde() {
    let cs = CollectionSchema {
        id: "c1".into(),
        workspace_id: "ws1".into(),
        name: "articles".into(),
        collection_type: CollectionType::Structured,
        schema_json: serde_json::json!([
            {"name": "title", "field_type": "string", "index": true, "embed": false},
            {"name": "content", "field_type": "string", "index": false, "embed": true}
        ]),
        embedding_source: Some("content".into()),
        embedding_dim: Some(1024),
        status: CollectionStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let json = serde_json::to_string(&cs).unwrap();
    let cs2: CollectionSchema = serde_json::from_str(&json).unwrap();
    assert_eq!(cs2.name, "articles");
    assert_eq!(cs2.collection_type, CollectionType::Structured);
    assert_eq!(cs2.embedding_dim, Some(1024));
}
