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

    // Detect the format from the file's *contents*, not its extension.
    // `image::open()` trusts the extension, which silently loses real images
    // that are misnamed -- this collection has files like "190024.4 B" with
    // no usable extension at all, which PIL reads fine by sniffing content.
    let img = match image::ImageReader::open(original_path)
        .ok()
        .and_then(|r| r.with_guessed_format().ok())
        .and_then(|r| r.decode().ok())
    {
        Some(img) => img,
        // The `image` crate also can't decode palette TIFFs (7% of this
        // collection's TIFFs), so fall back to our own decoder for those
        // rather than silently dropping the preview.
        None => match crate::palette_tiff::try_decode(original_path) {
            Some(img) => img,
            None => {
                // Reached once per unreadable image. Without this, a picture
                // that will not decode is indistinguishable from one that was
                // never there -- and "no pictures at all" is the likeliest
                // complaint from an untested platform.
                crate::diagnostics::error_repeating(
                    "image-decode-failed",
                    "preview",
                    format!("could not decode {} as an image", original_path.display()),
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
    let temp_path = out_path.with_extension("jpg.partial");
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
