use unicode_normalization::UnicodeNormalization;
use veda_types::{Result, VedaError};

pub fn normalize(path: &str) -> Result<String> {
    if path.is_empty() {
        return Ok("/".to_string());
    }

    let path = path.nfc().collect::<String>();

    if !path.starts_with('/') {
        return Err(VedaError::InvalidPath("path must start with /".to_string()));
    }

    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if parts.is_empty() {
                return Err(VedaError::InvalidPath("path escapes root".to_string()));
            }
            parts.pop();
            continue;
        }
        validate_segment(seg)?;
        parts.push(seg);
    }

    if parts.is_empty() {
        return Ok("/".to_string());
    }

    Ok(format!("/{}", parts.join("/")))
}

fn validate_segment(seg: &str) -> Result<()> {
    if seg.len() > 255 {
        return Err(VedaError::InvalidPath(
            "path segment exceeds 255 bytes".to_string(),
        ));
    }
    for c in seg.chars() {
        if c == '\0' || c == ':' || c.is_control() || is_bidi_control(c) {
            return Err(VedaError::InvalidPath(format!(
                "invalid character in segment: {seg}"
            )));
        }
    }
    Ok(())
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{200E}'  // LRM
        | '\u{200F}' // RLM
        | '\u{202A}' // LRE
        | '\u{202B}' // RLE
        | '\u{202C}' // PDF
        | '\u{202D}' // LRO
        | '\u{202E}' // RLO
        | '\u{2066}' // LRI
        | '\u{2067}' // RLI
        | '\u{2068}' // FSI
        | '\u{2069}' // PDI
        | '\u{2028}' // Line separator
        | '\u{2029}' // Paragraph separator
    )
}

pub fn parent(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "/",
    }
}

pub fn filename(path: &str) -> &str {
    if path == "/" {
        return "";
    }
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Basenames the FUSE layer synthesises as read-only summary
/// sidecars. New writes to these names must be rejected at every
/// surface (HTTP routes, SQL UDFs, internal callers) so a real file
/// can never collide with the synthetic entry. Enforced via
/// [`reject_reserved_basename`] which is called from `FsService`
/// after `normalize`, so trailing slashes / `/.` / SQL UDF call
/// sites all flow through the same gate.
pub const RESERVED_SIDECAR_BASENAMES: &[&str] = &[".abstract", ".overview"];

/// Reject paths whose normalized basename matches a reserved sidecar
/// name. Caller is responsible for normalizing first — passing a raw
/// path with a trailing slash would otherwise produce a basename of
/// `""` and silently let the write through.
pub fn reject_reserved_basename(normalized_path: &str) -> Result<()> {
    let basename = filename(normalized_path);
    if RESERVED_SIDECAR_BASENAMES.contains(&basename) {
        return Err(VedaError::InvalidPath(format!(
            "'{basename}' is reserved for the FUSE summary sidecar and \
             cannot be used as a real file or directory name"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_root() {
        assert_eq!(normalize("/").unwrap(), "/");
        assert_eq!(normalize("").unwrap(), "/");
        assert_eq!(normalize("///").unwrap(), "/");
    }

    #[test]
    fn normalize_simple() {
        assert_eq!(normalize("/foo/bar").unwrap(), "/foo/bar");
        assert_eq!(normalize("/foo//bar/").unwrap(), "/foo/bar");
        assert_eq!(normalize("/foo/./bar").unwrap(), "/foo/bar");
    }

    #[test]
    fn normalize_dotdot() {
        assert_eq!(normalize("/foo/bar/..").unwrap(), "/foo");
        assert_eq!(normalize("/foo/bar/../baz").unwrap(), "/foo/baz");
    }

    #[test]
    fn normalize_escape_root() {
        assert!(normalize("/..").is_err());
        assert!(normalize("/foo/../..").is_err());
    }

    #[test]
    fn normalize_no_leading_slash() {
        assert!(normalize("foo/bar").is_err());
    }

    #[test]
    fn normalize_invalid_chars() {
        assert!(normalize("/foo\0bar").is_err());
        assert!(normalize("/foo:bar").is_err());
        assert!(normalize("/foo\nbar").is_err());
        assert!(normalize("/foo\rbar").is_err());
        assert!(normalize("/foo\u{202E}bar").is_err()); // RLO
        assert!(normalize("/foo\u{200F}bar").is_err()); // RLM
    }

    // ── reject_reserved_basename ──────────────────────────────────

    #[test]
    fn reserved_basename_rejects_exact_match() {
        // Documented sidecar names must be unconditionally rejected
        // — the FUSE layer synthesises them and a real entry would
        // be permanently shadowed.
        let err = reject_reserved_basename("/docs/.abstract").unwrap_err();
        assert!(err.to_string().contains(".abstract"), "msg: {err}");
        let err = reject_reserved_basename("/notes/.overview").unwrap_err();
        assert!(err.to_string().contains("reserved"), "msg: {err}");
    }

    #[test]
    fn reserved_basename_rejects_root_basename() {
        // `/.abstract` (top-level reserved name) must also fail —
        // FUSE root would shadow it just the same as a nested one.
        let err = reject_reserved_basename("/.abstract").unwrap_err();
        assert!(err.to_string().contains(".abstract"), "msg: {err}");
    }

    #[test]
    fn reserved_basename_allows_substring_matches() {
        // Only the exact basename matters. Substring hits or
        // shadow-prefixes (.abstracts trailing s, foo.abstract.md
        // suffix) are unrelated user data and must pass.
        reject_reserved_basename("/abstracts/jan.md").unwrap();
        reject_reserved_basename("/notes/.abstracts").unwrap();
        reject_reserved_basename("/notes/something.abstract.md").unwrap();
    }

    #[test]
    fn reserved_basename_assumes_normalized_input() {
        // Helper is meant to run *after* normalize. The contract
        // pins that caller-provided raw forms (trailing slash, `/.`,
        // double slashes) MUST be passed through `normalize` first
        // — otherwise the basename derived here would be empty / a
        // dot, sneaking the write through. This test demonstrates
        // both: with a raw path the basename helper returns the
        // wrong basename, but `normalize` first then check works.
        // (Catches the surface-level bypass codex flagged in the
        // route-layer-only check.)
        assert!(reject_reserved_basename("/docs/.abstract/").is_ok(),
            "raw path with trailing slash should slip through this helper");
        // After normalize, the bypass is closed:
        let n = normalize("/docs/.abstract/").unwrap();
        assert_eq!(n, "/docs/.abstract");
        assert!(reject_reserved_basename(&n).is_err());
        // Same for the /. form.
        let n = normalize("/docs/.abstract/.").unwrap();
        assert_eq!(n, "/docs/.abstract");
        assert!(reject_reserved_basename(&n).is_err());
    }

    #[test]
    fn parent_cases() {
        assert_eq!(parent("/"), "/");
        assert_eq!(parent("/foo"), "/");
        assert_eq!(parent("/foo/bar"), "/foo");
        assert_eq!(parent("/a/b/c"), "/a/b");
    }

    #[test]
    fn filename_cases() {
        assert_eq!(filename("/"), "");
        assert_eq!(filename("/foo"), "foo");
        assert_eq!(filename("/foo/bar.txt"), "bar.txt");
    }
}
