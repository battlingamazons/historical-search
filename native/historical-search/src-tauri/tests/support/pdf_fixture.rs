//! A tiny PDF writer for tests: one line of Helvetica text per page, with a
//! correct cross-reference table. Shared by the unit tests (included from the
//! library) and by the integration test that runs the real program, so both
//! read the same kind of file.

/// A PDF with one page per entry in `pages`. An empty string makes a page
/// with no text on it at all, like a scan.
pub fn make_pdf(pages: &[&str]) -> Vec<u8> {
    build(
        pages
            .iter()
            .map(|text| {
                if text.is_empty() {
                    String::new()
                } else {
                    format!("BT /F1 18 Tf 72 700 Td ({}) Tj ET", escape(text))
                }
            })
            .collect(),
    )
}

/// One page whose lines are written with the `'` operator ("move to the next
/// line and show this") after the first, the way text-to-PDF converters and
/// Ghostscript write a block of text. A reader that ignores that operator
/// returns the first line and nothing else, without saying so.
pub fn make_pdf_with_next_line_operator(lines: &[&str]) -> Vec<u8> {
    let mut content = String::from("BT /F1 12 Tf 14 TL 72 700 Td");
    for (i, line) in lines.iter().enumerate() {
        content.push_str(&format!(" ({}) {}", escape(line), if i == 0 { "Tj" } else { "'" }));
    }
    content.push_str(" ET");
    build(vec![content])
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)")
}

fn build(contents: Vec<String>) -> Vec<u8> {
    let pages = &contents;
    let mut objects: Vec<String> = Vec::new();
    // 1 = catalog, 2 = page tree, 3 = font, then a (page, contents) pair per page.
    let kids: Vec<String> = (0..pages.len()).map(|i| format!("{} 0 R", 4 + i * 2)).collect();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
    objects.push(format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), pages.len()));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string());
    for (i, stream) in pages.iter().enumerate() {
        let contents_id = 5 + i * 2;
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {contents_id} 0 R >>"
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()));
    }

    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}
