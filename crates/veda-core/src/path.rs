use unicode_normalization::UnicodeNormalization;
use veda_types::{Result, VedaError};

pub fn normalize(path: &str) -> Result<String> {
    if path.is_empty() {
        return Ok("/".to_string());
    }

    let path = path.nfc().collect::<String>();

    if !path.starts_with('/') {
        return Err(VedaError::InvalidPath(
            "path must start with /".to_string(),
        ));
    }

    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if parts.is_empty() {
                return Err(VedaError::InvalidPath(
                    "path escapes root".to_string(),
                ));
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
    if seg.contains('\0') || seg.contains(':') {
        return Err(VedaError::InvalidPath(format!(
            "invalid character in segment: {seg}"
        )));
    }
    Ok(())
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
