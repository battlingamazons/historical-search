//! Runs the real program as a workbook reader, the way the application does.
//!
//! The unit tests read workbooks inside the test process. What they cannot
//! show is that a file which stops the reader costs only that file; this
//! starts the built program itself (see pdf_worker.rs, which does the same
//! for PDFs).

use historical_search_lib::sheets::read_in_worker;
use std::path::PathBuf;
use std::time::Duration;

fn app() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_historical-search"))
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support").join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hs_sheetworker_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("HS_STATE_DIR", dir.join("state"));
    dir
}

#[test]
fn the_program_reads_a_workbook_when_started_as_a_reader() {
    scratch("reads");
    let book = read_in_worker(&fixture("register.xlsx"), &app(), Duration::from_secs(60)).expect("should read it");
    assert_eq!(book.sheets[0].rows[1].cells[1], "Thaddeus Marsh");
    let old = read_in_worker(&fixture("donors.xls"), &app(), Duration::from_secs(60)).expect("should read it");
    assert_eq!(old.sheets[0].columns, ["Donor", "Gift", "Year"]);
}

#[test]
fn a_file_that_is_not_a_workbook_is_an_error_and_the_program_carries_on() {
    let dir = scratch("notbook");
    let junk = dir.join("junk.xlsx");
    std::fs::write(&junk, "nothing like a workbook").unwrap();
    assert!(read_in_worker(&junk, &app(), Duration::from_secs(60)).is_err());
    assert!(read_in_worker(&fixture("donors.xls"), &app(), Duration::from_secs(60)).is_ok());
}

#[test]
fn a_reader_that_runs_out_of_time_is_stopped() {
    scratch("timeout");
    let err = read_in_worker(&fixture("register.xlsx"), &app(), Duration::ZERO).unwrap_err();
    assert!(err.contains("longer than"), "got {err}");
}
