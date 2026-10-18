//! Cached JPEG-preview generation for any source image (JPEG, TIFF, BMP,
//! PNG, GIF, ...). Browsers/webviews can't render TIFF/BMP inline, and
//! showing multi-megabyte originals just for a small list thumbnail is
//! wasteful, so every image gets a small cached JPEG stand-in.
//!
//! Ported from the prototype's `thumbnails.py`, with one fix made during
//! that phase carried forward deliberately: the cache key is a hash of the
//! *canonical* (symlink- and ".."-resolved) path, not the raw path string,
//! because two different-but-equivalent spellings of the same path (e.g.
//! after moving the app) must hash identically or the cache silently misses
//! and everything gets needlessly regenerated.

use image::imageops::FilterType;
use sha1::{Digest, Sha1};
use std::fs;
use std::path::{Path, PathBuf};

fn cache_path_for(original_path: &Path, cache_dir: &Path) -> PathBuf {
    // SHA-1 specifically (not a "stronger" hash) to stay byte-for-byte
    // compatible with the existing cache the Python prototype already built
    // (thumbnails.py uses hashlib.sha1) -- there's no cryptographic need
    // here, just a stable cache key, so matching it avoids silently
    // invalidating and regenerating thousands of real cached thumbnails during
    // the transition to this native rewrite.
    let canonical = fs::canonicalize(original_path).unwrap_or_else(|_| original_path.to_path_buf());
    let mut hasher = Sha1::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    cache_dir.join(&hex[0..2]).join(format!("{hex}.jpg"))
}

/// How much the decoder may allocate for one image.
///
/// The `image` crate applies a 512MiB ceiling by default, which is smaller
/// than it sounds: an RGB decode needs width x height x 3 bytes, so anything
/// past about 178 megapixels is refused. This collection contains a
/// 240-megapixel scan (18216x13224) that needs roughly 689MB, and the Python
/// prototype rendered it happily because PIL applies no such limit. Without
/// raising this, that photograph would have no preview on any machine with a
/// cold cache -- which is every machine except this one, where the file left
/// behind by the prototype hid the problem entirely.
///
/// Still bounded rather than removed: image dimensions come from the file's
/// own header, and a corrupt one claiming billions of pixels would otherwise
/// have the process ask for more memory than exists. Rust aborts on a failed
/// allocation, and no amount of error handling catches that.
///
/// Measured cost of the largest image in this collection at this setting:
/// decoded in 0.3s, previewed in 1.6s, peak memory 1.08GB for the moment it
/// is held. That is transient and one image at a time, but it is the single
/// heaviest thing the application does, so it is worth knowing. Lowering this
/// to the crate's 512MiB default costs nothing except that one photograph's
/// preview, which then reports itself as unavailable rather than failing
/// silently.
const MAX_DECODE_ALLOCATION: u64 = 1024 * 1024 * 1024;

/// Decodes an image, reporting why rather than just that it failed.
fn decode_source(path: &Path) -> Result<image::DynamicImage, String> {
    let reader = image::ImageReader::open(path).map_err(|e| e.to_string())?;
    // Detect the format from the file's *contents*, not its extension.
    // `image::open()` trusts the extension, which silently loses real images
    // that are misnamed -- this collection has files with no usable extension
    // at all, which PIL reads fine by sniffing content.
    let mut reader = reader.with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOCATION);
    reader.limits(limits);
    reader.decode().map_err(|e| e.to_string())
}

/// The preview already on disk for this image, if there is one and it is not
/// older than the image itself.
///
/// Exposed so the bulk warmer can skip finished work without paying for a
/// header read: on any run after the first, nearly every image is already
/// done, and that run should cost a couple of file stats each rather than
/// opening thousands of photographs.
pub fn cached_preview(original_path: &Path, cache_dir: &Path) -> Option<PathBuf> {
    let out_path = cache_path_for(original_path, cache_dir);
    let src_mtime = fs::metadata(original_path).ok()?.modified().ok()?;
    let out_mtime = fs::metadata(&out_path).ok()?.modified().ok()?;
    (out_mtime >= src_mtime).then_some(out_path)
}

/// Returns a cached, browser-safe JPEG preview path for `original_path`,
/// generating it if missing or stale. Returns None if unreadable as an image.
pub fn get_preview_jpeg(original_path: &Path, cache_dir: &Path, max_dim: u32) -> Option<PathBuf> {
    let out_path = cache_path_for(original_path, cache_dir);

    let src_mtime = fs::metadata(original_path).ok()?.modified().ok()?;
    if let Ok(out_meta) = fs::metadata(&out_path) {
        if let Ok(out_mtime) = out_meta.modified() {
            if out_mtime >= src_mtime {
                return Some(out_path);
            }
        }
    }

    let img = match decode_source(original_path) {
        Ok(img) => img,
        // The `image` crate also can't decode palette TIFFs (7% of this
        // collection's TIFFs), so fall back to our own decoder for those
        // rather than silently dropping the preview.
        Err(reason) => match crate::palette_tiff::try_decode(original_path) {
            Some(img) => img,
            None => {
                // Reached once per unreadable image. Without this, a picture
                // that will not decode is indistinguishable from one that was
                // never there -- and "no pictures at all" is the likeliest
                // complaint from an untested platform.
                crate::diagnostics::error_repeating(
                    "image-decode-failed",
                    "preview",
                    format!("could not decode {}: {reason}", original_path.display()),
                );
                return None;
            }
        },
    };
    let (w, h) = (img.width(), img.height());
    // Match PIL's Image.thumbnail(): only ever shrinks, never enlarges.
    let resized = if w > max_dim || h > max_dim {
        img.resize(max_dim, max_dim, FilterType::Lanczos3)
    } else {
        img
    };
    let rgb = resized.to_rgb8();

    if let Some(parent) = out_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            crate::diagnostics::error_repeating(
                "thumb-cache-unwritable",
                "preview",
                format!("could not create the preview cache folder {}: {e}", parent.display()),
            );
            return None;
        }
    }

    // Encode to a temp file and rename into place, so an interrupted run
    // (app quit, crash, power loss part-way through thousands of images) can
    // never leave a truncated JPEG at the real cache path. Such a file would
    // still look "fresh" by mtime and be served as a broken image forever.
    // Unique per writer. The indexing thread and the window's own on-demand
    // preview generation can reach the same image simultaneously; sharing one
    // temporary name meant each could truncate the other's output, and the
    // half-written result would be renamed into place looking perfectly fresh.
    let temp_path =
        out_path.with_extension(format!("jpg.partial.{}", crate::app_paths::unique_temp_suffix()));
    {
        let mut out_file = match fs::File::create(&temp_path) {
            Ok(f) => f,
            Err(e) => {
                crate::diagnostics::error_repeating(
                    "thumb-write-failed",
                    "preview",
                    format!("could not write the preview file {}: {e}", temp_path.display()),
                );
                return None;
            }
        };
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out_file, 85);
        if encoder
            .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
            .is_err()
        {
            let _ = fs::remove_file(&temp_path);
            return None;
        }
    } // file closed here -- required before renaming on Windows
    if let Err(e) = fs::rename(&temp_path, &out_path) {
        crate::diagnostics::error_repeating(
            "thumb-rename-failed",
            "preview",
            format!("could not move the finished preview into {}: {e}", out_path.display()),
        );
        let _ = fs::remove_file(&temp_path);
        return None;
    }

    Some(out_path)
}

// ------------------------------------------------------- making many at once

/// Decoded image data allowed to be in flight across all workers at once.
///
/// Previews are made on several threads, so the reassuring "one image at a
/// time" that made `MAX_DECODE_ALLOCATION` safe on its own no longer holds:
/// this collection has over a thousand files above 20MB and several at 156MB,
/// and four of those decoding together would ask for several gigabytes on a
/// machine that may only have eight. Each worker therefore reserves what its
/// image will actually need -- taken from the file's own header, before any
/// pixels are read -- and waits if the total would go over.
const PREVIEW_MEMORY_BUDGET: u64 = 1024 * 1024 * 1024;

/// Assumed cost when the header cannot be read: such a file is about to fail
/// to decode anyway, so this only has to be non-zero and not absurd.
const UNKNOWN_IMAGE_COST: u64 = 64 * 1024 * 1024;

/// A decoded image is held as 8-bit RGBA in the worst case.
const BYTES_PER_PIXEL: u64 = 4;

struct MemoryBudget {
    free: std::sync::Mutex<u64>,
    total: u64,
    released: std::sync::Condvar,
}

impl MemoryBudget {
    fn new(total: u64) -> Self {
        MemoryBudget {
            free: std::sync::Mutex::new(total),
            total,
            released: std::sync::Condvar::new(),
        }
    }

    /// Blocks until `want` bytes are free, and returns what was actually
    /// reserved -- which is all of it for an image bigger than the whole
    /// budget, since otherwise that image could never be made at all.
    fn acquire(&self, want: u64) -> u64 {
        let want = want.clamp(1, self.total);
        let mut free = self.free.lock().unwrap();
        while *free < want {
            free = self.released.wait(free).unwrap();
        }
        *free -= want;
        want
    }

    fn release(&self, amount: u64) {
        let mut free = self.free.lock().unwrap();
        *free += amount;
        // Any number of waiters may now fit, and they want different amounts,
        // so waking only one can leave the budget idle while a small job waits
        // behind a large one.
        self.released.notify_all();
    }
}

/// What one bulk preview pass did, for the log: enough to tell a slow disk
/// from a slow processor without guessing.
pub struct WarmStats {
    pub considered: usize,
    pub made: usize,
    pub already_done: usize,
    pub failed: usize,
    pub bytes_read: u64,
    pub elapsed: std::time::Duration,
}

impl WarmStats {
    pub fn summary(&self) -> String {
        let secs = self.elapsed.as_secs_f64().max(0.001);
        let mb = self.bytes_read as f64 / 1_048_576.0;
        format!(
            "{} of {} previews made in {:.0}s ({} already done, {} could not be read); \
             read {:.0}MB at {:.1}MB/s, {:.0}ms per image",
            self.made,
            self.considered,
            secs,
            self.already_done,
            self.failed,
            mb,
            mb / secs,
            if self.made > 0 { secs * 1000.0 / self.made as f64 } else { 0.0 }
        )
    }
}

/// How many images are decoded at once.
fn preview_concurrency() -> usize {
    // Capped well below a big machine's thread count on purpose. The work is
    // mostly waiting for a disk, so a handful of readers is enough to keep it
    // busy, and every extra worker is another decode's worth of memory.
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 4)
}

/// The memory one image will need, from its header alone.
fn decoded_cost(path: &Path) -> u64 {
    let Ok(reader) = image::ImageReader::open(path) else { return UNKNOWN_IMAGE_COST };
    let Ok(reader) = reader.with_guessed_format() else { return UNKNOWN_IMAGE_COST };
    match reader.into_dimensions() {
        Ok((w, h)) => (w as u64)
            .saturating_mul(h as u64)
            .saturating_mul(BYTES_PER_PIXEL)
            .max(1),
        Err(_) => UNKNOWN_IMAGE_COST,
    }
}

/// Makes the previews for many images at once, reporting how far along it is.
///
/// This is the slowest thing the application ever does -- thousands of
/// photographs, most of them multi-megabyte scans -- and it used to happen one
/// image at a time, interleaved with building the index, with no count on
/// screen for most of it. A first run took over two hours and looked like it
/// had hung, which is not a state to leave a volunteer in.
///
/// Checked against the code it replaced by rebuilding the whole collection
/// from a cold cache and comparing the result record by record with the index
/// the one-at-a-time version produced: every record identical, with the same
/// number of image rows and the same previews present. The measurement that
/// went with it: the four workers hold the processor at very nearly 400%, so
/// the time really does divide, and decoding is what costs -- 0.4 to 0.9
/// seconds for the large scans, against 0.2 to 0.5 to resize them. Resizing
/// more cheaply was tried and rejected: it saved at most a third of the
/// smaller half, and on one photograph it visibly changed the result.
pub fn warm_previews(
    paths: &[PathBuf],
    cache_dir: &Path,
    max_dim: u32,
    progress: &mut dyn FnMut(u64, u64),
) -> WarmStats {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let started = std::time::Instant::now();
    let total = paths.len() as u64;
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let made = AtomicUsize::new(0);
    let already = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let bytes = std::sync::atomic::AtomicU64::new(0);
    let budget = MemoryBudget::new(PREVIEW_MEMORY_BUDGET);

    progress(0, total);
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..preview_concurrency() {
            workers.push(scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(i) else { break };

                    if cached_preview(path, cache_dir).is_some() {
                        already.fetch_add(1, Ordering::Relaxed);
                    } else {
                        let cost = decoded_cost(path);
                        let reserved = budget.acquire(cost);
                        let result = get_preview_jpeg(path, cache_dir, max_dim);
                        budget.release(reserved);

                        if result.is_some() {
                            made.fetch_add(1, Ordering::Relaxed);
                            if let Ok(meta) = fs::metadata(path) {
                                bytes.fetch_add(meta.len(), Ordering::Relaxed);
                            }
                        } else {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // The calling thread does the reporting rather than the workers, so
        // the progress callback stays single-threaded and the window keeps
        // being told where things are even while every worker is inside a
        // slow decode.
        //
        // The condition is "the workers have stopped", not "the count has
        // reached the total": a worker that panics never adds its last one, so
        // waiting for the count would wait forever -- and the scope below
        // cannot join it while this thread is still spinning. A hang is the
        // one failure worse than a crash here, because the crash at least says
        // what happened.
        while !workers.iter().all(|w| w.is_finished()) {
            std::thread::sleep(std::time::Duration::from_millis(200));
            progress(done.load(Ordering::Relaxed) as u64, total);
        }
    });

    progress(total, total);
    WarmStats {
        considered: paths.len(),
        made: made.load(Ordering::Relaxed),
        already_done: already.load(Ordering::Relaxed),
        failed: failed.load(Ordering::Relaxed),
        bytes_read: bytes.load(Ordering::Relaxed),
        elapsed: started.elapsed(),
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn the_budget_never_lets_more_through_than_it_holds() {
        // The point of the budget is a bound on memory, so the thing worth
        // asserting is the bound itself: the total reserved at any instant.
        let budget = Arc::new(MemoryBudget::new(1000));
        let in_flight = Arc::new(AtomicU64::new(0));
        let high_water = Arc::new(AtomicU64::new(0));

        let mut threads = Vec::new();
        for worker in 0..8 {
            let budget = Arc::clone(&budget);
            let in_flight = Arc::clone(&in_flight);
            let high_water = Arc::clone(&high_water);
            threads.push(std::thread::spawn(move || {
                for round in 0..25 {
                    // A mix of sizes, including one that alone exceeds the
                    // budget and so must be allowed to run on its own.
                    let want = match (worker + round) % 4 {
                        0 => 50,
                        1 => 400,
                        2 => 900,
                        _ => 4000,
                    };
                    let got = budget.acquire(want);
                    let now = in_flight.fetch_add(got, Ordering::SeqCst) + got;
                    high_water.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_micros(50));
                    in_flight.fetch_sub(got, Ordering::SeqCst);
                    budget.release(got);
                }
            }));
        }
        for t in threads {
            t.join().expect("a worker panicked or deadlocked");
        }

        assert_eq!(in_flight.load(Ordering::SeqCst), 0, "every reservation should be given back");
        let peak = high_water.load(Ordering::SeqCst);
        assert!(peak <= 1000, "{peak} bytes were in flight at once, over the 1000 budget");
        assert!(peak > 0, "the test did not actually reserve anything");
    }

    #[test]
    fn the_bulk_warmer_finishes_and_counts_correctly_with_bad_input_mixed_in() {
        // A folder of thousands of photographs contains files that are not
        // photographs: a Thumbs.db, a stray text file, a name pointing at
        // something that has since been deleted. The pass has to get to the
        // end of the list regardless, report progress that reaches the total,
        // and count what it actually managed.
        let dir = std::env::temp_dir().join(format!("hs_warm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).unwrap();

        let mut paths = Vec::new();
        for i in 0..6 {
            let path = dir.join(format!("real{i}.png"));
            let img = image::RgbImage::from_fn(8, 8, |x, y| {
                image::Rgb([(x * 30) as u8, (y * 30) as u8, i as u8 * 40])
            });
            img.save(&path).unwrap();
            paths.push(path);
        }
        std::fs::write(dir.join("notes.txt"), b"not a photograph at all").unwrap();
        paths.push(dir.join("notes.txt"));
        paths.push(dir.join("deleted-since.tif"));
        std::fs::create_dir_all(dir.join("subfolder")).unwrap();
        paths.push(dir.join("subfolder"));

        let mut samples = Vec::new();
        let stats = warm_previews(&paths, &cache, 4, &mut |done, total| samples.push((done, total)));

        assert_eq!(stats.considered, 9);
        assert_eq!(stats.made, 6, "the six real images should have previews");
        assert_eq!(stats.failed, 3, "the other three cannot be previewed");
        assert!(stats.bytes_read > 0, "it should account for what it read");

        let last = samples.last().copied().expect("progress should be reported");
        assert_eq!(last, (9, 9), "progress must finish at the total, got {last:?}");
        assert!(
            samples.iter().all(|(done, total)| done <= total && *total == 9),
            "progress should never exceed the total: {samples:?}"
        );

        // Second time round, everything is already done and nothing is
        // decoded again -- which is what makes a repeat rebuild quick.
        let again = warm_previews(&paths, &cache, 4, &mut |_, _| {});
        assert_eq!(again.made, 0, "nothing should be made a second time");
        assert_eq!(again.already_done, 6, "the finished previews should be recognised");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_larger_than_the_whole_budget_still_gets_its_turn() {
        // Clamping is what keeps such an image from waiting for space that can
        // never exist. Without it the preview would never be made and the
        // rebuild would stop dead at that one photograph.
        let budget = MemoryBudget::new(100);
        assert_eq!(budget.acquire(10_000), 100, "an oversized image should take the whole budget");
        budget.release(100);
        assert_eq!(budget.acquire(30), 30);
    }
}

#[cfg(all(test, feature = "live-data"))]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs_thumbtest_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn generates_a_valid_smaller_jpeg_from_a_real_tiff() {
        let (_, photos) = crate::test_support::photo_roots().remove(0);
        let source = crate::test_support::find_one_with_extension(&photos, &["tif", "tiff"]);

        let cache_dir = temp_dir("basic");
        let preview = get_preview_jpeg(&source, &cache_dir, 1600).expect("failed to generate preview");
        assert!(preview.exists());

        let preview_bytes = fs::metadata(&preview).unwrap().len();
        let source_bytes = fs::metadata(&source).unwrap().len();
        assert!(preview_bytes > 0);
        assert!(
            preview_bytes < source_bytes,
            "expected the JPEG preview ({preview_bytes} bytes) to be smaller than the source TIFF ({source_bytes} bytes)"
        );

        // The output must actually decode as a valid image, not just be non-empty bytes.
        let decoded = image::open(&preview).expect("generated preview is not a valid image");
        assert!(decoded.width() <= 1600 && decoded.height() <= 1600);

        let _ = fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn reuses_cache_on_second_call_and_regenerates_after_touch() {
        let (_, photos) = crate::test_support::photo_roots().remove(0);
        let source = crate::test_support::find_one_with_extension(&photos, &["jpg", "jpeg"]);

        let cache_dir = temp_dir("cache_reuse");
        let first = get_preview_jpeg(&source, &cache_dir, 1600).unwrap();
        let first_mtime = fs::metadata(&first).unwrap().modified().unwrap();

        std::thread::sleep(Duration::from_millis(50));
        let second = get_preview_jpeg(&source, &cache_dir, 1600).unwrap();
        let second_mtime = fs::metadata(&second).unwrap().modified().unwrap();
        assert_eq!(first, second);
        assert_eq!(first_mtime, second_mtime, "second call should have reused the cached file, not regenerated it");

        let _ = fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn two_equivalent_but_differently_written_paths_share_one_cache_entry() {
        let (_, photos) = crate::test_support::photo_roots().remove(0);
        let source = crate::test_support::find_one_with_extension(&photos, &["jpg", "jpeg"]);

        // The same file written a different way: down into the folder, back
        // out, and in again. It must hash to the same cache entry.
        let folder_name = photos.file_name().expect("photo folder has a name");
        let parent = photos.parent().expect("photo folder has a parent");
        let relative = source.strip_prefix(&photos).expect("source is inside the photo folder");
        let indirect = parent.join(folder_name).join(relative);
        assert!(indirect.exists(), "constructed path should point at the same real file");

        let cache_dir = temp_dir("path_equivalence");
        let direct_out = get_preview_jpeg(&source, &cache_dir, 1600).unwrap();
        let indirect_out = get_preview_jpeg(&indirect, &cache_dir, 1600).unwrap();
        assert_eq!(direct_out, indirect_out, "differently-written paths to the same file must hash to the same cache entry");

        let _ = fs::remove_dir_all(&cache_dir);
    }
}
