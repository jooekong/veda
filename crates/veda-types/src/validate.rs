//! v0 contract validators for the vector data plane (Stage 4).
//!
//! These are the only places the Pinecone-style schema constraints
//! (`docs/vectors-merge-plan.md` §2.3) are enforced. Stage 4 handlers call
//! these BEFORE any Milvus write so that bad input rejects with a stable
//! `VedaError::InvalidInput` instead of a downstream Milvus error.
//!
//! Limits are denominated in **UTF-8 bytes** for text/meta/tags. This aligns
//! with Milvus VARCHAR `max_length` semantics — despite the "characters"
//! wording in some user-guide pages, Milvus 2.6 operational FAQ confirms
//! the unit is bytes (see `text` field in `milvus.rs`).

use crate::{Result, VedaError};

/// The name of the per-workspace bootstrap dataset created automatically
/// when a db-kind workspace is provisioned (`provision_db_workspace`).
/// It is also the implicit fallback when vector API callers omit the
/// `dataset` field. **Cannot be deleted** — doing so would break the
/// implicit-default UX promise for any caller that doesn't specify dataset.
pub const DEFAULT_DATASET: &str = "default";

/// Default `category` when the caller omits it. Kept as a SEPARATE constant
/// from `DEFAULT_DATASET` even though both are "default": they're independent
/// schema fields, and coupling them risks one silently drifting onto the
/// other if either default ever changes.
pub const DEFAULT_CATEGORY: &str = "default";

/// `dataset` field VARCHAR(64) in Milvus + UNIQUE constraint key in MySQL.
const DATASET_NAME_MAX: usize = 64;

/// `id` field VARCHAR(128) in Milvus (column literally named `id`), but
/// the composite pk `{dataset}:{id}` is also bound by `PK_MAX = 128`.
/// With a 64-byte `dataset` worst-case and 1-byte ':' separator, the
/// budget for `id` is 63 bytes. We round to 64 for symmetry with
/// `DATASET_NAME_MAX` — `build_pk` still enforces the total ≤ `PK_MAX`
/// at runtime, so a 64+64 combo correctly rejects (64+1+64 = 129 > 128).
const ID_MAX: usize = 64;

/// `pk` field VARCHAR(128) in Milvus — total `{dataset}:{id}` budget.
const PK_MAX: usize = 128;

/// `text` field — UTF-8 byte cap, exactly matches Milvus VARCHAR
/// `max_length=65535` (Milvus 2.6's hard upper bound). Bumped from 16 KiB
/// (vss merge initial) to 64 KiB per Milvus official BM25 tutorial
/// recommendation. Records > 64 KiB UTF-8 must be chunked client-side
/// (Pinecone-style atomic record contract).
const TEXT_MAX_BYTES: usize = 65_535;

/// `meta` JSON serialized size cap.
const META_MAX_BYTES: usize = 16 * 1024;

/// `tags` Array max_capacity in Milvus schema.
const TAGS_MAX_COUNT: usize = 8;

/// Single tag VARCHAR(128).
const TAG_MAX_BYTES: usize = 128;

/// `category` field VARCHAR(64) in Milvus.
const CATEGORY_MAX: usize = 64;

/// Allowed character class for identifiers (`dataset`, `id`).
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

pub fn validate_id(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(invalid("id", "must not be empty"));
    }
    if s.len() > ID_MAX {
        return Err(invalid("id", &format!("exceeds {ID_MAX} bytes")));
    }
    if !s.chars().all(is_id_char) {
        return Err(invalid(
            "id",
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

pub fn validate_category(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(invalid("category", "must not be empty"));
    }
    if s.len() > CATEGORY_MAX {
        return Err(invalid("category", &format!("exceeds {CATEGORY_MAX} bytes")));
    }
    Ok(())
}

/// Fields a vectors search/query caller may request via `output_fields`.
/// `id` (and `score` for search) are ALWAYS returned and must not be listed.
/// Internal columns (`pk`, `vector`, `sparse_vector`, `status`, `expire_at`)
/// are deliberately absent — projecting them is rejected so implementation
/// detail never leaks to a caller.
pub const PROJECTABLE_FIELDS: &[&str] = &[
    "dataset",
    "category",
    "tags",
    "text",
    "meta",
    "created_at",
    "updated_at",
];

/// Validate a caller-supplied `output_fields` projection list. An empty list
/// is allowed (means "only id/score"). Any name outside `PROJECTABLE_FIELDS`
/// — including internal columns and `id`/`score` themselves — is rejected
/// with a stable `InvalidInput`.
pub fn validate_output_fields(fields: &[String]) -> Result<()> {
    for f in fields {
        if !PROJECTABLE_FIELDS.contains(&f.as_str()) {
            return Err(invalid(
                "output_fields",
                &format!(
                    "{f:?} is not projectable (allowed: {}; id/score are always returned)",
                    PROJECTABLE_FIELDS.join(", ")
                ),
            ));
        }
    }
    Ok(())
}

/// Compose the Milvus PK `{dataset}:{id}` after validating both parts
/// and the total ≤ `PK_MAX` budget.
pub fn build_pk(dataset: &str, id: &str) -> Result<String> {
    validate_dataset_name(dataset)?;
    validate_id(id)?;
    let pk = format!("{dataset}:{id}");
    if pk.len() > PK_MAX {
        return Err(invalid(
            "pk",
            &format!(
                "composed length {} exceeds {PK_MAX} bytes ({}+1+{})",
                pk.len(),
                dataset.len(),
                id.len()
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
    fn id_ok() {
        assert!(validate_id("sku-123").is_ok());
        assert!(validate_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn id_colon_rejected() {
        assert!(validate_id("a:b").is_err());
    }

    #[test]
    fn id_too_long_rejected() {
        let s = "a".repeat(ID_MAX + 1);
        assert!(validate_id(&s).is_err());
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
    fn category_ok() {
        assert!(validate_category("shoes").is_ok());
    }

    #[test]
    fn category_empty_rejected() {
        assert!(validate_category("").is_err());
    }

    #[test]
    fn category_oversize_rejected() {
        let s = "a".repeat(CATEGORY_MAX + 1);
        assert!(validate_category(&s).is_err());
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
    fn build_pk_rejects_invalid_id() {
        assert!(build_pk("products", "row:1").is_err());
    }

    #[test]
    fn build_pk_total_length_capped() {
        // 64-byte dataset + 1 ':' + 64-byte id = 129 → over PK_MAX
        let ds = "a".repeat(64);
        let rk = "b".repeat(64);
        let err = build_pk(&ds, &rk).unwrap_err();
        match err {
            VedaError::InvalidInput(msg) => assert!(msg.contains("pk")),
            _ => panic!("expected InvalidInput, got {err:?}"),
        }
    }

    #[test]
    fn output_fields_empty_ok() {
        // Empty list means "only id/score" — a legal rerank-style projection.
        assert!(validate_output_fields(&[]).is_ok());
    }

    #[test]
    fn output_fields_projectable_ok() {
        let f: Vec<String> = ["text", "meta", "dataset", "category", "tags"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(validate_output_fields(&f).is_ok());
    }

    #[test]
    fn output_fields_internal_columns_rejected() {
        // Projecting internal columns must never leak them.
        for bad in ["pk", "vector", "sparse_vector", "status", "expire_at"] {
            assert!(
                validate_output_fields(&[bad.to_string()]).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn output_fields_always_returned_fields_rejected() {
        // id/score are always returned and must not be listed explicitly —
        // keeps the contract unambiguous (output_fields = the optional set).
        assert!(validate_output_fields(&["id".to_string()]).is_err());
        assert!(validate_output_fields(&["score".to_string()]).is_err());
    }

    #[test]
    fn output_fields_unknown_rejected() {
        assert!(validate_output_fields(&["bogus".to_string()]).is_err());
    }
}
