//! Reading the words out of Word (.docx) and plain text (.txt, .md) files.
//!
//! These are read by the same helper process as PDFs (see `pdf.rs`), so a
//! damaged or hostile file costs that one file its text and nothing more.
//! A document has no fixed pages, so what comes back is one "page" holding
//! everything.
//!
//! Only .docx is read, not the older .doc: that is a binary format with no
//! reasonable reader short of a word processor.

use std::io::Read;
use std::path::Path;

/// The most of one file read, however it is stored. A .docx is a zip, so this
/// is also what stops a file that is a few kilobytes on disk and gigabytes
/// when unpacked.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The parts of a .docx that hold text people wrote, in reading order.
const DOCX_PARTS: &[&str] = &["word/document.xml", "word/footnotes.xml", "word/endnotes.xml"];

pub fn is_pdf(path: &Path) -> bool {
    !is_word(path) && !is_text(path)
}

/// "word", "text" or "pdf": what the window is told a document is.
pub fn kind(path: &Path) -> &'static str {
    if is_word(path) {
        "word"
    } else if is_text(path) {
        "text"
    } else {
        "pdf"
    }
}

pub fn is_word(path: &Path) -> bool {
    has_extension(path, &["docx"])
}

pub fn is_text(path: &Path) -> bool {
    has_extension(path, &["txt", "md"])
}

fn has_extension(path: &Path, wanted: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| wanted.iter().any(|w| e.eq_ignore_ascii_case(w)))
}

/// The raw text of a Word or text file. Whitespace is tidied by the caller.
pub fn read(path: &Path) -> Result<String, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if is_word(path) {
        read_docx(path)
    } else {
        if size > MAX_BYTES {
            return Err(format!("the file is {} MB, which is larger than this app will read", size / (1024 * 1024)));
        }
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        Ok(decode_text(&bytes))
    }
}

/// Turns the bytes of a text file into text. Modern files are UTF-8, and
/// Windows Notepad's older "Unicode" is UTF-16 with a marker at the start; a
/// file that is neither is taken to be the Windows Western European code page
/// that older word processors wrote, where every byte is a character.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    }
}

fn utf16(bytes: &[u8], from: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&pair| from(pair)).collect();
    String::from_utf16_lossy(&units)
}

/// One byte of Windows-1252. Only 0x80-0x9F differ from Latin-1.
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

fn read_docx(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| "this is not a .docx Word document".to_string())?;
    let mut text = String::new();
    let mut found_body = false;
    for part in DOCX_PARTS {
        let Ok(entry) = zip.by_name(part) else { continue };
        let mut xml = String::new();
        entry
            .take(MAX_BYTES)
            .read_to_string(&mut xml)
            .map_err(|_| "the Word document could not be unpacked".to_string())?;
        found_body |= *part == "word/document.xml";
        text.push_str(&xml_text(&xml));
        text.push('\n');
    }
    if !found_body {
        return Err("this is not a .docx Word document (it has no body)".to_string());
    }
    Ok(text)
}

/// The words in WordprocessingML: the contents of every `<w:t>`, with a break
/// at the end of each paragraph and at tabs and line breaks. Tags are
/// matched on their local name, so the prefix does not matter.
pub(crate) fn xml_text(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    let mut in_text = false;
    while let Some(open) = rest.find('<') {
        if in_text {
            out.push_str(&unescape(&rest[..open]));
        }
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else { break };
        let tag = &after[..close];
        rest = &after[close + 1..];
        let (closing, name) = match tag.strip_prefix('/') {
            Some(n) => (true, n),
            None => (false, tag),
        };
        let name = name.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or("");
        let self_closing = tag.ends_with('/');
        match (local, closing) {
            ("t", false) => in_text = !self_closing,
            ("t", true) => in_text = false,
            ("p", true) | ("tab", false) | ("br", false) | ("cr", false) => out.push('\n'),
            _ => {}
        }
    }
    out
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        match after.find(';').filter(|&i| i <= 10) {
            Some(semi) => {
                let entity = &after[..semi];
                let decoded = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                        .and_then(char::from_u32),
                };
                match decoded {
                    Some(c) => out.push(c),
                    None => out.push_str(&rest[amp..amp + semi + 2]),
                }
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_text_is_pulled_from_word_xml() {
        let xml = r#"<w:document><w:body><w:p><w:pPr><w:b/></w:pPr><w:r><w:t>Hartwell &amp; Sons</w:t></w:r><w:r><w:t xml:space="preserve"> ledger</w:t></w:r></w:p><w:p><w:r><w:t/></w:r><w:r><w:t>Second</w:t></w:r></w:p></w:body></w:document>"#;
        assert_eq!(xml_text(xml), "Hartwell & Sons ledger\nSecond\n");
    }

    #[test]
    fn markup_outside_text_is_not_text() {
        let xml = "<w:p><w:instrText>HYPERLINK secret</w:instrText><w:r><w:t>shown</w:t></w:r></w:p>";
        assert_eq!(xml_text(xml), "shown\n");
    }

    #[test]
    fn character_references_are_decoded_and_strays_kept() {
        assert_eq!(unescape("caf&#233; &#x2014; a &lt;b&gt; &bogus; & more"), "café — a <b> &bogus; & more");
    }

    #[test]
    fn text_files_in_older_encodings_still_read() {
        assert_eq!(decode_text("Zoë's mill".as_bytes()), "Zoë's mill");
        assert_eq!(decode_text(b"\xEF\xBB\xBFBOM text"), "BOM text");
        assert_eq!(decode_text(&[0xFF, 0xFE, b'H', 0, b'i', 0]), "Hi");
        assert_eq!(decode_text(&[0xFE, 0xFF, 0, b'H', 0, b'i']), "Hi");
        // Not UTF-8: 0xE9 is e-acute and 0x92 a curly apostrophe in Windows-1252.
        assert_eq!(decode_text(b"Caf\xE9 owner\x92s"), "Café owner’s");
    }

    #[test]
    fn which_kind_a_file_is() {
        assert!(is_word(Path::new("a/Minutes.DOCX")));
        assert!(is_text(Path::new("notes.txt")));
        assert!(is_text(Path::new("README.MD")));
        assert!(is_pdf(Path::new("scan.pdf")));
        assert!(!is_pdf(Path::new("notes.txt")));
    }
}
