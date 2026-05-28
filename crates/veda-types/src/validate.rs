//! v0 contract validators for the vector data plane (Stage 4).
//!
//! These are the only places the Pinecone-style schema constraints
//! (`docs/vectors-merge-plan.md` §2.3) are enforced. Stage 4 handlers call
//! these BEFORE any Milvus write so that bad input rejects with a stable
//! `VedaError::InvalidInput` instead of a downstream Milvus error.
//!
//! Limits are denominated in **bytes** for text/meta/tags (matches the plan's
//! "≤ 16KB" wording). Milvus schema declares text `max_length=16384` in
//! characters, which is slack — the API contract is the tighter bound.

use crate::{Result, VedaError};

/// `dataset` field VARCHAR(64) in Milvus + UNIQUE constraint key in MySQL.
const DATASET_NAME_MAX: usize = 64;

/// `row_key` field VARCHAR(128) in Milvus.
const ROW_KEY_MAX: usize = 128;

/// `pk` field VARCHAR(128) in Milvus — total `{dataset}:{row_key}` budget.
const PK_MAX: usize = 128;

/// `text` field — API contract bound, tighter than Milvus's character cap.
const TEXT_MAX_BYTES: usize = 16 * 1024;

/// `meta` JSON serialized size cap.
const META_MAX_BYTES: usize = 16 * 1024;

/// `tags` Array max_capacity in Milvus schema.
const TAGS_MAX_COUNT: usize = 8;

/// Single tag VARCHAR(128).
const TAG_MAX_BYTES: usize = 128;

/// Allowed character class for identifiers (`dataset`, `row_key`).
/// `:` is forbidden because it's the composite PK separator.
fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn invalid(field: &str, reason: &str) -> VedaError {
    VedaError::InvalidInput(format!("{field}: {reason}"))
}

pub fn validate_dataset_name(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(invalid("dataset", "must not be empty"));
    }
    if s.len() > DATASET_NAME_MAX {
        return Err(invalid(
            "dataset",
            &format!("exceeds {DATASET_NAME_MAX} bytes"),
        ));
    }
    if !s.chars().all(is_id_char) {
        return Err(invalid(
            "dataset",
            "must match [a-zA-Z0-9_-]+ (no ':' allowed; it is the PK separator)",
        ));
    }
    Ok(())
}

pub fn validate_row_key(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(invalid("row_key", "must not be empty"));
    }
    if s.len() > ROW_KEY_MAX {
        return Err(invalid("row_key", &format!("exceeds {ROW_KEY_MAX} bytes")));
    }
    if !s.chars().all(is_id_char) {
        return Err(invalid(
            "row_key",
            "must match [a-zA-Z0-9_-]+ (no ':' allowed; it is the PK separator)",
        ));
    }
    Ok(())
}

pub fn validate_text(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(invalid("text", "must not be empty"));
    }
    if s.len() > TEXT_MAX_BYTES {
        return Err(invalid(
            "text",
            &format!("exceeds {TEXT_MAX_BYTES} bytes"),
        ));
    }
    Ok(())
}

pub fn validate_meta(value: &serde_json::Value) -> Result<()> {
    // serde_json::to_vec is the canonical encoding — same bytes as what
    // sqlx::types::Json will send to MySQL.
    let bytes = serde_json::to_vec(value)
        .map_err(|e| invalid("meta", &format!("not serializable: {e}")))?;
    if bytes.len() > META_MAX_BYTES {
        return Err(invalid(
            "meta",
            &format!("serialized size {} exceeds {META_MAX_BYTES} bytes", bytes.len()),
        ));
    }
    Ok(())
}

pub fn validate_tags(tags: &[String]) -> Result<()> {
    if tags.len() > TAGS_MAX_COUNT {
        return Err(invalid(
            "tags",
            &format!("count {} exceeds {TAGS_MAX_COUNT}", tags.len()),
        ));
    }
    for (i, t) in tags.iter().enumerate() {
        if t.is_empty() {
            return Err(invalid("tags", &format!("entry {i} is empty")));
        }
        if t.len() > TAG_MAX_BYTES {
            return Err(invalid(
                "tags",
                &format!("entry {i} exceeds {TAG_MAX_BYTES} bytes"),
            ));
        }
    }
    Ok(())
}

/// Compose the Milvus PK `{dataset}:{row_key}` after validating both parts
/// and the total ≤ `PK_MAX` budget.
pub fn build_pk(dataset: &str, row_key: &str) -> Result<String> {
    validate_dataset_name(dataset)?;
    validate_row_key(row_key)?;
    let pk = format!("{dataset}:{row_key}");
    if pk.len() > PK_MAX {
        return Err(invalid(
            "pk",
            &format!(
                "composed length {} exceeds {PK_MAX} bytes ({}+1+{})",
                pk.len(),
                dataset.len(),
                row_key.len()
            ),
        ));
    }
    Ok(pk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dataset_name_ok() {
        assert!(validate_dataset_name("default").is_ok());
        assert!(validate_dataset_name("products").is_ok());
        assert!(validate_dataset_name("a_b-c-123").is_ok());
    }

    #[test]
    fn dataset_name_empty_rejected() {
        assert!(validate_dataset_name("").is_err());
    }

    #[test]
    fn dataset_name_too_long_rejected() {
        let s = "a".repeat(DATASET_NAME_MAX + 1);
        assert!(validate_dataset_name(&s).is_err());
    }

    #[test]
    fn dataset_name_colon_rejected() {
        assert!(validate_dataset_name("a:b").is_err());
    }

    #[test]
    fn dataset_name_invalid_chars_rejected() {
        assert!(validate_dataset_name("foo bar").is_err());
        assert!(validate_dataset_name("foo.bar").is_err());
        assert!(validate_dataset_name("café").is_err()); // non-ASCII
    }

    #[test]
    fn row_key_ok() {
        assert!(validate_row_key("sku-123").is_ok());
        assert!(validate_row_key("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn row_key_colon_rejected() {
        assert!(validate_row_key("a:b").is_err());
    }

    #[test]
    fn row_key_too_long_rejected() {
        let s = "a".repeat(ROW_KEY_MAX + 1);
        assert!(validate_row_key(&s).is_err());
    }

    #[test]
    fn text_ok() {
        assert!(validate_text("hello").is_ok());
    }

    #[test]
    fn text_empty_rejected() {
        assert!(validate_text("").is_err());
    }

    #[test]
    fn text_oversize_rejected() {
        let s = "x".repeat(TEXT_MAX_BYTES + 1);
        assert!(validate_text(&s).is_err());
    }

    #[test]
    fn meta_ok() {
        assert!(validate_meta(&json!({"price": 100, "name": "test"})).is_ok());
    }

    #[test]
    fn meta_oversize_rejected() {
        let large = "x".repeat(META_MAX_BYTES);
        let v = json!({"big": large});
        assert!(validate_meta(&v).is_err());
    }

    #[test]
    fn tags_ok() {
        assert!(validate_tags(&["sale".into(), "new".into()]).is_ok());
        assert!(validate_tags(&[]).is_ok());
    }

    #[test]
    fn tags_too_many_rejected() {
        let tags: Vec<String> = (0..TAGS_MAX_COUNT + 1).map(|i| format!("t{i}")).collect();
        assert!(validate_tags(&tags).is_err());
    }

    #[test]
    fn tags_entry_empty_rejected() {
        assert!(validate_tags(&["a".into(), "".into()]).is_err());
    }

    #[test]
    fn tags_entry_oversize_rejected() {
        let long = "x".repeat(TAG_MAX_BYTES + 1);
        assert!(validate_tags(&[long]).is_err());
    }

    #[test]
    fn build_pk_ok() {
        let pk = build_pk("products", "sku-123").unwrap();
        assert_eq!(pk, "products:sku-123");
    }

    #[test]
    fn build_pk_rejects_invalid_dataset() {
        assert!(build_pk("a:b", "sku-1").is_err());
    }

    #[test]
    fn build_pk_rejects_invalid_row_key() {
        assert!(build_pk("products", "row:1").is_err());
    }

    #[test]
    fn build_pk_total_length_capped() {
        // 64-byte dataset + 1 ':' + 64-byte row_key = 129 → over PK_MAX
        let ds = "a".repeat(64);
        let rk = "b".repeat(64);
        let err = build_pk(&ds, &rk).unwrap_err();
        match err {
            VedaError::InvalidInput(msg) => assert!(msg.contains("pk")),
            _ => panic!("expected InvalidInput, got {err:?}"),
        }
    }
}
