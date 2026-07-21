//! Percent-encoding for remote fs paths embedded in URL paths.
//!
//! Shared by the CLI HTTP client and veda-fuse: both build
//! `/v1/fs/{path}`-style URLs from raw user filenames. reqwest's URL
//! parser encodes most illegal bytes on its own, but a bare `%` in a
//! filename passes through verbatim and forms an invalid escape
//! sequence that proxies (nginx) reject with 400 before the request
//! reaches veda. `?` and `#` are worse: the parser reinterprets them
//! as query/fragment delimiters and silently truncates the path.

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// WHATWG path percent-encode set, plus `%` (raw filenames are never
/// pre-encoded, so a literal `%` must not survive as an escape
/// prefix) and a few bytes strict proxies reject unencoded. `/` stays
/// raw — it separates path segments. Non-ASCII is always encoded by
/// the percent-encoding crate, so the resulting URL is pure ASCII.
const FS_PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'\\')
    .add(b'|')
    .add(b'^')
    .add(b'[')
    .add(b']');

/// Encode a remote fs path for embedding in a URL path. The server's
/// router percent-decodes exactly once, restoring the original path.
pub fn encode_path(path: &str) -> String {
    utf8_percent_encode(path, FS_PATH).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use percent_encoding::percent_decode_str;

    #[test]
    fn bare_percent_is_encoded() {
        // The original incident: `较昨日降N%实验复盘.md` — the `%` followed
        // by an encoded CJK byte formed `%%E5…`, an invalid escape nginx
        // rejects with 400.
        assert_eq!(encode_path("a/N%b.md"), "a/N%25b.md");
    }

    #[test]
    fn query_and_fragment_delimiters_are_encoded() {
        // Unencoded `?`/`#` don't 400 — they silently truncate the path
        // into query/fragment, writing to the wrong remote file.
        assert_eq!(encode_path("a?b#c.md"), "a%3Fb%23c.md");
    }

    #[test]
    fn slashes_stay_raw_as_segment_separators() {
        assert_eq!(encode_path("docs/sub dir/x.md"), "docs/sub%20dir/x.md");
    }

    #[test]
    fn non_ascii_is_encoded() {
        assert_eq!(encode_path("实"), "%E5%AE%9E");
    }

    #[test]
    fn plain_ascii_names_pass_through_unchanged() {
        assert_eq!(
            encode_path("docs/readme-v2_final.md"),
            "docs/readme-v2_final.md"
        );
    }

    #[test]
    fn encode_then_decode_round_trips() {
        // Pins the contract with the server: axum percent-decodes the
        // wildcard capture exactly once, so one decode must restore the
        // original path byte-for-byte.
        let original = "wiki/复盘/较昨日降N%实验 (final)?.md";
        let encoded = encode_path(original);
        assert!(encoded.is_ascii(), "encoded URL must be pure ASCII: {encoded}");
        let decoded = percent_decode_str(&encoded).decode_utf8().unwrap();
        assert_eq!(decoded, original);
    }
}
