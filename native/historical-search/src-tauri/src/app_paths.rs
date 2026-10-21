//! Where the app's runtime state lives -- deliberately outside the source
//! tree: config.json (local folder settings), search_index.db (generated from
//! your data) and thumb_cache/ (generated previews) are all produced by
//! *running* the app, not building it.
//!
//! These intentionally match the paths the Python prototype used, so moving
//! to this native build carries over the existing settings and the thousands of
//! already-generated thumbnails instead of regenerating them from scratch.

use std::path::PathBuf;

const APP_NAME: &str = "Historical Society Search";

fn make(path: PathBuf) -> PathBuf {
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Overrides where the application keeps its own files.
///
/// Exists so a first run can actually be tested. Every other test works from
/// this machine's populated settings, index and preview cache, which is
/// exactly the state a newly installed copy does not have -- and a first run
/// is the very first thing that will happen on the museum's Windows machine.
/// Pointing this at an empty folder reproduces that, without the real state
/// being anywhere nearby.
fn state_dir_override() -> Option<PathBuf> {
    std::env::var_os("HS_STATE_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Persistent state: config.json, search_index.db.
pub fn data_dir() -> PathBuf {
    if let Some(base) = state_dir_override() {
        return make(base.join("data"));
    }
    let base = if cfg!(target_os = "macos") {
        dirs_home().join("Library/Application Support")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(dirs_home)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_home().join(".local/share"))
    };
    make(base.join(APP_NAME))
}

/// Disposable, regenerable state: thumb_cache/. Safe to delete at any time.
pub fn cache_dir() -> PathBuf {
    if let Some(base) = state_dir_override() {
        return make(base.join("cache"));
    }
    let base = if cfg!(target_os = "macos") {
        dirs_home().join("Library/Caches")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(dirs_home)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_home().join(".cache"))
    };
    make(base.join(APP_NAME))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A suffix unique to this process and to each call, for files that are
/// written under a temporary name and then renamed into place.
///
/// Those temporary names used to be fixed, which is safe only while exactly
/// one writer exists. Neither assumption holds. Nothing stops the application
/// being started twice -- on Windows a second click of the shortcut simply
/// runs it again -- and two copies rebuilding at once would have written to
/// one file and then renamed the result over the live index. Within a single
/// copy it is closer still: the indexing thread and the window's own
/// on-demand preview generation can reach the same image at the same moment,
/// and a half-written preview that is renamed into place looks perfectly
/// fresh by its timestamp, so it would be served as a broken image
/// indefinitely.
pub fn unique_temp_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{}.{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

pub fn db_path() -> PathBuf {
    data_dir().join("search_index.db")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn thumb_cache_dir() -> PathBuf {
    cache_dir().join("thumb_cache")
}

/// Enlarged copies made when a photograph is viewed full size. Kept apart
/// from `thumb_cache`, whose files are named after the photograph alone and
/// would otherwise be overwritten by (or serve) a copy of the other size.
pub fn large_cache_dir() -> PathBuf {
    cache_dir().join("large_cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every temporary name must be distinct. These files are written and then
    /// renamed into place, so two writers sharing a name corrupt each other's
    /// output -- and the result still looks valid afterwards.
    #[test]
    fn temporary_names_never_repeat() {
        let names: Vec<String> = (0..1000).map(|_| unique_temp_suffix()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "a temporary file name was handed out twice");
        assert!(
            names[0].starts_with(&format!("{}.", std::process::id())),
            "the suffix should identify this process, so a second copy of the app cannot collide"
        );
    }
}
