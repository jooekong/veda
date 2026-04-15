use veda_types::{Result, VedaError};

/// Best-effort text extraction. Only `text/plain` is supported for now.
pub fn extract_text(data: &[u8], mime_type: &str) -> Result<String> {
    match mime_type {
        "text/plain" | "text/plain; charset=utf-8" | "text/plain;charset=utf-8" => {
            String::from_utf8(data.to_vec()).map_err(|e| {
                VedaError::InvalidInput(format!("text/plain is not valid UTF-8: {e}"))
            })
        }
        other => Err(VedaError::InvalidInput(format!(
            "unsupported mime type for extraction: {other}"
        ))),
    }
}
