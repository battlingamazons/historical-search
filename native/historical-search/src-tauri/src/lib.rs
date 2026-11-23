mod app_paths;
mod dbf;
pub mod diagnostics;
pub mod ingest;
mod palette_tiff;
mod photo_check;
pub mod crop_match;
pub mod face_match;
pub mod scan_formats;
pub mod faces;
pub mod clip_tokenizer;
pub mod clip;
pub mod clip_download;
pub mod clip_index;
pub mod fingerprint;
mod look_alike;
pub mod pdf;
pub mod ocr;
mod query;
mod scan;
pub mod sheets;
mod store;
mod wordtext;
#[cfg(all(test, feature = "live-data"))]
mod test_support;
mod thumbnails;

use serde::Serialize;
use std::sync::{Arc, Mutex};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use store::{Config, ItemDetail, SearchResponse, ValidationResult};

/// Progress/state of an index rebuild. Shared between the background rebuild
/// thread and the UI, which polls it while a refresh runs.
#[derive(Default, Clone, Serialize)]
struct RefreshState {
    in_progress: bool,
    last_error: Option<String>,
    /// Non-fatal problems from the last rebuild (e.g. a catalog that
    /// couldn't be read, or missing memo text).
    warnings: Vec<String>,
    phase: Option<String>,
    phase_current: Option<u64>,
    phase_total: Option<u64>,
    /// A rebuild was asked for while one was already running, so another is
    /// owed as soon as this one ends.
    ///
    /// This is not a rare case. Saving folder settings asks for a rebuild, and
    /// a rebuild of this collection takes long enough -- hours, the first time
    /// -- that changing the settings during one is an ordinary thing to do. A
    /// running rebuild resolved its folders when it started and cannot pick up
    /// the change, so without this the request was simply dropped: the window
    /// followed the old rebuild to the end and reported success, leaving an
    /// index built from the folders the user had just replaced.
    rebuild_again: bool,
    /// Rebuilds that finished and replaced the index this session.
    completed: u64,
}

/// The rules for starting and finishing a rebuild, kept away from the thread
/// that runs one so they can be checked directly: a test cannot start a real
/// Tauri application, and what matters here is the order the flags change in.
impl RefreshState {
    /// A rebuild has been asked for. Returns whether to start one now; if not,
    /// the request is remembered and honoured when the running one ends.
    fn rebuild_requested(&mut self) -> bool {
        if self.in_progress {
            self.rebuild_again = true;
            return false;
        }
        *self = RefreshState {
            in_progress: true,
            phase: Some("Starting…".into()),
            // Carried across: it counts rebuilds for the whole session, and
            // resetting it would tell the window the numbering had not changed.
            completed: self.completed,
            ..RefreshState::default()
        };
        true
    }

    /// A rebuild has ended. Records the outcome and returns whether another one
    /// is owed.
    fn rebuild_finished(&mut self, result: Result<Vec<String>, String>) -> bool {
        match result {
            Ok(warnings) => {
                self.warnings = warnings;
                self.last_error = None;
                // Only on success: a rebuild that failed or was refused left
                // the previous index exactly where it was, so nothing the
                // window is showing has been renumbered.
                self.completed += 1;
            }
            Err(e) => self.last_error = Some(e),
        }

        let owed = std::mem::take(&mut self.rebuild_again);
        if owed {
            // Left marked as in progress across the handover, so the window
            // never sees a moment that looks finished and tells the user the
            // index is up to date when it is about to be rebuilt again.
            self.in_progress = true;
            self.phase = Some("Starting again, with the folders you just saved…".into());
        } else {
            self.in_progress = false;
            self.phase = None;
        }
        self.phase_current = None;
        self.phase_total = None;
        owed
    }
}

#[derive(Default)]
struct AppState {
    refresh: Mutex<RefreshState>,
    photo_check: Mutex<PhotoCheckState>,
    picture_search: Mutex<PictureDownloadState>,
    /// Set to ask a download in progress to stop.
    picture_stop: Arc<std::sync::atomic::AtomicBool>,
}

/// Progress of fetching the picture-search models (see `clip_download.rs`).
#[derive(Default, Clone)]
struct PictureDownloadState {
    downloading: bool,
    done_bytes: u64,
    total_bytes: u64,
    last_error: Option<String>,
}

/// Progress of bringing the photo list (see `photo_check.rs`) up to date. Kept
/// apart from `RefreshState`: it is a different job with its own button, and a
/// catalog rebuild neither waits for it nor owes it anything.
#[derive(Default, Clone)]
struct PhotoCheckState {
    in_progress: bool,
    looked_at: u64,
    /// What the update is doing now, and how far along, once it has moved on
    /// from listing the files to learning what the pictures look like.
    phase: Option<String>,
    phase_current: u64,
    phase_total: u64,
    last_error: Option<String>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct PhotoCheckStatus {
    /// Pictures in the list.
    files: i64,
    last_scanned: Option<f64>,
    in_progress: bool,
    looked_at: u64,
    phase: Option<String>,
    phase_current: u64,
    phase_total: u64,
    /// Pictures in the list that have not had their look-alike fingerprint made.
    not_yet_learned: u64,
    last_error: Option<String>,
    /// Things worth telling the person, such as a folder that could not be read.
    notes: Vec<String>,
}

#[derive(Serialize)]
struct Status {
    last_updated: Option<f64>,
    record_count: i64,
    /// How many rebuilds have finished and put a new index in place this
    /// session. The window watches it to know the record numbering has
    /// changed; "a rebuild stopped running" is not the same thing, because one
    /// rebuild can hand straight over to another.
    rebuilds_completed: u64,
    in_progress: bool,
    last_error: Option<String>,
    warnings: Vec<String>,
    phase: Option<String>,
    phase_current: Option<u64>,
    phase_total: Option<u64>,
}

/// The folders a rebuild will actually read, worked out from the settings.
struct Folders {
    data_dir: std::path::PathBuf,
    images_dir: std::path::PathBuf,
    multimedia_dir: std::path::PathBuf,
    roots: Vec<ingest::PhotoArchiveRoot>,
    documents: Vec<ingest::DocumentRoot>,
    spreadsheets: Vec<ingest::SpreadsheetRoot>,
}

/// Turns the saved settings into the folders to read from.
///
/// Two of them are optional and are worked out from the Data folder when
/// left blank, which is the arrangement PastPerfect installs use; and a photo
/// folder row left empty is dropped rather than becoming a reference to
/// nowhere. Separated from the command so the working-out can be checked
/// directly -- it decides what a rebuild reads, and a rebuild replaces the
/// index.
fn resolve_folders(config: &Config) -> Folders {
    let data_dir = std::path::PathBuf::from(config.data_dir.trim());
    // With no Data folder set there is nothing to sit these beside, so they
    // are left unset rather than becoming bare relative names like "Images",
    // which would resolve against whatever directory the application happened
    // to be started in.
    let beside_data = |name: &str| {
        if data_dir.as_os_str().is_empty() {
            return std::path::PathBuf::new();
        }
        data_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join(name))
            .unwrap_or_else(|| data_dir.join(name))
    };
    Folders {
        images_dir: if config.images_dir.trim().is_empty() {
            beside_data("Images")
        } else {
            std::path::PathBuf::from(config.images_dir.trim())
        },
        multimedia_dir: ingest::default_multimedia_dir(&config.multimedia_dir, &data_dir),
        roots: config
            .photo_archive_roots
            .iter()
            .filter(|r| !r.path.trim().is_empty())
            .map(|r| ingest::PhotoArchiveRoot {
                label: r.label.trim().to_string(),
                path: std::path::PathBuf::from(r.path.trim()),
            })
            .collect(),
        documents: config
            .document_roots
            .iter()
            .filter(|r| !r.path.trim().is_empty())
            .map(|r| ingest::DocumentRoot {
                label: r.label.trim().to_string(),
                path: std::path::PathBuf::from(r.path.trim()),
            })
            .collect(),
        spreadsheets: config
            .spreadsheet_roots
            .iter()
            .filter(|r| !r.path.trim().is_empty())
            .map(|r| ingest::SpreadsheetRoot {
                label: r.label.trim().to_string(),
                path: std::path::PathBuf::from(r.path.trim()),
            })
            .collect(),
        data_dir,
    }
}

/// Everything the status line should be telling the user.
fn assemble_warnings(
    from_last_rebuild: &[String],
    config_problem: Option<String>,
    rebuild_in_progress: bool,
    index_predates_image_rows: bool,
) -> Vec<String> {
    let mut warnings = from_last_rebuild.to_vec();
    warnings.extend(config_problem);
    // Held back while a rebuild is running: it is about to stop being true,
    // and saying it mid-rebuild just reads as a fault.
    if !rebuild_in_progress && index_predates_image_rows {
        warnings.push(
            "This search index was built before the app could show every image on a record. \
             Click Refresh Search Index to pick up additional images and high-resolution \
             originals."
                .to_string(),
        );
    }
    warnings
}

// ---------------------------------------------------------------- commands

#[tauri::command]
fn search(query: String, offset: i64, show_all: bool, datasets: Option<Vec<String>>) -> SearchResponse {
    store::search_in(&query, offset, show_all, &datasets.unwrap_or_default())
}

#[tauri::command]
fn get_item(id: i64) -> Option<ItemDetail> {
    store::get_item(id)
}

#[tauri::command]
fn get_config() -> Config {
    store::load_config()
}

#[tauri::command]
fn save_config(config: Config) -> Result<(), String> {
    store::save_config(&config)
}

/// Checks a folder the user typed or picked, off the main thread.
///
/// Deliberately `async`: Tauri runs synchronous commands on the main thread,
/// and this one walks an entire folder tree to count what is in it -- many
/// thousands of files for one of the photo folders here. That is only milliseconds
/// on a warm local disk, but it runs on every pause in typing, and a cold
/// cache, a virus scanner inspecting each file, or a slower filesystem would
/// turn it into a frozen window. The folder picker already had to be made
/// async for the same reason.
#[tauri::command]
async fn validate_path(kind: String, path: String) -> ValidationResult {
    tauri::async_runtime::spawn_blocking(move || store::validate_path(&kind, &path))
        .await
        .unwrap_or_else(|e| ValidationResult {
            ok: false,
            message: format!("This folder could not be checked: {e}"),
        })
}

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> Status {
    let refresh = state.refresh.lock().unwrap().clone();
    let (_, config_problem) = store::load_config_checked();
    let warnings = assemble_warnings(
        &refresh.warnings,
        config_problem,
        refresh.in_progress,
        store::index_needs_rebuild_for_images(),
    );
    Status {
        last_updated: store::last_updated(),
        record_count: store::record_count(),
        rebuilds_completed: refresh.completed,
        in_progress: refresh.in_progress,
        last_error: refresh.last_error,
        warnings,
        phase: refresh.phase,
        phase_current: refresh.phase_current,
        phase_total: refresh.phase_total,
    }
}

/// Every folder whose pictures count as "the collection" when a scan is
/// checked: the photo folders from the settings, and PastPerfect's own Images
/// folder (the display copies) and Multimedia folder (the high-resolution
/// originals) -- the same pictures the search shows against their records.
///
/// A PastPerfect folder named in the settings is always included, so that an
/// unplugged drive keeps what the list knew about it rather than dropping it.
/// One that was only assumed to sit beside the Data folder is included only if
/// it is there. A folder lying inside another one is left out: its pictures
/// are already found through the outer one, and would otherwise be listed twice.
fn photo_list_roots(config: &Config, folders: &Folders) -> Vec<ingest::PhotoArchiveRoot> {
    let mut roots: Vec<ingest::PhotoArchiveRoot> = folders
        .roots
        .iter()
        .map(|r| ingest::PhotoArchiveRoot { label: r.label.clone(), path: r.path.clone() })
        .collect();
    for (label, dir, setting) in [
        ("PastPerfect Images", &folders.images_dir, &config.images_dir),
        ("PastPerfect Multimedia", &folders.multimedia_dir, &config.multimedia_dir),
    ] {
        if !dir.as_os_str().is_empty() && (!setting.trim().is_empty() || dir.is_dir()) {
            roots.push(ingest::PhotoArchiveRoot { label: label.to_string(), path: dir.clone() });
        }
    }
    let resolved: Vec<std::path::PathBuf> =
        roots.iter().map(|r| std::fs::canonicalize(&r.path).unwrap_or_else(|_| r.path.clone())).collect();
    let mut kept = Vec::new();
    for (i, root) in roots.into_iter().enumerate() {
        let inside_another = resolved.iter().enumerate().any(|(j, other)| {
            j != i && (if other == &resolved[i] { j < i } else { resolved[i].starts_with(other) })
        });
        if !inside_another {
            kept.push(root);
        }
    }
    kept
}

#[tauri::command]
fn photo_check_status(state: tauri::State<'_, AppState>) -> PhotoCheckStatus {
    let run = state.photo_check.lock().unwrap().clone();
    let list = photo_check::status();
    PhotoCheckStatus {
        files: list.files,
        last_scanned: list.last_scanned,
        in_progress: run.in_progress,
        looked_at: run.looked_at,
        phase: run.phase,
        phase_current: run.phase_current,
        phase_total: run.phase_total,
        not_yet_learned: if list.files > 0 { look_alike::pending(&photo_check::db_path()) } else { 0 },
        last_error: run.last_error,
        notes: run.notes,
    }
}

/// Brings the photo list up to date on a background thread and returns at
/// once; the window polls `photo_check_status`. False means one is already
/// running.
#[tauri::command]
fn photo_check_update(app: tauri::AppHandle) -> bool {
    {
        let state = app.state::<AppState>();
        let mut run = state.photo_check.lock().unwrap();
        if run.in_progress {
            return false;
        }
        *run = PhotoCheckState { in_progress: true, ..PhotoCheckState::default() };
    }
    std::thread::spawn(move || {
        // Pictures that had washed-out previews were learned from those, so the
        // list is started over before anything is added to it.
        let stale = app_paths::photo_list_stale_marker();
        if stale.exists() {
            match photo_check::clear() {
                Ok(()) => {
                    let _ = std::fs::remove_file(&stale);
                    diagnostics::info("photo-check", "the photo list was learned from washed-out previews and is being made again");
                }
                Err(e) => diagnostics::error("photo-check", e),
            }
        }
        let config = store::load_config();
        let roots = photo_list_roots(&config, &resolve_folders(&config));
        let progress_app = app.clone();
        let mut progress = move |n: u64| {
            progress_app.state::<AppState>().photo_check.lock().unwrap().looked_at = n;
        };
        let learning_app = app.clone();
        let mut learning = move |done: u64, total: u64| {
            let state = learning_app.state::<AppState>();
            let mut run = state.photo_check.lock().unwrap();
            run.phase = Some("Getting the pictures ready to search".into());
            run.phase_current = done;
            run.phase_total = total;
        };
        // First the list of files, then a look-alike fingerprint for each new or
        // changed one. The second is the slow part, the first time. With no
        // photo folders set up there is simply nothing to list, which is not a
        // problem worth a message at every refresh.
        let result = photo_check::rescan(&roots, &mut progress).and_then(|report| {
            look_alike::build(&photo_check::db_path(), &app_paths::thumb_cache_dir(), &mut learning)?;
            Ok(report)
        });
        // Then, if picture search is on and its files are there, what each
        // picture shows. A problem here does not spoil the list.
        let mut picture_note = None;
        if result.is_ok() && config.picture_search && clip::installed() {
            let describing_app = app.clone();
            let mut describing = move |done: u64, total: u64| {
                let state = describing_app.state::<AppState>();
                let mut run = state.photo_check.lock().unwrap();
                run.phase = Some("Getting picture search ready".into());
                run.phase_current = done;
                run.phase_total = total;
            };
            if let Err(e) = clip_index::build(&photo_check::db_path(), &app_paths::thumb_cache_dir(), &mut describing) {
                diagnostics::error("picture-search", format!("describing the pictures failed: {e}"));
                picture_note = Some(format!("Picture search could not describe the pictures: {e}"));
            }
        }
        let state = app.state::<AppState>();
        let mut run = state.photo_check.lock().unwrap();
        run.in_progress = false;
        run.phase = None;
        match result {
            Ok(report) => {
                run.notes = report.notes();
                run.notes.extend(picture_note);
            }
            Err(e) => {
                diagnostics::error("photo-check", format!("updating the photo list failed: {e}"));
                run.last_error = Some(e);
            }
        }
    });
    true
}

// ------------------------------------------------------------ picture search

#[derive(Serialize)]
struct PictureSearchStatus {
    /// Turned on in the settings.
    enabled: bool,
    /// Both model files are downloaded.
    installed: bool,
    downloading: bool,
    download_done: u64,
    download_total: u64,
    download_error: Option<String>,
    /// Pictures that can be searched, and pictures still to be described.
    described: u64,
    pending: u64,
}

#[tauri::command]
fn picture_search_status(state: tauri::State<'_, AppState>) -> PictureSearchStatus {
    let download = state.picture_search.lock().unwrap().clone();
    let db = photo_check::db_path();
    let installed = clip::installed();
    PictureSearchStatus {
        enabled: store::load_config().picture_search,
        installed,
        downloading: download.downloading,
        download_done: download.done_bytes,
        download_total: if download.total_bytes > 0 { download.total_bytes } else { clip_download::total_size() },
        download_error: download.last_error,
        described: if installed { clip_index::described(&db) } else { 0 },
        pending: if installed && db.exists() { clip_index::pending(&db) } else { 0 },
    }
}

/// Fetches the picture-search models on a background thread and returns at
/// once; the window polls `picture_search_status`. False means a download is
/// already running.
#[tauri::command]
fn picture_search_download(app: tauri::AppHandle) -> bool {
    {
        let state = app.state::<AppState>();
        let mut run = state.picture_search.lock().unwrap();
        if run.downloading {
            return false;
        }
        *run = PictureDownloadState { downloading: true, total_bytes: clip_download::total_size(), ..Default::default() };
        state.picture_stop.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    std::thread::spawn(move || {
        let stop = app.state::<AppState>().picture_stop.clone();
        let progress_app = app.clone();
        let mut progress = move |done: u64, total: u64| {
            let state = progress_app.state::<AppState>();
            let mut run = state.picture_search.lock().unwrap();
            run.done_bytes = done;
            run.total_bytes = total;
        };
        let result = clip_download::download(&clip_download::model_files(), &app_paths::clip_models_dir(), &mut progress, &stop);
        let state = app.state::<AppState>();
        let mut run = state.picture_search.lock().unwrap();
        run.downloading = false;
        if let Err(e) = result {
            diagnostics::error("picture-search", format!("downloading the picture-search files failed: {e}"));
            run.last_error = Some(e);
        }
    });
    true
}

/// Asks a download in progress to stop; what has arrived is kept to carry on from.
#[tauri::command]
fn picture_search_stop(state: tauri::State<'_, AppState>) {
    state.picture_stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// One picture found by what it shows: a catalog record that uses it, or a
/// file in the photo folders that no record uses (by its place in `loose`).
#[derive(Serialize)]
struct PictureHit {
    /// "record" or "loose".
    kind: &'static str,
    /// The record's id; 0 for a loose picture.
    id: i64,
    /// Which of `loose`; 0 for a record.
    index: usize,
}

#[derive(Serialize)]
struct PictureSearch {
    /// The pictures best first, as records and loose files interleaved.
    hits: Vec<PictureHit>,
    /// The records among them, drawn as any search result.
    response: SearchResponse,
    loose: Vec<LooseMatch>,
    /// There may be more pictures after these.
    has_more: bool,
}

/// Finds pictures by what they show: by words (`phrase`), or by what another
/// picture looks like (`path`, a file the person chose themselves). `limit`
/// pictures are returned, best first.
#[tauri::command]
async fn picture_search(
    phrase: Option<String>,
    path: Option<String>,
    part: Option<[f32; 4]>,
    limit: Option<usize>,
) -> Result<PictureSearch, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let limit = limit.unwrap_or(24).clamp(1, 200);
        let db = photo_check::db_path();
        let hits = match (&phrase, &path) {
            (_, Some(p)) => {
                let file = scan_file(p)?;
                clip_index::search_picture(&db, &file, &app_paths::thumb_cache_dir(), part.map(|p| (p[0], p[1], p[2], p[3])), limit + 1)?
            }
            (Some(words), None) => clip_index::search_text(&db, words, limit + 1)?,
            (None, None) => Vec::new(),
        };
        let has_more = hits.len() > limit;
        let files: Vec<std::path::PathBuf> = hits.iter().take(limit).map(|h| h.path.clone()).collect();
        let (records, unlinked) = store::records_for_files(&files);
        let config = store::load_config();
        let roots = photo_list_roots(&config, &resolve_folders(&config));

        // Best first: each record by the picture that found it.
        let mut ranked: Vec<(usize, Result<usize, usize>)> = Vec::new(); // (file index, Ok(record) | Err(loose))
        let mut results = Vec::new();
        for (r, file) in records {
            ranked.push((file, Ok(results.len())));
            results.push(r);
        }
        let loose_paths: Vec<std::path::PathBuf> = unlinked.iter().map(|&i| files[i].clone()).collect();
        let mut loose = Vec::new();
        for (n, &i) in unlinked.iter().enumerate() {
            ranked.push((i, Err(n)));
            loose.push(loose_match(n, &files[i], &roots));
        }
        ranked.sort_by_key(|(file, _)| *file);
        let hits_out = ranked
            .iter()
            .map(|(_, what)| match what {
                Ok(r) => PictureHit { kind: "record", id: results[*r].id, index: 0 },
                Err(l) => PictureHit { kind: "loose", id: 0, index: *l },
            })
            .collect();
        photo_check::set_loose(loose_paths);
        let total = results.len() as i64;
        Ok(PictureSearch {
            hits: hits_out,
            response: SearchResponse { results, has_more: false, total, terms: Vec::new(), words: Vec::new(), datasets: Vec::new() },
            loose,
            has_more,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A picture in the photo folders that no record uses, described for the
/// window: its name, and which photo folder it is in.
fn loose_match(index: usize, path: &std::path::Path, roots: &[ingest::PhotoArchiveRoot]) -> LooseMatch {
    let place = roots
        .iter()
        .find_map(|r| path.strip_prefix(&r.path).ok().map(|rel| (r.label.clone(), rel.to_path_buf())));
    let folder = match place {
        Some((label, rel)) => match rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(inner) => format!("{label} / {}", inner.to_string_lossy().replace(['\\', '/'], " / ")),
            None => label,
        },
        None => String::new(),
    };
    LooseMatch {
        index,
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        folder,
        kind: "closest",
        percent: 0,
        cropped: false,
    }
}

/// One record a scan check found, with how it was found.
#[derive(Serialize)]
struct ScanMatch {
    /// The record's id, as in the search results beside it.
    id: i64,
    /// "identical" (the same file), "likely" (very probably the same
    /// photograph), "possible", or "closest" (the nearest there is, which is
    /// probably not the same photograph).
    kind: &'static str,
    /// How alike, as a percentage; 100 for an identical file.
    percent: u32,
    /// Found as a copy cropped differently from the scan, which the plain
    /// comparison did not see.
    cropped: bool,
}

/// A picture in the folders that matched the scan but that no catalog record
/// uses. It has no record number to open, so the window shows it by name and
/// where it is, with its picture fetched by `index` (see `loose-<n>` images).
#[derive(Serialize)]
struct LooseMatch {
    index: usize,
    name: String,
    /// The photo folder it is in, as the settings name it, and any folders
    /// inside that.
    folder: String,
    kind: &'static str,
    percent: u32,
    cropped: bool,
}

/// What a scan check tells the window: how thorough it was, and the records
/// that use pictures like the scan, in the form a search returns them -- so the
/// window draws them as it draws any search, and they open the same way.
#[derive(Serialize)]
struct ScanCheck {
    /// Pictures in the photo list that was searched.
    list_files: i64,
    /// How many of them are the same size as the scan, and so could be copies.
    same_size: u64,
    /// Candidates that could not be read, or that have gone from where the
    /// list says; neither could be ruled in or out.
    unreadable: u64,
    missing: u64,
    /// Identical files found.
    identical: u64,
    /// The scan that was dropped is itself one of the collection's files.
    is_collection_file: bool,
    /// Matching pictures (identical or alike) that no record uses: found in
    /// the folders all the same, and listed beside the records.
    loose: Vec<LooseMatch>,
    /// Pictures compared with for look-alikes, and how many more are not yet
    /// learned and so were left out.
    compared: u64,
    not_yet_learned: u64,
    /// Why the look-alike comparison could not be made, if it could not; the
    /// identical-file check is unaffected.
    look_alike_problem: Option<String>,
    /// Each record found, by how it was found; in the order of `response`.
    matches: Vec<ScanMatch>,
    response: SearchResponse,
}

/// Checks the scan at `path` against the collection: for an identical file, and
/// for pictures that look like it, and finds the records that use them. By path
/// because the person chose that file themselves, by dropping it or picking it;
/// the window is told only about records, and opens them by id as it always does.
#[tauri::command]
async fn photo_check_scan(path: String, more: Option<bool>, part: Option<[f32; 4]>) -> Result<ScanCheck, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let config = store::load_config();
        let roots = photo_list_roots(&config, &resolve_folders(&config));
        let file = scan_file(&path)?;
        photo_check::set_dropped(file.clone());
        let exact = photo_check::find_exact(&roots, &file)?;
        let similar = look_alike::find_similar_in(
            &photo_check::db_path(),
            &file,
            &app_paths::thumb_cache_dir(),
            24,
            part.map(|p| (p[0], p[1], p[2], p[3])),
        );

        // The files in the order they should be listed: identical first, then
        // by how alike. A file the exact check found is not listed twice.
        struct Found {
            path: std::path::PathBuf,
            kind: &'static str,
            percent: u32,
            cropped: bool,
        }
        let mut found: Vec<Found> = exact
            .matches
            .iter()
            .map(|m| Found { path: m.path.clone(), kind: "identical", percent: 100, cropped: false })
            .collect();
        let (compared, not_yet_learned, look_alike_problem) = match similar {
            Ok(s) => {
                let mut closest = Vec::new();
                for item in &s.closest {
                    if found.iter().any(|f| f.path == item.path) {
                        continue;
                    }
                    let percent = (item.score.clamp(0.0, 1.0) * 100.0).round() as u32;
                    let kind = if item.score >= look_alike::LIKELY {
                        "likely"
                    } else if item.score >= look_alike::POSSIBLE {
                        "possible"
                    } else {
                        "closest"
                    };
                    closest.push(Found { path: item.path.clone(), kind, percent, cropped: item.cropped });
                }
                // Always at least a few, as the nearest there is: a badly worn
                // copy can score low and still be recognised by eye.
                let strong = closest.iter().filter(|f| f.kind != "closest").count();
                // "Show more" lists a dozen of the nearest instead of a few.
                let floor: usize = if more.unwrap_or(false) { 12 } else { 3 };
                let keep = strong.max(floor.saturating_sub(found.len()));
                found.extend(closest.into_iter().take(keep));
                (s.compared as u64, s.pending, None)
            }
            Err(e) => (0, 0, Some(e)),
        };

        let files: Vec<std::path::PathBuf> = found.iter().map(|f| f.path.clone()).collect();
        let (records, unlinked) = store::records_for_files(&files);
        let matches: Vec<ScanMatch> = records
            .iter()
            .map(|(r, file)| ScanMatch {
                id: r.id,
                kind: found[*file].kind,
                percent: found[*file].percent,
                cropped: found[*file].cropped,
            })
            .collect();
        let loose_paths: Vec<std::path::PathBuf> = unlinked.iter().map(|&i| found[i].path.clone()).collect();
        let loose: Vec<LooseMatch> = unlinked
            .iter()
            .enumerate()
            .map(|(n, &i)| {
                let f = &found[i];
                LooseMatch { kind: f.kind, percent: f.percent, cropped: f.cropped, ..loose_match(n, &f.path, &roots) }
            })
            .collect();
        photo_check::set_loose(loose_paths);
        let results: Vec<store::SearchResult> = records.into_iter().map(|(r, _)| r).collect();
        let total = results.len() as i64;
        Ok(ScanCheck {
            list_files: exact.list_files,
            same_size: exact.same_size,
            unreadable: exact.unreadable,
            missing: exact.missing,
            identical: exact.matches.len() as u64,
            is_collection_file: exact.matches.iter().any(|m| m.is_the_same_file),
            loose,
            compared,
            not_yet_learned,
            look_alike_problem,
            matches,
            response: SearchResponse {
                results,
                has_more: false,
                total,
                terms: Vec::new(),
                words: Vec::new(),
                datasets: Vec::new(),
            },
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Chooses one scan to check. Async for the same reason as `pick_folder`.
#[tauri::command]
async fn pick_scan(app: tauri::AppHandle) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .add_filter("Pictures", scan_formats::PICTURE_EXTENSIONS)
            .blocking_pick_file()
            .map(|p| p.to_string())
    })
    .await
    .unwrap_or(None)
}

/// Forgets the photo list. It is only a copy of what the folders say, and
/// the next update makes it again.
#[tauri::command]
fn photo_check_clear(state: tauri::State<'_, AppState>) -> Result<(), String> {
    if state.photo_check.lock().unwrap().in_progress {
        return Err("The photo list is being updated; try again when it has finished.".into());
    }
    photo_check::clear()
}

/// Kicks off an index rebuild on a background thread and returns immediately,
/// so the window stays responsive; the UI polls `get_status` for progress.
#[tauri::command]
fn start_refresh(app: tauri::AppHandle) -> bool {
    let start_now = {
        let state = app.state::<AppState>();
        let mut refresh = state.refresh.lock().unwrap();
        refresh.rebuild_requested()
    };
    if start_now {
        spawn_rebuild(app);
    }
    start_now
}

/// Runs one rebuild on its own thread, and starts another if one was asked for
/// while this was running.
fn spawn_rebuild(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let config = store::load_config();
        let Folders { data_dir, images_dir, multimedia_dir, roots, documents, spreadsheets } = resolve_folders(&config);

        let app_for_progress = app.clone();
        let mut progress = move |phase: &str, current: Option<u64>, total: Option<u64>| {
            let state = app_for_progress.state::<AppState>();
            let mut refresh = state.refresh.lock().unwrap();
            refresh.phase = Some(phase.to_string());
            refresh.phase_current = current;
            refresh.phase_total = total;
        };

        // Rebuilding replaces the existing index, so check the folders really
        // hold what they are supposed to before going ahead.
        let result = if let Err(reason) =
            ingest::rebuild_readiness(&config.data_dir, &data_dir, &roots, &documents, &spreadsheets)
        {
            diagnostics::warn("index", format!("refused to rebuild: {reason}"));
            Err(reason)
        } else {
            // Looked for only when there is a PDF folder to need it, and only
            // when wanted: starting Tesseract just to ask its version is not
            // free on a machine that scans every new program.
            let engine = if documents.is_empty() || !config.recognize_scanned_pdfs {
                None
            } else {
                Some(ocr::Engine::locate(app.path().resource_dir().ok().as_deref()))
            };
            if let Some(Ok(engine)) = &engine {
                diagnostics::info("ocr", format!("text recognition: {}", engine.version));
            }
            let settings = ingest::DocumentSettings {
                roots: &documents,
                spreadsheets: &spreadsheets,
                recognition: match &engine {
                    _ if !config.recognize_scanned_pdfs => ingest::Recognition::Off,
                    Some(Ok(engine)) => ingest::Recognition::On(engine),
                    Some(Err(why)) => ingest::Recognition::Unavailable(why.clone()),
                    None => ingest::Recognition::Off,
                },
                saved_dir: ocr::cache_dir(&app_paths::data_dir()),
                work_dir: app_paths::cache_dir(),
            };
            ingest::build(
                &data_dir,
                &app_paths::db_path(),
                &images_dir,
                &multimedia_dir,
                &roots,
                &settings,
                &app_paths::thumb_cache_dir(),
                Some(&mut progress),
            )
            .map(|r| r.warnings)
            .map_err(|e| e.to_string())
        };

        let owed = {
            let state = app.state::<AppState>();
            let mut refresh = state.refresh.lock().unwrap();
            refresh.rebuild_finished(result)
        };

        if owed {
            diagnostics::info(
                "index",
                "a rebuild was asked for while one was running (the folder settings were \
                 saved); starting it now",
            );
            spawn_rebuild(app);
        }
    });
}

/// Builds the diagnostic report the user can send on when something goes
/// wrong on a machine that can't be inspected directly.
#[tauri::command]
async fn get_diagnostics() -> diagnostics::DiagnosticsReport {
    // Off the main thread: this opens every catalog table and decodes an
    // image, which would otherwise freeze the window while it runs.
    tauri::async_runtime::spawn_blocking(|| diagnostics::report(env!("CARGO_PKG_VERSION")))
        .await
        .unwrap_or_else(|e| diagnostics::DiagnosticsReport {
            text: format!("The diagnostic report itself failed to run: {e}"),
            probe_image: None,
            log_path: diagnostics::log_path().display().to_string(),
        })
}

/// Saves the diagnostic report to a file the user chooses, for sending on.
/// Copying from the window works too, but a report runs to a few hundred
/// lines and a file survives an email client's idea of formatting.
#[tauri::command]
async fn save_diagnostics(app: tauri::AppHandle, text: String) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<bool, String> {
        let Some(dest) = app
            .dialog()
            .file()
            .set_file_name("historical-search-diagnostics.txt")
            .blocking_save_file()
        else {
            return Ok(false); // user cancelled
        };
        let path = dest.into_path().map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| e.to_string())?;
        Ok(true)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Lets the frontend record what only it can see -- the webview's own errors,
/// and whether the image protocol actually worked in this browser engine.
#[tauri::command]
fn log_from_ui(level: String, message: String) {
    match level.as_str() {
        "error" => diagnostics::error("webview", message),
        "warn" => diagnostics::warn("webview", message),
        _ => diagnostics::info("webview", message),
    }
}

#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    // Must be `async` -- Tauri runs sync commands on the main thread, but the
    // blocking dialog needs that same thread free to actually display, so a
    // sync version deadlocks against itself.
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog().file().blocking_pick_folder().map(|p| p.to_string())
    })
    .await
    .unwrap_or(None)
}

/// Chooses one Excel workbook. Async for the same reason as `pick_folder`.
#[tauri::command]
async fn pick_spreadsheet(app: tauri::AppHandle) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .add_filter("Excel workbooks", &["xlsx", "xlsm", "xls"])
            .blocking_pick_file()
            .map(|p| p.to_string())
    })
    .await
    .unwrap_or(None)
}

/// Saves a copy of one image wherever the user chooses, at the best
/// resolution available -- the high-resolution master when PastPerfect has
/// one, otherwise its downsized display copy.
///
/// Takes an image id rather than a path, so the UI never handles (or can
/// leak) real filesystem locations inside the PastPerfect data folders.
#[tauri::command]
async fn download_image(
    app: tauri::AppHandle,
    kind: String,
    id: i64,
    suggested_name: String,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<bool, String> {
        // Same (kind, id) pairing the image protocol uses -- "imgthumb" is an
        // image id, "thumb" an item id on an index built before per-image rows.
        let source = match kind.as_str() {
            "imgthumb" => store::image_files(id)
                .ok_or("image not found")?
                .best_available()
                .to_path_buf(),
            "thumb" => store::image_paths(id)
                .ok_or("item not found")?
                .1
                .ok_or("this item has no image on file")?,
            _ => return Err("unknown image kind".into()),
        };
        if !source.is_file() {
            return Err("that image file is no longer where the index recorded it".into());
        }

        let extension = source.extension().and_then(|e| e.to_str()).unwrap_or("");
        let file_name = if extension.is_empty() {
            suggested_name
        } else {
            format!("{suggested_name}.{extension}")
        };

        let Some(dest) = app.dialog().file().set_file_name(&file_name).blocking_save_file() else {
            return Ok(false); // user cancelled
        };
        let dest_path = dest.into_path().map_err(|e| e.to_string())?;
        std::fs::copy(&source, &dest_path).map_err(|e| e.to_string())?;
        Ok(true)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Which pages of a PDF match what was typed, best first.
#[tauri::command]
fn find_pages(id: i64, query: String) -> Vec<store::PageHit> {
    store::find_pages(id, &query)
}

/// The words of a Word or text document, for its card.
#[tauri::command]
fn read_document_text(id: i64) -> Option<String> {
    store::document_text(id)
}

/// The most of a PDF handed over in one piece. The viewer asks for a file a
/// piece at a time -- a scanned volume can be over a gigabyte, and only the
/// page being looked at is needed -- and a single page's picture can itself
/// be tens of megabytes.
const MAX_PIECE_BYTES: u64 = 128 * 1024 * 1024;

/// A piece of a PDF, as raw bytes: from `start` up to but not including `end`
/// (or the end of the file, if that comes first).
///
/// By the record's id, like every other request for a file, so the window can
/// only read files the index recorded. Raw bytes rather than a list of
/// numbers: a list of nine million numbers is how a single scanned page would
/// otherwise arrive.
#[tauri::command]
async fn read_document_range(id: i64, start: u64, end: u64) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<tauri::ipc::Response, String> {
        use std::io::{Read, Seek, SeekFrom};
        let path = store::document_path(id).ok_or("that document is not in the index")?;
        let mut file = std::fs::File::open(&path).map_err(|e| format!("could not open the PDF: {e}"))?;
        let length = file.metadata().map_err(|e| e.to_string())?.len();
        let end = end.min(length).min(start.saturating_add(MAX_PIECE_BYTES));
        if start >= end {
            return Ok(tauri::ipc::Response::new(Vec::new()));
        }
        file.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
        let mut bytes = vec![0u8; (end - start) as usize];
        file.read_exact(&mut bytes).map_err(|e| format!("could not read the PDF: {e}"))?;
        Ok(tauri::ipc::Response::new(bytes))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Shows a PDF in the computer's file manager, selected -- for when opening it
/// in a PDF viewer is refused or fails.
#[tauri::command]
async fn reveal_document(app: tauri::AppHandle, id: i64) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let path = store::document_path(id).ok_or("that document is not in the index")?;
    if !path.exists() {
        return Err(format!("the file is no longer where the index recorded it ({})", path.display()));
    }
    app.opener().reveal_item_in_dir(&path).map_err(|e| {
        diagnostics::error("documents", format!("could not show {} in its folder: {e}", path.display()));
        format!("the computer could not show this file in its folder: {e}")
    })
}

/// Shows a matching file that no record uses in the computer's file manager.
#[tauri::command]
async fn reveal_loose_match(app: tauri::AppHandle, index: usize) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let path = photo_check::loose(index).ok_or("that picture is no longer in the list; check the scan again")?;
    if !path.exists() {
        return Err("the file is no longer where the list recorded it; refresh the search index".into());
    }
    app.opener().reveal_item_in_dir(&path).map_err(|e| format!("the computer could not show this file in its folder: {e}"))
}

/// Opens a document in whatever the computer uses for that kind of file.
///
/// Takes the record's id, not a path, for the same reason `download_image`
/// does: the window can then only ever open a file the index recorded, and
/// never holds -- or can be made to pass on -- a location of its own choosing.
#[tauri::command]
async fn open_document(app: tauri::AppHandle, id: i64) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let path = store::document_path(id).ok_or("that document is not in the index")?;
    if !path.is_file() {
        return Err(format!(
            "the file is no longer where the index recorded it ({}). It may have been moved or \
             renamed -- refreshing the index will update the list.",
            path.display()
        ));
    }
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(|e| {
        diagnostics::error("documents", format!("could not open {}: {e}", path.display()));
        format!("the computer could not open this file: {e}")
    })
}

// ------------------------------------------------------- image protocol

/// The longest side, in pixels, of the enlarged copy shown when a photograph
/// is viewed full size. The card's own picture is 1600; this is for reading
/// small writing, so it is as large as is sensible to decode and hold.
const LARGE_MAX_DIM: u32 = 4000;

/// The files an enlarged view can be made from: the best one (the
/// high-resolution master when PastPerfect has one, else the display copy)
/// and, when that is a master, the display copy to fall back on if the
/// master cannot be read.
///
/// By the same (kind, id) pairs `download_image` takes, so the window never
/// holds a path.
fn viewer_source(kind: &str, id: i64) -> Result<(std::path::PathBuf, Option<std::path::PathBuf>), String> {
    match kind {
        "imgthumb" => {
            let files = store::image_files(id).ok_or("image not found")?;
            let fallback = files.master.is_some().then(|| files.display.clone());
            Ok((files.best_available().to_path_buf(), fallback))
        }
        "thumb" => {
            let original = store::image_paths(id)
                .ok_or("item not found")?
                .1
                .ok_or("this item has no image on file")?;
            Ok((original, None))
        }
        _ => Err("unknown image kind".into()),
    }
}

/// A browser-safe JPEG of an image at `LARGE_MAX_DIM`, made once and kept.
/// Tries the best file first and the display copy if that cannot be decoded.
fn large_preview(
    best: &std::path::Path,
    fallback: Option<&std::path::Path>,
    cache_dir: &std::path::Path,
    max_dim: u32,
) -> Option<std::path::PathBuf> {
    thumbnails::get_preview_jpeg(best, cache_dir, max_dim)
        .or_else(|| fallback.and_then(|f| thumbnails::get_preview_jpeg(f, cache_dir, max_dim)))
}

/// Makes the enlarged copy of an image ready, so the window can then show it.
/// Done here, away from the image protocol, because a large scan takes a few
/// seconds to decode and the window should say it is working meanwhile.
#[tauri::command]
async fn prepare_large_image(kind: String, id: i64) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let (best, fallback) = viewer_source(&kind, id)?;
        if !best.is_file() && !fallback.as_deref().is_some_and(|f| f.is_file()) {
            return Err("that image file is no longer where the index recorded it".into());
        }
        large_preview(&best, fallback.as_deref(), &app_paths::large_cache_dir(), LARGE_MAX_DIM)
            .map(|_| ())
            .ok_or_else(|| "this image could not be enlarged".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The file a scan check or picture search is about. A person's own file is
/// named by its path. A catalog image is named `record-image:KIND:ID`, as the
/// window names images everywhere (the window never sees where they are kept),
/// so that choosing one runs exactly the same check as dropping its file would.
fn scan_file(path: &str) -> Result<std::path::PathBuf, String> {
    if let Some(rest) = path.strip_prefix("record-image:") {
        let (kind, id) = rest.split_once(':').ok_or("That picture could not be found.")?;
        let id: i64 = id.parse().map_err(|_| "That picture could not be found.".to_string())?;
        let (best, fallback) = viewer_source(kind, id)?;
        return if best.is_file() {
            Ok(best)
        } else {
            fallback.filter(|f| f.is_file()).ok_or_else(|| "The picture is no longer where the index recorded it.".to_string())
        };
    }
    Ok(std::path::PathBuf::from(store::clean_path_input(path)))
}

/// Opens an image in whatever the computer uses for pictures: the master when
/// there is one, else the display copy.
///
/// By id for the same reason `open_document` is: the window can only open
/// files the index recorded.
#[tauri::command]
async fn open_image(app: tauri::AppHandle, kind: String, id: i64) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let (best, fallback) = viewer_source(&kind, id)?;
    let path = if best.is_file() {
        best
    } else {
        fallback.filter(|f| f.is_file()).ok_or_else(|| {
            format!(
                "the image is no longer where the index recorded it ({}). It may have been moved \
                 or renamed -- refreshing the index will update the list.",
                best.display()
            )
        })?
    };
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(|e| {
        diagnostics::error("images", format!("could not open {}: {e}", path.display()));
        format!("the computer could not open this image: {e}")
    })
}

/// Serves images to the webview over a custom `hsimg://` scheme:
/// `hsimg://localhost/thumb-123` for an item's leading thumbnail (what the
/// results list shows) and `.../imgthumb-45` for one specific image of an
/// item that has several. Keyed by id rather than path so the UI never sees,
/// and cannot wander into, the real PastPerfect folders.
///
/// The `kind-id` form (rather than `kind/id`) is deliberate: the frontend
/// builds these URLs with Tauri's `convertFileSrc`, which percent-encodes
/// its argument, so a "/" would arrive here as "%2F" and fail to parse --
/// silently breaking every image. A hyphen survives encoding untouched.
fn handle_image_request(request: &tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    let not_found = || {
        tauri::http::Response::builder()
            .status(404)
            .body(Vec::new())
            .unwrap()
    };

    let path = request.uri().path().to_string();
    let Some((kind, id)) = parse_image_request_path(&path) else {
        // The webview builds these URLs itself, and the scheme is served
        // differently per platform ("hsimg://localhost/..." against
        // "http://hsimg.localhost/..."), so a shape this side does not
        // recognise is exactly the kind of difference worth catching.
        diagnostics::error_repeating(
            "image-url-unrecognised",
            "images",
            format!("the webview asked for an image at {path:?}, which is not a form this app serves"),
        );
        return not_found();
    };
    let Some((bytes, content_type)) = load_image_bytes(kind, id) else {
        diagnostics::error_repeating(
            "image-not-served",
            "images",
            format!("could not serve {kind} image {id} (see the preview entries above for why)"),
        );
        return not_found();
    };

    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", content_type)
        .header("Cache-Control", "no-cache")
        .body(bytes)
        .unwrap()
}

/// Loads the bytes for an item image, regenerating a missing thumbnail on
/// demand.
///
/// Thumbnails live in the OS cache directory, which macOS purges under disk
/// pressure and cleanup tools clear routinely -- that's the whole point of
/// putting regenerable data there. But without this, a purge left every
/// image in the app broken until someone happened to run a full refresh.
fn load_image_bytes(kind: &str, id: i64) -> Option<(Vec<u8>, &'static str)> {
    let cache_dir = app_paths::thumb_cache_dir();
    let file = match kind {
        // `id` is an item id: its leading image, as shown in the results list.
        "thumb" => {
            let (thumb, original) = store::image_paths(id)?;
            resolve_thumb(thumb, original.as_deref(), &cache_dir)
        }
        "original" => store::image_paths(id)?.1,
        // The enlarged copy of one image (`large`, by image id) or of an
        // item's leading image (`largeitem`, by item id, for an index built
        // before per-image rows).
        "large" | "largeitem" => {
            let source = if kind == "large" { "imgthumb" } else { "thumb" };
            let (best, fallback) = viewer_source(source, id).ok()?;
            large_preview(&best, fallback.as_deref(), &app_paths::large_cache_dir(), LARGE_MAX_DIM)
        }
        // `id` is an image id: one specific image of an item that has several.
        "imgthumb" => {
            let files = store::image_files(id)?;
            resolve_thumb(files.thumb, Some(&files.display), &cache_dir)
        }
        // The scan the person last checked: not a catalog record, and not in
        // the collection.
        "dropped" => thumbnails::get_preview_jpeg(&photo_check::dropped()?, &cache_dir, ingest::THUMB_MAX_DIM),
        // A matching file that no record uses: `id` is its place in the list
        // the last check made.
        "loose" => thumbnails::get_preview_jpeg(&photo_check::loose(usize::try_from(id).ok()?)?, &cache_dir, ingest::THUMB_MAX_DIM),
        _ => None,
    }?;
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) => {
            diagnostics::error_repeating(
                "image-read-failed",
                "images",
                format!("could not read {}: {e}", file.display()),
            );
            return None;
        }
    };

    let content_type = match file.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("tif") | Some("tiff") => "image/tiff",
        _ => "image/jpeg",
    };
    Some((bytes, content_type))
}

/// Returns a usable thumbnail path, regenerating it if the recorded cache
/// entry has gone missing (the OS purges the cache directory, which is
/// exactly why regenerable data lives there). Kept separate from the
/// request handler so it can be tested without touching the real cache.
fn resolve_thumb(
    recorded: Option<std::path::PathBuf>,
    original: Option<&std::path::Path>,
    cache_dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    match recorded {
        Some(p) if p.is_file() => Some(p), // cache hit
        _ => original
            .and_then(|src| thumbnails::get_preview_jpeg(src, cache_dir, ingest::THUMB_MAX_DIM)),
    }
}

/// Splits an image request path like "thumb-123" into ("thumb", 123).
/// Extracted so the parsing is testable without spinning up a webview.
fn parse_image_request_path(path: &str) -> Option<(&str, i64)> {
    let path = path.trim_start_matches('/');
    let (kind, id_text) = path.split_once('-')?;
    if !matches!(kind, "thumb" | "original" | "imgthumb" | "large" | "largeitem" | "dropped" | "loose") {
        return None;
    }
    id_text.parse::<i64>().ok().map(|id| (kind, id))
}



// ------------------------------------------------------------------ setup

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Before anything else: the release build aborts on panic, so without a
    // hook in place a crash during startup would leave no trace at all.
    diagnostics::install_panic_hook();
    diagnostics::start_session(env!("CARGO_PKG_VERSION"));
    // A rebuild that was interrupted leaves its half-finished index behind,
    // and startup is the moment when this copy of the application certainly
    // is not in the middle of one.
    ingest::remove_abandoned_build_files(&app_paths::data_dir(), ingest::ABANDONED_BUILD_AGE);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .setup(|app| {
            // The face models travel with the application, beside the program.
            if let Ok(dir) = app.path().resource_dir() {
                faces::set_model_dir(&dir.join("models"));
            }
            clip::set_model_dir(&app_paths::clip_models_dir());
            Ok(())
        })
        .register_uri_scheme_protocol("hsimg", |_ctx, request| handle_image_request(&request))
        .invoke_handler(tauri::generate_handler![
            search,
            get_item,
            get_config,
            save_config,
            validate_path,
            get_status,
            start_refresh,
            photo_check_status,
            photo_check_update,
            photo_check_clear,
            photo_check_scan,
            picture_search_status,
            picture_search_download,
            picture_search_stop,
            picture_search,
            pick_scan,
            pick_folder,
            pick_spreadsheet,
            download_image,
            prepare_large_image,
            open_image,
            open_document,
            find_pages,
            read_document_range,
            read_document_text,
            reveal_document,
            reveal_loose_match,
            get_diagnostics,
            save_diagnostics,
            log_from_ui,
        ])
        .build(tauri::generate_context!())
        .expect("error while starting the application")
        // Closing the window does not return from here -- the process ends
        // from inside, so anything written after this call is never written at
        // all. That is why the shutdown has to be recorded from the event:
        // otherwise every run looks like a crash to the next one, and the
        // warning about it becomes noise that hides a real crash.
        .run(|_handle, event| {
            if let tauri::RunEvent::Exit = event {
                diagnostics::end_session();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::parse_image_request_path;
    use super::RefreshState;

    #[test]
    fn asking_for_a_rebuild_during_one_gets_a_rebuild_rather_than_silence() {
        // Saving folder settings asks for a rebuild. A rebuild of this
        // collection takes hours the first time, so it is perfectly ordinary to
        // change the settings while one is running -- and the request used to
        // be dropped on the floor, leaving the window to report success for an
        // index built from the folders that had just been replaced.
        let mut state = RefreshState::default();

        assert!(state.rebuild_requested(), "with nothing running, one should start");
        assert!(state.in_progress);
        assert!(!state.rebuild_again);

        // Asked for again while that one runs.
        assert!(!state.rebuild_requested(), "a second one must not run at the same time");
        assert!(state.rebuild_again, "but it must be remembered");
        assert!(state.in_progress, "the running one carries on");

        // Several requests still only owe one more rebuild.
        assert!(!state.rebuild_requested());
        assert!(!state.rebuild_requested());

        let owed = state.rebuild_finished(Ok(vec![]));
        assert!(owed, "the remembered request should now be honoured");
        assert!(
            state.in_progress,
            "it must never look finished in between, or the window would announce an index \
             that is about to be replaced"
        );
        assert!(!state.rebuild_again, "the request has been taken, not left to repeat forever");
        assert_eq!(state.phase_current, None, "the count from the last one must not carry over");

        // The second rebuild ends with nothing else owed.
        assert!(!state.rebuild_finished(Ok(vec![])), "no third rebuild was asked for");
        assert!(!state.in_progress);
        assert_eq!(state.phase, None);
    }

    #[test]
    fn a_rebuild_that_failed_still_honours_the_one_asked_for_after_it() {
        // Likely the most useful case of all: the rebuild was refused because a
        // folder was wrong, the user corrects it in the settings while the
        // refusal is on screen, and saving must actually rebuild.
        let mut state = RefreshState::default();
        assert!(state.rebuild_requested());
        assert!(!state.rebuild_requested());

        assert!(state.rebuild_finished(Err("the Data folder holds no catalog".into())));
        assert_eq!(state.last_error.as_deref(), Some("the Data folder holds no catalog"));
        assert!(state.in_progress, "the owed rebuild is starting");

        // And a successful run clears the error from the failed one.
        assert!(!state.rebuild_finished(Ok(vec!["a folder could not be read".into()])));
        assert_eq!(state.last_error, None);
        assert_eq!(state.warnings.len(), 1);
    }

    /// A purged cache entry must regenerate rather than 404. Uses a scratch
    /// cache directory and a fabricated "missing" path, so it never disturbs
    /// the real cache that other tests read concurrently.
    #[test]
    #[cfg(feature = "live-data")]
    fn a_missing_thumbnail_is_regenerated_from_the_original() {
        // Any real image from the collection will do as the source.
        let original = crate::store::search("photo", 0, false)
            .results
            .iter()
            .filter_map(|r| crate::store::image_paths(r.id))
            .filter_map(|(_, original)| original)
            .find(|p| p.is_file())
            .expect("expected at least one item with a readable original image");

        let cache_dir = std::env::temp_dir()
            .join(format!("hs_thumb_regen_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache_dir);

        let recorded_but_gone = cache_dir.join("aa").join("does-not-exist.jpg");
        assert!(!recorded_but_gone.is_file(), "precondition: recorded thumbnail is missing");

        let resolved = super::resolve_thumb(
            Some(recorded_but_gone),
            Some(original.as_path()),
            &cache_dir,
        )
        .expect("a missing thumbnail should be regenerated, not dropped");

        assert!(resolved.is_file(), "regenerated thumbnail should exist on disk");
        let bytes = std::fs::read(&resolved).expect("could not read regenerated thumbnail");
        assert!(!bytes.is_empty());
        image::load_from_memory(&bytes).expect("regenerated thumbnail is not a valid image");

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    /// With no original to fall back on there is nothing to regenerate, and
    /// it must report that rather than inventing something.
    #[test]
    fn a_missing_thumbnail_with_no_original_resolves_to_nothing() {
        let cache_dir = std::env::temp_dir().join(format!("hs_thumb_none_{}", std::process::id()));
        let resolved = super::resolve_thumb(
            Some(cache_dir.join("gone.jpg")),
            None,
            &cache_dir,
        );
        assert!(resolved.is_none());
    }

    /// What a newly installed copy does before anything is configured.
    ///
    /// Every other test in this suite runs against this machine's populated
    /// settings, index and preview cache -- precisely the state a fresh
    /// installation does not have. A first run is also the very first thing
    /// that will happen on a user's Windows machine, and the release
    /// build aborts on panic, so anything that panics here is not a wrong
    /// answer but an application that closes the instant it is opened.
    ///
    /// Runs as a child process with its own state folder, for two reasons:
    /// the override is an environment variable and would otherwise leak into
    /// tests running alongside, and a crash shows up as a failed process
    /// rather than taking the whole test run down with it.
    #[test]
    fn a_first_run_with_nothing_configured_does_not_fall_over() {
        use std::process::Command;
        if std::env::var("HS_COLD_START_CHILD").is_ok() {
            return; // this process *is* the child; the work is below
        }
        let state = std::env::temp_dir()
            .join(format!("hs_coldstart_{}_{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("could not make an empty state folder");

        let output = Command::new(std::env::current_exe().expect("test binary path"))
            .args(["--exact", "tests::cold_start_child", "--include-ignored", "--nocapture"])
            .env("HS_STATE_DIR", &state)
            .env("HS_COLD_START_CHILD", "1")
            .output()
            .expect("could not run the cold-start child process");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "a first run with nothing configured failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        assert!(
            stdout.contains("COLD START OK"),
            "the child did not reach the end of the first-run checks.\n{stdout}\n{stderr}"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    /// The body of the test above. Ignored so it only runs when the parent
    /// starts it with an isolated state folder.
    #[test]
    #[ignore = "run by a_first_run_with_nothing_configured_does_not_fall_over"]
    fn cold_start_child() {
        if std::env::var("HS_COLD_START_CHILD").is_err() {
            return;
        }
        let data = crate::app_paths::data_dir();
        assert!(
            !crate::app_paths::db_path().exists(),
            "the state folder should be empty, but an index is already there at {}",
            data.display()
        );

        // Settings: none saved yet. Must read as empty, not as a fault.
        let (config, problem) = crate::store::load_config_checked();
        assert!(problem.is_none(), "a missing settings file was reported as a problem: {problem:?}");
        assert!(config.data_dir.is_empty() && config.photo_archive_roots.is_empty());

        // Searching before anything is indexed: empty, and no crash.
        for query in ["photo", "2010.19.18", "", "   ", "\"*(", "a"] {
            let r = crate::store::search(query, 0, false);
            assert!(r.results.is_empty() && r.total == 0, "{query:?} found something in an empty index");
        }
        assert!(crate::store::search("photo", 0, true).results.is_empty());
        assert_eq!(crate::store::record_count(), 0);
        assert!(crate::store::last_updated().is_none());

        // Opening an item, and the image protocol, with nothing to serve.
        assert!(crate::store::get_item(1).is_none());
        assert!(crate::store::image_paths(1).is_none());
        assert!(crate::store::image_files(1).is_none());
        assert!(crate::store::sample_image_for_probe().is_none());
        assert!(crate::store::probe_thumbnail().is_err(), "there is nothing to preview yet");

        // Checking folders the user has not chosen yet.
        for kind in ["pastperfect_data", "photo_folder", "multimedia_folder", "generic"] {
            assert!(!crate::store::validate_path(kind, "").ok);
            assert!(!crate::store::validate_path(kind, "/no/such/folder/at/all").ok);
        }

        // The diagnostic report has to work when there is nothing to report,
        // because that is exactly when someone will reach for it.
        let report = crate::diagnostics::report("test");
        assert!(report.probe_image.is_none());
        for expected in ["diagnostics", "platform:", "no index"] {
            assert!(
                report.text.contains(expected),
                "the first-run report is missing {expected:?}:\n{}",
                report.text
            );
        }
        assert!(crate::store::index_report().contains("no index"));
        assert_eq!(crate::ingest::longest_indexed_path().0, 0);
        // Producing a report must not bring an index into being. It did:
        // opening a SQLite database creates it, so asking for diagnostics on
        // a machine that had never indexed anything left an empty one behind,
        // after which the application reported having an index with no
        // records in it.
        assert!(
            !crate::app_paths::db_path().exists(),
            "producing a diagnostic report created an index file at {}",
            crate::app_paths::db_path().display()
        );
        assert!(crate::store::search("photo", 0, false).results.is_empty());
        assert!(crate::store::last_updated().is_none(), "an index appeared from nowhere");

        // Saving settings for the first time must create the file.
        let fresh = crate::store::Config {
            data_dir: "  \"/somewhere/that/does/not/exist\"  ".into(),
            ..Default::default()
        };
        crate::store::save_config(&fresh).expect("could not save settings on a first run");
        let (read_back, _) = crate::store::load_config_checked();
        assert_eq!(
            read_back.data_dir, "/somewhere/that/does/not/exist",
            "settings saved on a first run should come back cleaned of quotes and spacing"
        );

        println!("COLD START OK");
    }

    fn config_with(data: &str, images: &str, multimedia: &str, roots: &[(&str, &str)]) -> crate::store::Config {
        crate::store::Config {
            data_dir: data.into(),
            images_dir: images.into(),
            multimedia_dir: multimedia.into(),
            photo_archive_roots: roots
                .iter()
                .map(|(label, path)| crate::store::PhotoRootConfig {
                    label: (*label).into(),
                    path: (*path).into(),
                })
                .collect(),
            document_roots: Vec::new(),
            spreadsheet_roots: Vec::new(),
            recognize_scanned_pdfs: true,
            picture_search: false,
        }
    }

    /// What a rebuild will actually read, given the settings. This decides
    /// which folders are opened and a rebuild replaces the index, so the
    /// working-out is worth pinning rather than assuming.
    #[test]
    fn the_optional_folders_are_worked_out_from_the_data_folder() {
        use std::path::Path;
        let f = super::resolve_folders(&config_with("/collection/pp5/Data", "", "", &[]));
        assert_eq!(f.data_dir, Path::new("/collection/pp5/Data"));
        assert_eq!(
            f.images_dir,
            Path::new("/collection/pp5/Images"),
            "left blank, Images sits beside Data in a PastPerfect installation"
        );
        assert_eq!(f.multimedia_dir, Path::new("/collection/pp5/Multimedia"));

        // Given explicitly, they are used as given.
        let f = super::resolve_folders(&config_with(
            "/collection/pp5/Data", "/elsewhere/Pictures", "/elsewhere/Masters", &[]));
        assert_eq!(f.images_dir, Path::new("/elsewhere/Pictures"));
        assert_eq!(f.multimedia_dir, Path::new("/elsewhere/Masters"));
    }

    #[test]
    fn a_blank_data_folder_does_not_produce_folders_pointing_at_nowhere() {
        use std::path::Path;
        // Someone who only wants photo folders searched leaves the Data
        // setting empty. The old working-out turned that into the bare
        // relative name "Images", which resolves against whatever directory
        // the application happens to have been started in -- on Windows that
        // is wherever the shortcut points.
        let f = super::resolve_folders(&config_with("", "", "", &[("Scans", "/photos/Scans")]));
        assert!(
            !f.images_dir.is_relative() || f.images_dir.as_os_str().is_empty(),
            "with no Data folder set, the Images folder should not become a bare relative \
             name like {:?} that resolves against the working directory",
            f.images_dir
        );
        assert!(
            !f.multimedia_dir.is_relative() || f.multimedia_dir.as_os_str().is_empty(),
            "nor should the Multimedia folder, got {:?}", f.multimedia_dir
        );
        assert_eq!(f.roots.len(), 1, "the photo folder should still be used");
        assert_eq!(f.roots[0].path, Path::new("/photos/Scans"));
    }

    #[test]
    fn photo_folder_rows_left_empty_are_dropped_and_the_rest_tidied() {
        use std::path::Path;
        let f = super::resolve_folders(&config_with(
            "/c/Data", "", "",
            &[("Scans", "/photos/Scans"), ("Nothing", "   "), ("", ""), ("  Slides  ", "  /photos/Slides  ")],
        ));
        assert_eq!(f.roots.len(), 2, "rows with no folder should not become references to nowhere");
        assert_eq!(f.roots[0].label, "Scans");
        assert_eq!(f.roots[1].label, "Slides", "labels should be tidied");
        assert_eq!(f.roots[1].path, Path::new("/photos/Slides"), "and so should paths");
    }

    /// The status line gathers warnings from several places at once.
    #[test]
    fn warnings_are_gathered_from_everywhere_but_held_back_mid_rebuild() {
        let from_rebuild = vec!["MEDIA.DBF could not be read".to_string()];

        let idle = super::assemble_warnings(&from_rebuild, None, false, true);
        assert_eq!(idle.len(), 2, "the rebuild's own warning and the out-of-date index");
        assert!(idle[0].contains("MEDIA.DBF"));
        assert!(idle[1].contains("Refresh Search Index"));

        let busy = super::assemble_warnings(&from_rebuild, None, true, true);
        assert_eq!(
            busy.len(), 1,
            "while rebuilding, the out-of-date warning is about to stop being true and \
             should not be shown"
        );

        let broken_settings = super::assemble_warnings(
            &[], Some("Your saved settings could not be read".into()), false, false);
        assert_eq!(broken_settings.len(), 1);
        assert!(broken_settings[0].contains("settings"));

        assert!(super::assemble_warnings(&[], None, false, false).is_empty(),
            "nothing wrong means nothing said");
    }

    /// The field names the window is written against.
    ///
    /// The two test suites cannot see each other: the Rust tests never load
    /// the window, and the window's tests run against a stand-in backend
    /// written by hand. Rename a field on this side and both suites carry on
    /// passing while disagreeing about what actually crosses between them --
    /// the application would simply stop showing something, with nothing red
    /// to say why. This pins the names in one place that both sides check.
    #[test]
    fn the_shapes_the_window_expects_have_not_changed() {
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/command-shapes.json"),
            )
            .expect("tests/command-shapes.json should be beside the tests"),
        )
        .expect("command-shapes.json is not valid JSON");

        let mut actual = std::collections::BTreeMap::new();
        let mut record = |name: &str, value: serde_json::Value| {
            let mut keys: Vec<String> = value
                .as_object()
                .unwrap_or_else(|| panic!("{name} should serialise to an object"))
                .keys()
                .cloned()
                .collect();
            keys.sort();
            actual.insert(name.to_string(), keys);
        };

        record("SearchResponse", serde_json::to_value(crate::store::SearchResponse {
            results: vec![], has_more: false, total: 0, terms: vec![], words: vec![], datasets: vec![] }).unwrap());
        record("DatasetCount", serde_json::to_value(crate::store::DatasetCount {
            name: String::new(), count: 0 }).unwrap());
        record("SearchResult", serde_json::to_value(crate::store::SearchResult {
            id: 1, catalog: String::new(), accessno: String::new(), title: String::new(),
            snippet: String::new(), has_image: false, is_document: false, document_kind: String::new(), is_exact_id: false }).unwrap());
        record("ItemDetail", serde_json::to_value(crate::store::ItemDetail {
            id: 1, catalog: String::new(), accessno: String::new(), title: String::new(),
            images: vec![], fields: vec![], document: None }).unwrap());
        record("DocumentInfo", serde_json::to_value(crate::store::DocumentInfo {
            file_name: String::new(), pages: 0, has_text: false, size: 0, kind: "pdf" }).unwrap());
        record("PageHit", serde_json::to_value(crate::store::PageHit {
            page: 1, words: 1, snippet: String::new() }).unwrap());
        record("ItemImage", serde_json::to_value(crate::store::ItemImage {
            id: 1, kind: String::new(), caption: String::new(),
            has_thumb: false, has_master: false }).unwrap());
        record("LabeledField", serde_json::to_value(crate::store::LabeledField {
            label: String::new(), value: String::new() }).unwrap());
        record("ValidationResult", serde_json::to_value(crate::store::ValidationResult {
            ok: true, message: String::new() }).unwrap());
        record("Config", serde_json::to_value(crate::store::Config::default()).unwrap());
        record("Status", serde_json::to_value(super::Status {
            last_updated: None, record_count: 0, rebuilds_completed: 0, in_progress: false,
            last_error: None, warnings: vec![], phase: None, phase_current: None,
            phase_total: None }).unwrap());
        record("LooseMatch", serde_json::to_value(super::LooseMatch { index: 0, name: String::new(), folder: String::new(), kind: "identical", percent: 100, cropped: false }).unwrap());
        record("ScanMatch", serde_json::to_value(super::ScanMatch { id: 1, kind: "identical", percent: 100, cropped: false }).unwrap());
        record("ScanCheck", serde_json::to_value(super::ScanCheck {
            list_files: 0, same_size: 0, unreadable: 0, missing: 0, identical: 0,
            is_collection_file: false, loose: Vec::new(), compared: 0, not_yet_learned: 0,
            look_alike_problem: None, matches: vec![],
            response: crate::store::SearchResponse {
                results: vec![], has_more: false, total: 0, terms: vec![], words: vec![], datasets: vec![] } }).unwrap());
        record("PhotoCheckStatus", serde_json::to_value(super::PhotoCheckStatus {
            files: 0, last_scanned: None, in_progress: false, looked_at: 0, phase: None,
            phase_current: 0, phase_total: 0, not_yet_learned: 0, last_error: None,
            notes: vec![] }).unwrap());

        record("PictureSearchStatus", serde_json::to_value(super::PictureSearchStatus {
            enabled: false, installed: false, downloading: false, download_done: 0, download_total: 0,
            download_error: None, described: 0, pending: 0 }).unwrap());
        record("PictureHit", serde_json::to_value(super::PictureHit { kind: "record", id: 1, index: 0 }).unwrap());
        record("PictureSearch", serde_json::to_value(super::PictureSearch {
            hits: vec![], loose: vec![], has_more: false,
            response: crate::store::SearchResponse {
                results: vec![], has_more: false, total: 0, terms: vec![], words: vec![], datasets: vec![] } }).unwrap());

        for (name, keys) in &actual {
            let want = expected
                .get(name)
                .unwrap_or_else(|| panic!("{name} is missing from command-shapes.json"));
            let want: Vec<String> = serde_json::from_value(want.clone()).unwrap();
            assert_eq!(
                keys, &want,
                "{name} no longer serialises to the fields the window reads. Update \
                 tests/command-shapes.json and the window's stand-in backend together, \
                 or the two will disagree in silence."
            );
        }
        let listed: Vec<&String> = expected
            .as_object().unwrap().keys().filter(|k| !k.starts_with('_')).collect();
        assert_eq!(listed.len(), actual.len(), "command-shapes.json lists a type nothing checks");
    }

    /// The real `picture_search` command against a sandbox copy of the real
    /// collection (`HS_STATE_DIR` pointing at a folder holding config.json, the
    /// search index, the described photo list and the model files). Prints only
    /// counts: `cargo test --release --lib picture_search_end_to_end -- --ignored --nocapture`
    #[cfg(feature = "live-data")]
    #[test]
    #[ignore]
    fn picture_search_end_to_end() {
        assert!(std::env::var_os("HS_STATE_DIR").is_some(), "set HS_STATE_DIR to the sandbox folder");
        crate::clip::set_model_dir(&crate::app_paths::clip_models_dir());
        let run = |phrase: Option<&str>, path: Option<String>, limit: usize| {
            tauri::async_runtime::block_on(super::picture_search(phrase.map(String::from), path, None, Some(limit))).unwrap()
        };
        for phrase in ["a horse and wagon", "a wedding", "a school class photograph"] {
            let started = std::time::Instant::now();
            let found = run(Some(phrase), None, 24);
            let records = found.hits.iter().filter(|h| h.kind == "record").count();
            let loose = found.hits.len() - records;
            println!("{} words: {} pictures ({records} with a record, {loose} without), more: {}, in {:?}",
                phrase.split(' ').count(), found.hits.len(), found.has_more, started.elapsed());
            assert_eq!(found.hits.len(), found.response.results.len() + found.loose.len());
            assert!(found.hits.len() > 10, "a collection this size must have pictures for any ordinary phrase");
            for h in &found.hits {
                if h.kind == "record" {
                    assert!(found.response.results.iter().any(|r| r.id == h.id));
                } else {
                    assert!(h.index < found.loose.len());
                }
            }
        }
        // 48 asked for is more than 24 asked for, and starts the same.
        let (a, b) = (run(Some("a barn"), None, 24), run(Some("a barn"), None, 48));
        assert!(b.hits.len() >= a.hits.len());
        assert!(a.hits.iter().zip(&b.hits).take(5).all(|(x, y)| (x.kind, x.id) == (y.kind, y.id)), "asking for more changed the best ones");
    }

    /// Every command the application registers must also be permitted.
    ///
    /// These are two separate lists that have to agree, and when they do not
    /// the build still succeeds -- the failure arrives at runtime as
    /// "not allowed", with a button that quietly does nothing. That has
    /// happened here before, to the folder picker.
    #[test]
    fn every_registered_command_is_also_permitted() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let build_rs = std::fs::read_to_string(dir.join("build.rs")).unwrap();
        let capabilities: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("capabilities/default.json")).unwrap())
                .unwrap();

        let registered: std::collections::BTreeSet<String> = build_rs
            .lines()
            // Each command sits on its own line in build.rs as a quoted
            // string; taking what is between the first pair of quotes avoids
            // depending on the trailing punctuation.
            .filter_map(|line| {
                let trimmed = line.trim();
                if !trimmed.starts_with('"') {
                    return None;
                }
                trimmed.split('"').nth(1).map(str::to_string)
            })
            .collect();
        let permitted: std::collections::BTreeSet<String> = capabilities["permissions"]
            .as_array().unwrap().iter()
            .filter_map(|p| p.as_str()?.strip_prefix("allow-").map(|s| s.replace('-', "_")))
            .collect();

        assert!(!registered.is_empty(), "no commands were found in build.rs");
        let unpermitted: Vec<_> = registered.difference(&permitted).collect();
        assert!(
            unpermitted.is_empty(),
            "registered but not permitted, so calling these fails at runtime: {unpermitted:?}"
        );
        let unregistered: Vec<_> = permitted.difference(&registered).collect();
        assert!(
            unregistered.is_empty(),
            "permitted but not registered, so these permissions are stale: {unregistered:?}"
        );
    }

    #[test]
    fn parses_valid_image_paths() {
        assert_eq!(parse_image_request_path("/thumb-123"), Some(("thumb", 123)));
        assert_eq!(parse_image_request_path("original-7"), Some(("original", 7)));
        assert_eq!(parse_image_request_path("/imgthumb-45"), Some(("imgthumb", 45)));
        assert_eq!(parse_image_request_path("dropped-2"), Some(("dropped", 2)));
    }

    /// "thumb" takes an *item* id and "imgthumb" an *image* id. They are
    /// different numbering spaces, so a kind that parsed loosely would serve
    /// a confidently wrong picture rather than failing.
    #[test]
    fn image_kinds_are_matched_exactly() {
        for path in ["/imgthumbs-1", "/img-1", "/thumbnail-1", "/imgthumb2-1", "/IMGTHUMB-1"] {
            assert_eq!(parse_image_request_path(path), None, "{path:?} should not parse");
        }
    }

    #[test]
    fn the_enlarged_kinds_parse() {
        assert_eq!(parse_image_request_path("/large-12"), Some(("large", 12)));
        assert_eq!(parse_image_request_path("/largeitem-3"), Some(("largeitem", 3)));
        assert_eq!(parse_image_request_path("/larger-3"), None);
    }

    fn write_jpeg(path: &std::path::Path, w: u32, h: u32) {
        image::RgbImage::from_pixel(w, h, image::Rgb([120, 90, 60])).save(path).unwrap();
    }

    /// The enlarged copy keeps far more of the picture than the card's own,
    /// is made in its own folder, and falls back to the display copy when the
    /// master cannot be read.
    #[test]
    fn an_enlarged_copy_is_larger_than_a_thumbnail_and_falls_back() {
        let dir = std::env::temp_dir().join(format!("hs_large_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (master, display) = (dir.join("master.jpg"), dir.join("display.jpg"));
        write_jpeg(&master, 4400, 200);
        write_jpeg(&display, 1200, 60);
        let (thumbs, larges) = (dir.join("thumbs"), dir.join("larges"));

        let thumb = crate::thumbnails::get_preview_jpeg(&master, &thumbs, crate::ingest::THUMB_MAX_DIM).unwrap();
        let large = super::large_preview(&master, Some(&display), &larges, super::LARGE_MAX_DIM).unwrap();
        assert_ne!(thumb, large, "separate folders, so neither overwrites the other");
        let (t, l) = (image::open(&thumb).unwrap(), image::open(&large).unwrap());
        assert_eq!((t.width(), l.width()), (1600, 4000), "a thumbnail is 1600 across, the enlargement 4000");

        // A master that is not an image: the display copy is used instead.
        let broken = dir.join("broken.tif");
        std::fs::write(&broken, "not an image").unwrap();
        let fallback = super::large_preview(&broken, Some(&display), &larges, super::LARGE_MAX_DIM).unwrap();
        assert_eq!(image::open(&fallback).unwrap().width(), 1200, "never enlarged past the original");
        assert!(super::large_preview(&broken, None, &larges, super::LARGE_MAX_DIM).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_malformed_or_unknown_paths() {
        assert_eq!(parse_image_request_path("/thumb"), None);
        assert_eq!(parse_image_request_path("/thumb-abc"), None);
        assert_eq!(parse_image_request_path("/secret-1"), None);
        assert_eq!(parse_image_request_path(""), None);
        // The trap this format exists to avoid: a "/" separator arrives
        // percent-encoded from convertFileSrc and would never parse.
        assert_eq!(parse_image_request_path("/thumb%2F123"), None);
    }

    // ----------------------------------------- which folders the photo list reads

    struct ScratchDirs(std::path::PathBuf);
    impl ScratchDirs {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "hs-list-roots-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            ScratchDirs(dir)
        }
        fn dir(&self, name: &str) -> std::path::PathBuf {
            let p = self.0.join(name);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
    }
    impl Drop for ScratchDirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn path(p: &std::path::Path) -> String {
        p.to_string_lossy().to_string()
    }
    fn labels(roots: &[crate::ingest::PhotoArchiveRoot]) -> Vec<String> {
        roots.iter().map(|r| r.label.clone()).collect()
    }

    #[test]
    fn the_photo_list_reads_the_photo_folders_and_the_two_pastperfect_picture_folders() {
        let s = ScratchDirs::new();
        let pp = s.dir("pp/Data");
        let images = s.dir("pp/Images");
        let media = s.dir("pp/Multimedia");
        let photos = s.dir("photos");
        let config = config_with(&path(&pp), &path(&images), &path(&media), &[("Folder A", &path(&photos))]);
        let roots = super::photo_list_roots(&config, &super::resolve_folders(&config));
        assert_eq!(labels(&roots), vec!["Folder A", "PastPerfect Images", "PastPerfect Multimedia"]);
    }

    #[test]
    fn pastperfect_folders_left_to_their_defaults_count_only_when_they_exist() {
        let s = ScratchDirs::new();
        let pp = s.dir("pp/Data");
        s.dir("pp/Images"); // beside Data, as PastPerfect lays it out; no Multimedia folder
        let config = config_with(&path(&pp), "", "", &[]);
        let roots = super::photo_list_roots(&config, &super::resolve_folders(&config));
        assert_eq!(labels(&roots), vec!["PastPerfect Images"]);
    }

    #[test]
    fn a_pastperfect_folder_named_in_the_settings_is_kept_even_when_it_cannot_be_found() {
        // An unplugged drive: the list must keep what it knew, not forget it.
        let s = ScratchDirs::new();
        let pp = s.dir("pp/Data");
        let gone = s.0.join("not-plugged-in/Images");
        let config = config_with(&path(&pp), &path(&gone), "", &[]);
        let roots = super::photo_list_roots(&config, &super::resolve_folders(&config));
        assert_eq!(labels(&roots), vec!["PastPerfect Images"]);
    }

    #[test]
    fn a_folder_inside_another_is_not_listed_twice() {
        let s = ScratchDirs::new();
        let pp = s.dir("pp/Data");
        let images = s.dir("pp/Images");
        let inside = s.dir("pp/Images/older scans");
        let config = config_with(&path(&pp), &path(&images), "", &[("Inner", &path(&inside))]);
        let roots = super::photo_list_roots(&config, &super::resolve_folders(&config));
        assert_eq!(labels(&roots), vec!["PastPerfect Images"], "the inner folder is already covered");
    }

    #[test]
    fn the_same_folder_named_twice_is_listed_once() {
        let s = ScratchDirs::new();
        let pp = s.dir("pp/Data");
        let images = s.dir("pp/Images");
        let config = config_with(&path(&pp), &path(&images), "", &[("Same", &path(&images))]);
        let roots = super::photo_list_roots(&config, &super::resolve_folders(&config));
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn with_nothing_set_up_there_is_nothing_to_list() {
        let config = crate::store::Config::default();
        assert!(super::photo_list_roots(&config, &super::resolve_folders(&config)).is_empty());
    }
}
