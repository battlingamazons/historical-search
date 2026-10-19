//! Reading the words off scanned pages -- text recognition, or OCR.
//!
//! Most of a historical society's PDFs are scans: a picture of each page and
//! no text at all, so nothing in them can be searched until something reads
//! the pictures. That is done here with Tesseract, a separate program that
//! the application starts for each page. It was chosen over a recognition
//! library built into the application on the evidence of the society's own
//! kind of page: on a newspaper page, the built-in alternative tried read two
//! columns as one line and garbled more letters.
//!
//! Recognition is slow -- about a second a page -- so what it produces is kept
//! (see `Cache`) and never worked out twice for the same file, and a PDF that
//! is interrupted half-way resumes from the pages already done.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one page is given. A dense newspaper page takes a few seconds;
/// this only exists to stop one that never finishes.
const PAGE_TIME_LIMIT: Duration = Duration::from_secs(180);

/// A Tesseract that can be run: where it is, and the language data it reads.
pub struct Engine {
    exe: PathBuf,
    tessdata: Option<PathBuf>,
    /// Its own description of itself, for the diagnostics.
    pub version: String,
}

impl Engine {
    /// Finds a usable Tesseract, or says why there is none.
    ///
    /// The copy shipped with the application comes first, so what the society
    /// runs is the version it was tested with; after that, one named by
    /// `HS_TESSERACT` (for testing), then whatever is installed on the
    /// machine.
    pub fn locate(resource_dir: Option<&Path>) -> Result<Engine, String> {
        let exe_name = if cfg!(windows) { "tesseract.exe" } else { "tesseract" };
        let mut candidates: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();

        if let Some(base) = resource_dir {
            let base = &plain_path(base);
            let bundled = base.join("ocr");
            candidates.push((bundled.join(exe_name), Some(bundled.join("tessdata"))));
            // Tauri places resource folders one level down on some platforms.
            let nested = base.join("resources").join("ocr");
            candidates.push((nested.join(exe_name), Some(nested.join("tessdata"))));
        }
        if let Some(explicit) = std::env::var_os("HS_TESSERACT").filter(|v| !v.is_empty()) {
            candidates.push((PathBuf::from(explicit), None));
        }
        if let Some(path_var) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_var) {
                candidates.push((dir.join(exe_name), None));
            }
        }
        for fixed in [
            "/opt/homebrew/bin/tesseract",
            "/usr/local/bin/tesseract",
            "/usr/bin/tesseract",
            r"C:\Program Files\Tesseract-OCR\tesseract.exe",
        ] {
            candidates.push((PathBuf::from(fixed), None));
        }

        let mut last_problem = None;
        for (exe, tessdata) in candidates {
            if !exe.is_file() {
                continue;
            }
            let tessdata = tessdata.filter(|d| d.is_dir());
            match Engine::try_out(exe.clone(), tessdata) {
                Ok(engine) => return Ok(engine),
                Err(problem) => last_problem = Some(format!("{}: {problem}", exe.display())),
            }
        }
        Err(last_problem.unwrap_or_else(|| {
            "the text recognition program (Tesseract) was not found on this computer".to_string()
        }))
    }

    fn try_out(exe: PathBuf, tessdata: Option<PathBuf>) -> Result<Engine, String> {
        let mut engine = Engine { exe, tessdata, version: String::new() };
        let version = engine.run_quick(&["--version"])?;
        engine.version = version.lines().next().unwrap_or("").trim().to_string();
        let languages = engine.run_quick(&["--list-langs"])?;
        if !languages.lines().any(|l| l.trim() == "eng") {
            return Err("it has no English language data".to_string());
        }
        Ok(engine)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.exe);
        // One thread per page: several pages are done at once instead, which
        // keeps all the cores busy without each page fighting the others.
        command.env("OMP_THREAD_LIMIT", "1");
        if let Some(data) = &self.tessdata {
            command.env("TESSDATA_PREFIX", data);
        }
        command.stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        command
    }

    fn run_quick(&self, args: &[&str]) -> Result<String, String> {
        let out = self.command().args(args).output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("it exited with {}", out.status));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// The words on one page picture.
    pub fn recognize(&self, image: &Path, dpi: Option<u32>) -> Result<String, String> {
        let mut command = self.command();
        command.arg(image).arg("stdout").args(["-l", "eng"]);
        if let Some(dpi) = dpi {
            command.args(["--dpi", &dpi.to_string()]);
        }
        let mut child = command.spawn().map_err(|e| format!("could not start Tesseract: {e}"))?;

        // Its output is read as it arrives. Left in the pipe it would fill
        // it, and a page with a lot of text would stall Tesseract until the
        // time limit.
        let mut stdout = child.stdout.take().ok_or("no output from Tesseract")?;
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut bytes);
            bytes
        });

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > PAGE_TIME_LIMIT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(format!("Tesseract took longer than {} seconds", PAGE_TIME_LIMIT.as_secs()));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(format!("lost track of Tesseract: {e}")),
            }
        };
        let bytes = reader.join().unwrap_or_default();
        if !status.success() {
            return Err(format!("Tesseract could not read this page ({status})"));
        }
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }
}

/// How many pages are read at once.
fn concurrency() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    // One core is left free so the computer stays usable during a long run.
    cores.saturating_sub(1).clamp(1, 8)
}

/// What came of recognising one PDF.
pub struct Recognized {
    pub text: crate::pdf::PdfText,
    /// Pages that gave no text: a picture in a format that cannot be taken
    /// out, or one Tesseract failed on. The rest of the document is still
    /// searchable, and the record says some pages were not read.
    pub pages_not_read: usize,
    /// Whether this came from the saved results rather than a fresh run.
    pub from_cache: bool,
}

/// Where recognised text is kept between runs. It lives with the settings,
/// not with the disposable caches: it is hours of work, and deleting it
/// should be a decision, not a side effect of clearing previews.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("ocr_cache")
}

/// A name for a file that survives being moved or renamed, without reading
/// all of it -- which, for a book-sized scan on a slow or shared drive, would
/// itself take minutes. Its size, its modification time, and the two ends of
/// its content.
fn fingerprint(path: &Path) -> std::io::Result<String> {
    use sha1::{Digest, Sha1};
    use std::io::{Read, Seek, SeekFrom};

    let meta = std::fs::metadata(path)?;
    let mut hasher = Sha1::new();
    hasher.update(meta.len().to_le_bytes());
    let modified = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    hasher.update(modified.to_le_bytes());

    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = file.read(&mut buf)?;
    hasher.update(&buf[..n]);
    if meta.len() > 128 * 1024 {
        file.seek(SeekFrom::End(-(64 * 1024)))?;
        let n = file.read(&mut buf)?;
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    pages: usize,
    pages_not_read: usize,
    /// The text of each page, in order. Results saved before this existed
    /// lack it and are read again: a search has to say which page a word is
    /// on, and the whole text alone cannot.
    #[serde(default)]
    page_text: Vec<String>,
}

/// Recognises the text of a scanned PDF, or returns what was saved from the
/// last time. `progress` is called with (pages done, pages in all) from the
/// calling thread.
pub fn recognize_pdf(
    engine: &Engine,
    pdf: &Path,
    saved_dir: &Path,
    work_dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Recognized, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let key = fingerprint(pdf).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(saved_dir).map_err(|e| e.to_string())?;
    let saved_path = saved_dir.join(format!("{key}.json"));
    if let Some(saved) = std::fs::read_to_string(&saved_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Saved>(&t).ok())
        .filter(|saved| !saved.page_text.is_empty())
    {
        let text = saved.page_text.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join(" ");
        return Ok(Recognized {
            text: crate::pdf::PdfText { pages: saved.pages, text, page_text: saved.page_text },
            pages_not_read: saved.pages_not_read,
            from_cache: true,
        });
    }

    // Pages are taken out of the PDF once, in a separate process, then read
    // from files: the PDF can be gigabytes and must not stay in memory
    // while Tesseract runs.
    let pictures_dir = work_dir.join(format!("pictures.{}", crate::app_paths::unique_temp_suffix()));
    let _cleanup = RemoveDirOnDrop(&pictures_dir);
    let extracted = crate::pdf::extract_pictures(pdf, &pictures_dir)?;
    let total = extracted.images.len();
    if total == 0 {
        return Err(extracted
            .skipped
            .first()
            .map(|(page, why)| format!("page {page}: {why}"))
            .unwrap_or_else(|| "no page pictures were found in it".to_string()));
    }

    // Pages finished in an earlier, interrupted run.
    let partial_path = saved_dir.join(format!("{key}.partial"));
    let mut done_text: HashMap<u32, String> = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(&partial_path) {
        for line in text.lines() {
            if let Ok((page, body)) = serde_json::from_str::<(u32, String)>(line) {
                done_text.insert(page, body);
            }
        }
    }
    let partial = Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&partial_path)
            .map_err(|e| e.to_string())?,
    );

    let todo: Vec<&crate::scan::PageImage> =
        extracted.images.iter().filter(|p| !done_text.contains_key(&p.page)).collect();
    let results: Mutex<HashMap<u32, Result<String, String>>> = Mutex::new(HashMap::new());
    let next = AtomicUsize::new(0);
    let finished = AtomicUsize::new(total - todo.len());

    progress(finished.load(Ordering::Relaxed) as u64, total as u64);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..concurrency().min(todo.len().max(1)))
            .map(|_| {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(page) = todo.get(i) else { break };
                    let outcome = engine.recognize(&page.file, page.dpi);
                    if let Ok(text) = &outcome {
                        if let Ok(mut file) = partial.lock() {
                            if let Ok(line) = serde_json::to_string(&(page.page, text)) {
                                let _ = writeln!(file, "{line}");
                            }
                        }
                    }
                    results.lock().unwrap().insert(page.page, outcome);
                    finished.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();
        while workers.iter().any(|w| !w.is_finished()) {
            std::thread::sleep(Duration::from_millis(200));
            progress(finished.load(Ordering::Relaxed) as u64, total as u64);
        }
    });
    progress(total as u64, total as u64);

    let mut by_page = results.into_inner().unwrap();
    let mut failures = 0usize;
    // One entry for every page of the document, in order; the pages that had
    // no picture to read stay empty.
    let mut pieces: Vec<String> = vec![String::new(); extracted.total_pages];
    for image in &extracted.images {
        let text = match done_text.remove(&image.page) {
            Some(text) => text,
            None => match by_page.remove(&image.page) {
                Some(Ok(text)) => text,
                _ => {
                    failures += 1;
                    String::new()
                }
            },
        };
        if let Some(slot) = pieces.get_mut(image.page as usize - 1) {
            *slot = text;
        }
    }
    let pages_not_read = failures + extracted.skipped.len();
    if failures == total {
        return Err("Tesseract could not read any of the pages".to_string());
    }

    let tidied = crate::pdf::PdfText::from_pages(&pieces);
    let saved = Saved { pages: extracted.total_pages, pages_not_read, page_text: tidied.page_text.clone() };
    // Written whole and then renamed, so an interruption cannot leave half a
    // result that looks finished.
    let temp = saved_path.with_extension(format!("json.partial.{}", crate::app_paths::unique_temp_suffix()));
    let written = serde_json::to_string(&saved)
        .map_err(|e| e.to_string())
        .and_then(|json| std::fs::write(&temp, json).map_err(|e| e.to_string()))
        .and_then(|_| std::fs::rename(&temp, &saved_path).map_err(|e| e.to_string()));
    match written {
        Ok(()) => {
            drop(partial);
            let _ = std::fs::remove_file(&partial_path);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            crate::diagnostics::warn("ocr", format!("could not save the recognised text of {}: {e}", pdf.display()));
        }
    }
    Ok(Recognized { text: tidied, pages_not_read, from_cache: false })
}

struct RemoveDirOnDrop<'a>(&'a Path);
impl Drop for RemoveDirOnDrop<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}

/// A Windows path without the `\\?\` prefix.
///
/// Windows gives the application its install folder in that long form
/// (`\\?\C:\Program Files\...`), and it is valid for the application's own
/// use. Tesseract is not so tolerant: handed a language-data folder written
/// that way it cannot list or open anything in it and exits with an error, so
/// text recognition was reported as unavailable on every Windows install while
/// working fine from an ordinary path. Programs we start get the plain form.
pub(crate) fn plain_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        // Only a drive path ("C:\...") is safe to shorten; other forms
        // (\\?\Volume{...}\) have no plain equivalent and are left alone.
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interrupted_run_resumes_from_the_pages_already_done() {
        // The one page was finished before the interruption, so nothing is
        // left for Tesseract to do -- which is why a program that does not
        // exist will do as the engine. Had the page been read again, this
        // would fail rather than return the saved text.
        let dir = std::env::temp_dir().join(format!("hs_ocr_resume_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("scan.pdf");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/scan_jpeg.pdf"), &pdf).unwrap();
        let saved = dir.join("saved");
        std::fs::create_dir_all(&saved).unwrap();
        let key = fingerprint(&pdf).unwrap();
        let line = serde_json::to_string(&(1u32, "words saved before the interruption")).unwrap();
        std::fs::write(saved.join(format!("{key}.partial")), format!("{line}\n")).unwrap();

        let engine = Engine { exe: PathBuf::from("/no/such/tesseract"), tessdata: None, version: String::new() };
        let mut last = (0, 0);
        let found = recognize_pdf(&engine, &pdf, &saved, &dir.join("work"), &mut |d, t| last = (d, t)).unwrap();
        assert!(found.text.text.contains("saved before the interruption"), "got {:?}", found.text.text);
        assert_eq!(last, (1, 1));
        assert!(saved.join(format!("{key}.json")).exists(), "the finished result is kept");
        assert!(!saved.join(format!("{key}.partial")).exists(), "and the partial one is cleared");
    }

    #[test]
    fn a_files_fingerprint_follows_its_content_not_its_name() {
        let dir = std::env::temp_dir().join(format!("hs_ocr_fp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a.pdf"), dir.join("renamed.pdf"), dir.join("c.pdf"));
        std::fs::write(&a, vec![7u8; 300_000]).unwrap();
        std::fs::copy(&a, &b).unwrap();
        let mut other = vec![7u8; 300_000];
        let last = other.len() - 1;
        other[last] = 8;
        std::fs::write(&c, other).unwrap();

        // Same size and ends; the copy's modification time differs, so set
        // it to match, as moving a file would have kept it.
        let modified = std::fs::metadata(&a).unwrap().modified().unwrap();
        std::fs::File::options().write(true).open(&b).unwrap().set_modified(modified).unwrap();
        assert_eq!(fingerprint(&a).unwrap(), fingerprint(&b).unwrap(), "a rename must not lose the text");
        assert_ne!(fingerprint(&a).unwrap(), fingerprint(&c).unwrap(), "a different file must not reuse it");
    }

    #[test]
    fn the_long_windows_path_form_is_shortened_for_programs_that_cannot_read_it() {
        let plain = |p: &str| plain_path(Path::new(p)).to_string_lossy().to_string();
        assert_eq!(plain(r"\\?\C:\Program Files\Historical Society Search"), r"C:\Program Files\Historical Society Search");
        assert_eq!(plain(r"\\?\UNC\server\share\app"), r"\\server\share\app");
        // Anything else is left as it was.
        assert_eq!(plain(r"C:\Program Files\App"), r"C:\Program Files\App");
        assert_eq!(plain("/usr/lib/app"), "/usr/lib/app");
        assert_eq!(plain(r"\\?\Volume{1234}\app"), r"\\?\Volume{1234}\app");
    }
}
