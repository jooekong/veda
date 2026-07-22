//! Text extraction for Word documents.
//!
//! - `.docx` (OOXML): unzip `word/document.xml`, collect `<w:t>` runs.
//! - `.doc` (Word 97-2003 binary): walk the FIB → CLX → piece table per
//!   [MS-DOC] and decode each piece as CP1252 or UTF-16LE.
//!
//! Both parsers are pure Rust, never index out of bounds (all slice access is
//! checked), and return `InvalidInput` on malformed/unsupported files — the
//! ExtractSync worker treats that as "nothing to index", keeping the blob
//! downloadable.

use std::io::Read;

use veda_types::{Result, VedaError};

/// Cap on decompressed `document.xml` size and on total extracted text, so a
/// crafted file (zip bomb / absurd piece table) cannot balloon memory.
const MAX_EXTRACT_BYTES: u64 = 64 * 1024 * 1024;

fn invalid(msg: impl Into<String>) -> VedaError {
    VedaError::InvalidInput(msg.into())
}

// ── docx ───────────────────────────────────────────────

/// Extract plain text from a .docx: read `word/document.xml` out of the zip
/// and collect text inside `<w:t>` (and `<m:t>` math runs), inserting `\n`
/// per paragraph and `\t`/`\n` for tab/break marks. Field instructions
/// (`w:instrText`) and tracked deletions (`w:delText`) are naturally skipped
/// because only `t` elements are collected. `mc:Fallback` blocks are skipped
/// to avoid double-extracting AlternateContent (e.g. text boxes).
pub fn extract_docx_text(data: &[u8]) -> Result<String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(data))
        .map_err(|e| invalid(format!("not a valid docx (zip): {e}")))?;
    let file = archive
        .by_name("word/document.xml")
        .map_err(|_| invalid("not a docx: missing word/document.xml"))?;
    if file.size() > MAX_EXTRACT_BYTES {
        return Err(invalid("docx document.xml exceeds size cap"));
    }
    let mut xml = Vec::with_capacity(file.size() as usize);
    // `take` re-caps in case the zip header understates the true size.
    file.take(MAX_EXTRACT_BYTES + 1)
        .read_to_end(&mut xml)
        .map_err(|e| invalid(format!("docx read failed: {e}")))?;
    if xml.len() as u64 > MAX_EXTRACT_BYTES {
        return Err(invalid("docx document.xml exceeds size cap"));
    }

    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_text = false;
    let mut fallback_depth = 0u32;
    loop {
        use quick_xml::events::Event;
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| invalid(format!("docx xml parse failed: {e}")))?
        {
            Event::Start(e) => match e.local_name().as_ref() {
                b"t" if fallback_depth == 0 => in_text = true,
                b"Fallback" => fallback_depth += 1,
                _ => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" if fallback_depth == 0 => out.push('\n'),
                b"Fallback" => fallback_depth = fallback_depth.saturating_sub(1),
                _ => {}
            },
            Event::Empty(e) if fallback_depth == 0 => match e.local_name().as_ref() {
                b"tab" => out.push('\t'),
                b"br" | b"cr" => out.push('\n'),
                _ => {}
            },
            Event::Text(t) if in_text && fallback_depth == 0 => {
                let text = t
                    .unescape()
                    .map_err(|e| invalid(format!("docx xml entity error: {e}")))?;
                out.push_str(&text);
            }
            Event::Eof => break,
            _ => {}
        }
        if out.len() as u64 > MAX_EXTRACT_BYTES {
            break; // truncate, don't fail: partial text still indexes
        }
        buf.clear();
    }
    Ok(out)
}

// ── OLE compound file (CFB) reader ─────────────────────

/// Sector-chain terminator per [MS-CFB].
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
/// FAT entries >= this are special markers (DIFAT/FAT/free/end), not sectors.
const MAX_REGSECT: u32 = 0xFFFF_FFFA;

/// Minimal read-only CFB parser — deliberately permissive. Strict parsers
/// (e.g. the `cfb` crate) reject spec violations that real-world writers
/// commit (macOS textutil leaves FAT sectors unmarked and double-points mini
/// sectors); extraction only needs to follow chains, so global FAT/MiniFAT
/// consistency is not validated. All access is bounds-checked and every chain
/// walk is step-capped, so malformed files terminate instead of looping.
struct Cfb<'a> {
    data: &'a [u8],
    sector_size: usize,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    /// (name, start_sector, size) per directory entry; entry 0 is the root.
    dir: Vec<(String, u32, u64)>,
    /// The root entry's stream, which stores all mini-stream sectors.
    mini_stream: Vec<u8>,
    mini_cutoff: u64,
}

impl<'a> Cfb<'a> {
    fn parse(data: &'a [u8]) -> Result<Cfb<'a>> {
        if data.get(..8) != Some(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
            return Err(invalid("doc: not an OLE compound file"));
        }
        let sector_shift = u16_at(data, 0x1E)?;
        if !(7..=16).contains(&sector_shift) {
            return Err(invalid("doc: bad CFB sector size"));
        }
        let sector_size = 1usize << sector_shift;
        let first_dir = u32_at(data, 0x30)?;
        let mini_cutoff = u32_at(data, 0x38)? as u64;
        let first_minifat = u32_at(data, 0x3C)?;
        let first_difat = u32_at(data, 0x44)?;

        let sector = |idx: u32| -> Option<&'a [u8]> {
            let off = (idx as usize + 1) << sector_shift;
            data.get(off..off + sector_size)
        };

        // DIFAT: 109 header entries, then a chain of DIFAT sectors whose last
        // u32 links to the next one.
        let mut fat_sectors: Vec<u32> = Vec::new();
        for i in 0..109 {
            let e = u32_at(data, 0x4C + i * 4)?;
            if e < MAX_REGSECT {
                fat_sectors.push(e);
            }
        }
        let max_sectors = data.len() / sector_size + 1;
        let mut difat_cur = first_difat;
        let mut difat_steps = 0;
        while difat_cur < MAX_REGSECT && difat_steps < max_sectors {
            let Some(s) = sector(difat_cur) else { break };
            for chunk in s[..sector_size - 4].chunks_exact(4) {
                let e = u32::from_le_bytes(chunk.try_into().unwrap());
                if e < MAX_REGSECT {
                    fat_sectors.push(e);
                }
            }
            difat_cur = u32::from_le_bytes(s[sector_size - 4..].try_into().unwrap());
            difat_steps += 1;
        }

        let mut fat: Vec<u32> = Vec::new();
        for fs in &fat_sectors {
            let Some(s) = sector(*fs) else { continue };
            fat.extend(s.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())));
        }

        let follow = |start: u32, fat: &[u32]| -> Vec<u32> {
            let mut out = Vec::new();
            let mut cur = start;
            while cur < MAX_REGSECT && out.len() <= fat.len() {
                out.push(cur);
                cur = fat.get(cur as usize).copied().unwrap_or(ENDOFCHAIN);
            }
            out
        };

        // Directory: 128-byte entries across the directory chain.
        let mut dir: Vec<(String, u32, u64)> = Vec::new();
        for ds in follow(first_dir, &fat) {
            let Some(s) = sector(ds) else { continue };
            for entry in s.chunks_exact(128) {
                let name_len = u16::from_le_bytes(entry[64..66].try_into().unwrap()) as usize;
                if !(2..=64).contains(&name_len) {
                    continue;
                }
                let name: String = entry[..name_len - 2]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
                    .map(|u| char::from_u32(u as u32).unwrap_or('\u{FFFD}'))
                    .collect();
                let start = u32::from_le_bytes(entry[116..120].try_into().unwrap());
                // v3 files may leave garbage in the high half of size.
                let size = u32::from_le_bytes(entry[120..124].try_into().unwrap()) as u64;
                dir.push((name, start, size));
            }
        }

        let mini_fat: Vec<u32> = follow(first_minifat, &fat)
            .into_iter()
            .filter_map(&sector)
            .flat_map(|s| {
                s.chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                    .collect::<Vec<_>>()
            })
            .collect();

        // Root entry's regular-FAT stream backs all mini streams.
        let mini_stream = match dir.first() {
            Some(&(_, root_start, root_size)) => {
                let mut buf = Vec::new();
                for sc in follow(root_start, &fat) {
                    if let Some(s) = sector(sc) {
                        buf.extend_from_slice(s);
                    }
                    if buf.len() as u64 >= root_size {
                        break;
                    }
                }
                buf.truncate(root_size as usize);
                buf
            }
            None => Vec::new(),
        };

        Ok(Cfb { data, sector_size, fat, mini_fat, dir, mini_stream, mini_cutoff })
    }

    /// Read a top-level stream by exact name, or `None` if absent. Streams
    /// smaller than the mini cutoff live in 64-byte mini sectors inside the
    /// root's mini stream; larger ones in regular sectors.
    fn read_stream(&self, name: &str) -> Option<Vec<u8>> {
        // Entry 0 is the root storage, never a named data stream.
        let &(_, start, size) = self.dir.iter().skip(1).find(|(n, _, _)| n == name)?;
        let mut buf: Vec<u8> = Vec::with_capacity(size.min(MAX_EXTRACT_BYTES) as usize);
        if size < self.mini_cutoff {
            let mut cur = start;
            let mut steps = 0;
            while cur < MAX_REGSECT && steps <= self.mini_fat.len() {
                let off = cur as usize * 64;
                if let Some(s) = self.mini_stream.get(off..off + 64) {
                    buf.extend_from_slice(s);
                }
                cur = self.mini_fat.get(cur as usize).copied().unwrap_or(ENDOFCHAIN);
                steps += 1;
            }
        } else {
            let mut cur = start;
            let mut steps = 0;
            while cur < MAX_REGSECT && steps <= self.fat.len() {
                let off = (cur as usize + 1) * self.sector_size;
                if let Some(s) = self.data.get(off..off + self.sector_size) {
                    buf.extend_from_slice(s);
                }
                cur = self.fat.get(cur as usize).copied().unwrap_or(ENDOFCHAIN);
                steps += 1;
            }
        }
        buf.truncate(size as usize);
        Some(buf)
    }
}

// ── doc (Word 97-2003) ─────────────────────────────────

/// [MS-DOC] FibBase field offsets within the WordDocument stream.
const FIB_W_IDENT: usize = 0x00; // magic 0xA5EC
const FIB_N_FIB: usize = 0x02; // format version; 0x00C1+ = Word 97+
const FIB_FLAGS: usize = 0x0A; // bitfield below
const FLAG_ENCRYPTED: u16 = 0x0100; // fEncrypted
const FLAG_WHICH_TBL: u16 = 0x0200; // fWhichTblStm: 1Table vs 0Table
const FIB_CCP_TEXT: usize = 0x4C; // FibRgLw97.ccpText: CP count of main document
const FIB_FC_CLX: usize = 0x01A2; // FibRgFcLcb97.fcClx: CLX offset in table stream
const FIB_LCB_CLX: usize = 0x01A6; // FibRgFcLcb97.lcbClx

fn u16_at(buf: &[u8], off: usize) -> Result<u16> {
    let b = buf
        .get(off..off + 2)
        .ok_or_else(|| invalid("doc: truncated stream"))?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(buf: &[u8], off: usize) -> Result<u32> {
    let b = buf
        .get(off..off + 4)
        .ok_or_else(|| invalid("doc: truncated stream"))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Extract plain text from a Word 97-2003 `.doc` binary file.
///
/// Path: WordDocument stream FIB → table stream CLX → PlcPcd piece table →
/// decode pieces (CP1252 when the fCompressed bit is set, else UTF-16LE),
/// truncated to `ccpText` so only the main document body is returned (no
/// headers/footnotes). Encrypted and pre-Word-97 files are rejected.
pub fn extract_doc_text(data: &[u8]) -> Result<String> {
    let comp = Cfb::parse(data)?;

    let word_stream = comp
        .read_stream("WordDocument")
        .ok_or_else(|| invalid("doc: missing WordDocument stream"))?;

    if u16_at(&word_stream, FIB_W_IDENT)? != 0xA5EC {
        return Err(invalid("doc: not a Word document (bad FIB magic)"));
    }
    let n_fib = u16_at(&word_stream, FIB_N_FIB)?;
    if n_fib < 0x00C1 {
        return Err(invalid(format!(
            "doc: Word 95 or older not supported (nFib={n_fib:#06x})"
        )));
    }
    let flags = u16_at(&word_stream, FIB_FLAGS)?;
    if flags & FLAG_ENCRYPTED != 0 {
        return Err(invalid("doc: encrypted document not supported"));
    }

    let table_name = if flags & FLAG_WHICH_TBL != 0 {
        "1Table"
    } else {
        "0Table"
    };
    let table_stream = comp
        .read_stream(table_name)
        .ok_or_else(|| invalid(format!("doc: missing {table_name} stream")))?;

    let ccp_text = u32_at(&word_stream, FIB_CCP_TEXT)? as u64;
    let fc_clx = u32_at(&word_stream, FIB_FC_CLX)? as usize;
    let lcb_clx = u32_at(&word_stream, FIB_LCB_CLX)? as usize;
    let clx = table_stream
        .get(fc_clx..fc_clx.checked_add(lcb_clx).ok_or_else(|| invalid("doc: CLX overflow"))?)
        .ok_or_else(|| invalid("doc: CLX out of table stream bounds"))?;

    let plc_pcd = find_plc_pcd(clx)?;
    let raw = decode_pieces(plc_pcd, &word_stream, ccp_text)?;
    Ok(clean_doc_text(&raw))
}

/// Walk the CLX (a sequence of Prc blocks followed by one Pcdt) and return
/// the PlcPcd bytes inside the Pcdt.
fn find_plc_pcd(clx: &[u8]) -> Result<&[u8]> {
    let mut pos = 0usize;
    loop {
        match clx.get(pos) {
            Some(1) => {
                // Prc: 0x01, u16 cbGrpprl, grpprl bytes — property data, skip.
                let cb = u16_at(clx, pos + 1)? as usize;
                pos = pos
                    .checked_add(3 + cb)
                    .ok_or_else(|| invalid("doc: CLX overflow"))?;
            }
            Some(2) => {
                // Pcdt: 0x02, u32 lcb, PlcPcd.
                let lcb = u32_at(clx, pos + 1)? as usize;
                let start = pos + 5;
                return clx
                    .get(start..start.checked_add(lcb).ok_or_else(|| invalid("doc: CLX overflow"))?)
                    .ok_or_else(|| invalid("doc: PlcPcd out of CLX bounds"));
            }
            Some(_) => return Err(invalid("doc: malformed CLX")),
            None => return Err(invalid("doc: CLX has no piece table")),
        }
    }
}

/// Decode up to `ccp_text` characters (main-document CPs) from the piece
/// table. PlcPcd layout: (n+1) u32 CPs, then n 8-byte PCDs. Each PCD's `fc`
/// carries the fCompressed flag in bit 30: set → 8-bit CP1252 at fc/2,
/// clear → UTF-16LE at fc.
fn decode_pieces(plc: &[u8], word_stream: &[u8], ccp_text: u64) -> Result<String> {
    if plc.len() < 4 + 8 || !(plc.len() - 4).is_multiple_of(12) {
        return Err(invalid("doc: malformed PlcPcd"));
    }
    let n = (plc.len() - 4) / 12;
    let cp_of = |i: usize| u32_at(plc, i * 4);

    let mut out = String::new();
    let mut remaining = ccp_text;
    for i in 0..n {
        if remaining == 0 || out.len() as u64 > MAX_EXTRACT_BYTES {
            break;
        }
        let cp_start = cp_of(i)? as u64;
        let cp_end = cp_of(i + 1)? as u64;
        if cp_end < cp_start {
            return Err(invalid("doc: piece CPs not monotonic"));
        }
        let take = (cp_end - cp_start).min(remaining) as usize;
        remaining -= take as u64;

        let pcd_off = (n + 1) * 4 + i * 8;
        let fc_raw = u32_at(plc, pcd_off + 2)?;
        let compressed = fc_raw & 0x4000_0000 != 0;
        let fc = (fc_raw & 0x3FFF_FFFF) as usize;

        let piece = if compressed {
            let start = fc / 2;
            let bytes = word_stream
                .get(start..start.checked_add(take).ok_or_else(|| invalid("doc: piece overflow"))?)
                .ok_or_else(|| invalid("doc: piece out of WordDocument bounds"))?;
            encoding_rs::WINDOWS_1252
                .decode_without_bom_handling(bytes)
                .0
        } else {
            let len = take.checked_mul(2).ok_or_else(|| invalid("doc: piece overflow"))?;
            let bytes = word_stream
                .get(fc..fc.checked_add(len).ok_or_else(|| invalid("doc: piece overflow"))?)
                .ok_or_else(|| invalid("doc: piece out of WordDocument bounds"))?;
            encoding_rs::UTF_16LE.decode_without_bom_handling(bytes).0
        };
        out.push_str(&piece);
    }
    Ok(out)
}

/// Map Word's in-text control characters to plain text and drop field
/// instructions. Fields are `0x13 <instruction> 0x14 <result> 0x15` and may
/// nest (e.g. HYPERLINK wrapping PAGEREF) — instruction spans are dropped,
/// result spans kept.
fn clean_doc_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    // One bool per open field: true while inside its instruction span.
    let mut field_stack: Vec<bool> = Vec::new();
    for ch in raw.chars() {
        match ch {
            '\u{13}' => {
                if field_stack.len() < 64 {
                    field_stack.push(true);
                }
            }
            '\u{14}' => {
                if let Some(top) = field_stack.last_mut() {
                    *top = false;
                }
            }
            '\u{15}' => {
                field_stack.pop();
            }
            _ if field_stack.iter().any(|&instr| instr) => {}
            '\r' | '\u{0B}' | '\u{0C}' | '\u{07}' => out.push('\n'),
            '\u{1E}' => out.push('-'),          // non-breaking hyphen
            '\u{1F}' | '\u{01}' | '\u{02}' | '\u{05}' | '\u{08}' => {} // soft hyphen / anchors / refs
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real fixtures generated on macOS via `textutil -convert doc/docx`
    // (Cocoa writes genuine Word 97 / OOXML files). Both contain the
    // sentinel "VEDA_WORD_SENTINEL_88" plus Chinese text.
    const DOC_BYTES: &[u8] = include_bytes!("../tests/fixtures/veda_word_e2e.doc");
    const DOCX_BYTES: &[u8] = include_bytes!("../tests/fixtures/veda_word_e2e.docx");

    #[test]
    fn docx_extracts_sentinel_and_chinese() {
        let text = extract_docx_text(DOCX_BYTES).unwrap();
        assert!(text.contains("VEDA_WORD_SENTINEL_88"), "text: {text:?}");
        assert!(text.contains("中文段落"), "text: {text:?}");
    }

    #[test]
    fn doc_extracts_sentinel_and_chinese() {
        let text = extract_doc_text(DOC_BYTES).unwrap();
        assert!(text.contains("VEDA_WORD_SENTINEL_88"), "text: {text:?}");
        assert!(text.contains("中文段落"), "text: {text:?}");
    }

    #[test]
    fn doc_extracts_genuine_msword_file() {
        // Written by real Microsoft Office Word (Apache POI test corpus) —
        // exercises the spec-conformant writer path, complementing the
        // spec-violating textutil fixture above.
        let text =
            extract_doc_text(include_bytes!("../tests/fixtures/msword_sample.doc")).unwrap();
        assert!(text.contains("I am a test document"), "text: {text:?}");
        assert!(text.contains("It’s Arial Black in 16 point"), "text: {text:?}");
    }

    #[test]
    fn docx_rejects_non_zip() {
        assert!(matches!(
            extract_docx_text(b"not a zip at all"),
            Err(VedaError::InvalidInput(_))
        ));
    }

    #[test]
    fn docx_rejects_zip_without_document_xml() {
        // A minimal empty zip (EOCD record only).
        let empty_zip: &[u8] = &[
            0x50, 0x4B, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(matches!(
            extract_docx_text(empty_zip),
            Err(VedaError::InvalidInput(_))
        ));
    }

    #[test]
    fn doc_rejects_non_ole() {
        assert!(matches!(
            extract_doc_text(b"plain text, not OLE"),
            Err(VedaError::InvalidInput(_))
        ));
    }

    #[test]
    fn doc_rejects_encrypted() {
        // Flip the fEncrypted bit in the real fixture's FIB flags.
        let mut data = DOC_BYTES.to_vec();
        let fib = doc_fib_offset(&data);
        data[fib + FIB_FLAGS + 1] |= 0x01; // FLAG_ENCRYPTED = 0x0100, high byte
        let err = extract_doc_text(&data).unwrap_err();
        assert!(err.to_string().contains("encrypted"), "err: {err}");
    }

    #[test]
    fn doc_rejects_word95() {
        let mut data = DOC_BYTES.to_vec();
        let fib = doc_fib_offset(&data);
        // nFib = 0x0065 (Word 95)
        data[fib + FIB_N_FIB] = 0x65;
        data[fib + FIB_N_FIB + 1] = 0x00;
        let err = extract_doc_text(&data).unwrap_err();
        assert!(err.to_string().contains("Word 95"), "err: {err}");
    }

    #[test]
    fn field_instructions_dropped_results_kept() {
        let raw = "before \u{13}HYPERLINK \"http://x\"\u{14}shown\u{15} after";
        assert_eq!(clean_doc_text(raw), "before shown after");
    }

    #[test]
    fn nested_fields() {
        let raw = "\u{13}IF \u{13}PAGE\u{14}1\u{15} > 0\u{14}yes\u{15}";
        assert_eq!(clean_doc_text(raw), "yes");
    }

    #[test]
    fn control_chars_mapped() {
        assert_eq!(clean_doc_text("a\rb\u{07}c\u{1E}d\u{1F}e"), "a\nb\nc-de");
    }

    /// Locate the FIB inside the raw .doc bytes: the WordDocument stream
    /// lives at some CFB sector, so find the 0xA5EC magic. For test fixtures
    /// (small files) it is in the first handful of sectors.
    fn doc_fib_offset(data: &[u8]) -> usize {
        data.windows(2)
            .position(|w| w == [0xEC, 0xA5])
            .expect("FIB magic not found in fixture")
    }
}
