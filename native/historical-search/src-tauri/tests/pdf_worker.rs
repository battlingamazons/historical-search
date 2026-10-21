//! Runs the real program as a PDF reader, the way the application does.
//!
//! The unit tests read PDFs inside the test process, because the test
//! harness is not the application and cannot be started as a reader. What
//! they cannot show is the part that matters most: that a file which stops
//! the reader costs only that file. This starts the built program itself.

#[path = "support/pdf_fixture.rs"]
mod pdf_fixture;

use historical_search_lib::pdf::read_in_worker;
use std::path::PathBuf;
use std::time::Duration;

fn app() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_historical-search"))
}

/// A folder for the PDFs, with the application's own files kept away from
/// the real ones.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hs_worker_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("HS_STATE_DIR", dir.join("state"));
    dir
}

#[test]
fn the_program_reads_a_pdf_when_started_as_a_reader() {
    let dir = scratch("reads");
    let pdf = dir.join("ledger.pdf");
    std::fs::write(&pdf, pdf_fixture::make_pdf(&["Ledger of the Mill Company", "Hartwell accounts"])).unwrap();

    let got = read_in_worker(&pdf, &app(), Duration::from_secs(60)).expect("should read it");
    assert_eq!(got.pages, 2);
    assert!(got.text.contains("Mill Company") && got.text.contains("Hartwell"), "got {:?}", got.text);
}

#[test]
fn a_file_that_is_not_a_pdf_is_an_error_and_the_program_carries_on() {
    let dir = scratch("notpdf");
    let junk = dir.join("junk.pdf");
    std::fs::write(&junk, "nothing like a pdf").unwrap();
    assert!(read_in_worker(&junk, &app(), Duration::from_secs(60)).is_err());

    // The next file is read as normal: nothing was left in a bad state.
    let good = dir.join("good.pdf");
    std::fs::write(&good, pdf_fixture::make_pdf(&["Meeting about the new bridge"])).unwrap();
    assert!(read_in_worker(&good, &app(), Duration::from_secs(60)).unwrap().has_text());
}

#[test]
fn a_reader_that_runs_out_of_time_is_stopped() {
    let dir = scratch("timeout");
    let pdf = dir.join("any.pdf");
    std::fs::write(&pdf, pdf_fixture::make_pdf(&["Anything"])).unwrap();
    let started = std::time::Instant::now();
    let err = read_in_worker(&pdf, &app(), Duration::ZERO).unwrap_err();
    assert!(err.contains("longer than"), "got {err}");
    assert!(started.elapsed() < Duration::from_secs(30), "the wait was not bounded");
}

#[cfg(unix)]
#[test]
fn a_reader_that_dies_leaves_an_error_rather_than_a_silent_blank() {
    let dir = scratch("dies");
    let pdf = dir.join("any.pdf");
    std::fs::write(&pdf, pdf_fixture::make_pdf(&["Anything"])).unwrap();
    let err = read_in_worker(&pdf, std::path::Path::new("/usr/bin/false"), Duration::from_secs(30)).unwrap_err();
    assert!(err.contains("stopped"), "got {err}");
}

#[test]
fn the_program_reads_word_and_text_files_when_started_as_a_reader() {
    use std::io::Write;
    let dir = scratch("words");

    let txt = dir.join("notes.txt");
    std::fs::write(&txt, "Notes on the Hartwell tannery").unwrap();
    let got = read_in_worker(&txt, &app(), Duration::from_secs(60)).expect("should read the text file");
    assert!(got.text.contains("Hartwell tannery"), "got {:?}", got.text);

    let docx = dir.join("minutes.docx");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&docx).unwrap());
    zip.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(b"<w:document><w:body><w:p><w:r><w:t>Bridge fund approved by the trustees</w:t></w:r></w:p></w:body></w:document>").unwrap();
    zip.finish().unwrap();
    let got = read_in_worker(&docx, &app(), Duration::from_secs(60)).expect("should read the Word file");
    assert_eq!(got.text, "Bridge fund approved by the trustees");

    let broken = dir.join("broken.docx");
    std::fs::write(&broken, "not a zip").unwrap();
    let err = read_in_worker(&broken, &app(), Duration::from_secs(60)).unwrap_err();
    assert!(err.contains("not a .docx"), "got {err}");
}
