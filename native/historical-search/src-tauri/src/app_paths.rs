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

/// Persistent state: config.json, search_index.db.
pub fn data_dir() -> PathBuf {
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

pub fn db_path() -> PathBuf {
    data_dir().join("search_index.db")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn thumb_cache_dir() -> PathBuf {
    cache_dir().join("thumb_cache")
}
