//! Where the tests find the real collection.
//!
//! The museum's data is not in this repository and never will be, and neither
//! is anything about the machine it happens to sit on -- no user name, no home
//! directory, no folder names particular to one society. Tests that need the
//! real data read its location from the environment instead.
//!
//! Set these before running the full suite:
//!
//! * `HS_TEST_PP5_DIR`    -- the PastPerfect "pp5" folder, the one containing
//!   `Data`, `Images` and `Multimedia`.
//! * `HS_TEST_PHOTOS_DIR` -- a folder whose subfolders are standalone photo
//!   collections. Each subfolder is indexed as its own archive, so the tests
//!   never name a particular one.
//!
//! When either is missing the tests using it fail immediately and say so.
//! They do not quietly pass: a test that reports success without its fixture
//! is worse than no test at all. To leave them out altogether, build with
//! `--no-default-features`, which stops them being compiled -- that is what
//! continuous integration does, since the data cannot exist there.

use std::path::PathBuf;

fn required_dir(var: &str, description: &str) -> PathBuf {
    let value = std::env::var(var).unwrap_or_default();
    assert!(
        !value.trim().is_empty(),
        "{var} is not set. It must point at {description}.\n\
         These tests read the real collection; without it there is nothing to \
         check, and passing anyway would be dishonest.\n\
         Either set {var}, or run `cargo test --no-default-features` to leave \
         the data-backed tests out entirely."
    );
    let path = PathBuf::from(value.trim());
    assert!(
        path.is_dir(),
        "{var} is set to {}, which is not a folder that exists.",
        path.display()
    );
    path
}

/// The PastPerfect `pp5` folder.
pub fn pp5_dir() -> PathBuf {
    required_dir("HS_TEST_PP5_DIR", "the PastPerfect pp5 folder (containing Data, Images, Multimedia)")
}

pub fn data_dir() -> PathBuf {
    pp5_dir().join("Data")
}

pub fn images_dir() -> PathBuf {
    pp5_dir().join("Images")
}

pub fn multimedia_dir() -> PathBuf {
    pp5_dir().join("Multimedia")
}

/// The app's own preview cache, wherever this operating system puts it, so the
/// tests reuse the previews already generated instead of spending twenty
/// minutes rebuilding them.
pub fn thumb_cache_dir() -> PathBuf {
    crate::app_paths::thumb_cache_dir()
}

/// The standalone photo folders, discovered rather than named: every
/// subfolder of `HS_TEST_PHOTOS_DIR`, labelled by its own folder name.
pub fn photo_roots() -> Vec<(String, PathBuf)> {
    let root = required_dir("HS_TEST_PHOTOS_DIR", "a folder whose subfolders are photo collections");
    let mut roots: Vec<(String, PathBuf)> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", root.display()))
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect();
    // Sorted so a rebuild produces the same index every time.
    roots.sort();
    assert!(
        !roots.is_empty(),
        "HS_TEST_PHOTOS_DIR ({}) has no subfolders, so there are no photo \
         collections to index",
        root.display()
    );
    roots
}

/// Finds files under `dir` that satisfy `wanted`, stopping at `limit`.
/// Used instead of naming individual files, which differ between collections.
pub fn find_files<F>(dir: &std::path::Path, limit: usize, mut wanted: F) -> Vec<PathBuf>
where
    F: FnMut(&std::path::Path) -> bool,
{
    fn walk<F>(dir: &std::path::Path, limit: usize, wanted: &mut F, found: &mut Vec<PathBuf>)
    where
        F: FnMut(&std::path::Path) -> bool,
    {
        if found.len() >= limit {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if found.len() >= limit {
                return;
            }
            if path.is_dir() {
                walk(&path, limit, wanted, found);
            } else if wanted(&path) {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, limit, &mut wanted, &mut found);
    found
}

/// A file under `dir` with one of the given extensions (matched
/// case-insensitively, since this collection mixes `.TIF` and `.tif`).
pub fn find_one_with_extension(dir: &std::path::Path, extensions: &[&str]) -> PathBuf {
    let found = find_files(dir, 1, |p| {
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| extensions.iter().any(|w| e.eq_ignore_ascii_case(w)))
            .unwrap_or(false)
    });
    found.into_iter().next().unwrap_or_else(|| {
        panic!("no {extensions:?} file found anywhere under {}", dir.display())
    })
}
