//! Diagnostics for problems that happen on a machine nobody can inspect at
//! the time -- specifically, this app running on Windows when it has only
//! ever been built and tested on a Mac.
//!
//! Two halves:
//!
//! * a small, size-capped log file written beside the config, recording what
//!   the app did and -- more importantly -- everything it would otherwise
//!   have swallowed. Most failures in this codebase degrade to `None` or an
//!   empty result, which on an unfamiliar platform looks identical to "there
//!   is nothing there" and leaves nothing to work from.
//!
//! * an on-demand report (`report()`) gathering the environment, the resolved
//!   paths, what the configured folders actually contain, the shape of the
//!   index, a set of live probes, and the tail of the log -- as one block of
//!   text the user can copy out and send on.
//!
//! Nothing in here may panic. The release profile is built with
//! `panic = "abort"`, so a panic inside the diagnostics would take the whole
//! app down and destroy the very information it exists to collect. Every
//! filesystem call is therefore ignored or defaulted rather than unwrapped.

use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Rotate at this size, keeping one previous file. Big enough to hold a full
/// index rebuild's worth of messages, small enough to paste into an email.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
/// How many identical problems to record before summarising. A collection
/// where every image fails would otherwise write thousands of identical lines and
/// push the useful history out of the file.
const MAX_REPEATS: u64 = 10;
const LOG_TAIL_LINES: usize = 300;

pub fn log_path() -> PathBuf {
    // The test suite exercises the very code paths that log, and several
    // tests deliberately provoke failures. Writing those into the real log
    // would corrupt the one artifact this module exists to produce, so tests
    // get their own file.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("hs_test_diagnostics_{}.log", std::process::id()));
    }
    crate::app_paths::data_dir().join("diagnostics.log")
}

/// Derived from `log_path` rather than rebuilt independently, so the two can
/// never disagree about which folder the log lives in. They did: under test
/// the live log is redirected to a temporary file while this was still
/// pointing at the real data folder, so a rotation would have moved a test's
/// log into the user's own diagnostics.
fn rotated_log_path() -> PathBuf {
    log_path().with_extension("prev.log")
}

fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn repeat_counts() -> &'static Mutex<HashMap<String, u64>> {
    static COUNTS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------- writing

/// How much of one message to keep. Long is fine -- a panic backtrace runs to
/// a few thousand characters and is worth every one -- but a single runaway
/// message should not be able to fill the file and push the history out.
const MAX_MESSAGE_BYTES: usize = 16 * 1024;

/// Makes a message safe to put in the log without it being able to invent
/// entries.
///
/// Messages carry file paths, and a file name may contain a newline on macOS
/// and Linux; the window can also send text of its own. Written straight out,
/// anything containing a newline produces further lines that look exactly
/// like real entries -- which would mean reading a diagnostic report and
/// chasing something that never happened, or missing something that did.
///
/// Genuinely multi-line messages are wanted (a rebuild lists its folders, a
/// panic its backtrace), so they are kept and indented instead: a continuation
/// line can then never begin where a timestamp begins.
fn sanitize_for_log(message: &str) -> String {
    let mut truncated = message;
    let mut suffix = "";
    if truncated.len() > MAX_MESSAGE_BYTES {
        // Cut on a character boundary, not a byte one.
        let mut end = MAX_MESSAGE_BYTES;
        while end > 0 && !truncated.is_char_boundary(end) {
            end -= 1;
        }
        truncated = &truncated[..end];
        suffix = " …(message truncated)";
    }

    let mut out = String::with_capacity(truncated.len() + suffix.len());
    for (index, line) in truncated.split('\n').enumerate() {
        if index > 0 {
            out.push_str("\n    ");
        }
        for ch in line.chars() {
            // Tab is left alone; everything else that could move the cursor
            // around or terminate a line is written out visibly.
            if ch == '\t' {
                out.push(ch);
            } else if ch.is_control() {
                out.push_str(&format!("\\u{{{:04x}}}", ch as u32));
            } else {
                out.push(ch);
            }
        }
    }
    out.push_str(suffix);
    out
}

fn append_line(level: &str, area: &str, message: &str) {
    let line = format!("{} {level:<5} [{area}] {}\n", timestamp(), sanitize_for_log(message));

    // A poisoned lock means another thread panicked mid-write. The log is
    // still the best record of that, so carry on with the guard either way.
    let _guard = match write_lock().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };

    let path = log_path();
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() + line.len() as u64 > MAX_LOG_BYTES {
            let _ = std::fs::remove_file(rotated_log_path());
            let _ = std::fs::rename(&path, rotated_log_path());
        }
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
        // Flushed every line on purpose: the messages that matter most are
        // the ones written immediately before a crash.
        let _ = file.flush();
    }
}

pub fn info(area: &str, message: impl AsRef<str>) {
    append_line("INFO", area, message.as_ref());
}

pub fn warn(area: &str, message: impl AsRef<str>) {
    append_line("WARN", area, message.as_ref());
}

pub fn error(area: &str, message: impl AsRef<str>) {
    append_line("ERROR", area, message.as_ref());
}

/// What to do with the nth occurrence of a repeating problem.
#[derive(Debug, PartialEq, Eq)]
enum RepeatAction {
    /// Write it out in full.
    Write,
    /// Write it out, and say that further ones will only be counted.
    WriteThenQuieten,
    /// Note the running total, so a systematic failure stays visible.
    Tally,
    /// Count it, but say nothing.
    Stay,
}

/// Kept apart from the counting itself so the policy can be checked directly,
/// without depending on process-wide state that other work resets.
fn repeat_action(count: u64) -> RepeatAction {
    if count < MAX_REPEATS {
        RepeatAction::Write
    } else if count == MAX_REPEATS {
        RepeatAction::WriteThenQuieten
    } else if count.is_multiple_of(1000) {
        RepeatAction::Tally
    } else {
        RepeatAction::Stay
    }
}

fn bump(counts: &mut HashMap<String, u64>, key: &str) -> u64 {
    let entry = counts.entry(key.to_string()).or_insert(0);
    *entry += 1;
    *entry
}

/// Like `error_repeating`, but for a recurring situation that is expected
/// rather than wrong -- so a report about it does not read as a fault.
pub fn info_repeating(key: &str, area: &str, message: impl AsRef<str>) {
    repeating("INFO", key, area, message);
}

/// Records a standing fact once per run, however many times it is noticed.
///
/// For a state rather than an event: on a newly installed copy there is no
/// index, and the window asks about it twice a second for as long as the first
/// rebuild takes. Counted as a repeating problem, that produced a "further
/// problems will be counted" notice and a tally climbing into the thousands
/// -- which reads like something going badly wrong, in a log whose whole
/// purpose is to be read by someone looking for what went wrong. Saying it
/// once is the whole of the information.
/// Counted apart from the repeating *problems*, not alongside them: the report
/// lists those under a heading saying they are problems, and "there is no index
/// yet, thousands of times" belongs nowhere near it.
pub fn info_once(key: &str, area: &str, message: impl AsRef<str>) {
    static SAID: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let said = SAID.get_or_init(|| Mutex::new(std::collections::HashSet::new()));

    let first = {
        let mut said = match said.lock() {
            Ok(s) => s,
            Err(poisoned) => poisoned.into_inner(),
        };
        said.insert(key.to_string())
    };
    if first {
        append_line("INFO", area, message.as_ref());
    }
}

/// Records a problem that could recur thousands of times in one run (one per
/// image, one per folder). The first few are written in full; after that only
/// a periodic tally, so a systematic failure is still obvious without burying
/// everything else in the log.
pub fn error_repeating(key: &str, area: &str, message: impl AsRef<str>) {
    repeating("ERROR", key, area, message);
}

fn repeating(level: &str, key: &str, area: &str, message: impl AsRef<str>) {
    let count = {
        let mut counts = match repeat_counts().lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };
        bump(&mut counts, key)
    };

    match repeat_action(count) {
        RepeatAction::Write => append_line(level, area, message.as_ref()),
        RepeatAction::WriteThenQuieten => {
            append_line(level, area, message.as_ref());
            append_line(
                level,
                area,
                &format!("(further '{key}' problems will be counted, not listed individually)"),
            );
        }
        RepeatAction::Tally => {
            append_line(level, area, &format!("'{key}' has now happened {count} times"))
        }
        RepeatAction::Stay => {}
    }
}

/// Totals for problems that were summarised rather than listed.
pub fn repeat_totals() -> Vec<(String, u64)> {
    let counts = match repeat_counts().lock() {
        Ok(c) => c,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut totals: Vec<(String, u64)> = counts.iter().map(|(k, v)| (k.clone(), *v)).collect();
    // Most frequent first: that is the one worth investigating.
    totals.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    totals
}

pub fn reset_repeat_counts() {
    if let Ok(mut counts) = repeat_counts().lock() {
        counts.clear();
    }
}

// ------------------------------------------------------------- timestamps

/// Civil date from a Unix day number (Howard Hinnant's algorithm). Written
/// out rather than pulled from a date crate because this is the only place
/// the app needs a calendar, and a wrong timestamp makes every log line
/// harder to trust.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// UTC, so timestamps from different machines can be compared directly.
fn format_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn timestamp() -> String {
    format_epoch(now_epoch())
}

fn format_system_time(t: SystemTime) -> String {
    t.duration_since(UNIX_EPOCH)
        .map(|d| format_epoch(d.as_secs() as i64))
        .unwrap_or_else(|_| "(before 1970)".into())
}

// ------------------------------------------------------------ panic hook

/// Records panics before the process dies.
///
/// The release profile sets `panic = "abort"`, so a panic anywhere -- the
/// indexing thread, an image decode, a command -- terminates the app
/// instantly. On a machine nobody can watch, that presents as "it just
/// disappeared" with nothing to go on. The hook still runs before the abort,
/// which is the one chance to write down where it happened.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".into());

        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "(non-text panic payload)".to_string()
        };

        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        error("panic", format!("PANIC in thread '{thread}' at {location}: {payload}"));
        error(
            "panic",
            format!("backtrace:\n{}", std::backtrace::Backtrace::force_capture()),
        );
        error("panic", "the app will now stop; this is the last entry of this run");
        previous(info);
    }));
}

/// Called once at startup.
pub fn start_session(app_version: &str) {
    let unclean = previous_run_ended_uncleanly();
    info("session", "----------------------------------------------------------");
    info(
        "session",
        format!(
            "started: version {app_version}, {} {}, webview {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            webview_version()
        ),
    );
    info("session", format!("data folder: {}", log_display(&crate::app_paths::data_dir())));
    info("session", format!("cache folder: {}", log_display(&crate::app_paths::cache_dir())));
    if unclean {
        warn(
            "session",
            "the previous run did not shut down cleanly -- look for a PANIC above, \
             or it was force-quit / killed",
        );
    }
}

pub fn end_session() {
    info("session", "clean shutdown");
}

/// Whether the last recorded run stopped without logging a clean shutdown --
/// which means it crashed, was killed, or aborted on a panic.
fn previous_run_ended_uncleanly() -> bool {
    let Ok(text) = std::fs::read_to_string(log_path()) else { return false };
    ended_uncleanly(&text)
}

/// Separated from the file reading so it can be tested against a written-out
/// log: under test every test in the process shares one log file, so reading
/// the real one would be checking whichever test ran alongside.
fn ended_uncleanly(log: &str) -> bool {
    let mut started = false;
    for line in log.lines() {
        if line.contains("[session] started:") {
            started = true;
        } else if line.contains("[session] clean shutdown") {
            started = false;
        }
    }
    started
}

fn webview_version() -> String {
    tauri::webview_version().unwrap_or_else(|e| format!("(unavailable: {e})"))
}

/// Paths go into the log verbatim. They are local folder names the user
/// already knows; the log stays on their machine unless they send it.
fn log_display(path: &Path) -> String {
    path.display().to_string()
}

// ---------------------------------------------------------------- probing

fn describe_dir(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        return "(not set)".into();
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => match std::fs::read_dir(path) {
            Ok(entries) => format!("exists, readable, {} entries", entries.count()),
            // The failure that matters on Windows: the folder is there but
            // this account cannot list it (permissions, or an offline share).
            Err(e) => format!("EXISTS BUT CANNOT BE READ: {e}"),
        },
        Ok(_) => "EXISTS BUT IS A FILE, NOT A FOLDER".into(),
        Err(e) => format!("NOT USABLE: {e}"),
    }
}

/// Confirms the app can actually create files where it needs to. Antivirus,
/// roaming profiles and locked-down accounts all break this on Windows, and
/// the symptom -- settings that will not save, an index that never appears --
/// otherwise gives no clue why.
fn probe_writable(dir: &Path) -> String {
    let probe = dir.join(format!("write-probe-{}.tmp", std::process::id()));
    match std::fs::write(&probe, b"probe") {
        Ok(()) => {
            let renamed = dir.join(format!("write-probe-{}-2.tmp", std::process::id()));
            // The whole app saves via write-then-rename, so rename is tested
            // too rather than assumed to follow from writing.
            let rename_result = match std::fs::rename(&probe, &renamed) {
                Ok(()) => "write OK, rename OK".to_string(),
                Err(e) => format!("write OK, but RENAME FAILED: {e}"),
            };
            let _ = std::fs::remove_file(&probe);
            let _ = std::fs::remove_file(&renamed);
            rename_result
        }
        Err(e) => format!("CANNOT WRITE HERE: {e}"),
    }
}

fn describe_file(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => format!(
            "{} bytes, modified {}",
            meta.len(),
            meta.modified().map(format_system_time).unwrap_or_else(|_| "unknown".into())
        ),
        Err(e) => format!("not present ({e})"),
    }
}

/// The longest path the app dealt with while indexing, against the limit
/// Windows applies unless long paths have been enabled. Deep Multimedia
/// folders plus a long user name get closer to 260 than expected.
fn path_length_note(longest: usize, example: &str) -> String {
    // Deliberately a runtime check rather than a compile-time one: this whole
    // module has to compile and behave identically on a platform it cannot be
    // built for here, so nothing is hidden behind a target-specific block.
    let on_windows = std::env::consts::OS == "windows";
    let verdict = match (on_windows, longest) {
        (true, n) if n >= 260 => {
            "OVER the 260-character limit Windows applies unless long paths are enabled. \
             Files at this depth will appear to be missing. Either enable long path \
             support, or move the folders somewhere shorter"
        }
        (true, n) if n >= 200 => "close to the 260-character limit Windows applies unless long paths are enabled",
        (true, _) => "within the 260-character limit Windows applies by default",
        (false, n) if n >= 240 => {
            "fine on this platform, but over the 260-character limit Windows applies by \
             default -- if this collection is opened on Windows, files this deep may not \
             be found"
        }
        (false, _) => "within the limit Windows would apply if this collection were opened there",
    };
    format!("{longest} characters ({verdict})\n    longest was: {example}")
}

// ----------------------------------------------------------------- report

#[derive(Serialize)]
pub struct ProbeImage {
    pub kind: String,
    pub id: i64,
}

#[derive(Serialize)]
pub struct DiagnosticsReport {
    /// The whole report as text, ready to be copied out and sent on.
    pub text: String,
    /// A real image the frontend can try to load, to prove the custom image
    /// protocol works in this webview. The URL form differs between platforms
    /// (`hsimg://localhost/...` against `http://hsimg.localhost/...`), so this
    /// cannot be checked from the Rust side.
    pub probe_image: Option<ProbeImage>,
    pub log_path: String,
}

pub fn report(app_version: &str) -> DiagnosticsReport {
    let mut out = String::new();
    let w = &mut out;

    let _ = writeln!(w, "=== Historical Society Search -- diagnostics ===");
    let _ = writeln!(w, "generated:     {} (UTC)", timestamp());
    let _ = writeln!(w, "app version:   {app_version}");
    let _ = writeln!(w, "platform:      {} {}", std::env::consts::OS, std::env::consts::ARCH);
    let _ = writeln!(w, "webview:       {}", webview_version());
    let _ = writeln!(w, "family:        {}", std::env::consts::FAMILY);
    let _ = writeln!(w, "path separator: {:?}", std::path::MAIN_SEPARATOR);

    // --- paths ---
    let data_dir = crate::app_paths::data_dir();
    let cache_dir = crate::app_paths::cache_dir();
    let _ = writeln!(w, "\n--- where the app keeps its own files ---");
    let _ = writeln!(w, "data folder:   {}", log_display(&data_dir));
    let _ = writeln!(w, "  status:      {}", describe_dir(&data_dir));
    let _ = writeln!(w, "  writable:    {}", probe_writable(&data_dir));
    let _ = writeln!(w, "  config.json: {}", describe_file(&crate::app_paths::config_path()));
    let _ = writeln!(w, "  index:       {}", describe_file(&crate::app_paths::db_path()));
    for (name, size) in crate::ingest::abandoned_build_files(&data_dir) {
        let _ = writeln!(
            w,
            "  LEFTOVER:    {name} ({:.1}MB) -- a rebuild that did not finish; removed \
             automatically once it is a day old, or a rebuild may be running right now",
            size as f64 / 1_048_576.0
        );
    }
    let _ = writeln!(w, "cache folder:  {}", log_display(&cache_dir));
    let _ = writeln!(w, "  status:      {}", describe_dir(&cache_dir));
    let _ = writeln!(w, "  writable:    {}", probe_writable(&cache_dir));
    let thumb_dir = crate::app_paths::thumb_cache_dir();
    let _ = writeln!(w, "  thumbnails:  {}", log_display(&thumb_dir));
    let _ = writeln!(w, "               {} cached previews", count_files_capped(&thumb_dir, 500_000));

    // --- configured folders ---
    let (config, config_problem) = crate::store::load_config_checked();
    let _ = writeln!(w, "\n--- folders the user pointed the app at ---");
    if let Some(problem) = &config_problem {
        let _ = writeln!(w, "PROBLEM READING SETTINGS: {problem}");
    }
    let data_folder = PathBuf::from(&config.data_dir);
    let _ = writeln!(w, "PastPerfect Data: {}", log_display(&data_folder));
    let _ = writeln!(w, "  status:       {}", describe_dir(&data_folder));
    let _ = writeln!(w, "{}", describe_catalog_tables(&data_folder));

    let images_dir = resolved_images_dir(&config, &data_folder);
    let _ = writeln!(w, "Images:           {}", log_display(&images_dir));
    let _ = writeln!(w, "  status:       {}", describe_dir(&images_dir));
    let _ = writeln!(
        w,
        "  configured:   {}",
        if config.images_dir.trim().is_empty() { "no -- defaulted from the Data folder" } else { "yes" }
    );

    let multimedia_dir = crate::ingest::default_multimedia_dir(&config.multimedia_dir, &data_folder);
    let _ = writeln!(w, "Multimedia:       {}", log_display(&multimedia_dir));
    let _ = writeln!(w, "  status:       {}", describe_dir(&multimedia_dir));
    let _ = writeln!(
        w,
        "  configured:   {}",
        if config.multimedia_dir.trim().is_empty() { "no -- defaulted from the Data folder" } else { "yes" }
    );

    for root in &config.photo_archive_roots {
        let path = PathBuf::from(&root.path);
        let _ = writeln!(w, "Photo folder {:?}: {}", root.label, log_display(&path));
        let _ = writeln!(w, "  status:       {}", describe_dir(&path));
        let _ = writeln!(w, "  read speed:   {}", read_speed_note(&path));
    }
    for root in &config.document_roots {
        let path = PathBuf::from(&root.path);
        let _ = writeln!(w, "PDF folder {:?}: {}", root.label, log_display(&path));
        let _ = writeln!(w, "  status:       {}", describe_dir(&path));
        let _ = writeln!(w, "  read speed:   {}", read_speed_note(&path));
    }
    let _ = writeln!(w, "Images read speed: {}", read_speed_note(&images_dir));
    if !config.document_roots.is_empty() {
        let _ = writeln!(
            w,
            "Text recognition (scanned PDFs): {}",
            if config.recognize_scanned_pdfs { "on" } else { "OFF in Folder Settings" }
        );
        let _ = writeln!(w, "  engine:       {}", describe_recognition_engine());
        let saved = crate::ocr::cache_dir(&crate::app_paths::data_dir());
        let (files, bytes) = std::fs::read_dir(&saved)
            .map(|entries| {
                entries.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).fold(
                    (0usize, 0u64),
                    |(n, b), e| (n + 1, b + e.metadata().map(|m| m.len()).unwrap_or(0)),
                )
            })
            .unwrap_or((0, 0));
        let _ = writeln!(w, "  recognised:   {files} PDF(s) kept, {} KB, in {}", bytes / 1024, log_display(&saved));
    }

    // --- the index ---
    let _ = writeln!(w, "\n--- the search index ---");
    let _ = write!(w, "{}", crate::store::index_report());

    // --- live probes ---
    let _ = writeln!(w, "\n--- checks run just now ---");
    let _ = write!(w, "{}", run_probes(&data_folder));

    // --- suppressed repeats ---
    let totals = repeat_totals();
    if !totals.is_empty() {
        let _ = writeln!(w, "\n--- recurring problems since the last index rebuild ---");
        for (key, count) in totals {
            let _ = writeln!(w, "  {key}: {count}");
        }
    }

    // --- log tail ---
    let _ = writeln!(w, "\n--- last {LOG_TAIL_LINES} log lines ({}) ---", log_display(&log_path()));
    let _ = writeln!(w, "{}", log_tail(LOG_TAIL_LINES));

    DiagnosticsReport {
        text: out,
        probe_image: crate::store::sample_image_for_probe(),
        log_path: log_display(&log_path()),
    }
}

/// Whether text recognition could run here, and what it would use.
fn describe_recognition_engine() -> String {
    // The bundled copy is looked for beside the program; the folder the
    // application is told about at runtime is not available to this report.
    let beside_program = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    match crate::ocr::Engine::locate(beside_program.as_deref()) {
        Ok(engine) => engine.version,
        Err(why) => format!("NOT AVAILABLE -- {why}"),
    }
}

fn resolved_images_dir(config: &crate::store::Config, data_folder: &Path) -> PathBuf {
    if config.images_dir.trim().is_empty() {
        data_folder
            .parent()
            .map(|p| p.join("Images"))
            .unwrap_or_else(|| data_folder.join("Images"))
    } else {
        PathBuf::from(config.images_dir.trim())
    }
}

/// Tries to open each catalog table the way indexing does.
///
/// This is the check that matters most on Windows: PastPerfect holds its
/// tables open, and a second program reading them can be refused with a
/// sharing violation that simply does not arise on macOS. Reporting the exact
/// operating-system error distinguishes "PastPerfect is running" from "the
/// file is missing" from "this account cannot read it".
fn describe_catalog_tables(data_folder: &Path) -> String {
    if !data_folder.is_dir() {
        return "  tables:       (cannot check -- the folder above is not usable)".into();
    }
    let mut out = String::new();
    let _ = writeln!(&mut out, "  tables:");
    for name in [
        "OBJECTS.DBF", "PHOTOS.DBF", "ARCHIVES.DBF", "LIBRARY.DBF", "PEOPLE.DBF", "MEDIA.DBF",
    ] {
        let found = crate::dbf::find_file(data_folder, name);
        match found {
            None => {
                let _ = writeln!(&mut out, "    {name:<13} not found in this folder");
            }
            Some(path) => match crate::dbf::DbfTable::open(&path) {
                Ok(table) => {
                    let memo = if table.memo_unavailable() {
                        " -- WARNING: its .FPT memo file is missing or unreadable, so \
                         descriptions and notes will be blank"
                    } else {
                        ""
                    };
                    let _ = writeln!(
                        &mut out,
                        "    {name:<13} opens OK, {} records{memo}",
                        table.record_count()
                    );
                }
                Err(e) => {
                    let _ = writeln!(
                        &mut out,
                        "    {name:<13} CANNOT BE OPENED: {e}\n                  \
                         (if PastPerfect is running, close it and try again -- Windows \
                         refuses a second reader while it holds the file)",
                    );
                }
            },
        }
    }
    out
}

fn run_probes(data_folder: &Path) -> String {
    let mut out = String::new();

    // Longest path the app would deal with, for the Windows 260 limit.
    let (longest, example) = crate::ingest::longest_indexed_path();
    let _ = writeln!(&mut out, "longest file path: {}", path_length_note(longest, &example));

    // Can an image actually be decoded and cached on this machine?
    match crate::store::probe_thumbnail() {
        Ok(note) => {
            let _ = writeln!(&mut out, "image preview:     OK -- {note}");
        }
        Err(e) => {
            let _ = writeln!(&mut out, "image preview:     FAILED -- {e}");
        }
    }

    // Does search work end to end, and how quickly?
    let started = std::time::Instant::now();
    let results = crate::store::search("photo", 0, false);
    let _ = writeln!(
        &mut out,
        "search test:       {:?} for a common word -> {} matches, {} shown",
        started.elapsed(),
        results.total,
        results.results.len()
    );

    let _ = writeln!(
        &mut out,
        "catalog lookup:    {}",
        crate::store::probe_catalog_number_lookup()
    );

    // The DBF reader's own view of the data folder, byte-level.
    let _ = writeln!(&mut out, "data folder check: {}", describe_dir(data_folder));

    out
}

/// How fast photographs can actually be read from where they are kept.
///
/// The first index rebuild on the society's computer took two hours eight
/// minutes for work this development machine does in minutes, and the decoding
/// itself accounts for almost none of that -- so the question is how quickly
/// the files arrive. A local disk gives hundreds of megabytes a second; a
/// network share or a folder synced from cloud storage, where each file may be
/// fetched on demand, gives a small fraction of that and explains the whole
/// difference. Without measuring it, that is guesswork.
pub fn read_speed_note(dir: &Path) -> String {
    // Small on purpose: this runs when someone presses the diagnostics
    // button, and on the slow storage it is meant to detect, reading much
    // more than this would leave them waiting on a blank screen.
    const SAMPLE: usize = 8;
    const MAX_BYTES: u64 = 24 * 1024 * 1024;

    let mut files: Vec<PathBuf> = Vec::new();
    collect_sample_files(dir, &mut files, SAMPLE);
    if files.is_empty() {
        return "no files to read".to_string();
    }

    let started = std::time::Instant::now();
    let mut bytes = 0u64;
    let mut read = 0usize;
    for path in &files {
        match std::fs::read(path) {
            Ok(data) => {
                bytes += data.len() as u64;
                read += 1;
            }
            Err(_) => continue,
        }
        if bytes >= MAX_BYTES {
            break;
        }
    }
    let secs = started.elapsed().as_secs_f64();
    if read == 0 {
        return "none of the sampled files could be read".to_string();
    }
    let mb = bytes as f64 / 1_048_576.0;
    format!(
        "{read} files, {mb:.1}MB in {secs:.2}s = {:.1}MB/s ({:.0}ms per file). \
         A local disk manages hundreds of MB/s; much less than that means the files \
         are coming from a network share or being fetched from cloud storage, which \
         is what makes a first rebuild slow.",
        mb / secs.max(0.001),
        secs * 1000.0 / read as f64
    )
}

/// A handful of files from anywhere under `dir`, for the read-speed probe.
fn collect_sample_files(dir: &Path, out: &mut Vec<PathBuf>, want: usize) {
    if out.len() >= want {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        if out.len() >= want {
            return;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => subdirs.push(path),
            Ok(t) if t.is_file() => out.push(path),
            _ => {}
        }
    }
    for sub in subdirs {
        collect_sample_files(&sub, out, want);
    }
}

fn count_files_capped(dir: &Path, cap: usize) -> String {
    fn walk(dir: &Path, cap: usize, seen: &mut usize, depth: usize) {
        if *seen >= cap || depth > 8 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            if *seen >= cap {
                return;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => walk(&entry.path(), cap, seen, depth + 1),
                Ok(_) => *seen += 1,
                Err(_) => {}
            }
        }
    }
    let mut seen = 0;
    walk(dir, cap, &mut seen, 0);
    if seen >= cap {
        format!("{cap}+ (stopped counting)")
    } else {
        seen.to_string()
    }
}

fn log_tail(lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(log_path()) else {
        return "(no log file yet)".into();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_speed_probe_reports_what_it_read_and_copes_with_nothing_to_read() {
        let dir = std::env::temp_dir().join(format!("hs_speed_probe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nested")).unwrap();

        assert!(
            read_speed_note(&dir.join("does-not-exist")).contains("no files"),
            "a missing folder should say so rather than divide by nothing"
        );
        assert!(read_speed_note(&dir).contains("no files"), "an empty folder has nothing to time");

        // Files are found in subfolders too, which is how the photo
        // collections are actually arranged.
        let payload = vec![b'x'; 200_000];
        for i in 0..3 {
            std::fs::write(dir.join("nested").join(format!("photo{i}.tif")), &payload).unwrap();
        }
        let note = read_speed_note(&dir);
        assert!(note.contains("3 files"), "should have found all three: {note}");
        assert!(note.contains("MB/s"), "should report a rate: {note}");
        assert!(note.contains("0.6MB"), "should report what it read: {note}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_standing_fact_is_recorded_once_however_often_it_is_noticed() {
        // The window asks for the record count twice a second, so on a first
        // run "there is no index yet" was noticed thousands of times. Once
        // is the information; the rest is noise in the one file someone reads
        // when they are trying to find a real fault.
        let marker = format!("no-index-marker-{}", std::process::id());
        for _ in 0..500 {
            info_once(&marker, "test", format!("standing fact {marker}"));
        }
        let log = std::fs::read_to_string(log_path()).unwrap_or_default();
        let written = log.matches(&format!("standing fact {marker}")).count();
        assert_eq!(written, 1, "the fact was written {written} times, not once");
        assert!(
            !log.contains(&format!("further '{marker}'")),
            "a standing fact should not announce itself as a recurring problem"
        );

        // Nor should it appear among the problems in the report, which is what
        // someone reads to find out what went wrong.
        assert!(
            !repeat_totals().iter().any(|(k, _)| *k == marker),
            "a standing fact was listed among the recurring problems"
        );
    }

    #[test]
    fn a_run_that_logged_its_shutdown_is_not_reported_as_a_crash() {
        // Every run wrote this warning for months because the shutdown was
        // never recorded, so a genuine crash looked exactly like an ordinary
        // quit. Both halves matter: the warning has to appear when it should
        // and stay away when it should not.
        let started = "2026-01-01T00:00:00Z [session] started: version 0.1.0, macos aarch64";
        let clean = "2026-01-01T00:10:00Z [session] clean shutdown";

        assert!(!ended_uncleanly(""), "no log at all is a first run, not a crash");
        assert!(
            !ended_uncleanly(&format!("{started}\n{clean}\n")),
            "a run that recorded its shutdown must not be called unclean"
        );
        assert!(ended_uncleanly(&format!("{started}\n")), "a run with no shutdown crashed");
        assert!(
            !ended_uncleanly(&format!("{started}\n{clean}\n{started}\n{clean}\n")),
            "several clean runs in one log stay clean"
        );
        assert!(
            ended_uncleanly(&format!("{started}\n{clean}\n{started}\n")),
            "a crash after earlier clean runs must still be noticed"
        );
        assert!(
            ended_uncleanly(&format!("{started}\n{clean}\n{started}\nPANIC somewhere\n")),
            "a panic is not a shutdown"
        );
    }

    #[test]
    fn the_shutdown_is_recorded_from_the_exit_event_and_not_after_run() {
        // Tauri's `run` ends the process from the inside and never returns, so
        // a call placed after it is dead code that happens to compile -- which
        // is exactly the mistake this catches. The check is structural because
        // the alternative is starting a real window, which a test cannot do;
        // it verifies the call sits inside the closure handed to `run`, which
        // is the one place it will actually happen.
        let source = include_str!("lib.rs");
        let run_at = source.find(".run(").expect("lib.rs should start the application with run()");

        // Take the closure by matching the parenthesis `run(` opens.
        let after = &source[run_at + ".run(".len()..];
        let mut depth = 1usize;
        let mut end = after.len();
        for (i, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let argument = &after[..end];
        let rest = &after[end..];

        assert!(
            argument.contains("end_session"),
            "the clean shutdown must be recorded inside run()'s event handler; \
             run() was given: {argument}"
        );
        assert!(
            argument.contains("RunEvent::Exit"),
            "it should be recorded on the exit event specifically, so that it means \
             the application really is stopping"
        );
        assert!(
            !rest.contains("end_session"),
            "there is a call to end_session after run(), which never returns -- \
             it would never run, and every following start would report a crash"
        );
    }

    #[test]
    fn formats_dates_correctly_across_awkward_boundaries() {
        // A wrong timestamp makes every line in the log harder to trust, and
        // this calendar maths is written out by hand -- so it is pinned
        // against known values, including a leap day and a century boundary.
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch(1), "1970-01-01T00:00:01Z");
        assert_eq!(format_epoch(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(format_epoch(86_400), "1970-01-02T00:00:00Z");
        // 2000-02-29: a leap year that the "divisible by 100" rule alone
        // would get wrong.
        assert_eq!(format_epoch(951_782_400), "2000-02-29T00:00:00Z");
        // 2100-03-01: 2100 is not a leap year.
        assert_eq!(format_epoch(4_107_542_400), "2100-03-01T00:00:00Z");
        assert_eq!(format_epoch(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    /// Uses a local counter rather than the process-wide one: an index
    /// rebuild resets that, and tests share a process, so asserting against
    /// it would be checking whichever test happened to run alongside.
    #[test]
    fn repeated_problems_are_counted_even_once_they_stop_being_listed() {
        let mut counts = HashMap::new();
        let key = "everything-is-broken";
        let mut written = 0;
        for _ in 0..2500 {
            let n = bump(&mut counts, key);
            if matches!(repeat_action(n), RepeatAction::Write | RepeatAction::WriteThenQuieten) {
                written += 1;
            }
        }
        assert_eq!(counts[key], 2500, "every occurrence must be counted");
        assert_eq!(
            written, MAX_REPEATS,
            "only the first {MAX_REPEATS} should be written out in full"
        );
    }

    #[test]
    fn a_systematic_failure_keeps_being_reported_periodically() {
        // The point of quietening repeats is to keep the log readable, not to
        // hide an ongoing failure -- so it has to keep surfacing a total.
        assert_eq!(repeat_action(1), RepeatAction::Write);
        assert_eq!(repeat_action(MAX_REPEATS), RepeatAction::WriteThenQuieten);
        assert_eq!(repeat_action(MAX_REPEATS + 1), RepeatAction::Stay);
        assert_eq!(repeat_action(1000), RepeatAction::Tally);
        assert_eq!(repeat_action(2000), RepeatAction::Tally);
        assert_eq!(repeat_action(1001), RepeatAction::Stay);
    }

    #[test]
    fn a_write_probe_reports_failure_for_somewhere_it_cannot_write() {
        // The probe exists to turn "settings will not save" into a specific
        // reason, so it has to actually fail when writing is impossible.
        let result = probe_writable(Path::new("/definitely/not/a/real/folder/anywhere"));
        assert!(
            result.contains("CANNOT WRITE"),
            "expected a clear failure, got: {result}"
        );

        let ok = probe_writable(&std::env::temp_dir());
        assert!(ok.contains("write OK"), "expected the temp folder to be writable, got: {ok}");
    }

    /// The log the user sends must contain only what the app really did.
    /// Tests provoke failures on purpose, and those lines appearing in a real
    /// report would send any investigation chasing problems that never
    /// happened on the user's machine.
    #[test]
    fn tests_never_write_into_the_real_log() {
        let real = crate::app_paths::data_dir().join("diagnostics.log");
        assert_ne!(
            log_path(),
            real,
            "the test suite is logging into the real diagnostics file"
        );
        error("selftest", "this line must never reach the user's log");
        if let Ok(text) = std::fs::read_to_string(&real) {
            assert!(
                !text.contains("this line must never reach the user's log"),
                "a test's log output ended up in the real diagnostics file at {}",
                real.display()
            );
        }
    }

    #[test]
    fn long_paths_are_called_out_whichever_platform_the_report_runs_on() {
        // This collection really does contain a path over 260 characters, so
        // the warning has to appear on macOS too -- otherwise the one machine
        // that can be inspected is the one that stays silent about it.
        let long = path_length_note(300, "/some/very/long/path");
        assert!(
            long.contains("260"),
            "a 300-character path should mention the Windows limit, got: {long}"
        );
        let short = path_length_note(80, "/short");
        assert!(
            !short.contains("OVER") && !short.contains("may not\n     be found"),
            "a short path should not be flagged as a problem, got: {short}"
        );
    }

    /// The rotated log must land beside the log it came from. These two paths
    /// are derived separately, and a mismatch would move a test's log into the
    /// real data folder -- silently defeating the isolation above.
    #[test]
    fn the_rotated_log_stays_beside_the_live_one() {
        assert_eq!(
            log_path().parent(),
            rotated_log_path().parent(),
            "rotating the log would move it into a different folder"
        );
    }

    /// Nothing in a message may be able to fabricate a log entry.
    ///
    /// This log is the only account of what happened on a machine that cannot
    /// be looked at directly, so an entry that was never written is worse
    /// than a missing one -- it sends the reader after something that did not
    /// occur.
    #[test]
    fn a_message_cannot_invent_log_entries() {
        let forged = "could not read file\n2026-01-01T00:00:00Z ERROR [index] everything is broken";
        let line = format!("{} {:<5} [{}] {}", "TS", "INFO", "area", sanitize_for_log(forged));

        let lines: Vec<&str> = line.split('\n').collect();
        assert_eq!(lines.len(), 2, "the message keeps its shape");
        assert!(
            lines[1].starts_with("    "),
            "a continuation line must be indented so it cannot be read as an entry of its own, \
             got {:?}",
            lines[1]
        );
        assert!(
            !lines[1].starts_with("2026-01-01T"),
            "the forged entry begins a line where a real one would, so it reads as genuine: {:?}",
            lines[1]
        );

        // Carriage returns and other control characters are shown, not obeyed.
        let sneaky = sanitize_for_log("before\rafter\u{7}\u{1b}[2Jcleared");
        assert!(!sneaky.contains('\r'), "a carriage return could overwrite the line already written");
        assert!(!sneaky.contains('\u{1b}'), "an escape sequence could rewrite the display");
        assert!(sneaky.contains("before") && sneaky.contains("after"));

        // A real multi-line message still reads properly.
        let genuine = sanitize_for_log("rebuild starting\n    data:   /somewhere");
        assert!(genuine.contains("rebuild starting"));
        assert!(genuine.contains("data:   /somewhere"));

        // And one runaway message cannot fill the file.
        let huge = sanitize_for_log(&"x".repeat(MAX_MESSAGE_BYTES * 3));
        assert!(huge.len() < MAX_MESSAGE_BYTES + 100, "a message should be capped");
        assert!(huge.ends_with("(message truncated)"), "and say that it was");
    }

    #[test]
    fn describe_dir_distinguishes_missing_from_unreadable_from_fine() {
        assert!(describe_dir(Path::new("/no/such/folder/here")).contains("NOT USABLE"));
        assert!(describe_dir(&std::env::temp_dir()).contains("exists, readable"));
        assert_eq!(describe_dir(Path::new("")), "(not set)");
    }
}
