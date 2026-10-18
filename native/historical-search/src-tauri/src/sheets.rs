//! Reading Excel workbooks so each row can be searched as a record.
//!
//! Like PDFs, workbooks come from outside and from every era (.xls goes back
//! to the 1990s), and the release build aborts on a panic. So the reading is
//! done by a second copy of this program started for one file with a time
//! limit (see `pdf.rs`, which this follows): a workbook that crashes, hangs
//! or exhausts memory costs that one file its rows, not the application.
//!
//! What comes back is plain text. Dates, numbers and formulas are reduced to
//! what Excel would show, because a search matches what a person sees.

use calamine::{Data, Reader, SheetVisible};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// The first argument that turns this program into a workbook reader.
const WORKER_FLAG: &str = "--read-spreadsheet";

/// Past this a workbook is not read. An .xlsx is a zip of XML and is held
/// whole while it is read, so a very large one needs far more memory than its
/// size on disk.
const MAX_FILE_BYTES: u64 = 200 * 1024 * 1024;

/// Rows kept from one workbook. A catalogue of a society's holdings is
/// thousands of rows; a million-row export is a database and would make the
/// index slow to build and to search.
const MAX_ROWS: usize = 500_000;

const TIME_LIMIT: Duration = Duration::from_secs(300);

/// One row of a sheet that has something in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// The row's number as Excel shows it, so a record can say where to look.
    pub number: u32,
    /// One entry per column of the sheet; empty where the cell is.
    pub cells: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sheet {
    pub name: String,
    /// Column headings, one per column, never empty or repeated.
    pub columns: Vec<String>,
    /// Text found above the headings (a title line), searchable with each row.
    pub preamble: String,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Workbook {
    pub sheets: Vec<Sheet>,
    /// Sheets the author hid. They are usually lookup lists, not records, and
    /// are left out.
    pub hidden_sheets: usize,
    /// Rows not kept because of `MAX_ROWS`.
    pub rows_dropped: usize,
}

/// Reads one workbook, safely. `Err` is a short reason fit to show.
pub fn read(path: &Path) -> Result<Workbook, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "the file is {} MB, which is larger than the {} MB this app will read",
            size / (1024 * 1024),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    if cfg!(test) {
        return read_here(path);
    }
    read_in_worker(path, &crate::pdf::worker_exe()?, TIME_LIMIT)
}

pub fn read_in_worker(path: &Path, exe: &Path, limit: Duration) -> Result<Workbook, String> {
    let out_path = crate::app_paths::cache_dir()
        .join(format!("sheet.{}.tmp", crate::app_paths::unique_temp_suffix()));
    let _cleanup = crate::pdf::RemoveOnDrop(&out_path);
    let status = crate::pdf::run_child(
        exe,
        &[WORKER_FLAG.as_ref(), path.as_os_str(), out_path.as_os_str()],
        limit,
    )?;
    let report = std::fs::read_to_string(&out_path).unwrap_or_default();
    parse_report(&report).unwrap_or_else(|| {
        Err(if status.success() {
            "the spreadsheet reader left no result".to_string()
        } else {
            "the spreadsheet reader stopped on this file".to_string()
        })
    })
}

/// The worker's answer: `OK` and a line of JSON, or `ERR` and a reason.
fn parse_report(report: &str) -> Option<Result<Workbook, String>> {
    if let Some(reason) = report.strip_prefix("ERR ") {
        return Some(Err(reason.trim().to_string()));
    }
    let json = report.strip_prefix("OK\n")?;
    Some(serde_json::from_str(json).map_err(|e| format!("the spreadsheet reader's answer was garbled ({e})")))
}

/// Called first thing in `main`: when started as a reader, reads the file and
/// exits instead of opening a window.
pub fn run_worker_if_asked() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).and_then(|a| a.to_str()) != Some(WORKER_FLAG) {
        return;
    }
    let (Some(input), Some(output)) = (args.get(2), args.get(3)) else {
        std::process::exit(2);
    };
    let report = match read_here(Path::new(input)) {
        Ok(book) => format!("OK\n{}", serde_json::to_string(&book).unwrap_or_default()),
        Err(reason) => format!("ERR {}", reason.replace('\n', " ")),
    };
    std::process::exit(if std::fs::write(output, report).is_ok() { 0 } else { 2 });
}

fn read_here(path: &Path) -> Result<Workbook, String> {
    let mut workbook = calamine::open_workbook_auto(path).map_err(friendly)?;
    let metadata = workbook.sheets_metadata().to_vec();
    let mut book = Workbook::default();
    let mut kept = 0usize;
    for meta in metadata {
        if meta.visible != SheetVisible::Visible {
            book.hidden_sheets += 1;
            continue;
        }
        let range = match workbook.worksheet_range(&meta.name) {
            Ok(range) => range,
            // One unreadable sheet should not cost the others.
            Err(_) => continue,
        };
        // The range may not start at A1; rows are numbered as Excel shows them.
        let (first_row, first_col) = range.start().unwrap_or((0, 0));
        let mut grid: Vec<(u32, Vec<String>)> = Vec::new();
        for (i, cells) in range.rows().enumerate() {
            let mut texts: Vec<String> = cells.iter().map(cell_text).collect();
            while texts.last().is_some_and(String::is_empty) {
                texts.pop();
            }
            if texts.is_empty() {
                continue;
            }
            if kept >= MAX_ROWS {
                book.rows_dropped += 1;
                continue;
            }
            kept += 1;
            // Pad the front so column positions agree between rows even when
            // the used range starts to the right of column A.
            let mut padded = vec![String::new(); first_col as usize];
            padded.extend(texts);
            grid.push((first_row + i as u32 + 1, padded));
        }
        if let Some(sheet) = shape(&meta.name, grid) {
            book.sheets.push(sheet);
        }
    }
    Ok(book)
}

fn friendly(e: calamine::Error) -> String {
    let text = e.to_string();
    // The two likeliest causes get words a person can act on.
    if text.to_lowercase().contains("password") || text.to_lowercase().contains("encrypt") {
        "the workbook is password-protected".to_string()
    } else {
        format!("it could not be opened as a workbook ({text})")
    }
}

/// What Excel would show for a cell.
fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => float_text(*f),
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Data::DateTime(d) => match d.as_datetime() {
            Some(dt) if dt.format("%H:%M:%S").to_string() == "00:00:00" => dt.format("%Y-%m-%d").to_string(),
            Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
            None => float_text(d.as_f64()),
        },
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.trim().to_string(),
        // "#N/A" and friends are what the cell shows, but they are not
        // something anyone searches for.
        Data::Error(_) => String::new(),
    }
}

/// 1987 rather than 1987.0, and no float noise (0.1+0.2) on the long ones.
fn float_text(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        let s = format!("{f:.10}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Whether a cell reads as a number, for telling a heading row from data.
fn looks_numeric(text: &str) -> bool {
    let t = text.trim().trim_start_matches(['$', '£', '€']).replace(',', "");
    !t.is_empty() && t.parse::<f64>().is_ok()
}

/// Turns a sheet's rows into headings and records.
///
/// The headings are the first row with two or more cells filled in (so a title
/// line above them is not mistaken for headings), unless that row is mostly
/// numbers -- then it is data, and the columns are named by letter.
fn shape(name: &str, grid: Vec<(u32, Vec<String>)>) -> Option<Sheet> {
    if grid.is_empty() {
        return None;
    }
    let filled = |cells: &[String]| cells.iter().filter(|c| !c.is_empty()).count();
    let width = grid.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
    let header_at = grid.iter().position(|(_, c)| filled(c) >= 2).unwrap_or(0);
    let (_, head) = &grid[header_at];
    let numeric = head.iter().filter(|c| looks_numeric(c)).count();
    let has_headings = numeric * 2 < filled(head);

    let preamble = if has_headings {
        grid[..header_at]
            .iter()
            .flat_map(|(_, c)| c.iter().filter(|s| !s.is_empty()).cloned())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        String::new()
    };

    let columns = unique_columns(if has_headings { head } else { &[] }, width);
    let skip = if has_headings { header_at + 1 } else { 0 };
    let rows: Vec<Row> = grid
        .into_iter()
        .skip(skip)
        .map(|(number, mut cells)| {
            cells.resize(width, String::new());
            Row { number, cells }
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    Some(Sheet { name: name.to_string(), columns, preamble, rows })
}

/// Headings that are never empty and never repeat, so each can name a field.
/// Words reserved for the facts the app adds about a row are avoided too.
fn unique_columns(head: &[String], width: usize) -> Vec<String> {
    const RESERVED: &[&str] = &["SPREADSHEET", "SHEET", "ROW", "__order"];
    let mut seen = std::collections::HashSet::new();
    (0..width)
        .map(|i| {
            let mut name = head.get(i).map(|h| h.trim().to_string()).unwrap_or_default();
            if name.is_empty() {
                name = format!("Column {}", column_letters(i));
            }
            if RESERVED.contains(&name.as_str()) {
                name.push_str(" (column)");
            }
            let base = name.clone();
            let mut n = 2;
            while !seen.insert(name.clone()) {
                name = format!("{base} ({n})");
                n += 1;
            }
            name
        })
        .collect()
}

/// A, B, ... Z, AA, AB ...
fn column_letters(mut i: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push((b'A' + (i % 26) as u8) as char);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    letters.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support").join(name)
    }

    #[test]
    fn a_workbook_is_read_with_its_headings_and_a_title_line_above_them() {
        let book = read(&fixture("register.xlsx")).unwrap();
        let register = &book.sheets[0];
        assert_eq!(register.name, "Register");
        assert_eq!(register.columns, ["Accession No.", "Name", "Died", "Plot", "Age", "Notes"]);
        assert_eq!(register.preamble, "Village Cemetery Register, copied 1987");
        // The blank row between records is not a record.
        assert_eq!(register.rows.len(), 3);
        assert_eq!(register.rows[0].cells[1], "Eleanor Whitcombe");
        // As Excel numbers it, not as the file's own count of the row.
        assert_eq!(register.rows[0].number, 3);
        assert_eq!(register.rows[2].number, 6);
    }

    #[test]
    fn dates_and_numbers_read_the_way_excel_shows_them() {
        let book = read(&fixture("register.xlsx")).unwrap();
        let rows = &book.sheets[0].rows;
        assert_eq!(rows[0].cells[2], "1921-03-04");
        assert_eq!(rows[0].cells[4], "78", "78, not 78.0");
        assert_eq!(rows[1].cells[4], "64.5");
        assert_eq!(rows[2].cells[2], "", "an empty cell stays empty");
    }

    #[test]
    fn hidden_sheets_are_left_out_and_counted() {
        let book = read(&fixture("register.xlsx")).unwrap();
        let names: Vec<_> = book.sheets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Register", "Veterans"]);
        assert_eq!(book.hidden_sheets, 1);
    }

    #[test]
    fn the_older_xls_format_is_read_too() {
        let book = read(&fixture("donors.xls")).unwrap();
        let sheet = &book.sheets[0];
        assert_eq!(sheet.columns, ["Donor", "Gift", "Year"]);
        assert_eq!(sheet.rows[0].cells, ["Mrs. Annabel Quint", "Quilt, wedding ring pattern", "1962"]);
    }

    #[test]
    fn a_file_that_is_not_a_workbook_is_an_error() {
        let path = std::env::temp_dir().join(format!("hs_notbook_{}.xlsx", std::process::id()));
        std::fs::write(&path, "just some text").unwrap();
        assert!(read(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    fn grid(rows: &[&[&str]]) -> Vec<(u32, Vec<String>)> {
        rows.iter().enumerate().map(|(i, r)| (i as u32 + 1, r.iter().map(|c| c.to_string()).collect())).collect()
    }

    #[test]
    fn a_sheet_of_numbers_has_no_headings_to_use() {
        let sheet = shape("Counts", grid(&[&["1920", "14"], &["1930", "22"]])).unwrap();
        assert_eq!(sheet.columns, ["Column A", "Column B"]);
        assert_eq!(sheet.rows.len(), 2, "the first row is data");
    }

    #[test]
    fn repeated_and_missing_headings_are_made_distinct() {
        let sheet = shape("S", grid(&[&["Name", "Name", "", "ROW"], &["a", "b", "c", "d"]])).unwrap();
        assert_eq!(sheet.columns, ["Name", "Name (2)", "Column C", "ROW (column)"]);
    }

    #[test]
    fn a_heading_row_alone_has_no_records() {
        assert!(shape("S", grid(&[&["Name", "Date"]])).is_none());
    }

    #[test]
    fn columns_past_z_are_lettered_like_excel() {
        assert_eq!(
            [0, 25, 26, 27, 701, 702].map(column_letters),
            ["A", "Z", "AA", "AB", "ZZ", "AAA"]
        );
    }

    #[test]
    fn the_workers_answer_round_trips() {
        let book = read(&fixture("donors.xls")).unwrap();
        let report = format!("OK\n{}", serde_json::to_string(&book).unwrap());
        assert_eq!(parse_report(&report).unwrap().unwrap(), book);
        assert_eq!(parse_report("ERR it broke").unwrap().unwrap_err(), "it broke");
        assert!(parse_report("").is_none());
    }
}
