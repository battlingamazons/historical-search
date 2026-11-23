//! Describing every picture in the collection once, and searching those
//! descriptions.
//!
//! Each picture in the photo list gets 512 numbers from `clip.rs`, kept in the
//! photo list's database beside the look-alike fingerprints. Searching is then
//! only arithmetic: a typed phrase (or another picture) becomes 512 numbers
//! too, and the pictures whose numbers point the same way come first.
//!
//! A description is kept with the size and date of the file it was made from,
//! so one made from a file that has since changed is simply not used, and is
//! made again -- nothing else has to remember to throw it away. A picture that
//! could not be read is kept as an empty description, so it is not tried
//! again every time.

use crate::clip;
use crate::photo_check::open_for_writing;
use crate::thumbnails;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

/// Bumped when how a picture is described changes, so that old descriptions
/// are not compared with new ones.
const VERSION: i64 = 1;

/// Pictures are shrunk to this (longest side) before the model sees them: it
/// only ever looks at 224 pixels, and decoding a huge scan is the slow part.
const SHRINK_TO: u32 = 1024;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BuildReport {
    pub considered: u64,
    pub made: u64,
    pub unreadable: u64,
    pub missing: u64,
}

/// A picture, and how well it fits what was asked for (about -1 to 1; only the
/// order means much).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub score: f32,
}

static GENERATION: AtomicU64 = AtomicU64::new(1);

fn forget_cached() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn prepare(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clip_vectors (
             file_id INTEGER PRIMARY KEY,
             size    INTEGER NOT NULL,   -- of the file it was made from
             mtime   INTEGER NOT NULL,
             vec     BLOB NOT NULL       -- 512 little-endian f32; empty if the picture could not be read
         );",
    )
    .map_err(|e| e.to_string())?;
    let have: Option<i64> = conn
        .query_row("SELECT value FROM meta WHERE key = 'clip_version'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok());
    if have != Some(VERSION) {
        conn.execute("DELETE FROM clip_vectors", []).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('clip_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![VERSION.to_string()],
        )
        .map_err(|e| e.to_string())?;
        forget_cached();
    }
    Ok(())
}

/// The rows of `files` with no usable description: none at all, or one made
/// from the file as it was before it changed.
const NEEDS_MAKING: &str = "c.file_id IS NULL";
const JOIN: &str = "LEFT JOIN clip_vectors c ON c.file_id = f.id AND c.size = f.size AND c.mtime = f.mtime";

/// Pictures in the list with no description yet.
pub fn pending(db: &Path) -> u64 {
    if !db.exists() {
        return 0;
    }
    let counted = open_for_writing(db).ok().and_then(|conn| {
        prepare(&conn).ok()?;
        conn.query_row(&format!("SELECT COUNT(*) FROM files f {JOIN} WHERE {NEEDS_MAKING}"), [], |r| r.get::<_, i64>(0)).ok()
    });
    // Not countable: say there is something to do, so that doing it is tried
    // and reports what is wrong.
    counted.map(|n| n as u64).unwrap_or(1)
}

/// How many pictures are described at once: most of the processors, leaving one
/// for the window.
fn workers() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    cores.saturating_sub(1).clamp(1, 6)
}

/// One picture described: its numbers, or empty if it could not be read; `None`
/// when the file is not where the list says.
fn describe_file(path: &Path, cache_dir: &Path) -> Option<Vec<u8>> {
    if !path.exists() {
        return None;
    }
    let described = thumbnails::decode_for_matching(path, cache_dir).and_then(|img| {
        let img = if img.width().max(img.height()) > SHRINK_TO {
            img.resize(SHRINK_TO, SHRINK_TO, image::imageops::FilterType::Triangle)
        } else {
            img
        };
        clip::embed_image(&img.to_rgb8())
    });
    Some(described.map(|v| clip::to_bytes(&v)).unwrap_or_default())
}

/// Describes every picture in the list that has no description, and tells
/// `progress` how many of them are done and how many there are. Can be stopped
/// at any time: what is finished is kept, and the next run carries on.
pub fn build(db: &Path, cache_dir: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<BuildReport, String> {
    if !clip::installed() {
        return Err("the picture-search files are not downloaded yet".into());
    }
    let mut conn = open_for_writing(db).map_err(|e| format!("could not open the photo list: {e}"))?;
    prepare(&conn)?;
    // Descriptions of files that have left the list.
    conn.execute("DELETE FROM clip_vectors WHERE file_id NOT IN (SELECT id FROM files)", [])
        .map_err(|e| e.to_string())?;
    let todo: Vec<(i64, i64, i64, PathBuf)> = conn
        .prepare(&format!(
            "SELECT f.id, f.size, f.mtime, f.root, f.rel FROM files f {JOIN} WHERE {NEEDS_MAKING} ORDER BY f.id"
        ))
        .and_then(|mut s| {
            s.query_map([], |r| {
                let (root, rel): (String, String) = (r.get(3)?, r.get(4)?);
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, Path::new(&root).join(rel)))
            })?
            .collect()
        })
        .map_err(|e| e.to_string())?;

    let total = todo.len() as u64;
    let mut report = BuildReport { considered: total, ..BuildReport::default() };
    progress(0, total);
    if todo.is_empty() {
        return Ok(report);
    }
    // Held in memory as it goes: only so many decoded pictures at once.
    let budget = thumbnails::MemoryBudget::new(thumbnails::PREVIEW_MEMORY_BUDGET);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, Option<Vec<u8>>)>();
    let mut done = 0u64;
    let mut rows: Vec<(i64, i64, i64, Vec<u8>)> = Vec::new();

    let flush = |conn: &mut Connection, rows: &mut Vec<(i64, i64, i64, Vec<u8>)>| -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        let t = conn.transaction().map_err(|e| e.to_string())?;
        for (id, size, mtime, vec) in rows.drain(..) {
            t.execute(
                "INSERT OR REPLACE INTO clip_vectors (file_id, size, mtime, vec) VALUES (?1, ?2, ?3, ?4)",
                params![id, size, mtime, vec],
            )
            .map_err(|e| e.to_string())?;
        }
        t.commit().map_err(|e| e.to_string())
    };

    let outcome: Result<(), String> = std::thread::scope(|scope| {
        for _ in 0..workers() {
            let tx = tx.clone();
            let (todo, next, budget) = (&todo, &next, &budget);
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((_, _, _, path)) = todo.get(i) else { break };
                let source = thumbnails::cached_preview(path, cache_dir).unwrap_or_else(|| path.clone());
                let reserved = budget.acquire(thumbnails::decoded_cost(&source));
                let message = describe_file(path, cache_dir);
                budget.release(reserved);
                if tx.send((i, message)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok((i, message)) => {
                    done += 1;
                    let (id, size, mtime, _) = &todo[i];
                    match message {
                        None => report.missing += 1,
                        Some(vec) => {
                            if vec.is_empty() {
                                report.unreadable += 1;
                            } else {
                                report.made += 1;
                            }
                            rows.push((*id, *size, *mtime, vec));
                        }
                    }
                    if rows.len() >= 100 {
                        flush(&mut conn, &mut rows)?;
                    }
                    if done.is_multiple_of(25) {
                        progress(done, total);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => progress(done, total),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    });
    flush(&mut conn, &mut rows)?;
    forget_cached();
    outcome?;
    progress(total, total);
    crate::diagnostics::info(
        "picture-search",
        format!(
            "described {} pictures, {} unreadable, {} not found, of {} needed",
            report.made, report.unreadable, report.missing, report.considered
        ),
    );
    Ok(report)
}

// ------------------------------------------------------------ searching

struct Held {
    /// Which photo list these were read from, and at which change.
    db: PathBuf,
    generation: u64,
    ids: Vec<i64>,
    /// `clip::EMB_LEN` numbers for each of `ids`, one after another.
    data: Vec<f32>,
}

fn held() -> &'static Mutex<Option<Held>> {
    static HELD: Mutex<Option<Held>> = Mutex::new(None);
    &HELD
}

/// The descriptions, kept in memory between searches and read again only when
/// they have changed.
fn load<'a>(conn: &Connection, db: &Path) -> Result<std::sync::MutexGuard<'a, Option<Held>>, String> {
    let generation = GENERATION.load(Ordering::SeqCst);
    let mut slot = held().lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|h| h.generation == generation && h.db == db) {
        return Ok(slot);
    }
    let mut ids = Vec::new();
    let mut data = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT c.file_id, c.vec FROM clip_vectors c JOIN files f
             ON f.id = c.file_id AND f.size = c.size AND f.mtime = c.mtime
             WHERE length(c.vec) = ?1 ORDER BY c.file_id",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![(clip::EMB_LEN * 4) as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (id, bytes) = row.map_err(|e| e.to_string())?;
        if let Some(v) = clip::from_bytes(&bytes) {
            ids.push(id);
            data.extend(v);
        }
    }
    *slot = Some(Held { db: db.to_path_buf(), generation, ids, data });
    Ok(slot)
}

/// The `limit` best-fitting of `ids` for `query`, best first.
fn rank(ids: &[i64], data: &[f32], query: &[f32], limit: usize) -> Vec<(i64, f32)> {
    let mut scored: Vec<(i64, f32)> = ids
        .iter()
        .zip(data.as_chunks::<{ clip::EMB_LEN }>().0.iter())
        .map(|(id, v)| (*id, clip::similarity(v, query)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    scored
}

fn hits_for(db: &Path, query: &[f32], limit: usize) -> Result<Vec<Hit>, String> {
    if !db.exists() {
        return Err("The photo list has not been made yet. Refresh the search index first.".into());
    }
    let conn = open_for_writing(db).map_err(|e| format!("could not open the photo list: {e}"))?;
    prepare(&conn)?;
    let slot = load(&conn, db)?;
    let h = slot.as_ref().ok_or("the picture descriptions could not be loaded")?;
    let mut out = Vec::new();
    for (id, score) in rank(&h.ids, &h.data, query, limit) {
        let found: Option<(String, String)> = conn
            .query_row("SELECT root, rel FROM files WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok();
        if let Some((root, rel)) = found {
            out.push(Hit { path: Path::new(&root).join(rel), score });
        }
    }
    Ok(out)
}

/// How many pictures have a description to be searched.
pub fn described(db: &Path) -> u64 {
    open_for_writing(db)
        .ok()
        .and_then(|conn| {
            prepare(&conn).ok()?;
            conn.query_row(
                "SELECT COUNT(*) FROM clip_vectors c JOIN files f
                 ON f.id = c.file_id AND f.size = c.size AND f.mtime = c.mtime WHERE length(c.vec) > 0",
                [],
                |r| r.get::<_, i64>(0),
            )
            .ok()
        })
        .unwrap_or(0) as u64
}

const NOT_READY: &str = "Picture search is not set up yet. Turn it on in the settings and download its files.";

/// The pictures that best fit what the words describe.
pub fn search_text(db: &Path, phrase: &str, limit: usize) -> Result<Vec<Hit>, String> {
    if phrase.trim().is_empty() {
        return Ok(Vec::new());
    }
    let query = clip::embed_text(phrase).ok_or(NOT_READY)?;
    hits_for(db, &query, limit)
}

/// The pictures that look most like the one at `scan`, which is not itself
/// listed if it is one of them.
pub fn search_picture(db: &Path, scan: &Path, cache_dir: &Path, part: Option<(f32, f32, f32, f32)>, limit: usize) -> Result<Vec<Hit>, String> {
    let meta = std::fs::metadata(scan).map_err(|_| "That file could not be opened.".to_string())?;
    if !meta.is_file() {
        return Err("That is not a file. Drop a single picture.".into());
    }
    let image = thumbnails::decode_for_matching(scan, cache_dir).ok_or("That file could not be read as a picture.")?;
    // Only the outlined part, when the person outlined one (fractions of the picture).
    let image = match part {
        Some((x0, y0, x1, y1)) if x1 > x0 && y1 > y0 => {
            let (w, h) = (image.width() as f32, image.height() as f32);
            let left = (x0.clamp(0.0, 1.0) * w) as u32;
            let top = (y0.clamp(0.0, 1.0) * h) as u32;
            let width = (((x1.clamp(0.0, 1.0) - x0.clamp(0.0, 1.0)) * w) as u32).max(1);
            let height = (((y1.clamp(0.0, 1.0) - y0.clamp(0.0, 1.0)) * h) as u32).max(1);
            image.crop_imm(left, top, width.min(image.width().saturating_sub(left).max(1)), height.min(image.height().saturating_sub(top).max(1)))
        }
        _ => image,
    };
    let image = if image.width().max(image.height()) > SHRINK_TO {
        image.resize(SHRINK_TO, SHRINK_TO, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let query = clip::embed_image(&image.to_rgb8()).ok_or(NOT_READY)?;
    // One more than asked for, in case the picture is in the collection.
    let mut hits = hits_for(db, &query, limit + 1)?;
    let same_file = |p: &Path| std::fs::canonicalize(p).ok().zip(std::fs::canonicalize(scan).ok()).is_some_and(|(a, b)| a == b);
    hits.retain(|h| !same_file(&h.path));
    hits.truncate(limit);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(seed: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..clip::EMB_LEN).map(|i| (((i * 31 + seed * 17) % 97) as f32) - 48.0).collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= n);
        v
    }

    #[test]
    fn the_closest_description_comes_first() {
        let ids = vec![10, 20, 30];
        let data: Vec<f32> = [unit(1), unit(2), unit(3)].concat();
        let ranked = rank(&ids, &data, &unit(2), 3);
        assert_eq!(ranked[0].0, 20);
        assert!((ranked[0].1 - 1.0).abs() < 1e-4);
        assert!(ranked[1].1 <= ranked[0].1 && ranked[2].1 <= ranked[1].1);
        assert_eq!(rank(&ids, &data, &unit(2), 2).len(), 2);
    }

    fn database(files: &[(&str, i64, i64)]) -> (PathBuf, Connection) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let db = std::env::temp_dir().join(format!("hs-clip-index-{}-{}.db", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_file(&db);
        let conn = open_for_writing(&db).unwrap();
        prepare(&conn).unwrap();
        for (i, (rel, size, mtime)) in files.iter().enumerate() {
            conn.execute(
                "INSERT INTO files (id, root, rel, size, mtime) VALUES (?1, '/pictures', ?2, ?3, ?4)",
                params![i as i64 + 1, rel, size, mtime],
            )
            .unwrap();
        }
        (db, conn)
    }

    fn store(conn: &Connection, id: i64, size: i64, mtime: i64, v: &[f32]) {
        conn.execute(
            "INSERT OR REPLACE INTO clip_vectors (file_id, size, mtime, vec) VALUES (?1, ?2, ?3, ?4)",
            params![id, size, mtime, clip::to_bytes(v)],
        )
        .unwrap();
    }

    #[test]
    fn only_pictures_without_a_current_description_are_pending() {
        let (db, conn) = database(&[("a.jpg", 100, 1), ("b.jpg", 200, 2), ("c.jpg", 300, 3)]);
        assert_eq!(pending(&db), 3);
        store(&conn, 1, 100, 1, &unit(1));
        store(&conn, 2, 999, 2, &unit(2)); // made from b.jpg before it changed
        assert_eq!(pending(&db), 2, "a changed file must be described again");
        store(&conn, 3, 300, 3, &[]); // tried, unreadable
        assert_eq!(pending(&db), 1, "an unreadable picture is not tried every time");
        drop(conn);
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn a_description_made_from_a_changed_file_is_not_searched() {
        let (db, conn) = database(&[("a.jpg", 100, 1), ("b.jpg", 200, 2)]);
        store(&conn, 1, 100, 1, &unit(1));
        store(&conn, 2, 5, 2, &unit(2)); // stale
        let hits = hits_for(&db, &unit(2), 10).unwrap();
        let names: Vec<_> = hits.iter().map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a.jpg"]);
        assert_eq!(described(&db), 1);
        drop(conn);
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn a_new_description_is_searchable_straight_away() {
        let (db, conn) = database(&[("a.jpg", 100, 1), ("b.jpg", 200, 2)]);
        store(&conn, 1, 100, 1, &unit(1));
        assert_eq!(hits_for(&db, &unit(1), 10).unwrap().len(), 1);
        store(&conn, 2, 200, 2, &unit(2));
        forget_cached(); // what finishing a build does
        assert_eq!(hits_for(&db, &unit(1), 10).unwrap().len(), 2);
        drop(conn);
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn searching_before_the_list_exists_says_what_to_do() {
        let err = hits_for(Path::new("/nonexistent/photo_check.db"), &unit(1), 5).unwrap_err();
        assert!(err.contains("Refresh the search index"), "{err}");
    }

    #[test]
    fn building_without_the_files_says_so_and_changes_nothing() {
        let _turn = clip::tests_support::one_at_a_time();
        let (db, conn) = database(&[("a.jpg", 100, 1)]);
        let dir = std::env::temp_dir().join(format!("hs-clip-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        clip::set_model_dir(&dir);
        let err = build(&db, &dir, &mut |_, _| {}).unwrap_err();
        assert!(err.contains("not downloaded"), "{err}");
        assert_eq!(pending(&db), 1);
        drop(conn);
        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}
