use veda_types::{Result, VedaError, MIME_DOC, MIME_DOCX, MIME_OLE_STORAGE};

use crate::word;

/// Text extraction from a file's raw bytes, for search indexing: PDFs give
/// their text layer, Word documents (.doc/.docx) their body text. Only ever
/// called for those mimes — plain text is indexed straight from the blob, so
/// it never reaches an ExtractSync task.
pub fn extract_text(data: &[u8], mime_type: &str) -> Result<String> {
    match mime_type {
        "application/pdf" => extract_pdf_text(data),
        MIME_DOCX => word::extract_docx_text(data),
        // x-ole-storage: OLE container of undetermined sub-type — attempt the
        // Word path; extract_doc_text rejects non-Word OLE (xls/ppt) cleanly.
        MIME_DOC | MIME_OLE_STORAGE => word::extract_doc_text(data),
        other => Err(VedaError::InvalidInput(format!(
            "unsupported mime type for extraction: {other}"
        ))),
    }
}

/// Extract the text layer from a PDF via the pure-Rust `pdf-extract` (no native
/// deps, so the attack surface stays in safe Rust). Scanned / image-only PDFs
/// carry no text layer and yield an empty (or near-empty) string — callers
/// treat that as "nothing to index", not an error, and leave OCR to a later
/// stage. May panic on malformed PDFs; callers run it on a blocking task so a
/// panic is contained as a skip rather than crashing the worker.
fn extract_pdf_text(data: &[u8]) -> Result<String> {
    pdf_extract::extract_text_from_mem(data)
        .map_err(|e| VedaError::InvalidInput(format!("pdf text extraction failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_mime_errors() {
        let err = extract_text(b"PK\x03\x04", "application/zip").unwrap_err();
        assert!(matches!(err, VedaError::InvalidInput(_)));
    }
}
