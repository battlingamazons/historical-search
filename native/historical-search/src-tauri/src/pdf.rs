//! Reading the text out of PDF files so they can be searched.
//!
//! PDFs are the one kind of input this app does not control at all: they come
//! from scanners, word processors and a hundred other programs, over decades,
//! and a reader that handles nine thousand of them can still meet one that
//! makes it panic, run for an hour, or ask for gigabytes. The release build
//! aborts on a panic, so a bad file read inside the application would close
//! the window -- and, since the next refresh would meet the same file, close
//! it again every time.
//!
//! So the reading is done by a second copy of this very program, started for
//! one file and given a time limit. Whatever that copy does to itself --
//! crash, hang, run out of memory -- costs one PDF its text and nothing more.
//! The file is still listed, and still found by its name.

use std::path::Path;
use std::time::{Duration, Instant};

/// The first argument that turns this program into a PDF reader instead of a
/// window. See `run_worker_if_asked`.
const WORKER_FLAG: &str = "--read-pdf-text";

/// The same, for taking the page pictures out of a scanned PDF.
const PICTURES_FLAG: &str = "--extract-pdf-pictures";

/// Past this size a PDF is listed by name only. It is not a judgement about
/// what is worth reading: the reader loads a file whole, using about two and
/// a half times the file's size in memory (measured), so a scan of a bound
/// volume can be more than a small machine has free. The limit used to be
/// 300 MB, which turned away ordinary scanned newspapers and directories --
/// and, being one setting for everything, reported them as unreadable rather
/// than as what they are.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// From this size a PDF is read on its own, never beside another large one,
/// so that two or three of them together do not ask for gigabytes at once.
const LARGE_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// How long one file is given: a minute and a half, and a second more for
/// every four megabytes, up to a quarter of an hour. Large files really do
/// take longer, and a flat limit either gives up on them or lets a stuck
/// small file run for as long.
fn time_limit_for(size: u64) -> Duration {
    let seconds = 90 + size / (4 * 1024 * 1024);
    Duration::from_secs(seconds.min(15 * 60))
}

/// The most text kept from one PDF. A long book is a few hundred thousand
/// characters; this stops a damaged file that "extracts" without end from
/// swelling the index.
const MAX_TEXT_CHARS: usize = 4_000_000;

/// Fewer letters and digits than this across a whole PDF is not text worth
/// calling text: it is a page number or a stray mark on what is really a
/// scan.
const MIN_MEANINGFUL_CHARS: usize = 20;

#[derive(Debug)]
pub struct PdfText {
    pub pages: usize,
    /// All the text, whitespace collapsed to single spaces. Empty for a PDF
    /// that is only pictures of pages.
    pub text: String,
}

impl PdfText {
    pub fn has_text(&self) -> bool {
        !self.text.is_empty()
    }
}

/// Collapses every run of whitespace (including the line breaks and runs of
/// padding PDF text comes with) to one space, drops control characters, and
/// stops at the size limit. Returns an empty string when what is left is too
/// little to count as text.
pub(crate) fn tidy(raw: &str) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut meaningful = 0usize;
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() || ch.is_control() {
            pending_space = true;
            continue;
        }
        if ch == '\u{fffd}' {
            continue;
        }
        // Room for a separating space as well as the character, so the limit
        // is a limit rather than a target that is overshot by one.
        if count + 2 > MAX_TEXT_CHARS {
            break;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
            count += 1;
        }
        pending_space = false;
        out.push(ch);
        count += 1;
        if ch.is_alphanumeric() {
            meaningful += 1;
        }
    }
    if meaningful < MIN_MEANINGFUL_CHARS {
        String::new()
    } else {
        out
    }
}

/// Reads one PDF in this process. Only the worker, and the tests, call this
/// directly -- everything else goes through `read`.
fn read_here(path: &Path) -> Result<PdfText, String> {
    let pages = std::panic::catch_unwind(|| pdf_extract::extract_text_by_pages(path))
        .map_err(|_| "the PDF reader failed on this file".to_string())?
        .map_err(|e| e.to_string())?;
    let count = pages.len();
    Ok(PdfText { pages: count, text: tidy(&pages.join(" ")) })
}

/// The program to start as a reader: this one. `HS_WORKER_EXE` names another,
/// for the integration tests, where the running program is the test harness
/// and not the application.
fn worker_exe() -> Result<std::path::PathBuf, String> {
    if let Some(explicit) = std::env::var_os("HS_WORKER_EXE").filter(|v| !v.is_empty()) {
        return Ok(explicit.into());
    }
    std::env::current_exe().map_err(|e| e.to_string())
}

/// Reads one PDF's text, safely. `Err` carries a short reason that is fit to
/// show on screen.
pub fn read(path: &Path) -> Result<PdfText, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "the file is {} MB, which is larger than the {} MB this app will read for text",
            size / (1024 * 1024),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    // Under test the program running is the test harness, not this
    // application, so it cannot be started as a reader. The separate-process
    // path is covered by tests/pdf_worker.rs against the real binary.
    if cfg!(test) {
        return read_here(path);
    }
    // Held for the length of a large file's read, so only one is in memory
    // at a time; small files carry on in parallel around it.
    static LARGE_READS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_large_at_a_time = (size > LARGE_FILE_BYTES)
        .then(|| LARGE_READS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    read_in_worker(path, &worker_exe()?, time_limit_for(size))
}

/// How many PDFs are read at once. Each is a separate process with its own
/// memory, so this is kept well below the number of cores: a few at a time is
/// most of the gain, and a large scan being read by every core at once is how
/// a small machine runs out of memory.
fn concurrency() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    (cores / 2).clamp(1, 4)
}

/// Reads every PDF in `paths`, several at a time, returning the results in
/// the same order. `progress` is called with (done, total) from the calling
/// thread only, so it need not be thread-safe.
pub fn read_many(
    paths: &[std::path::PathBuf],
    progress: &mut dyn FnMut(u64, u64),
) -> Vec<Result<PdfText, String>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let total = paths.len();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Result<PdfText, String>>>> =
        Mutex::new((0..total).map(|_| None).collect());

    progress(0, total as u64);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..concurrency().min(total.max(1)))
            .map(|_| {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(i) else { break };
                    let result = read(path);
                    results.lock().unwrap()[i] = Some(result);
                    done.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();
        while workers.iter().any(|w| !w.is_finished()) {
            std::thread::sleep(Duration::from_millis(100));
            progress(done.load(Ordering::Relaxed) as u64, total as u64);
        }
    });
    progress(total as u64, total as u64);

    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err("the PDF was never read".to_string())))
        .collect()
}

/// Starts `exe` as a reader for `path` and waits up to `limit` for it.
pub fn read_in_worker(path: &Path, exe: &Path, limit: Duration) -> Result<PdfText, String> {
    let out_path = crate::app_paths::cache_dir()
        .join(format!("pdf-text.{}.tmp", crate::app_paths::unique_temp_suffix()));
    let _cleanup = RemoveOnDrop(&out_path);

    let status = run_child(exe, &[WORKER_FLAG.as_ref(), path.as_os_str(), out_path.as_os_str()], limit)?;
    let report = std::fs::read_to_string(&out_path).unwrap_or_default();
    parse_report(&report).unwrap_or_else(|| {
        Err(if status.success() {
            "the PDF reader left no result".to_string()
        } else {
            "the PDF reader stopped on this file".to_string()
        })
    })
}

/// Runs `exe` with `args`, without a window or console, and waits up to
/// `limit` for it to finish -- after which it is stopped.
fn run_child(
    exe: &Path,
    args: &[&std::ffi::OsStr],
    limit: Duration,
) -> Result<std::process::ExitStatus, String> {
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        // The program is a windowed one, so no console appears by itself, but
        // say so anyway: a flash of black window per PDF would be a poor way
        // to find out otherwise.
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|e| format!("could not start the PDF reader: {e}"))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("reading it took longer than {} seconds", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("lost track of the PDF reader: {e}")),
        }
    }
}

/// Takes the page pictures out of a scanned PDF into `out_dir`, safely.
pub fn extract_pictures(path: &Path, out_dir: &Path) -> Result<crate::scan::Extracted, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_FILE_BYTES {
        return Err(format!("the file is {} MB, which is larger than this app will read", size / (1024 * 1024)));
    }
    if cfg!(test) {
        return crate::scan::extract_here(path, out_dir);
    }
    extract_pictures_in_worker(path, out_dir, &worker_exe()?, time_limit_for(size))
}

pub fn extract_pictures_in_worker(
    path: &Path,
    out_dir: &Path,
    exe: &Path,
    limit: Duration,
) -> Result<crate::scan::Extracted, String> {
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let manifest = out_dir.join("manifest.txt");
    let status = run_child(
        exe,
        &[PICTURES_FLAG.as_ref(), path.as_os_str(), out_dir.as_os_str(), manifest.as_os_str()],
        limit,
    )?;
    let text = std::fs::read_to_string(&manifest).unwrap_or_default();
    parse_manifest(&text, out_dir).unwrap_or_else(|| {
        Err(if status.success() {
            "the PDF reader left no result".to_string()
        } else {
            "the PDF reader stopped on this file".to_string()
        })
    })
}

/// The picture extractor's answer: `OK <pages>` or `ERR <reason>`, then a
/// line per page -- `PIC<TAB>page<TAB>file<TAB>dpi-or-dash` or
/// `SKIP<TAB>page<TAB>reason`.
fn parse_manifest(text: &str, dir: &Path) -> Option<Result<crate::scan::Extracted, String>> {
    let mut lines = text.lines();
    let first = lines.next()?;
    if let Some(reason) = first.strip_prefix("ERR ") {
        return Some(Err(reason.to_string()));
    }
    let total_pages: usize = first.strip_prefix("OK ")?.trim().parse().ok()?;
    let mut extracted = crate::scan::Extracted { total_pages, images: Vec::new(), skipped: Vec::new() };
    for line in lines {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.as_slice() {
            ["PIC", page, file, dpi] => extracted.images.push(crate::scan::PageImage {
                page: page.parse().ok()?,
                file: dir.join(file),
                dpi: dpi.parse().ok(),
            }),
            ["SKIP", page, reason] => extracted.skipped.push((page.parse().ok()?, reason.to_string())),
            _ => return None,
        }
    }
    Some(Ok(extracted))
}

struct RemoveOnDrop<'a>(&'a Path);
impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// The worker's answer, as one text file: a first line of `OK <pages>` or
/// `ERR <reason>`, then the text.
fn parse_report(report: &str) -> Option<Result<PdfText, String>> {
    let (first, rest) = report.split_once('\n').unwrap_or((report, ""));
    if let Some(pages) = first.strip_prefix("OK ") {
        return Some(Ok(PdfText { pages: pages.trim().parse().ok()?, text: rest.to_string() }));
    }
    first.strip_prefix("ERR ").map(|reason| Err(reason.to_string()))
}

/// Turns this process into a PDF reader when it was started as one. Called
/// first thing in `main`, before any window exists. Returns normally when
/// this is an ordinary start.
pub fn run_worker_if_asked() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match args.get(1).and_then(|a| a.to_str()) {
        Some(WORKER_FLAG) => {
            let (Some(input), Some(output)) = (args.get(2), args.get(3)) else {
                std::process::exit(2);
            };
            let report = match read_here(Path::new(input)) {
                Ok(t) => format!("OK {}\n{}", t.pages, t.text),
                Err(reason) => format!("ERR {}", reason.replace('\n', " ")),
            };
            let code = if std::fs::write(output, report).is_ok() { 0 } else { 2 };
            std::process::exit(code);
        }
        Some(PICTURES_FLAG) => {
            let (Some(input), Some(dir), Some(manifest)) = (args.get(2), args.get(3), args.get(4)) else {
                std::process::exit(2);
            };
            let report = match crate::scan::extract_here(Path::new(input), Path::new(dir)) {
                Ok(e) => {
                    let mut text = format!("OK {}\n", e.total_pages);
                    for img in &e.images {
                        let name = img.file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        let dpi = img.dpi.map(|d| d.to_string()).unwrap_or_else(|| "-".into());
                        text.push_str(&format!("PIC\t{}\t{name}\t{dpi}\n", img.page));
                    }
                    for (page, reason) in &e.skipped {
                        text.push_str(&format!("SKIP\t{page}\t{}\n", reason.replace(['\n', '\t'], " ")));
                    }
                    text
                }
                Err(reason) => format!("ERR {}", reason.replace('\n', " ")),
            };
            let code = if std::fs::write(manifest, report).is_ok() { 0 } else { 2 };
            std::process::exit(code);
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "../tests/support/pdf_fixture.rs"]
pub mod fixture;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigger_files_get_longer_and_it_is_bounded() {
        assert_eq!(time_limit_for(1024), Duration::from_secs(90));
        assert_eq!(time_limit_for(400 * 1024 * 1024), Duration::from_secs(190));
        assert_eq!(time_limit_for(MAX_FILE_BYTES), Duration::from_secs(602));
        assert_eq!(time_limit_for(u64::MAX), Duration::from_secs(900));
    }

    #[test]
    fn whitespace_is_collapsed_and_padding_dropped() {
        let raw = "The Great   Flood\n\nof 1913        swept\tthrough the mill district.   ";
        assert_eq!(tidy(raw), "The Great Flood of 1913 swept through the mill district.");
    }

    #[test]
    fn a_page_number_on_a_scan_is_not_text() {
        assert_eq!(tidy("  \n 14 \n"), "");
        assert_eq!(tidy(""), "");
    }

    #[test]
    fn replacement_characters_from_unreadable_fonts_are_dropped() {
        assert_eq!(tidy("Hartwell \u{fffd}\u{fffd} family accounts for the year"), "Hartwell family accounts for the year");
    }

    #[test]
    fn text_is_cut_off_at_the_limit() {
        let long = "word ".repeat(MAX_TEXT_CHARS);
        assert!(tidy(&long).chars().count() <= MAX_TEXT_CHARS);
    }

    #[test]
    fn the_workers_answer_round_trips() {
        let ok = parse_report("OK 3\nledger of the mill").unwrap().unwrap();
        assert_eq!((ok.pages, ok.text.as_str()), (3, "ledger of the mill"));
        assert_eq!(parse_report("ERR not a PDF").unwrap().unwrap_err(), "not a PDF");
        assert!(parse_report("").is_none(), "no answer must not read as an empty success");
        assert!(parse_report("OK many\ntext").is_none());
    }

    #[test]
    fn many_pdfs_come_back_in_order_with_progress_that_ends_complete() {
        let dir = std::env::temp_dir().join(format!("hs_pdf_many_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for i in 0..9 {
            let path = dir.join(format!("doc{i}.pdf"));
            if i == 4 {
                std::fs::write(&path, "not a pdf").unwrap();
            } else {
                std::fs::write(&path, fixture::make_pdf(&[&format!("Document number {i} of the collection")])).unwrap();
            }
            paths.push(path);
        }
        let mut last = (0, 0);
        let results = read_many(&paths, &mut |done, total| last = (done, total));
        assert_eq!(results.len(), 9);
        for (i, r) in results.iter().enumerate() {
            if i == 4 {
                assert!(r.is_err(), "the broken one stays in its own place");
            } else {
                assert!(r.as_ref().unwrap().text.contains(&format!("number {i} of")), "doc {i}: {r:?}");
            }
        }
        assert_eq!(last, (9, 9));
        assert!(read_many(&[], &mut |_, _| {}).is_empty());
    }

    #[test]
    fn a_file_that_is_not_a_pdf_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("hs_pdf_notapdf_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fake.pdf");
        std::fs::write(&path, "this is not a pdf").unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn text_comes_out_of_a_real_pdf_with_its_page_count() {
        let dir = std::env::temp_dir().join(format!("hs_pdf_real_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.pdf");
        std::fs::write(
            &path,
            fixture::make_pdf(&["Ledger of the Mill Company 1887", "The Hartwell family accounts (summer)"]),
        )
        .unwrap();
        let got = read(&path).unwrap();
        assert_eq!(got.pages, 2);
        assert!(got.text.contains("Mill Company 1887"), "got {:?}", got.text);
        assert!(got.text.contains("Hartwell family accounts (summer)"), "got {:?}", got.text);
    }

    #[test]
    fn every_line_is_read_when_a_pdf_writes_them_with_the_next_line_operator() {
        // The reader this uses dropped these lines without a word, which
        // would have left a PDF half-searchable with nothing to say so.
        let dir = std::env::temp_dir().join(format!("hs_pdf_quote_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lines.pdf");
        std::fs::write(
            &path,
            fixture::make_pdf_with_next_line_operator(&[
                "Minutes of the annual meeting",
                "The bridge committee reviewed the petition",
                "Hartwell family donated the loom",
            ]),
        )
        .unwrap();
        let got = read(&path).unwrap();
        for wanted in ["annual meeting", "bridge committee", "Hartwell family"] {
            assert!(got.text.contains(wanted), "lost {wanted:?}; got {:?}", got.text);
        }
    }

    #[test]
    fn a_pdf_with_no_text_layer_reports_no_text() {
        let dir = std::env::temp_dir().join(format!("hs_pdf_scan_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scan.pdf");
        std::fs::write(&path, fixture::make_pdf(&["", ""])).unwrap();
        let got = read(&path).unwrap();
        assert_eq!(got.pages, 2);
        assert!(!got.has_text());
    }
}
