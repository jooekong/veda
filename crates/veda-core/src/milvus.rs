//! Milvus conventions shared across crates: collection naming, expression
//! string escaping, and the public Filter DSL → boolean-expression compiler.
//!
//! Lives in veda-core (not veda-store) because both the HTTP service layer
//! and the store implementation need these, and core must not depend on
//! store. veda-store re-exports the two pure helpers for its own callers.
//!
//! Filter DSL v0 contract (`docs/archive/vectors-merge-plan.md` §3.3): only `must`,
//! only `meta.<top_level>` paths, ops {eq, in, gt, gte, lt, lte}.
//!
//! `in` is implemented as an `OR` expansion (`(meta["x"] == a || meta["x"]
//! == b)`) rather than relying on Milvus 2.6's TermExpr on JSON paths,
//! whose support isn't explicitly documented (Codex Stage 4.4 design
//! review). The expansion is deterministic and works in every backend
//! that supports basic `==` / `<` on JSON paths.

use serde_json::Value;
use veda_types::api::{FilterClause, FilterOp, VectorFilter};
use veda_types::{Result, VedaError};

/// Physical Milvus collection for a db-kind workspace.
/// Format: `ws_<16-hex-chars-of-sha256(workspace_id)>_default`.
///
/// The `_default` suffix distinguishes from v1 dedicated collections
/// (named `ws_<ws>_<dataset>_dim<DIM>_v<VER>` per docs/archive/vectors-merge-plan.md §2.6).
/// Hash is over workspace.id (UUID), not workspace.name — workspace
/// rename is safe and never affects the underlying Milvus collection.
/// 16 hex = 64-bit hash space → collision probability is negligible even at
/// the plan's 1500-workspace target. 8 hex (32-bit) was the v1 of this code;
/// at 1500 ws the birthday-paradox probability was ~3e-4 and the "DB check
/// uniq" the plan mentioned was never actually wired.
pub fn vector_collection_name(workspace_id: &str) -> String {
    let hash = crate::checksum::sha256_hex(workspace_id.as_bytes());
    format!("ws_{}_default", &hash[..16])
}

/// Milvus boolean expressions use double-quoted string literals (see Milvus docs).
///
/// This is the single escaping point for interpolating strings into Milvus
/// expressions across the whole workspace — injection safety depends on every
/// caller going through here rather than keeping a local copy.
pub fn milvus_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

const META_PREFIX: &str = "meta.";

/// Parse and validate a v0 `VectorFilter` into a Milvus filter expression.
/// Returns `None` if the filter has no clauses — caller's responsibility
/// to skip AND-merge in that case.
pub fn to_milvus_expr(filter: &VectorFilter) -> Result<Option<String>> {
    if filter.must.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::with_capacity(filter.must.len());
    for clause in &filter.must {
        parts.push(clause_to_expr(clause)?);
    }
    Ok(Some(parts.join(" && ")))
}

fn clause_to_expr(clause: &FilterClause) -> Result<String> {
    let key = parse_meta_field(&clause.field)?;
    let path = format!("meta[{}]", milvus_quote(&key));
    match clause.op {
        FilterOp::Eq => Ok(format!("{path} == {}", scalar_to_expr(&clause.value)?)),
        FilterOp::Gt => Ok(format!("{path} > {}", numeric_or_string(&clause.value)?)),
        FilterOp::Gte => Ok(format!("{path} >= {}", numeric_or_string(&clause.value)?)),
        FilterOp::Lt => Ok(format!("{path} < {}", numeric_or_string(&clause.value)?)),
        FilterOp::Lte => Ok(format!("{path} <= {}", numeric_or_string(&clause.value)?)),
        FilterOp::In => expand_in(&path, &clause.value),
    }
}

/// Validate field is `meta.<top_level_key>` and return the key.
/// Rejects:
///   - missing `meta.` prefix
///   - empty key
///   - nested paths like `meta.a.b`
///   - keys containing characters that would need escaping beyond `"`
///     (keep v0 simple; only ASCII alphanumeric, `_`, `-` allowed)
fn parse_meta_field(field: &str) -> Result<String> {
    let key = field.strip_prefix(META_PREFIX).ok_or_else(|| {
        invalid(format!(
            "field {field:?}: only meta.<key> paths are allowed in v0 (no platform fields, no nesting)"
        ))
    })?;
    if key.is_empty() {
        return Err(invalid("field: meta key must not be empty".into()));
    }
    if key.contains('.') {
        return Err(invalid(format!(
            "field {field:?}: nested meta paths not supported in v0"
        )));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(invalid(format!(
            "field {field:?}: meta key must match [a-zA-Z0-9_-]+"
        )));
    }
    Ok(key.to_string())
}

/// `Eq` accepts any JSON scalar (string, number, bool). Arrays and objects
/// are rejected — Milvus equality on composite JSON values isn't part of
/// the v0 contract.
fn scalar_to_expr(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(milvus_quote(s)),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => Err(invalid(
            "eq value must be a scalar (string / number / bool)".into(),
        )),
    }
}

/// Range ops require a numeric or string value. Null / bool / array / object
/// rejected — Milvus comparison semantics on bool / null are undefined.
fn numeric_or_string(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(milvus_quote(s)),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => Err(invalid(
            "range op value must be a number or string".into(),
        )),
    }
}

/// `In` expands to `(p == v1 || p == v2 || …)`. Empty array → 400 (an
/// `in [] ` filter would match nothing and is almost certainly a caller
/// bug rather than intent).
fn expand_in(path: &str, value: &Value) -> Result<String> {
    let arr = value
        .as_array()
        .ok_or_else(|| invalid("in value must be an array of scalars".into()))?;
    if arr.is_empty() {
        return Err(invalid("in value must not be empty".into()));
    }
    // Cap the OR-expansion: a pathological `in [huge array]` would otherwise
    // build an unbounded Milvus expr string. Same order of magnitude as
    // MAX_TOP_K; callers needing more should narrow their query.
    const MAX_IN_VALUES: usize = 100;
    if arr.len() > MAX_IN_VALUES {
        return Err(invalid(format!(
            "in value has {} items, exceeds {MAX_IN_VALUES}",
            arr.len()
        )));
    }
    let mut alts = Vec::with_capacity(arr.len());
    for item in arr {
        alts.push(format!("{path} == {}", scalar_to_expr(item)?));
    }
    Ok(format!("({})", alts.join(" || ")))
}

fn invalid(msg: String) -> VedaError {
    VedaError::InvalidInput(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn must(clauses: Vec<FilterClause>) -> VectorFilter {
        VectorFilter { must: clauses }
    }

    fn clause(field: &str, op: FilterOp, value: Value) -> FilterClause {
        FilterClause {
            field: field.to_string(),
            op,
            value,
        }
    }

    #[test]
    fn empty_must_returns_none() {
        let f = must(vec![]);
        assert_eq!(to_milvus_expr(&f).unwrap(), None);
    }

    #[test]
    fn single_eq_string() {
        let f = must(vec![clause("meta.category", FilterOp::Eq, json!("shoes"))]);
        assert_eq!(
            to_milvus_expr(&f).unwrap().unwrap(),
            r#"meta["category"] == "shoes""#
        );
    }

    #[test]
    fn single_eq_number() {
        let f = must(vec![clause("meta.price", FilterOp::Eq, json!(42))]);
        assert_eq!(
            to_milvus_expr(&f).unwrap().unwrap(),
            r#"meta["price"] == 42"#
        );
    }

    #[test]
    fn range_lt_number() {
        let f = must(vec![clause("meta.price", FilterOp::Lt, json!(100))]);
        assert_eq!(
            to_milvus_expr(&f).unwrap().unwrap(),
            r#"meta["price"] < 100"#
        );
    }

    #[test]
    fn multiple_clauses_anded() {
        let f = must(vec![
            clause("meta.price", FilterOp::Lt, json!(100)),
            clause("meta.category", FilterOp::Eq, json!("shoes")),
        ]);
        assert_eq!(
            to_milvus_expr(&f).unwrap().unwrap(),
            r#"meta["price"] < 100 && meta["category"] == "shoes""#
        );
    }

    #[test]
    fn in_expands_to_or_chain() {
        let f = must(vec![clause(
            "meta.brand",
            FilterOp::In,
            json!(["nike", "adidas"]),
        )]);
        assert_eq!(
            to_milvus_expr(&f).unwrap().unwrap(),
            r#"(meta["brand"] == "nike" || meta["brand"] == "adidas")"#
        );
    }

    #[test]
    fn in_empty_array_rejected() {
        let f = must(vec![clause("meta.brand", FilterOp::In, json!([]))]);
        assert!(to_milvus_expr(&f).is_err());
    }

    #[test]
    fn in_non_array_rejected() {
        let f = must(vec![clause("meta.brand", FilterOp::In, json!("nike"))]);
        assert!(to_milvus_expr(&f).is_err());
    }

    #[test]
    fn field_without_meta_prefix_rejected() {
        let f = must(vec![clause("price", FilterOp::Eq, json!(100))]);
        assert!(to_milvus_expr(&f).is_err());
        // Platform fields explicitly rejected.
        let f2 = must(vec![clause("dataset", FilterOp::Eq, json!("x"))]);
        assert!(to_milvus_expr(&f2).is_err());
        let f3 = must(vec![clause("status", FilterOp::Eq, json!("active"))]);
        assert!(to_milvus_expr(&f3).is_err());
    }

    #[test]
    fn nested_meta_path_rejected() {
        let f = must(vec![clause("meta.a.b", FilterOp::Eq, json!(1))]);
        assert!(to_milvus_expr(&f).is_err());
    }

    #[test]
    fn meta_key_with_invalid_chars_rejected() {
        let f = must(vec![clause("meta.a b", FilterOp::Eq, json!(1))]);
        assert!(to_milvus_expr(&f).is_err());
        let f2 = must(vec![clause(r#"meta."x""#, FilterOp::Eq, json!(1))]);
        assert!(to_milvus_expr(&f2).is_err());
    }

    #[test]
    fn empty_meta_key_rejected() {
        let f = must(vec![clause("meta.", FilterOp::Eq, json!(1))]);
        assert!(to_milvus_expr(&f).is_err());
    }

    #[test]
    fn eq_array_rejected() {
        let f = must(vec![clause("meta.x", FilterOp::Eq, json!([1, 2]))]);
        assert!(to_milvus_expr(&f).is_err());
    }

    #[test]
    fn range_bool_rejected() {
        let f = must(vec![clause("meta.x", FilterOp::Gt, json!(true))]);
        assert!(to_milvus_expr(&f).is_err());
    }

    #[test]
    fn string_value_escaped() {
        let f = must(vec![clause(
            "meta.note",
            FilterOp::Eq,
            json!(r#"has "quotes" and \ slashes"#),
        )]);
        assert_eq!(
            to_milvus_expr(&f).unwrap().unwrap(),
            r#"meta["note"] == "has \"quotes\" and \\ slashes""#
        );
    }
}
