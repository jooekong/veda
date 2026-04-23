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
