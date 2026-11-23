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
use crate::face_match;
use crate::faces;
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

/// The same for the faces found in each picture.
const FACES_VERSION: i64 = 1;

/// Rows that still need making: no fingerprint at all, or one made before
/// cropped copies, or faces, were looked for. An unreadable picture is not
/// made again. Faces count only where the face models are there to look.
fn needs_making() -> String {
    let faces = if faces::available() { " OR p.faces IS NULL" } else { "" };
    format!("p.file_id IS NULL OR (length(p.data) > 0 AND (p.crop_desc IS NULL{faces}))")
}

/// The score at and above which two pictures are very probably the same
/// photograph, and the one at and above which they are possibly the same.
/// Settled by the program this was ported from, on a real collection's scans.
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

/// How many pictures are learned at once. Finding the faces in them is real
/// work, so this uses most of the processors -- leaving one for the window --
/// where the preview job, which holds large scans in memory, uses at most four.
/// The memory budget still bounds what is held at any moment.
fn workers() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    cores.saturating_sub(1).clamp(thumbnails::preview_concurrency(), 6)
}

/// What is learned about one picture: its fingerprint (empty if it could not be
/// read) and what is kept for finding cropped copies of it.
struct Made {
    data: Vec<u8>,
    crop: Option<crop_match::Stored>,
    /// The faces in it, packed; None if the face models are not there.
    faces: Option<Vec<u8>>,
}

/// How many pictures in the list have no fingerprint yet.
pub fn pending(db: &Path) -> u64 {
    if !db.exists() {
        return 0;
    }
    // Opened the way everything else opens it, so that a list made by an older
    // version is brought up to date first: asking an old list about columns it
    // does not have yet would fail, and a failure must never read as "nothing
    // left to learn".
    let counted = open_for_writing(db).ok().and_then(|conn| {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM files f LEFT JOIN fingerprints p ON p.file_id = f.id WHERE {}", needs_making()),
            [],
            |r| r.get::<_, i64>(0),
        )
        .ok()
    });
    // If it cannot be counted, say there is something to do, so that doing it
    // is tried and reports what is wrong.
    counted.map(|n| n as u64).unwrap_or(1)
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
    let have_faces: Option<i64> = conn
        .query_row("SELECT value FROM meta WHERE key = 'faces_version'", [], |r| r.get::<_, String>(0))
        .ok()
        .and_then(|v| v.parse().ok());
    if have_faces != Some(FACES_VERSION) {
        conn.execute("UPDATE fingerprints SET faces = NULL", []).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('faces_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![FACES_VERSION.to_string()],
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
                 LEFT JOIN fingerprints p ON p.file_id = f.id WHERE {} ORDER BY f.id",
                needs_making()
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
                "INSERT OR REPLACE INTO fingerprints (file_id, data, crop_desc, crop_thumb, faces) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, made.data, desc, thumb, made.faces],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    };

    let outcome: Result<(), String> = std::thread::scope(|scope| {
        for _ in 0..workers() {
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
                        let faces = faces::find(&faces::shrink_for_faces(&img)).map(|f| faces::pack(&f));
                        Made {
                            data: fingerprint::pack(&fingerprint::describe(&gray)),
                            crop: Some(crop_match::describe(&gray)),
                            faces,
                        }
                    });
                    budget.release(reserved);
                    Some(made.unwrap_or(Made { data: Vec::new(), crop: None, faces: None }))
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
    /// The faces in the pictures, with the index in `ids` of each one's picture.
    faces: face_match::StoredFaces,
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
    let mut stored_faces = face_match::StoredFaces::default();
    let mut stmt = conn
        .prepare("SELECT file_id, data, crop_desc, faces FROM fingerprints WHERE length(data) = ?1 ORDER BY file_id")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![(STORED_LEN * 2) as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (id, bytes, windows, face_bytes) = row.map_err(|e| e.to_string())?;
        if let Some(found) = face_bytes.as_deref().and_then(faces::unpack) {
            stored_faces.add(ids.len(), &found);
        }
        ids.push(id);
        data.extend(bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)));
        match windows.filter(|w| w.len() == CROP_STORED_LEN * 2) {
            Some(w) => crop.extend(w.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c))),
            None => crop.extend(std::iter::repeat_n(0u16, CROP_STORED_LEN)),
        }
    }
    *held = Some(Collection { generation, ids, data, crop, faces: stored_faces });
    Ok(())
}

/// The pictures in the collection that look most like the scan at `scan`.
#[cfg(test)]
pub fn find_similar(db: &Path, scan: &Path, cache_dir: &Path, limit: usize) -> Result<SimilarResult, String> {
    find_similar_in(db, scan, cache_dir, limit, None)
}

/// Like [`find_similar`], looking only at the part of the scan inside `part`
/// (fractions left, top, right, bottom) when one is given: a piece the person
/// outlined themselves, taken as it is, with no margin trimmed from it.
pub fn find_similar_in(
    db: &Path,
    scan: &Path,
    cache_dir: &Path,
    limit: usize,
    part: Option<(f32, f32, f32, f32)>,
) -> Result<SimilarResult, String> {
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
        .ok_or_else(|| unreadable_scan_message(scan))?;
    let image = match part {
        Some(b) => fingerprint::cut_to(&image, b),
        None => image,
    };
    let gray = fingerprint::gray_for_matching(&image);
    let scan_faces = faces::available().then(|| {
        let rgb = faces::shrink_for_faces(&image);
        faces::find(&rgb).filter(|f| !f.faces.is_empty()).map(|found| {
            let turned = face_match::Gray::from_rgb(&faces::turn(&rgb, found.rot));
            (found, turned)
        })
    });
    // A scan with blank paper or scanner margin round it is also looked at
    // trimmed to the picture, and each photo keeps whichever fits better.
    let trimmed = part
        .is_none()
        .then(|| fingerprint::content_box(&image))
        .flatten()
        .map(|b| fingerprint::gray_for_matching(&fingerprint::cut_to(&image, b)));
    drop(image);
    let query = fingerprint::query(&gray);
    let crop_query = crop_match::query(&gray);
    let trimmed_queries = trimmed.as_ref().map(|g| (fingerprint::query(g), crop_match::query(g)));

    load(&conn)?;
    let held = collection().lock().unwrap();
    let held = held.as_ref().ok_or("the fingerprints could not be loaded")?;
    let mut scores = fingerprint::score_all(&query, &held.data);
    let mut cropped = crop_scores(&conn, held, &crop_query, &mut scores);
    if let Some((q, cq)) = &trimmed_queries {
        let mut again = fingerprint::score_all(q, &held.data);
        let more = crop_scores(&conn, held, cq, &mut again);
        for i in 0..scores.len() {
            if again[i] > scores[i] {
                scores[i] = again[i];
                cropped[i] = more[i];
            }
        }
    }
    if let Some(Some((found, turned))) = &scan_faces {
        face_scores(&conn, held, found, turned, cache_dir, &mut scores, &mut cropped);
    }

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

/// Why a dropped file could not be read, with what to do about it when the
/// reason is a phone or web picture (HEIC, AVIF) this computer has no reader for.
fn unreadable_scan_message(scan: &Path) -> String {
    let ext = scan.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let heic = matches!(ext.as_str(), "heic" | "heif" | "hif");
    let avif = ext == "avif";
    if (heic || avif) && !cfg!(target_os = "macos") {
        let kind = if heic { "HEIC" } else { "AVIF" };
        if cfg!(windows) {
            let fix = if heic {
                "Microsoft's free \"HEIF Image Extensions\" (and, for most phone photos, the \"HEVC Video Extensions\")"
            } else {
                "Microsoft's free \"AV1 Video Extension\""
            };
            return format!(
                "That {kind} picture could not be read. Windows needs {fix} from the Microsoft Store. \
                 Or share or export the photo as a JPEG and drop that."
            );
        }
        let fix = if heic { "\"heif-gdk-pixbuf\" and \"libheif-plugin-libde265\"" } else { "\"heif-gdk-pixbuf\"" };
        return format!(
            "That {kind} picture could not be read on this computer. Whoever looks after it can install {fix}, \
             or export the photo as a JPEG and drop that."
        );
    }
    "That file could not be read as a picture.".into()
}

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

/// For a scan with faces in it: looks at the pictures that hold the same faces,
/// to see whether the scan is a small piece cut from one of them, and raises
/// their scores where it is. Like the crop pass, it never spoils a search.
fn face_scores(
    conn: &Connection,
    held: &Collection,
    found: &faces::Found,
    scan: &face_match::Gray,
    cache_dir: &Path,
    scores: &mut [f32],
    raised: &mut [bool],
) {
    let picks = face_match::candidates(found, &held.faces, scores, None);
    if picks.is_empty() {
        return;
    }
    let paths: Vec<Option<PathBuf>> = picks
        .iter()
        .map(|p| {
            conn.query_row("SELECT root, rel FROM files WHERE id = ?1", params![held.ids[p.photo]], |r| {
                Ok(Path::new(&r.get::<_, String>(0)?).join(r.get::<_, String>(1)?))
            })
            .ok()
        })
        .collect();
    let work: Vec<(&face_match::Pick, &PathBuf)> =
        picks.iter().zip(&paths).filter_map(|(p, path)| path.as_ref().map(|path| (p, path))).collect();
    let results = crop_match::parallel_map(&work, |&(pick, path)| {
        let image = thumbnails::decode_for_matching(path, cache_dir)?;
        let rgb = faces::shrink_for_faces(&image);
        let stored = face_match::Gray::from_rgb(&faces::turn(&rgb, pick.rot));
        Some((pick.photo, face_match::report(face_match::place_score(scan, &stored, &pick.pairs))))
    });
    for (photo, reported) in results.into_iter().flatten() {
        if reported > scores[photo] {
            // Only called a crop when the ordinary comparison clearly missed it.
            raised[photo] = reported > scores[photo] + 0.05;
            scores[photo] = reported;
        }
    }
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
    fn a_photograph_scanned_on_a_wide_blank_margin_is_found() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        for seed in 40..46 {
            save(&s.photos(), &format!("m{seed}.png"), &picture(seed, 320, 240));
        }
        list(&s);
        build_all(&s);
        let photo = picture(43, 320, 240);
        let mut page = GrayImage::from_pixel(640, 640, image::Luma([255]));
        image::imageops::overlay(&mut page, &photo, 170, 200);
        let scan = save(&s.0, "paper.png", &page);
        let found = find_similar(&s.db(), &scan, &s.previews(), 3).unwrap();
        assert_eq!(name_of(&found.closest[0].path), "m43.png");
        assert!(found.closest[0].score >= LIKELY, "{}", found.closest[0].score);
    }

    /// How well crops of REAL photographs are found. Needs a copy of the
    /// real photo list and the settings; prints counts only, never names.
    ///   HS_REAL_DB=<copy of photo_check.db> HS_REAL_CONFIG=<config.json>
    ///   HS_REAL_PREVIEWS=<thumb_cache> cargo test crop_recall_on_real_photos -- --ignored --nocapture
    #[test]
    #[ignore = "needs the real photo list; run by hand"]
    fn crop_recall_on_real_photos() {
        use sha2::{Digest, Sha256};
        let db = PathBuf::from(std::env::var("HS_REAL_DB").expect("HS_REAL_DB"));
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(std::env::var("HS_REAL_CONFIG").expect("HS_REAL_CONFIG")).unwrap()).unwrap();
        let previews = PathBuf::from(std::env::var("HS_REAL_PREVIEWS").expect("HS_REAL_PREVIEWS"));
        let want: usize = std::env::var("HS_REAL_N").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        let roots: Vec<(String, PathBuf)> = config["photo_archive_roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["label"].as_str().unwrap_or("").to_string(), PathBuf::from(r["path"].as_str().unwrap())))
            .collect();
        let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut all: Vec<PathBuf> = Vec::new();
        let mut q = conn.prepare("SELECT root, rel FROM files WHERE lower(rel) LIKE '%.jpg' OR lower(rel) LIKE '%.jpeg' OR lower(rel) LIKE '%.tif' OR lower(rel) LIKE '%.tiff' ORDER BY id").unwrap();
        let mut rows = q.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let (root, rel): (String, String) = (r.get(0).unwrap(), r.get(1).unwrap());
            let base = roots.iter().find(|(l, p)| *l == root || p.to_string_lossy() == root).map(|(_, p)| p.clone());
            if let Some(b) = base {
                all.push(b.join(rel));
            }
        }
        let step = (all.len() / want).max(1);
        let sample: Vec<&PathBuf> = all.iter().step_by(step).take(want).collect();
        let dir = std::env::temp_dir().join(format!("hs-realcrop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sha = |p: &Path| -> Option<[u8; 32]> { Some(Sha256::digest(std::fs::read(p).ok()?).into()) };
        println!("sample {} of {} candidate photographs", sample.len(), all.len());
        // Which other pictures are copies of each sampled one (the whole
        // photograph, checked against the list), so that finding another copy
        // counts as right rather than wrong.
        let copies: Vec<Vec<PathBuf>> = sample
            .iter()
            .map(|o| {
                find_similar_in(&db, o, &previews, 12, None)
                    .map(|f| f.closest.iter().filter(|s| s.score >= LIKELY).map(|s| s.path.clone()).collect())
                    .unwrap_or_default()
            })
            .collect();
        println!("pictures with another copy in the collection: {}", copies.iter().filter(|c| c.len() > 1).count());
        for frac in [0.8f32, 0.6, 0.45, 0.3] {
            // right: flagged as likely and it is a copy; right-possible: same but only "possible";
            // wrong-sure: a different photograph flagged likely; wrong-possible: different, "possible";
            // seen-low: a copy is in the list but scored below possible; missing: not in the list at all.
            let (mut n, mut right, mut right_possible, mut wrong_sure, mut wrong_possible, mut seen_low, mut missing) = (0, 0, 0, 0, 0, 0, 0);
            let mut low_scores: Vec<f32> = Vec::new();
            let started = std::time::Instant::now();
            for (i, original) in sample.iter().enumerate() {
                let Some(img) = thumbnails::decode_for_matching(original, &previews) else { continue };
                let (w, h) = (img.width(), img.height());
                if w < 200 || h < 200 {
                    continue;
                }
                let (cw, ch) = (((w as f32) * frac) as u32, ((h as f32) * frac) as u32);
                // A corner or the middle, varied from photo to photo.
                let (fx, fy) = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.5, 0.5)][i % 5];
                let (x, y) = (((w - cw) as f32 * fx) as u32, ((h - ch) as f32 * fy) as u32);
                let mut piece = img.crop_imm(x, y, cw, ch);
                if piece.width().max(piece.height()) > 1400 {
                    piece = piece.resize(1400, 1400, image::imageops::FilterType::Triangle);
                }
                let scan = dir.join("piece.png");
                piece.to_rgb8().save(&scan).unwrap();
                let Ok(found) = find_similar_in(&db, &scan, &previews, 12, None) else { continue };
                n += 1;
                let is_copy = |p: &Path| p == original.as_path() || copies[i].iter().any(|c| c == p);
                let Some(first) = found.closest.first() else { missing += 1; continue };
                if is_copy(&first.path) {
                    if first.score >= LIKELY { right += 1 } else if first.score >= POSSIBLE { right_possible += 1 } else { seen_low += 1; low_scores.push(first.score) }
                } else if first.score >= LIKELY {
                    wrong_sure += 1;
                    // a copy further down still counts as seen
                } else if first.score >= POSSIBLE {
                    wrong_possible += 1;
                } else if let Some(c) = found.closest.iter().find(|s| is_copy(&s.path)) {
                    seen_low += 1;
                    low_scores.push(c.score);
                } else {
                    missing += 1;
                }
            }
            println!(
                "crop {:>3.0}% of each side ({:>2.0}% of the area): n={n}  found-likely={right} found-possible={right_possible}  WRONG-likely={wrong_sure} wrong-possible={wrong_possible}  nothing-flagged={}  [{:.1}s each]",
                frac * 100.0, frac * frac * 100.0, seen_low + missing, started.elapsed().as_secs_f32() / n.max(1) as f32
            );
            low_scores.sort_by(|a, b| b.total_cmp(a));
            println!(
                "      of the nothing-flagged: {seen_low} had the copy in the nearest 12 but scored low (highest few: {:?}), {missing} did not have it in the nearest 12",
                low_scores.iter().take(6).map(|v| (v * 100.0).round() / 100.0).collect::<Vec<_>>()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_photograph_kept_in_an_old_format_is_learned_and_found() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        for seed in 60..65 {
            save(&s.photos(), &format!("n{seed}.png"), &picture(seed, 320, 240));
        }
        // One of the collection's pictures is a PCX file, which the ordinary
        // picture readers cannot open.
        let old = picture(66, 320, 240);
        let rgb = image::DynamicImage::ImageLuma8(old.clone()).to_rgb8();
        let mut bytes = Vec::new();
        {
            let mut w = pcx::WriterRgb::new(std::io::Cursor::new(&mut bytes), (320, 240), (72, 72)).unwrap();
            for row in rgb.as_raw().chunks(320 * 3) {
                w.write_row(row).unwrap();
            }
            w.finish().unwrap();
        }
        std::fs::write(s.photos().join("old66.pcx"), &bytes).unwrap();
        list(&s);
        build_all(&s);
        let scan = save(&s.0, "scan66.png", &picture(66, 320, 240));
        let found = find_similar(&s.db(), &scan, &s.previews(), 3).unwrap();
        assert_eq!(name_of(&found.closest[0].path), "old66.pcx");
        assert!(found.closest[0].score >= LIKELY, "{}", found.closest[0].score);
    }

    #[test]
    fn a_part_the_person_outlines_is_what_is_checked() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        for seed in 50..56 {
            save(&s.photos(), &format!("o{seed}.png"), &picture(seed, 320, 240));
        }
        list(&s);
        build_all(&s);
        // A scan holding two photographs side by side; only one half is outlined.
        let (left, right) = (picture(52, 320, 240), picture(54, 320, 240));
        let mut both = GrayImage::new(640, 240);
        image::imageops::replace(&mut both, &left, 0, 0);
        image::imageops::replace(&mut both, &right, 320, 0);
        let scan = save(&s.0, "two.png", &both);
        let l = find_similar_in(&s.db(), &scan, &s.previews(), 3, Some((0.0, 0.0, 0.5, 1.0))).unwrap();
        assert_eq!(name_of(&l.closest[0].path), "o52.png");
        let r = find_similar_in(&s.db(), &scan, &s.previews(), 3, Some((0.5, 0.0, 1.0, 1.0))).unwrap();
        assert_eq!(name_of(&r.closest[0].path), "o54.png");
    }

    #[test]
    fn faces_are_looked_for_when_learning_and_not_looked_for_again() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "a.png", &picture(60, 200, 150));
        save(&s.photos(), "b.png", &picture(61, 200, 150));
        list(&s);
        assert_eq!(build_all(&s).made, 2);
        assert_eq!(pending(&s.db()), 0);
        let conn = open_for_writing(&s.db()).unwrap();
        let kept: i64 = conn.query_row("SELECT COUNT(*) FROM fingerprints WHERE faces IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 2, "even a picture with no face is marked as looked at");
        // As the version before faces left it.
        conn.execute("UPDATE fingerprints SET faces = NULL", []).unwrap();
        drop(conn);
        assert_eq!(pending(&s.db()), 2);
        assert_eq!(build_all(&s).made, 2);
        assert_eq!(pending(&s.db()), 0);
    }

    #[test]
    fn a_small_piece_cut_around_a_face_is_found_through_the_face() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        let whole = picture(70, 640, 480);
        for seed in 70..74 {
            save(&s.photos(), &format!("f{seed}.png"), &picture(seed, 640, 480));
        }
        list(&s);
        build_all(&s);
        let conn = open_for_writing(&s.db()).unwrap();
        load(&conn).unwrap();
        let ids = collection().lock().unwrap().as_ref().unwrap().ids.clone();
        // Folders are not walked in the same order everywhere, so find where
        // the photograph the piece comes from was put.
        let k = ids
            .iter()
            .position(|id| {
                let rel: String = conn.query_row("SELECT rel FROM files WHERE id = ?1", params![id], |r| r.get(0)).unwrap();
                rel == "f70.png"
            })
            .unwrap();
        // The faces themselves are made up: that photograph holds one at (300, 180),
        // 40 across, and the scan is a piece cut around it.
        let emb: Vec<f32> = (0..faces::EMB_LEN).map(|i| if i == 5 { 1.0 } else { 0.0 }).collect();
        let mut stored = face_match::StoredFaces::default();
        stored.add(k, &faces::Found {
            rot: 0,
            faces: vec![faces::Face { score: 1.0, bbox: [300.0 / 640.0, 180.0 / 480.0, 40.0 / 640.0, 40.0 / 480.0], emb: emb.clone() }],
        });
        let held = Collection { generation: 0, ids, data: Vec::new(), crop: Vec::new(), faces: stored };
        let piece = image::imageops::crop_imm(&whole, 200, 120, 240, 200).to_image();
        let found = faces::Found {
            rot: 0,
            faces: vec![faces::Face { score: 1.0, bbox: [100.0 / 240.0, 60.0 / 200.0, 40.0 / 240.0, 40.0 / 200.0], emb }],
        };
        let scan = face_match::Gray {
            w: piece.width() as usize,
            h: piece.height() as usize,
            d: piece.pixels().map(|p| p.0[0] as f32).collect(),
        };
        let mut scores = vec![0.2f32; 4];
        let mut raised = vec![false; 4];
        face_scores(&conn, &held, &found, &scan, &s.previews(), &mut scores, &mut raised);
        assert!(scores[k] >= 0.9, "{scores:?}");
        assert!(raised[k], "marked as found by cropping");
        assert!(scores.iter().enumerate().all(|(i, &v)| i == k || v == 0.2), "the others are untouched");
        // A picture that already scores well is not looked at again.
        let mut scores = vec![0.2f32; 4];
        scores[k] = 0.95;
        let mut raised = vec![false; 4];
        face_scores(&conn, &held, &found, &scan, &s.previews(), &mut scores, &mut raised);
        assert_eq!(scores[k], 0.95);
        assert!(!raised[k]);
    }

    #[test]
    fn a_list_in_the_older_format_is_counted_correctly_and_not_taken_for_complete() {
        let _one = one_at_a_time();
        let s = Scratch::new();
        save(&s.photos(), "a.png", &picture(50, 200, 150));
        save(&s.photos(), "b.png", &picture(51, 200, 150));
        list(&s);
        // As step 3 left it: a fingerprints table without the columns for crops,
        // and none of the pictures learned yet.
        let conn = open_for_writing(&s.db()).unwrap();
        conn.execute_batch(
            "DROP TABLE fingerprints; CREATE TABLE fingerprints (file_id INTEGER PRIMARY KEY, data BLOB NOT NULL);
             DELETE FROM meta WHERE key = 'crop_version';",
        )
        .unwrap();
        drop(conn);
        assert_eq!(pending(&s.db()), 2, "both still to learn, not 'nothing left'");
        assert_eq!(build_all(&s).made, 2);
        assert_eq!(pending(&s.db()), 0);
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
        assert!(find_similar(&s.db(), &scan, &s.previews(), 3).unwrap_err().contains("Settings"));
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
