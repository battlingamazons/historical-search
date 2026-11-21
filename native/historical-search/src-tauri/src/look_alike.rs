//! Finding look-alikes of a scan among the collection's pictures.
//!
//! The exact-copy check finds a file with the same contents. This finds the
//! same *photograph* in a different file: scanned again, saved again, resized,
//! turned, framed differently, faded. Each picture in the photo list gets a
//! fingerprint (see `fingerprint.rs`), made once and kept until the file
//! changes; a new scan is then compared with all of them, which takes under a
//! second for the whole collection.
//!
//! Making the fingerprints is the slow part -- every picture has to be opened
//! once -- so it is done as a job with progress, using the previews the app has
//! already made wherever there are some, and as few processors as the preview
//! job uses. A picture that cannot be read is remembered as such, so it is not
//! tried again at every update; a picture that merely is not there right now
//! (an unplugged drive) is not, and is tried again when it is.
//!
//! Nothing in the picture folders is changed, and file names never reach the
//! diagnostics log.

use crate::crop_match::{self, CROP_STORED_LEN};
use crate::fingerprint::{self, STORED_LEN};
use crate::photo_check::{open_for_writing, NOT_MADE};
use crate::thumbnails;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

/// Bumped when the fingerprint itself changes, so that old ones are not
/// compared with new ones.
const FINGERPRINT_VERSION: i64 = 1;
/// The same for what is kept for finding cropped copies.
const CROP_VERSION: i64 = 1;

/// Rows that still need making: no fingerprint at all, or one made before
/// cropped copies were looked for. An unreadable picture is not made again.
const NEEDS_MAKING: &str = "p.file_id IS NULL OR (p.crop_desc IS NULL AND length(p.data) > 0)";

/// The score at and above which two pictures are very probably the same
/// photograph, and the one at and above which they are possibly the same.
/// Settled by the program this was ported from, on the museum's own scans.
pub const LIKELY: f32 = 0.80;
pub const POSSIBLE: f32 = 0.66;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BuildReport {
    /// Pictures that needed a fingerprint.
    pub considered: u64,
    pub made: u64,
    /// Present but not readable as a picture.
    pub unreadable: u64,
    /// Not where the list says, so not tried.
    pub missing: u64,
}

/// What is learned about one picture: its fingerprint (empty if it could not be
/// read) and what is kept for finding cropped copies of it.
struct Made {
    data: Vec<u8>,
    crop: Option<crop_match::Stored>,
}

/// How many pictures in the list have no fingerprint yet.
pub fn pending(db: &Path) -> u64 {
    let Ok(conn) = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return 0;
    };
    conn.query_row(
        &format!("SELECT COUNT(*) FROM files f LEFT JOIN fingerprints p ON p.file_id = f.id WHERE {NEEDS_MAKING}"),
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n as u64)
    .unwrap_or(0)
}

/// Counts the changes to the stored fingerprints, so that the copy kept in
/// memory for searching knows when it is out of date.
static GENERATION: AtomicU64 = AtomicU64::new(1);

fn forget_cached() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Drops fingerprints made by a different version of the fingerprint.
fn check_version(conn: &Connection) -> Result<(), String> {
    let have: Option<i64> = conn
        .query_row("SELECT value FROM meta WHERE key = 'fingerprint_version'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok());
    if have != Some(FINGERPRINT_VERSION) {
        conn.execute("DELETE FROM fingerprints", []).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('fingerprint_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![FINGERPRINT_VERSION.to_string()],
        )
        .map_err(|e| e.to_string())?;
        forget_cached();
    }
    let have_crop: Option<i64> = conn
        .query_row("SELECT value FROM meta WHERE key = 'crop_version'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok());
    if have_crop != Some(CROP_VERSION) {
        conn.execute("UPDATE fingerprints SET crop_desc = NULL, crop_thumb = NULL", []).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('crop_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![CROP_VERSION.to_string()],
        )
        .map_err(|e| e.to_string())?;
        forget_cached();
    }
    Ok(())
}

/// Makes a fingerprint for every picture in the list that lacks one.
///
/// `progress` is told how many of the pictures that needed one are done, and
/// how many there are in all.
pub fn build(db: &Path, cache_dir: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<BuildReport, String> {
    let mut conn = open_for_writing(db).map_err(|e| format!("could not open the photo list: {e}"))?;
    check_version(&conn)?;
    let todo: Vec<(i64, PathBuf)> = conn
        .prepare(
            &format!(
                "SELECT f.id, f.root, f.rel FROM files f
                 LEFT JOIN fingerprints p ON p.file_id = f.id WHERE {NEEDS_MAKING} ORDER BY f.id"
            ),
        )
        .and_then(|mut s| {
            s.query_map([], |r| {
                let (id, root, rel): (i64, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
                Ok((id, Path::new(&root).join(rel)))
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

    let next = AtomicUsize::new(0);
    let budget = thumbnails::MemoryBudget::new(thumbnails::PREVIEW_MEMORY_BUDGET);
    // None: the picture is not there; Some with empty data: there, but not readable.
    let (tx, rx) = mpsc::channel::<(i64, Option<Made>)>();
    let mut done = 0u64;
    let mut pending_rows: Vec<(i64, Made)> = Vec::new();

    let flush = |conn: &mut Connection, rows: &mut Vec<(i64, Made)>| -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for (id, made) in rows.drain(..) {
            let (desc, thumb) = match &made.crop {
                Some(c) => (Some(&c.windows), Some(&c.picture)),
                None => (None, None),
            };
            tx.execute(
                "INSERT OR REPLACE INTO fingerprints (file_id, data, crop_desc, crop_thumb) VALUES (?1, ?2, ?3, ?4)",
                params![id, made.data, desc, thumb],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    };

    let outcome: Result<(), String> = std::thread::scope(|scope| {
        for _ in 0..thumbnails::preview_concurrency() {
            let tx = tx.clone();
            let (todo, next, budget) = (&todo, &next, &budget);
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((id, path)) = todo.get(i) else { break };
                let message = if !path.exists() {
                    None
                } else {
                    let source = thumbnails::cached_preview(path, cache_dir).unwrap_or_else(|| path.clone());
                    let reserved = budget.acquire(thumbnails::decoded_cost(&source));
                    let image = thumbnails::decode_for_matching(path, cache_dir);
                    let made = image.map(|img| {
                        let gray = fingerprint::gray_for_matching(&img);
                        Made { data: fingerprint::pack(&fingerprint::describe(&gray)), crop: Some(crop_match::describe(&gray)) }
                    });
                    budget.release(reserved);
                    Some(made.unwrap_or(Made { data: Vec::new(), crop: None }))
                };
                if tx.send((*id, message)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        // Written here, by the one thread that talks to the database, as the
        // workers finish; so the report stays single-threaded too.
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok((id, message)) => {
                    done += 1;
                    match message {
                        None => report.missing += 1,
                        Some(made) if made.data.is_empty() => {
                            report.unreadable += 1;
                            pending_rows.push((id, made));
                        }
                        Some(made) => {
                            report.made += 1;
                            pending_rows.push((id, made));
                        }
                    }
                    if pending_rows.len() >= 100 {
                        flush(&mut conn, &mut pending_rows)?;
                    }
                    if done.is_multiple_of(50) {
                        progress(done, total);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => progress(done, total),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    });
    flush(&mut conn, &mut pending_rows)?;
    forget_cached();
    outcome?;
    progress(total, total);
    crate::diagnostics::info(
        "photo-check",
        format!(
            "look-alike fingerprints: {} made, {} unreadable, {} not found, of {} needed",
            report.made, report.unreadable, report.missing, report.considered
        ),
    );
    Ok(report)
}

/// One picture in the collection that looks like the scan.
#[derive(Debug, Clone, PartialEq)]
pub struct Similar {
    pub path: PathBuf,
    /// 0 to 1, with 1 for the same picture.
    pub score: f32,
    /// Found as a cropped copy: the fingerprint alone did not see it.
    pub cropped: bool,
}

#[derive(Debug, Default, Clone)]
pub struct SimilarResult {
    /// The closest pictures, best first.
    pub closest: Vec<Similar>,
    /// Fingerprints compared with.
    pub compared: usize,
    /// Pictures in the list with no fingerprint yet, so not compared.
    pub pending: u64,
}

struct Collection {
    generation: u64,
    ids: Vec<i64>,
    data: Vec<u16>,
    /// The coarse windows for cropped-copy matching, `CROP_STORED_LEN` for each
    /// of `ids` (zeros for one that has none).
    crop: Vec<u16>,
}

fn collection() -> &'static Mutex<Option<Collection>> {
    static COLLECTION: Mutex<Option<Collection>> = Mutex::new(None);
    &COLLECTION
}

/// The stored fingerprints, held in memory between searches and read again
/// only when they have changed.
fn load(conn: &Connection) -> Result<(), String> {
    let generation = GENERATION.load(Ordering::SeqCst);
    let mut held = collection().lock().unwrap();
    if held.as_ref().is_some_and(|c| c.generation == generation) {
        return Ok(());
    }
    let mut ids = Vec::new();
    let mut data = Vec::new();
    let mut crop = Vec::new();
    let mut stmt = conn
        .prepare("SELECT file_id, data, crop_desc FROM fingerprints WHERE length(data) = ?1 ORDER BY file_id")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![(STORED_LEN * 2) as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Option<Vec<u8>>>(2)?))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (id, bytes, windows) = row.map_err(|e| e.to_string())?;
        ids.push(id);
        data.extend(bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)));
        match windows.filter(|w| w.len() == CROP_STORED_LEN * 2) {
            Some(w) => crop.extend(w.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c))),
            None => crop.extend(std::iter::repeat_n(0u16, CROP_STORED_LEN)),
        }
    }
    *held = Some(Collection { generation, ids, data, crop });
    Ok(())
}

/// The pictures in the collection that look most like the scan at `scan`.
pub fn find_similar(db: &Path, scan: &Path, cache_dir: &Path, limit: usize) -> Result<SimilarResult, String> {
    let meta = std::fs::metadata(scan).map_err(|_| "That file could not be opened.".to_string())?;
    if !meta.is_file() {
        return Err("That is not a file. Drop a single picture.".into());
    }
    if !db.exists() {
        return Err(NOT_MADE.into());
    }
    let conn = open_for_writing(db).map_err(|e| format!("could not open the photo list: {e}"))?;
    check_version(&conn)?;
    let image = thumbnails::decode_for_matching(scan, cache_dir)
        .ok_or_else(|| "That file could not be read as a picture.".to_string())?;
    let gray = fingerprint::gray_for_matching(&image);
    drop(image);
    let query = fingerprint::query(&gray);
    let crop_query = crop_match::query(&gray);

    load(&conn)?;
    let held = collection().lock().unwrap();
    let held = held.as_ref().ok_or("the fingerprints could not be loaded")?;
    let mut scores = fingerprint::score_all(&query, &held.data);
    let cropped = crop_scores(&conn, held, &crop_query, &mut scores);

    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
    let mut closest = Vec::new();
    for &i in order.iter().take(limit) {
        let found: Option<(String, String)> = conn
            .query_row("SELECT root, rel FROM files WHERE id = ?1", params![held.ids[i]], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok();
        if let Some((root, rel)) = found {
            closest.push(Similar { path: Path::new(&root).join(rel), score: scores[i], cropped: cropped[i] });
        }
    }
    Ok(SimilarResult { closest, compared: scores.len(), pending: pending(db) })
}

/// Looks again, closely, at the pictures that might be a differently cropped
/// copy of the scan, and raises their scores where one fits. Returns which
/// scores were raised by it.
///
/// This pass must never spoil a search: if the data it needs cannot be read,
/// the scores stay as the fingerprint alone gave them.
fn crop_scores(conn: &Connection, held: &Collection, query: &crop_match::Query, scores: &mut [f32]) -> Vec<bool> {
    let mut raised = vec![false; scores.len()];
    let coarse = crop_match::coarse_scores(query, &held.crop);
    let mut stmt = match conn.prepare("SELECT crop_thumb FROM fingerprints WHERE file_id = ?1") {
        Ok(s) => s,
        Err(_) => return raised,
    };
    let mut candidates = Vec::new();
    for i in crop_match::shortlist(&coarse, crop_match::CROP_K) {
        let picture: Option<Vec<u8>> = stmt.query_row(params![held.ids[i]], |r| r.get(0)).ok().flatten();
        if let Some(picture) = picture {
            candidates.push(crop_match::Candidate {
                index: i,
                windows: held.crop[i * CROP_STORED_LEN..(i + 1) * CROP_STORED_LEN].to_vec(),
                picture,
            });
        }
    }
    for (i, reported) in crop_match::close_look(query, &candidates) {
        if reported > scores[i] {
            // Only called a crop when the ordinary comparison clearly missed it.
            raised[i] = reported > scores[i] + 0.05;
            scores[i] = reported;
        }
    }
    raised
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::test_pictures::picture;
    use image::{GrayImage, Luma};
    use std::fs;
    use std::sync::atomic::AtomicU32;

    /// Fingerprints are kept in step through one shared in-memory copy, so the
    /// tests that search must not interleave with ones that change it.
    fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Scratch {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir()
                .join(format!("hs-look-alike-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
        fn db(&self) -> PathBuf {
            self.0.join("list.db")
        }
        fn photos(&self) -> PathBuf {
            let p = self.0.join("photos");
            fs::create_dir_all(&p).unwrap();
            p
        }
        fn previews(&self) -> PathBuf {
            let p = self.0.join("previews");
            fs::create_dir_all(&p).unwrap();
            p
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn save(dir: &Path, name: &str, img: &GrayImage) -> PathBuf {
        let p = dir.join(name);
        img.save(&p).unwrap();
        p
    }
    fn root(dir: &Path) -> crate::ingest::PhotoArchiveRoot {
        crate::ingest::PhotoArchiveRoot { label: "A".into(), path: dir.to_path_buf() }
    }
    fn list(s: &Scratch) {
        crate::photo_check::rescan_for_tests(&s.db(), &[root(&s.photos())]).unwrap();
    }
    fn build_all(s: &Scratch) -> BuildReport {
        build(&s.db(), &s.previews(), &mut |_, _| {}).unwrap()
    }
    fn name_of(p: &Path) -> String {
        p.file_name().unwrap().to_string_lossy().to_string()
    }

    #[test]
    fn a_resaved_resized_faded_copy_finds_its_original_and_an_unrelated_picture_does_not() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        let originals: Vec<GrayImage> = (1..=6).map(|seed| picture(seed, 320, 240)).collect();
        for (i, o) in originals.iter().enumerate() {
            save(&s.photos(), &format!("photo{i}.png"), o);
        }
        list(&s);
        let report = build_all(&s);
        assert_eq!((report.considered, report.made, report.unreadable), (6, 6, 0));

        // A smaller, fainter copy of photo 3, saved as a different kind of file.
        let small = image::imageops::resize(&originals[3], 150, 112, image::imageops::FilterType::Triangle);
        let faded = GrayImage::from_fn(small.width(), small.height(), |x, y| Luma([(small.get_pixel(x, y).0[0] as f32 * 0.6 + 60.0) as u8]));
        let scan = save(&s.0, "new scan.jpg", &faded);
        let found = find_similar(&s.db(), &scan, &s.previews(), 3).unwrap();
        assert_eq!(found.compared, 6);
        assert_eq!(name_of(&found.closest[0].path), "photo3.png");
        assert!(found.closest[0].score >= LIKELY, "{}", found.closest[0].score);
        assert!(found.closest[1].score < POSSIBLE, "the next closest is not a match: {}", found.closest[1].score);

        let stranger = save(&s.0, "other.png", &picture(99, 320, 240));
        let none = find_similar(&s.db(), &stranger, &s.previews(), 3).unwrap();
        assert!(none.closest[0].score < POSSIBLE, "{}", none.closest[0].score);
    }

    #[test]
    fn a_quarter_turned_framed_copy_is_found() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        for seed in 10..14 {
            save(&s.photos(), &format!("p{seed}.png"), &picture(seed, 320, 240));
        }
        list(&s);
        build_all(&s);
        let turned = image::imageops::rotate90(&picture(12, 320, 240));
        let mut framed = GrayImage::from_pixel(turned.width() + 50, turned.height() + 50, Luma([250]));
        image::imageops::replace(&mut framed, &turned, 25, 25);
        let scan = save(&s.0, "scan.png", &framed);
        let found = find_similar(&s.db(), &scan, &s.previews(), 2).unwrap();
        assert_eq!(name_of(&found.closest[0].path), "p12.png");
        assert!(found.closest[0].score >= POSSIBLE, "{}", found.closest[0].score);
    }

    #[test]
    fn a_copy_cropped_differently_is_found_and_marked_as_cropped() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        for seed in 30..36 {
            save(&s.photos(), &format!("c{seed}.png"), &picture(seed, 320, 240));
        }
        list(&s);
        build_all(&s);
        let whole = picture(33, 320, 240);
        let cut = image::imageops::crop_imm(&whole, 64, 40, 250, 190).to_image();
        let scan = save(&s.0, "scan.png", &cut);
        let found = find_similar(&s.db(), &scan, &s.previews(), 3).unwrap();
        assert_eq!(name_of(&found.closest[0].path), "c33.png");
        assert!(found.closest[0].score >= POSSIBLE, "{}", found.closest[0].score);
        assert!(found.closest[0].cropped, "the fingerprint alone missed it, so it is said to be a crop");
        assert!(!found.closest[1].cropped);

        // The same picture, whole, is found without any need for the crop pass.
        let again = save(&s.0, "again.png", &whole);
        let found = find_similar(&s.db(), &again, &s.previews(), 3).unwrap();
        assert_eq!(name_of(&found.closest[0].path), "c33.png");
        assert!(!found.closest[0].cropped);
    }

    #[test]
    fn a_list_made_before_crops_were_looked_for_is_brought_up_to_date() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "a.png", &picture(40, 200, 150));
        save(&s.photos(), "b.png", &picture(41, 200, 150));
        list(&s);
        build_all(&s);
        assert_eq!(pending(&s.db()), 0);
        // As an older version left it: fingerprints, but nothing for crops.
        let conn = open_for_writing(&s.db()).unwrap();
        conn.execute("UPDATE fingerprints SET crop_desc = NULL, crop_thumb = NULL", []).unwrap();
        drop(conn);
        assert_eq!(pending(&s.db()), 2);
        let r = build_all(&s);
        assert_eq!((r.considered, r.made), (2, 2));
        assert_eq!(pending(&s.db()), 0);
    }

    #[test]
    fn a_second_build_does_nothing_and_a_changed_file_is_done_again() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "a.png", &picture(20, 200, 150));
        save(&s.photos(), "b.png", &picture(21, 200, 150));
        list(&s);
        assert_eq!(build_all(&s).made, 2);
        assert_eq!(build_all(&s).considered, 0, "nothing new to learn");

        // A different picture saved over a.png, with a new date.
        save(&s.photos(), "a.png", &picture(22, 210, 150));
        let later = std::time::SystemTime::now() + Duration::from_secs(120);
        fs::File::options().write(true).open(s.photos().join("a.png")).unwrap().set_modified(later).unwrap();
        list(&s);
        assert_eq!(pending(&s.db()), 1, "only the changed file needs a new fingerprint");
        let r = build_all(&s);
        assert_eq!((r.considered, r.made), (1, 1));

        let scan = save(&s.0, "scan.png", &picture(22, 210, 150));
        let found = find_similar(&s.db(), &scan, &s.previews(), 1).unwrap();
        assert_eq!(name_of(&found.closest[0].path), "a.png", "the new contents, not the old, are what is compared");
        assert!(found.closest[0].score >= LIKELY);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_remembered_and_not_tried_again() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "good.png", &picture(30, 200, 150));
        fs::write(s.photos().join("broken.jpg"), b"this is not a picture").unwrap();
        list(&s);
        let r = build_all(&s);
        assert_eq!((r.made, r.unreadable), (1, 1));
        assert_eq!(build_all(&s).considered, 0, "the broken one is not retried at every update");
        let scan = save(&s.0, "scan.png", &picture(30, 200, 150));
        let found = find_similar(&s.db(), &scan, &s.previews(), 5).unwrap();
        assert_eq!(found.compared, 1, "and never compared with");
    }

    #[test]
    fn a_file_that_is_not_there_is_not_marked_unreadable() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "a.png", &picture(40, 200, 150));
        save(&s.photos(), "b.png", &picture(41, 200, 150));
        list(&s);
        fs::remove_file(s.photos().join("a.png")).unwrap(); // gone since the list was made
        let r = build_all(&s);
        assert_eq!((r.made, r.missing, r.unreadable), (1, 1, 0));
        assert_eq!(pending(&s.db()), 1, "it is tried again when it is back");
    }

    #[test]
    fn a_new_file_is_found_after_the_next_update_and_not_before() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "a.png", &picture(50, 200, 150));
        list(&s);
        build_all(&s);
        save(&s.photos(), "b.png", &picture(51, 200, 150));
        list(&s);
        let scan = save(&s.0, "scan.png", &picture(51, 200, 150));
        let before = find_similar(&s.db(), &scan, &s.previews(), 3).unwrap();
        assert_eq!((before.compared, before.pending), (1, 1), "the report says one picture is not yet learned");
        build_all(&s);
        let after = find_similar(&s.db(), &scan, &s.previews(), 3).unwrap();
        assert_eq!(after.compared, 2);
        assert_eq!(name_of(&after.closest[0].path), "b.png");
    }

    #[test]
    fn a_picture_taken_off_the_list_is_no_longer_found() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "a.png", &picture(60, 200, 150));
        save(&s.photos(), "b.png", &picture(61, 200, 150));
        list(&s);
        build_all(&s);
        fs::remove_file(s.photos().join("a.png")).unwrap();
        list(&s);
        let scan = save(&s.0, "scan.png", &picture(60, 200, 150));
        let found = find_similar(&s.db(), &scan, &s.previews(), 3).unwrap();
        assert_eq!(found.compared, 1);
        assert_eq!(name_of(&found.closest[0].path), "b.png");
    }

    #[test]
    fn progress_counts_the_pictures_that_needed_a_fingerprint() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        for seed in 70..75 {
            save(&s.photos(), &format!("p{seed}.png"), &picture(seed, 120, 90));
        }
        list(&s);
        let mut seen = Vec::new();
        build(&s.db(), &s.previews(), &mut |done, total| seen.push((done, total))).unwrap();
        assert_eq!(seen.first(), Some(&(0, 5)));
        assert_eq!(seen.last(), Some(&(5, 5)));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "never goes backwards");
    }

    #[test]
    fn fingerprints_from_another_version_are_discarded() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "a.png", &picture(80, 200, 150));
        list(&s);
        build_all(&s);
        Connection::open(s.db()).unwrap().execute("UPDATE meta SET value = '0' WHERE key = 'fingerprint_version'", []).unwrap();
        assert_eq!(build_all(&s).made, 1, "made again, not trusted");
    }

    #[test]
    fn checking_before_the_list_exists_or_with_a_bad_file_says_what_is_wrong() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        let scan = save(&s.0, "scan.png", &picture(90, 100, 80));
        assert!(find_similar(&s.db(), &scan, &s.previews(), 3).unwrap_err().contains("Update photo list"));
        save(&s.photos(), "a.png", &picture(91, 100, 80));
        list(&s);
        build_all(&s);
        fs::write(s.0.join("text.png"), b"not a picture").unwrap();
        assert!(find_similar(&s.db(), &s.0.join("text.png"), &s.previews(), 3).unwrap_err().contains("could not be read"));
        assert!(find_similar(&s.db(), &s.photos(), &s.previews(), 3).unwrap_err().contains("not a file"));
        assert!(find_similar(&s.db(), &s.0.join("nothing.png"), &s.previews(), 3).unwrap_err().contains("could not be opened"));
    }

    #[test]
    fn a_preview_is_used_in_place_of_the_original_when_there_is_one() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        let original = picture(100, 400, 300);
        let path = save(&s.photos(), "a.png", &original);
        list(&s);
        // Make the preview the way the application does, then damage the
        // original: the fingerprint can only have come from the preview.
        crate::thumbnails::get_preview_jpeg(&path, &s.previews(), 200).unwrap();
        let kept = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, b"damaged").unwrap();
        fs::File::options().write(true).open(&path).unwrap().set_modified(kept).unwrap();
        let r = build_all(&s);
        assert_eq!((r.made, r.unreadable), (1, 0));
        let scan = save(&s.0, "scan.png", &original);
        let found = find_similar(&s.db(), &scan, &s.previews(), 1).unwrap();
        assert!(found.closest[0].score >= LIKELY, "{}", found.closest[0].score);
    }
}
