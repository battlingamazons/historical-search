mod app_paths;
mod dbf;
pub mod diagnostics;
pub mod ingest;
mod palette_tiff;
mod store;
#[cfg(all(test, feature = "live-data"))]
mod test_support;
mod thumbnails;

use serde::Serialize;
use std::sync::Mutex;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use store::{Config, ItemDetail, SearchResponse, ValidationResult};

/// Progress/state of an index rebuild. Shared between the background rebuild
/// thread and the UI, which polls it while a refresh runs.
#[derive(Default, Clone, Serialize)]
struct RefreshState {
    in_progress: bool,
    last_error: Option<String>,
    /// Non-fatal problems from the last rebuild (e.g. a catalogue that
    /// couldn't be read, or missing memo text).
    warnings: Vec<String>,
    phase: Option<String>,
    phase_current: Option<u64>,
    phase_total: Option<u64>,
}

#[derive(Default)]
struct AppState {
    refresh: Mutex<RefreshState>,
}

#[derive(Serialize)]
struct Status {
    last_updated: Option<f64>,
    record_count: i64,
    in_progress: bool,
    last_error: Option<String>,
    warnings: Vec<String>,
    phase: Option<String>,
    phase_current: Option<u64>,
    phase_total: Option<u64>,
}

// ---------------------------------------------------------------- commands

#[tauri::command]
fn search(query: String, offset: i64, show_all: bool) -> SearchResponse {
    store::search(&query, offset, show_all)
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

#[tauri::command]
fn validate_path(kind: String, path: String) -> ValidationResult {
    store::validate_path(&kind, &path)
}

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> Status {
    let refresh = state.refresh.lock().unwrap().clone();
    let (_, config_problem) = store::load_config_checked();
    let mut warnings = refresh.warnings.clone();
    warnings.extend(config_problem);
    // Silence this while a rebuild is running -- it is about to stop being
    // true, and saying so mid-refresh just reads as a fault.
    if !refresh.in_progress && store::index_needs_rebuild_for_images() {
        warnings.push(
            "This search index was built before the app could show every image on a record. \
             Click Refresh Search Index to pick up additional images and high-resolution \
             originals."
                .to_string(),
        );
    }
    Status {
        last_updated: store::last_updated(),
        record_count: store::record_count(),
        in_progress: refresh.in_progress,
        last_error: refresh.last_error,
        warnings,
        phase: refresh.phase,
        phase_current: refresh.phase_current,
        phase_total: refresh.phase_total,
    }
}

/// Kicks off an index rebuild on a background thread and returns immediately,
/// so the window stays responsive; the UI polls `get_status` for progress.
#[tauri::command]
fn start_refresh(app: tauri::AppHandle) -> bool {
    {
        let state = app.state::<AppState>();
        let mut refresh = state.refresh.lock().unwrap();
        if refresh.in_progress {
            return false;
        }
        *refresh = RefreshState {
            in_progress: true,
            last_error: None,
            warnings: Vec::new(),
            phase: Some("Starting…".into()),
            phase_current: None,
            phase_total: None,
        };
    }

    std::thread::spawn(move || {
        let config = store::load_config();
        let data_dir = std::path::PathBuf::from(&config.data_dir);
        let images_dir = if config.images_dir.trim().is_empty() {
            data_dir.parent().map(|p| p.join("Images")).unwrap_or_else(|| data_dir.join("Images"))
        } else {
            std::path::PathBuf::from(&config.images_dir)
        };
        let multimedia_dir = ingest::default_multimedia_dir(&config.multimedia_dir, &data_dir);
        let roots: Vec<ingest::PhotoArchiveRoot> = config
            .photo_archive_roots
            .iter()
            .filter(|r| !r.path.trim().is_empty())
            .map(|r| ingest::PhotoArchiveRoot {
                label: r.label.clone(),
                path: std::path::PathBuf::from(&r.path),
            })
            .collect();

        let app_for_progress = app.clone();
        let mut progress = move |phase: &str, current: Option<u64>, total: Option<u64>| {
            let state = app_for_progress.state::<AppState>();
            let mut refresh = state.refresh.lock().unwrap();
            refresh.phase = Some(phase.to_string());
            refresh.phase_current = current;
            refresh.phase_total = total;
        };

        // Refuse to rebuild when nothing usable is configured: otherwise a
        // blank/mistyped Folder Setting would quietly replace a perfectly
        // good index with an empty one, and the only symptom would be that
        // every search suddenly returns nothing.
        let has_catalog = !config.data_dir.trim().is_empty() && data_dir.is_dir();
        let has_photos = roots.iter().any(|r| r.path.is_dir());
        let result = if !has_catalog && !has_photos {
            Err("No readable folders are configured. Open Folder Settings and \
                 choose your PastPerfect Data folder (and any photo folders) \
                 first -- the existing index has been left alone."
                .to_string())
        } else {
            ingest::build(
                &data_dir,
                &app_paths::db_path(),
                &images_dir,
                &multimedia_dir,
                &roots,
                &app_paths::thumb_cache_dir(),
                Some(&mut progress),
            )
            .map(|r| r.warnings)
            .map_err(|e| e.to_string())
        };

        let state = app.state::<AppState>();
        let mut refresh = state.refresh.lock().unwrap();
        refresh.in_progress = false;
        refresh.phase = None;
        refresh.phase_current = None;
        refresh.phase_total = None;
        match result {
            Ok(warnings) => {
                refresh.warnings = warnings;
                refresh.last_error = None;
            }
            Err(e) => refresh.last_error = Some(e),
        }
    });

    true
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

// ------------------------------------------------------- image protocol

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
        // `id` is an image id: one specific image of an item that has several.
        "imgthumb" => {
            let files = store::image_files(id)?;
            resolve_thumb(files.thumb, Some(&files.display), &cache_dir)
        }
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
    if !matches!(kind, "thumb" | "original" | "imgthumb") {
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

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .register_uri_scheme_protocol("hsimg", |_ctx, request| handle_image_request(&request))
        .invoke_handler(tauri::generate_handler![
            search,
            get_item,
            get_config,
            save_config,
            validate_path,
            get_status,
            start_refresh,
            pick_folder,
            download_image,
            get_diagnostics,
            save_diagnostics,
            log_from_ui,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    diagnostics::end_session();
}

#[cfg(test)]
mod tests {
    use super::parse_image_request_path;

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

    #[test]
    fn parses_valid_image_paths() {
        assert_eq!(parse_image_request_path("/thumb-123"), Some(("thumb", 123)));
        assert_eq!(parse_image_request_path("original-7"), Some(("original", 7)));
        assert_eq!(parse_image_request_path("/imgthumb-45"), Some(("imgthumb", 45)));
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
    fn rejects_malformed_or_unknown_paths() {
        assert_eq!(parse_image_request_path("/thumb"), None);
        assert_eq!(parse_image_request_path("/thumb-abc"), None);
        assert_eq!(parse_image_request_path("/secret-1"), None);
        assert_eq!(parse_image_request_path(""), None);
        // The trap this format exists to avoid: a "/" separator arrives
        // percent-encoded from convertFileSrc and would never parse.
        assert_eq!(parse_image_request_path("/thumb%2F123"), None);
    }
}
