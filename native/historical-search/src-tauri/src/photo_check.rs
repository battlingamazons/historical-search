//! The photo list behind "have we already got this photo?".
//!
//! Checking a new scan against the collection starts with knowing what the
//! collection holds. This module keeps that list: every picture in the photo
//! folders, with its size and the time it was last changed. It lives in its own
//! file, beside the search index but not in it, so nothing here can touch the
//! catalogue search, and the whole thing can be deleted and rebuilt at will.
//!
//! The list is brought up to date, not rebuilt. A second run reads only what
//! the folders say (names, sizes, times) and does nothing for a file that is
//! unchanged -- the first pass over tens of thousands of large scans is slow,
//! and repeating it every time would make the check something nobody runs.
//!
//! Two rules keep a bad moment from damaging the list:
//!
//! * A folder that cannot be read -- a drive that is unplugged, a share that is
//!   offline -- keeps what it had. It is not treated as a folder with nothing
//!   in it.
//! * A folder that was read only in part (one subfolder refused) keeps the
//!   entries it could not confirm, for the same reason: "I could not look" is
//!   not "it is gone".
//!
//! Nothing in the folders is ever changed, and file names are not written to
//! the diagnostics log -- only counts.

use crate::ingest::{PhotoArchiveRoot, IMAGE_EXTENSIONS};
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

/// Bumped when the table layout changes. The list is only a copy of what the
/// folders say, so an old layout is dropped and filled again rather than
/// converted.
const SCHEMA_VERSION: i64 = 1;

/// Real photo folders here nest about three deep; this only stops a loop of
/// links from descending until the stack runs out.
const MAX_WALK_DEPTH: usize = 32;

pub fn db_path() -> PathBuf {
    crate::app_paths::data_dir().join("photo_check.db")
}

/// What one folder looked like when the list was brought up to date.
#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct RootReport {
    pub label: String,
    /// False when the folder could not be opened at all.
    pub readable: bool,
    /// Pictures found in the folder this time.
    pub found: u64,
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
    /// Entries kept although they were not seen, because the folder could not
    /// be read in full.
    pub kept_unconfirmed: u64,
    /// Plain-language things the person running it should know.
    pub notes: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct ScanReport {
    pub roots: Vec<RootReport>,
    /// Entries dropped because their folder is no longer one of the photo
    /// folders in the settings.
    pub removed_with_folder: u64,
}

impl ScanReport {
    pub fn notes(&self) -> Vec<String> {
        self.roots.iter().flat_map(|r| r.notes.iter().cloned()).collect()
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Status {
    /// Pictures in the list.
    pub files: i64,
    /// When the list was last brought up to date (seconds since 1970).
    pub last_scanned: Option<f64>,
}

pub(crate) fn open_for_writing(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    prepare_schema(&conn)?;
    Ok(conn)
}

fn prepare_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
    let have: Option<i64> = conn
        .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok());
    if have != Some(SCHEMA_VERSION) {
        conn.execute_batch("DROP TABLE IF EXISTS files;")?;
        conn.execute("DELETE FROM meta", [])?;
        conn.execute("INSERT INTO meta (key, value) VALUES ('schema', ?1)", params![SCHEMA_VERSION.to_string()])?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
             id     INTEGER PRIMARY KEY,
             root   TEXT NOT NULL,   -- the folder, as it is written in the settings
             rel    TEXT NOT NULL,   -- the path below it, with '/' between the parts
             size   INTEGER NOT NULL,
             mtime  INTEGER NOT NULL, -- nanoseconds since 1970
             sha256 BLOB,             -- filled in when a comparison needs it, not before
             UNIQUE (root, rel)
         );
         CREATE INDEX IF NOT EXISTS files_by_size ON files (size);
         -- What a picture looks like, for finding look-alikes. One row per file;
         -- empty data means the file was tried and could not be read. Dropped
         -- with the file's row whenever the file changes or goes.
         CREATE TABLE IF NOT EXISTS fingerprints (
             file_id INTEGER PRIMARY KEY,
             data    BLOB NOT NULL,
             -- For finding a differently cropped copy: its coarse windows, and a
             -- small picture of it. Null until made.
             crop_desc  BLOB,
             crop_thumb BLOB
         );",
    )?;
    // A list made before cropped copies were looked for has no such columns.
    let has: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('fingerprints') WHERE name = 'crop_desc'",
        [],
        |r| r.get(0),
    )?;
    if has == 0 {
        conn.execute_batch("ALTER TABLE fingerprints ADD COLUMN crop_desc BLOB; ALTER TABLE fingerprints ADD COLUMN crop_thumb BLOB;")?;
    }
    Ok(())
}

/// The list's size and age, without creating the file if it is not there yet.
pub fn status() -> Status {
    status_at(&db_path())
}

fn status_at(path: &Path) -> Status {
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Status::default();
    };
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let files = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).unwrap_or(0);
    let last_scanned = conn
        .query_row("SELECT value FROM meta WHERE key = 'last_scanned'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok());
    Status { files, last_scanned }
}

/// When a file was last changed, in nanoseconds since 1970.
fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// One picture as the folder shows it.
struct Found {
    rel: String,
    size: i64,
    mtime: i64,
}

/// Every picture under `root`, and whether every folder in it could be read.
fn walk(root: &Path) -> (Vec<Found>, bool) {
    let mut out = Vec::new();
    let mut complete = true;
    walk_dir(root, root, 0, &mut out, &mut complete);
    (out, complete)
}

fn walk_dir(dir: &Path, root: &Path, depth: usize, out: &mut Vec<Found>, complete: &mut bool) {
    if depth > MAX_WALK_DEPTH {
        *complete = false;
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        *complete = false;
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *complete = false;
            continue;
        };
        let path = entry.path();
        // Followed through links, as the catalogue indexer does, so a folder
        // that is a shortcut to somewhere else still counts.
        let Ok(meta) = fs::metadata(&path) else {
            *complete = false;
            continue;
        };
        if meta.is_dir() {
            walk_dir(&path, root, depth + 1, out, complete);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        // The "._" companions a Mac leaves on other drives, and Word's "~$"
        // markers, carry a real file's name without being one.
        if name.starts_with("._") || name.starts_with("~$") {
            continue;
        }
        let is_picture = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str()));
        if !is_picture {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else { continue };
        let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        out.push(Found { rel, size: meta.len() as i64, mtime: mtime_ns(&meta) });
    }
}

/// Brings the list up to date, for tests of the parts that build on it.
#[cfg(test)]
pub(crate) fn rescan_for_tests(db: &Path, roots: &[PhotoArchiveRoot]) -> Result<ScanReport, String> {
    rescan_into(db, roots, &mut |_| {})
}

/// Brings the list up to date with the photo folders.
///
/// `progress` is told how many pictures have been looked at so far.
pub fn rescan(roots: &[PhotoArchiveRoot], progress: &mut dyn FnMut(u64)) -> Result<ScanReport, String> {
    rescan_into(&db_path(), roots, progress)
}

fn rescan_into(db: &Path, roots: &[PhotoArchiveRoot], progress: &mut dyn FnMut(u64)) -> Result<ScanReport, String> {
    let mut conn = open_for_writing(db).map_err(|e| format!("could not open the photo list: {e}"))?;
    let mut report = ScanReport::default();
    let mut looked_at = 0u64;

    // Folders no longer in the settings are no longer watched.
    let current: Vec<String> = roots.iter().map(|r| r.path.to_string_lossy().to_string()).collect();
    {
        let known: Vec<String> = conn
            .prepare("SELECT DISTINCT root FROM files")
            .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0))?.collect())
            .map_err(|e| e.to_string())?;
        for gone in known.iter().filter(|k| !current.contains(k)) {
            conn.execute(
                "DELETE FROM fingerprints WHERE file_id IN (SELECT id FROM files WHERE root = ?1)",
                params![gone],
            )
            .map_err(|e| e.to_string())?;
            let n = conn.execute("DELETE FROM files WHERE root = ?1", params![gone]).map_err(|e| e.to_string())?;
            report.removed_with_folder += n as u64;
        }
    }

    for root in roots {
        let key = root.path.to_string_lossy().to_string();
        let mut r = RootReport { label: root.label.clone(), ..RootReport::default() };

        let existing: HashMap<String, (i64, i64, i64)> = conn
            .prepare("SELECT rel, id, size, mtime FROM files WHERE root = ?1")
            .and_then(|mut s| {
                s.query_map(params![key], |row| Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?, row.get(3)?))))?
                    .collect()
            })
            .map_err(|e| e.to_string())?;

        if !root.path.is_dir() {
            r.kept_unconfirmed = existing.len() as u64;
            r.notes.push(format!(
                "The folder \"{}\" could not be opened, so what the list already had for it was kept.",
                r.label
            ));
            report.roots.push(r);
            continue;
        }

        let (found, complete) = walk(&root.path);
        looked_at += found.len() as u64;
        progress(looked_at);
        r.readable = true;
        r.found = found.len() as u64;

        // A folder that suddenly holds nothing, when it held something, is far
        // more likely to be a drive that went away than a collection that was
        // deleted; keep the entries and say so.
        let looks_vanished = found.is_empty() && !existing.is_empty();
        let may_remove = complete && !looks_vanished;

        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for f in &found {
            seen.insert(f.rel.as_str());
            match existing.get(&f.rel) {
                None => {
                    tx.execute(
                        "INSERT INTO files (root, rel, size, mtime) VALUES (?1, ?2, ?3, ?4)",
                        params![key, f.rel, f.size, f.mtime],
                    )
                    .map_err(|e| e.to_string())?;
                    r.added += 1;
                }
                Some(&(id, size, mtime)) if size != f.size || mtime != f.mtime => {
                    // Whatever was worked out about the old contents no longer applies.
                    tx.execute(
                        "UPDATE files SET size = ?1, mtime = ?2, sha256 = NULL WHERE id = ?3",
                        params![f.size, f.mtime, id],
                    )
                    .map_err(|e| e.to_string())?;
                    tx.execute("DELETE FROM fingerprints WHERE file_id = ?1", params![id]).map_err(|e| e.to_string())?;
                    r.changed += 1;
                }
                Some(_) => {}
            }
        }
        for (rel, &(id, _, _)) in &existing {
            if seen.contains(rel.as_str()) {
                continue;
            }
            if may_remove {
                tx.execute("DELETE FROM files WHERE id = ?1", params![id]).map_err(|e| e.to_string())?;
                tx.execute("DELETE FROM fingerprints WHERE file_id = ?1", params![id]).map_err(|e| e.to_string())?;
                r.removed += 1;
            } else {
                r.kept_unconfirmed += 1;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;

        if looks_vanished {
            r.notes.push(format!(
                "The folder \"{}\" opened but held no pictures this time, so its earlier list was kept. \
                 If the pictures really were removed, open Folder Settings and choose \"Start the photo list over\".",
                r.label
            ));
        } else if !complete {
            r.notes.push(format!(
                "Part of the folder \"{}\" could not be read, so those pictures were left as they were.",
                r.label
            ));
        }
        report.roots.push(r);
    }

    let now = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('last_scanned', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![now.to_string()],
    )
    .map_err(|e| e.to_string())?;

    let total: u64 = report.roots.iter().map(|r| r.found).sum();
    crate::diagnostics::info(
        "photo-check",
        format!(
            "photo list updated: {total} pictures in {} folders ({} added, {} changed, {} removed)",
            report.roots.len(),
            report.roots.iter().map(|r| r.added).sum::<u64>(),
            report.roots.iter().map(|r| r.changed).sum::<u64>(),
            report.roots.iter().map(|r| r.removed).sum::<u64>() + report.removed_with_folder,
        ),
    );
    Ok(report)
}

// ------------------------------------------------------------ exact copies

/// The SHA-256 of a file's contents, read a piece at a time so that a very
/// large scan is never held in memory whole.
pub fn hash_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().to_vec())
}

/// One file in the collection with exactly the same contents as the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactMatch {
    pub path: PathBuf,
    /// The scan that was dropped is itself this file, not a second copy of it.
    pub is_the_same_file: bool,
}

#[derive(Debug, Default, Clone)]
pub struct ExactResult {
    /// Pictures in the list that was searched.
    pub list_files: i64,
    /// How many of them are the same size as the scan, and so could be copies.
    pub same_size: u64,
    pub matches: Vec<ExactMatch>,
    /// Candidates that could not be read, so could not be ruled in or out.
    pub unreadable: u64,
    /// Candidates the list names that are no longer where it says.
    pub missing: u64,
}

pub(crate) const NOT_MADE: &str = "The photo list has not been made yet. Press \"Update photo list\" first, then try again.";

/// Looks for a file in the collection with exactly the same contents as
/// `scan`.
///
/// Only files the same size as the scan are even considered: two files with
/// different contents cannot have the same size, so reading the rest would
/// only cost time -- tens of gigabytes of it. A candidate's fingerprint is
/// worked out the first time it is needed and kept, and is trusted afterwards
/// only while the file still has the size and date the list recorded for it.
pub fn find_exact(roots: &[PhotoArchiveRoot], scan: &Path) -> Result<ExactResult, String> {
    find_exact_in(&db_path(), roots, scan)
}

fn find_exact_in(db: &Path, roots: &[PhotoArchiveRoot], scan: &Path) -> Result<ExactResult, String> {
    let meta = fs::metadata(scan).map_err(|_| "That file could not be opened.".to_string())?;
    if !meta.is_file() {
        return Err("That is not a file. Drop a single picture.".into());
    }
    if !db.exists() {
        return Err(NOT_MADE.into());
    }
    let conn = open_for_writing(db).map_err(|e| format!("could not open the photo list: {e}"))?;
    let list_files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if list_files == 0 {
        return Err(NOT_MADE.into());
    }

    let size = meta.len() as i64;
    let scan_hash = hash_file(scan).map_err(|e| format!("That file could not be read: {e}"))?;
    let scan_identity = fs::canonicalize(scan).ok();
    let current: std::collections::HashSet<String> =
        roots.iter().map(|r| r.path.to_string_lossy().to_string()).collect();

    type Row = (i64, String, String, i64, Option<Vec<u8>>);
    let candidates: Vec<Row> = conn
        .prepare("SELECT id, root, rel, mtime, sha256 FROM files WHERE size = ?1")
        .and_then(|mut s| {
            s.query_map(params![size], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect()
        })
        .map_err(|e| e.to_string())?;

    let mut result = ExactResult { list_files, ..ExactResult::default() };
    for (id, root, rel, recorded_mtime, stored_hash) in candidates {
        // A folder taken out of the settings is dropped from the list at the
        // next update; until then it is not part of the collection.
        if !current.contains(&root) {
            continue;
        }
        result.same_size += 1;
        let path = Path::new(&root).join(&rel);
        let Ok(now) = fs::metadata(&path) else {
            result.missing += 1;
            continue;
        };
        let (now_size, now_mtime) = (now.len() as i64, mtime_ns(&now));
        let mut hash = stored_hash;
        if now_size != size || now_mtime != recorded_mtime {
            // Changed since the list was made, so what it knows is out of date.
            conn.execute(
                "UPDATE files SET size = ?1, mtime = ?2, sha256 = NULL WHERE id = ?3",
                params![now_size, now_mtime, id],
            )
            .map_err(|e| e.to_string())?;
            conn.execute("DELETE FROM fingerprints WHERE file_id = ?1", params![id]).map_err(|e| e.to_string())?;
            if now_size != size {
                continue;
            }
            hash = None;
        }
        let hash = match hash {
            Some(h) => h,
            None => match hash_file(&path) {
                Ok(h) => {
                    conn.execute("UPDATE files SET sha256 = ?1 WHERE id = ?2", params![h, id])
                        .map_err(|e| e.to_string())?;
                    h
                }
                Err(_) => {
                    result.unreadable += 1;
                    continue;
                }
            },
        };
        if hash == scan_hash {
            let is_the_same_file = scan_identity.is_some() && fs::canonicalize(&path).ok() == scan_identity;
            result.matches.push(ExactMatch { path, is_the_same_file });
        }
    }
    result.matches.sort_by(|a, b| a.path.cmp(&b.path));
    crate::diagnostics::info(
        "photo-check",
        format!(
            "exact-copy check: {} candidates of {} listed, {} identical",
            result.same_size,
            list_files,
            result.matches.len()
        ),
    );
    Ok(result)
}

/// The scan most recently checked, so the window can show it back without
/// ever being handed a path to read from.
static DROPPED: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn set_dropped(path: PathBuf) {
    *DROPPED.lock().unwrap() = Some(path);
}

pub fn dropped() -> Option<PathBuf> {
    DROPPED.lock().unwrap().clone()
}

/// Throws the list away. It is only a copy of what the folders say, so the
/// next update simply makes it again.
pub fn clear() -> Result<(), String> {
    let path = db_path();
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("could not clear the photo list: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch folder for one test, removed when it goes out of scope. The
    /// tests use made-up files only: nothing from a real collection.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Scratch {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "hs-photo-check-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn folder(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::create_dir_all(&p).unwrap();
            p
        }
        fn db(&self) -> PathBuf {
            self.0.join("list.db")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                // A test may have made a folder unreadable; undo that so it can go.
                use std::os::unix::fs::PermissionsExt;
                if let Ok(entries) = fs::read_dir(&self.0) {
                    for e in entries.flatten() {
                        let _ = fs::set_permissions(e.path(), fs::Permissions::from_mode(0o755));
                        if let Ok(inner) = fs::read_dir(e.path()) {
                            for i in inner.flatten() {
                                let _ = fs::set_permissions(i.path(), fs::Permissions::from_mode(0o755));
                            }
                        }
                    }
                }
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn root(label: &str, path: &Path) -> PhotoArchiveRoot {
        PhotoArchiveRoot { label: label.into(), path: path.to_path_buf() }
    }
    fn put(dir: &Path, name: &str, bytes: &[u8]) {
        if let Some(parent) = dir.join(name).parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(dir.join(name), bytes).unwrap();
    }
    fn scan(s: &Scratch, roots: &[PhotoArchiveRoot]) -> ScanReport {
        rescan_into(&s.db(), roots, &mut |_| {}).unwrap()
    }
    fn count(s: &Scratch) -> i64 {
        status_at(&s.db()).files
    }

    #[test]
    fn first_scan_lists_pictures_and_ignores_everything_else() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"1");
        put(&a, "two.TIF", b"22"); // capital letters in the extension still count
        put(&a, "sub/deeper/three.png", b"333");
        put(&a, "notes.txt", b"words");
        put(&a, "._one.jpg", b"mac companion");
        put(&a, "~$draft.jpg", b"office marker");
        let r = scan(&s, &[root("A", &a)]);
        assert_eq!(r.roots[0].found, 3);
        assert_eq!(r.roots[0].added, 3);
        assert_eq!(count(&s), 3);
        assert!(r.notes().is_empty());
    }

    #[test]
    fn a_second_scan_of_unchanged_folders_changes_nothing() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"1");
        put(&a, "two.jpg", b"22");
        scan(&s, &[root("A", &a)]);
        // Something already worked out about a file must survive an unchanged scan.
        let conn = Connection::open(s.db()).unwrap();
        conn.execute("UPDATE files SET sha256 = x'AB' WHERE rel = 'one.jpg'", []).unwrap();
        drop(conn);
        let r = scan(&s, &[root("A", &a)]);
        assert_eq!((r.roots[0].added, r.roots[0].changed, r.roots[0].removed), (0, 0, 0));
        let conn = Connection::open(s.db()).unwrap();
        let kept: Option<Vec<u8>> =
            conn.query_row("SELECT sha256 FROM files WHERE rel = 'one.jpg'", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, Some(vec![0xAB]));
    }

    #[test]
    fn a_changed_file_is_noticed_and_its_stored_contents_fingerprint_dropped() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"1");
        put(&a, "two.jpg", b"22");
        scan(&s, &[root("A", &a)]);
        Connection::open(s.db()).unwrap().execute("UPDATE files SET sha256 = x'AB'", []).unwrap();
        put(&a, "one.jpg", b"now longer");
        let r = scan(&s, &[root("A", &a)]);
        assert_eq!(r.roots[0].changed, 1);
        let conn = Connection::open(s.db()).unwrap();
        let hash: Option<Vec<u8>> =
            conn.query_row("SELECT sha256 FROM files WHERE rel = 'one.jpg'", [], |r| r.get(0)).unwrap();
        assert_eq!(hash, None, "the old fingerprint described the old contents");
        let other: Option<Vec<u8>> =
            conn.query_row("SELECT sha256 FROM files WHERE rel = 'two.jpg'", [], |r| r.get(0)).unwrap();
        assert_eq!(other, Some(vec![0xAB]), "an untouched file keeps its own");
    }

    #[test]
    fn a_file_with_the_same_size_but_a_new_time_counts_as_changed() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"1");
        put(&a, "two.jpg", b"2");
        scan(&s, &[root("A", &a)]);
        Connection::open(s.db()).unwrap().execute("UPDATE files SET mtime = 5 WHERE rel = 'one.jpg'", []).unwrap();
        let r = scan(&s, &[root("A", &a)]);
        assert_eq!(r.roots[0].changed, 1);
    }

    #[test]
    fn a_deleted_file_leaves_the_list() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"1");
        put(&a, "two.jpg", b"22");
        scan(&s, &[root("A", &a)]);
        fs::remove_file(a.join("one.jpg")).unwrap();
        let r = scan(&s, &[root("A", &a)]);
        assert_eq!(r.roots[0].removed, 1);
        assert_eq!(count(&s), 1);
    }

    #[test]
    fn a_folder_that_cannot_be_opened_keeps_its_entries() {
        let s = Scratch::new();
        let a = s.folder("a");
        let b = s.folder("b");
        put(&a, "one.jpg", b"1");
        put(&b, "two.jpg", b"22");
        scan(&s, &[root("A", &a), root("B", &b)]);
        fs::remove_dir_all(&b).unwrap(); // the drive went away
        let r = scan(&s, &[root("A", &a), root("B", &b)]);
        assert_eq!(count(&s), 2, "B's entry must survive");
        assert!(!r.roots[1].readable);
        assert_eq!(r.roots[1].kept_unconfirmed, 1);
        assert_eq!(r.notes().len(), 1);
    }

    #[test]
    fn a_folder_that_opens_but_is_suddenly_empty_keeps_its_entries() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"1");
        put(&a, "two.jpg", b"22");
        scan(&s, &[root("A", &a)]);
        fs::remove_file(a.join("one.jpg")).unwrap();
        fs::remove_file(a.join("two.jpg")).unwrap();
        let r = scan(&s, &[root("A", &a)]);
        assert_eq!(count(&s), 2);
        assert_eq!(r.roots[0].kept_unconfirmed, 2);
        assert_eq!(r.notes().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_subfolder_that_cannot_be_read_keeps_what_it_had_and_the_rest_is_still_updated() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "open/one.jpg", b"1");
        put(&a, "shut/two.jpg", b"22");
        scan(&s, &[root("A", &a)]);
        put(&a, "open/new.jpg", b"333");
        fs::set_permissions(a.join("shut"), fs::Permissions::from_mode(0o000)).unwrap();
        // Skip where permissions are not enforced (running as the super-user).
        if fs::read_dir(a.join("shut")).is_ok() {
            return;
        }
        let r = scan(&s, &[root("A", &a)]);
        fs::set_permissions(a.join("shut"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(r.roots[0].added, 1, "what could be read is still brought up to date");
        assert_eq!(r.roots[0].removed, 0);
        assert_eq!(r.roots[0].kept_unconfirmed, 1);
        assert_eq!(count(&s), 3);
        assert_eq!(r.notes().len(), 1);
    }

    #[test]
    fn a_folder_taken_out_of_the_settings_is_dropped_from_the_list() {
        let s = Scratch::new();
        let a = s.folder("a");
        let b = s.folder("b");
        put(&a, "one.jpg", b"1");
        put(&b, "two.jpg", b"22");
        scan(&s, &[root("A", &a), root("B", &b)]);
        let r = scan(&s, &[root("A", &a)]);
        assert_eq!(r.removed_with_folder, 1);
        assert_eq!(count(&s), 1);
    }

    #[test]
    fn the_same_name_in_two_folders_is_two_entries() {
        let s = Scratch::new();
        let a = s.folder("a");
        let b = s.folder("b");
        put(&a, "same.jpg", b"1");
        put(&b, "same.jpg", b"1");
        scan(&s, &[root("A", &a), root("B", &b)]);
        assert_eq!(count(&s), 2);
    }

    #[test]
    fn status_before_any_scan_is_empty_and_creates_nothing() {
        let s = Scratch::new();
        let st = status_at(&s.db());
        assert_eq!(st.files, 0);
        assert!(st.last_scanned.is_none());
        assert!(!s.db().exists());
    }

    #[test]
    fn status_reports_when_the_list_was_last_brought_up_to_date() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"1");
        scan(&s, &[root("A", &a)]);
        assert!(status_at(&s.db()).last_scanned.unwrap() > 1.0e9);
    }

    #[test]
    fn an_old_layout_is_replaced_not_misread() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"1");
        scan(&s, &[root("A", &a)]);
        Connection::open(s.db()).unwrap().execute("UPDATE meta SET value = '0' WHERE key = 'schema'", []).unwrap();
        let r = scan(&s, &[root("A", &a)]);
        assert_eq!(r.roots[0].added, 1, "filled again from the folder");
        assert_eq!(count(&s), 1);
    }

    #[test]
    fn progress_counts_the_pictures_looked_at() {
        let s = Scratch::new();
        let a = s.folder("a");
        let b = s.folder("b");
        put(&a, "one.jpg", b"1");
        put(&b, "two.jpg", b"2");
        put(&b, "three.jpg", b"3");
        let mut seen = Vec::new();
        rescan_into(&s.db(), &[root("A", &a), root("B", &b)], &mut |n| seen.push(n)).unwrap();
        assert_eq!(seen, vec![1, 3]);
    }

    // ---------------------------------------------------------- exact copies

    fn check(s: &Scratch, roots: &[PhotoArchiveRoot], scan: &Path) -> Result<ExactResult, String> {
        find_exact_in(&s.db(), roots, scan)
    }
    fn stored_hashes(s: &Scratch) -> Vec<(String, bool)> {
        let conn = Connection::open(s.db()).unwrap();
        let mut q = conn.prepare("SELECT rel, sha256 IS NOT NULL FROM files ORDER BY rel").unwrap();
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn the_hash_is_the_standard_one() {
        let s = Scratch::new();
        put(&s.0, "abc.bin", b"abc");
        let h = hash_file(&s.0.join("abc.bin")).unwrap();
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn an_identical_copy_is_found_whatever_it_is_called_and_wherever_it_is() {
        let s = Scratch::new();
        let a = s.folder("a");
        let b = s.folder("b");
        put(&a, "first/original.jpg", b"the same bytes");
        put(&b, "elsewhere/renamed copy.jpg", b"the same bytes");
        put(&a, "other.jpg", b"different bytes!");
        let roots = [root("A", &a), root("B", &b)];
        scan(&s, &roots);
        let scan_file = s.0.join("new scan.jpg");
        fs::write(&scan_file, b"the same bytes").unwrap();
        let r = check(&s, &roots, &scan_file).unwrap();
        assert_eq!(r.matches.len(), 2);
        assert_eq!(r.matches[0].path, a.join("first").join("original.jpg"));
        assert_eq!(r.matches[1].path, b.join("elsewhere").join("renamed copy.jpg"));
        assert!(r.matches.iter().all(|m| !m.is_the_same_file));
    }

    #[test]
    fn a_file_of_the_same_size_with_different_contents_is_not_a_match() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"aaaa");
        let roots = [root("A", &a)];
        scan(&s, &roots);
        let scan_file = s.0.join("new.jpg");
        fs::write(&scan_file, b"aaab").unwrap();
        let r = check(&s, &roots, &scan_file).unwrap();
        assert_eq!(r.same_size, 1, "it had to be read to be ruled out");
        assert!(r.matches.is_empty());
    }

    #[test]
    fn only_files_of_the_same_size_are_ever_read() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "same size.jpg", b"1234");
        put(&a, "shorter.jpg", b"123");
        put(&a, "longer.jpg", b"12345");
        let roots = [root("A", &a)];
        scan(&s, &roots);
        let scan_file = s.0.join("new.jpg");
        fs::write(&scan_file, b"1234").unwrap();
        let r = check(&s, &roots, &scan_file).unwrap();
        assert_eq!((r.same_size, r.matches.len()), (1, 1));
        assert_eq!(
            stored_hashes(&s),
            vec![("longer.jpg".into(), false), ("same size.jpg".into(), true), ("shorter.jpg".into(), false)],
            "no other file was opened"
        );
    }

    #[test]
    fn a_stored_fingerprint_is_used_again_and_not_worked_out_twice() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"abcd");
        let roots = [root("A", &a)];
        scan(&s, &roots);
        let scan_file = s.0.join("new.jpg");
        fs::write(&scan_file, b"abcd").unwrap();
        check(&s, &roots, &scan_file).unwrap();
        // Plant a fingerprint that is wrong for the contents. If the next check
        // reads the file again it will find the match anyway; if it uses what
        // is stored it will not -- which is how to tell which it did.
        Connection::open(s.db()).unwrap().execute("UPDATE files SET sha256 = x'00'", []).unwrap();
        let r = check(&s, &roots, &scan_file).unwrap();
        assert!(r.matches.is_empty(), "the stored fingerprint was trusted, as an unchanged file's should be");
    }

    #[test]
    fn a_file_changed_since_the_list_was_made_is_not_judged_by_its_old_fingerprint() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"aaaa");
        let roots = [root("A", &a)];
        scan(&s, &roots);
        let old = s.0.join("old.jpg");
        let new = s.0.join("new.jpg");
        fs::write(&old, b"aaaa").unwrap();
        fs::write(&new, b"bbbb").unwrap();
        assert_eq!(check(&s, &roots, &old).unwrap().matches.len(), 1);
        // Same size, new contents, and a new date: the list has not heard yet.
        put(&a, "one.jpg", b"bbbb");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        fs::File::options().write(true).open(a.join("one.jpg")).unwrap().set_modified(later).unwrap();
        assert!(check(&s, &roots, &old).unwrap().matches.is_empty(), "no longer a copy of the old contents");
        assert_eq!(check(&s, &roots, &new).unwrap().matches.len(), 1, "and is now a copy of the new");
    }

    #[test]
    fn a_listed_file_that_has_gone_is_counted_not_fatal() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"abcd");
        put(&a, "two.jpg", b"abcd");
        let roots = [root("A", &a)];
        scan(&s, &roots);
        fs::remove_file(a.join("one.jpg")).unwrap();
        let scan_file = s.0.join("new.jpg");
        fs::write(&scan_file, b"abcd").unwrap();
        let r = check(&s, &roots, &scan_file).unwrap();
        assert_eq!((r.missing, r.matches.len()), (1, 1));
    }

    #[test]
    fn dropping_a_file_that_is_in_the_collection_says_so() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"abcd");
        put(&a, "copy.jpg", b"abcd");
        let roots = [root("A", &a)];
        scan(&s, &roots);
        let r = check(&s, &roots, &a.join("one.jpg")).unwrap();
        assert_eq!(r.matches.len(), 2);
        let itself: Vec<_> = r.matches.iter().filter(|m| m.is_the_same_file).map(|m| m.path.clone()).collect();
        assert_eq!(itself, vec![a.join("one.jpg")], "only that one is the file itself; the other is a real copy");
    }

    #[test]
    fn a_folder_left_out_of_the_settings_does_not_count_as_the_collection() {
        let s = Scratch::new();
        let a = s.folder("a");
        let b = s.folder("b");
        put(&a, "one.jpg", b"abcd");
        put(&b, "two.jpg", b"abcd");
        scan(&s, &[root("A", &a), root("B", &b)]);
        let scan_file = s.0.join("new.jpg");
        fs::write(&scan_file, b"abcd").unwrap();
        let r = check(&s, &[root("A", &a)], &scan_file).unwrap();
        assert_eq!(r.matches.len(), 1);
    }

    #[test]
    fn checking_before_the_list_exists_says_what_to_do() {
        let s = Scratch::new();
        let scan_file = s.0.join("new.jpg");
        fs::write(&scan_file, b"abcd").unwrap();
        let e = check(&s, &[], &scan_file).unwrap_err();
        assert!(e.contains("Update photo list"), "{e}");
        assert!(!s.db().exists(), "and does not create the list as a side effect");
    }

    #[test]
    fn an_empty_list_is_treated_as_not_made() {
        let s = Scratch::new();
        let a = s.folder("a");
        scan(&s, &[root("A", &a)]);
        let scan_file = s.0.join("new.jpg");
        fs::write(&scan_file, b"abcd").unwrap();
        assert!(check(&s, &[root("A", &a)], &scan_file).unwrap_err().contains("Update photo list"));
    }

    #[test]
    fn a_folder_or_a_missing_file_is_refused_politely() {
        let s = Scratch::new();
        let a = s.folder("a");
        put(&a, "one.jpg", b"abcd");
        scan(&s, &[root("A", &a)]);
        assert!(check(&s, &[root("A", &a)], &a).unwrap_err().contains("not a file"));
        assert!(check(&s, &[root("A", &a)], &s.0.join("nothing.jpg")).unwrap_err().contains("could not be opened"));
    }
}
