//! Reading the words off scanned PDFs, end to end: the real program takes the
//! page pictures out of a scan, the real Tesseract reads them, and the result
//! lands in the search index.
//!
//! These need Tesseract installed (with English data). They fail, and say how
//! to fix it, when it is not -- a test that quietly passed without the engine
//! would be reporting on nothing.

use historical_search_lib::ingest::{build, DocumentRoot, DocumentSettings, Recognition};
use historical_search_lib::ocr::{recognize_pdf, Engine};
use std::path::{Path, PathBuf};

#[path = "support/pdf_fixture.rs"]
mod pdf_fixture;

fn support(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support").join(name)
}

fn setup(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hs_ocr_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("HS_STATE_DIR", dir.join("state"));
    std::env::set_var("HS_WORKER_EXE", env!("CARGO_BIN_EXE_historical-search"));
    dir
}

fn engine() -> Engine {
    // The copy beside the source (which the Windows build puts there) first,
    // then whatever is installed on the machine.
    Engine::locate(Some(Path::new(env!("CARGO_MANIFEST_DIR")))).unwrap_or_else(|why| {
        panic!(
            "these tests need Tesseract with English data, and: {why}. Install it \
             (macOS: brew install tesseract; Debian/Ubuntu: apt install tesseract-ocr; \
             Windows: run scripts/bundle-tesseract.ps1), or set HS_TESSERACT to its path."
        )
    })
}

#[test]
fn a_scan_made_of_a_jpeg_is_read() {
    let dir = setup("jpeg");
    let found = recognize_pdf(&engine(), &support("scan_jpeg.pdf"), &dir.join("saved"), &dir.join("work"), &mut |_, _| {})
        .expect("should be recognised");
    assert_eq!(found.text.pages, 1);
    assert!(!found.from_cache);
    let text = found.text.text.to_lowercase();
    for word in ["hartwell", "ledger", "quarry", "bridge", "1887"] {
        assert!(text.contains(word), "missing {word:?} in {:?}", found.text.text);
    }
}

#[test]
fn a_black_and_white_fax_compressed_scan_is_read() {
    // The form most archive scans take, and the one that has to be wrapped
    // in a TIFF to be read at all.
    let dir = setup("g4");
    let found = recognize_pdf(&engine(), &support("scan_g4.pdf"), &dir.join("saved"), &dir.join("work"), &mut |_, _| {})
        .expect("should be recognised");
    let text = found.text.text.to_lowercase();
    assert!(text.contains("hartwell") && text.contains("bridge"), "got {:?}", found.text.text);
}

#[test]
fn the_second_time_comes_from_what_was_saved_and_survives_a_rename() {
    let dir = setup("cache");
    let engine = engine();
    let original = dir.join("original.pdf");
    std::fs::copy(support("scan_jpeg.pdf"), &original).unwrap();
    let first = recognize_pdf(&engine, &original, &dir.join("saved"), &dir.join("work"), &mut |_, _| {}).unwrap();
    assert!(!first.from_cache);

    let renamed = dir.join("renamed.pdf");
    std::fs::rename(&original, &renamed).unwrap();
    let again = recognize_pdf(&engine, &renamed, &dir.join("saved"), &dir.join("work"), &mut |_, _| {}).unwrap();
    assert!(again.from_cache, "a renamed file must not be read again");
    assert_eq!(again.text.text, first.text.text);
}

#[test]
fn a_scan_in_a_format_that_cannot_be_read_says_so() {
    let dir = setup("jbig2");
    let pdf = dir.join("jbig2.pdf");
    std::fs::write(
        &pdf,
        pdf_fixture::make_image_pdf(100, 100, "/Filter /JBIG2Decode /ColorSpace /DeviceGray /BitsPerComponent 1", &[0u8; 100]),
    )
    .unwrap();
    let err = recognize_pdf(&engine(), &pdf, &dir.join("saved"), &dir.join("work"), &mut |_, _| {})
        .err()
        .expect("nothing could be read");
    assert!(err.contains("JBIG2"), "got {err}");
}

#[test]
fn scanned_pdfs_become_searchable_through_a_rebuild() {
    let dir = setup("rebuild");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::copy(support("scan_jpeg.pdf"), docs.join("Mill Ledger Scan.pdf")).unwrap();
    std::fs::write(docs.join("Notes.pdf"), pdf_fixture::make_pdf(&["Typed notes about the schoolhouse bell"])).unwrap();

    let engine = engine();
    let roots = [DocumentRoot { label: "Archive".into(), path: docs }];
    let settings = DocumentSettings {
        roots: &roots,
        spreadsheets: &[],
        recognition: Recognition::On(&engine),
        saved_dir: dir.join("saved"),
        work_dir: dir.join("work"),
    };
    let empty = PathBuf::new();
    let db = dir.join("index.db");
    let mut phases: Vec<String> = Vec::new();
    let mut progress = |phase: &str, _: Option<u64>, _: Option<u64>| phases.push(phase.to_string());
    let result = build(&empty, &db, &empty, &empty, &[], &settings, &dir.join("cache"), Some(&mut progress)).unwrap();

    assert_eq!(result.total_records, 2);
    assert!(result.warnings.is_empty(), "nothing should need a warning, got {:?}", result.warnings);
    assert!(
        phases.iter().any(|p| p.starts_with("Recognizing text in Mill Ledger Scan.pdf (scan 1 of 1)")),
        "the slow part should say what it is doing, got {phases:?}"
    );

    let conn = rusqlite::Connection::open(&db).unwrap();
    let titles = |query: &str| -> Vec<String> {
        conn.prepare(
            "SELECT items.title FROM items_fts JOIN items ON items.id = items_fts.rowid
             WHERE items_fts MATCH ?1",
        )
        .unwrap()
        .query_map([query], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    };
    assert_eq!(titles("\"hartwell\"*"), vec!["Mill Ledger Scan"], "words from inside the scan");
    assert_eq!(titles("\"schoolhouse\"*"), vec!["Notes"]);

    let (detail, has_text): (String, i64) = conn
        .query_row(
            "SELECT items.detail_json, documents.has_text FROM items
             JOIN documents ON documents.item_id = items.id WHERE items.title = 'Mill Ledger Scan'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(has_text, 1);
    assert!(detail.contains("Recognized from the scanned pages"), "the record should say so: {detail}");
}

#[test]
fn with_recognition_off_a_scan_is_listed_and_the_status_says_why() {
    let dir = setup("off");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::copy(support("scan_jpeg.pdf"), docs.join("Mill Ledger Scan.pdf")).unwrap();
    let roots = [DocumentRoot { label: "Archive".into(), path: docs }];
    let settings = DocumentSettings::new(&roots);
    let empty = PathBuf::new();
    let result = build(&empty, &dir.join("index.db"), &empty, &empty, &[], &settings, &dir.join("cache"), None).unwrap();
    assert_eq!(result.total_records, 1);
    assert!(
        result.warnings.iter().any(|w| w.contains("turned off") && w.contains("Mill Ledger Scan.pdf")),
        "got {:?}",
        result.warnings
    );
}

#[test]
fn with_no_engine_a_scan_is_listed_and_the_status_says_why() {
    let dir = setup("unavailable");
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::copy(support("scan_jpeg.pdf"), docs.join("Mill Ledger Scan.pdf")).unwrap();
    let roots = [DocumentRoot { label: "Archive".into(), path: docs }];
    let settings = DocumentSettings {
        roots: &roots,
        spreadsheets: &[],
        recognition: Recognition::Unavailable("the text recognition program was not found".into()),
        saved_dir: dir.join("saved"),
        work_dir: dir.join("work"),
    };
    let empty = PathBuf::new();
    let result = build(&empty, &dir.join("index.db"), &empty, &empty, &[], &settings, &dir.join("cache"), None).unwrap();
    assert_eq!(result.total_records, 1);
    assert!(result.warnings.iter().any(|w| w.contains("was not found")), "got {:?}", result.warnings);
}

/// Windows hands the application its install folder in the long `\\?\C:\...`
/// form, and Tesseract cannot use a language-data folder written that way. The
/// engine must still be found, and must still read a page, from such a folder.
#[cfg(windows)]
#[test]
fn the_engine_works_from_a_long_form_windows_folder() {
    let long = format!(r"\\?\{}", env!("CARGO_MANIFEST_DIR"));
    let engine = Engine::locate(Some(std::path::Path::new(&long))).expect("should be found from the long form");
    let dir = setup("longform");
    let found = recognize_pdf(&engine, &support("scan_jpeg.pdf"), &dir.join("saved"), &dir.join("work"), &mut |_, _| {})
        .expect("should read the scan");
    assert!(found.text.text.to_lowercase().contains("hartwell"), "got {:?}", found.text.text);
}
