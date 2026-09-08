//! Decoder for palette (indexed-colour) TIFF files.
//!
//! Why this exists: the `image` crate (via `tiff` 0.11) explicitly does not
//! implement `PhotometricInterpretation::RGBPalette` -- it returns an
//! "unsupported" error before any pixel data is reachable, even through the
//! low-level API. That is not an obscure corner for this collection: about 7% of the real
//! TIFFs here are palette TIFFs, mostly older scans, and
//! Python/PIL reads them fine. Dropping them would be a visible regression
//! against the prototype, so we decode them ourselves.
//!
//! Scope is deliberately narrow -- exactly the shapes that actually occur in
//! this collection, verified by scanning every TIFF in it:
//!   * photometric = 3 (palette), 8 bits per sample, 1 sample per pixel
//!   * planar configuration 1 (chunky), strip-based (never tiled)
//!   * compression 1 (none) or 5 (LZW), predictor 1 (none) or 2 (horizontal)
//!
//! Anything else is declined (returns None) rather than guessed at.

use image::{DynamicImage, RgbImage};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use tiff::decoder::Decoder;
use tiff::tags::Tag;

const COMPRESSION_NONE: u32 = 1;
const COMPRESSION_LZW: u32 = 5;
const PREDICTOR_NONE: u32 = 1;
const PREDICTOR_HORIZONTAL: u32 = 2;
const PHOTOMETRIC_PALETTE: u32 = 3;

/// Attempts to decode `path` as a palette TIFF. Returns None if the file
/// isn't one, or isn't one of the narrow shapes described above.
pub fn try_decode(path: &Path) -> Option<DynamicImage> {
    let file = File::open(path).ok()?;
    let mut dec = Decoder::new(BufReader::new(file)).ok()?;

    // Tag reading works fine in the tiff crate; it's only pixel decoding
    // that refuses palette images, so we can still learn the layout.
    if dec.get_tag_u32(Tag::PhotometricInterpretation).ok()? != PHOTOMETRIC_PALETTE {
        return None;
    }
    if dec.get_tag_u32(Tag::SamplesPerPixel).unwrap_or(1) != 1 {
        return None;
    }
    let bits = dec.get_tag_u32_vec(Tag::BitsPerSample).ok()?;
    if bits != vec![8] {
        return None;
    }
    if dec.get_tag_u32(Tag::PlanarConfiguration).unwrap_or(1) != 1 {
        return None;
    }
    if dec.get_tag_u32(Tag::TileWidth).is_ok() {
        return None; // tiled, not stripped -- doesn't occur here
    }

    let width = dec.get_tag_u32(Tag::ImageWidth).ok()?;
    let height = dec.get_tag_u32(Tag::ImageLength).ok()?;
    // Dimensions come straight from the file, and we allocate based on them,
    // so refuse absurd values rather than trying to allocate gigabytes for a
    // corrupt header. (The largest genuine scan in this collection is ~99
    // megapixels; 512 leaves generous headroom.)
    const MAX_PIXELS: u64 = 512 * 1024 * 1024;
    if width == 0 || height == 0 || (width as u64) * (height as u64) > MAX_PIXELS {
        return None;
    }
    let compression = dec.get_tag_u32(Tag::Compression).unwrap_or(COMPRESSION_NONE);
    let predictor = dec.get_tag_u32(Tag::Predictor).unwrap_or(PREDICTOR_NONE);
    if !matches!(compression, COMPRESSION_NONE | COMPRESSION_LZW) {
        return None;
    }
    if !matches!(predictor, PREDICTOR_NONE | PREDICTOR_HORIZONTAL) {
        return None;
    }

    // TIFF ColorMap: 3 * 2^bits u16 entries, laid out as all reds, then all
    // greens, then all blues, each scaled to 0..=65535.
    let colormap = dec.get_tag_u16_vec(Tag::ColorMap).ok()?;
    if colormap.len() != 3 * 256 {
        return None;
    }

    let strip_offsets = dec.get_tag_u64_vec(Tag::StripOffsets).ok()?;
    let strip_byte_counts = dec.get_tag_u64_vec(Tag::StripByteCounts).ok()?;
    if strip_offsets.len() != strip_byte_counts.len() || strip_offsets.is_empty() {
        return None;
    }
    let rows_per_strip = dec.get_tag_u32(Tag::RowsPerStrip).unwrap_or(height).max(1);
    // Every strip holds at least one row, so a file cannot honestly declare
    // more strips than it has rows. Without this the row number below --
    // strip number times rows per strip, both taken from the file -- can be
    // made to overflow, which wraps silently in a release build and produces
    // nonsense rather than a refusal.
    if strip_offsets.len() as u64 > height as u64 {
        return None;
    }

    // Re-open for raw reads; the decoder owns its own reader position.
    let mut raw = File::open(path).ok()?;
    let file_len = raw.metadata().ok()?.len();
    let row_bytes = width as usize; // 8 bits, 1 sample per pixel
    let mut indices: Vec<u8> = Vec::with_capacity(row_bytes * height as usize);

    for (strip_index, (&offset, &byte_count)) in
        strip_offsets.iter().zip(strip_byte_counts.iter()).enumerate()
    {
        // Strip offsets/sizes are also file-supplied; a corrupt one could ask
        // us to allocate far more than the file could possibly contain.
        if byte_count > file_len || offset.saturating_add(byte_count) > file_len {
            return None;
        }
        let mut compressed = vec![0u8; byte_count as usize];
        raw.seek(SeekFrom::Start(offset)).ok()?;
        raw.read_exact(&mut compressed).ok()?;

        let mut strip = match compression {
            COMPRESSION_NONE => compressed,
            COMPRESSION_LZW => weezl::decode::Decoder::with_tiff_size_switch(
                weezl::BitOrder::Msb,
                8,
            )
            .decode(&compressed)
            .ok()?,
            _ => return None,
        };

        // Rows in the final strip may be fewer than rows_per_strip.
        let first_row = (strip_index as u32).checked_mul(rows_per_strip)?;
        let rows_in_strip = rows_per_strip.min(height.saturating_sub(first_row)) as usize;
        strip.truncate(row_bytes * rows_in_strip);
        if strip.len() < row_bytes * rows_in_strip {
            return None; // short strip -- refuse rather than emit garbage
        }

        if predictor == PREDICTOR_HORIZONTAL {
            // Horizontal differencing is applied per row, over the palette
            // index bytes themselves (1 sample per pixel).
            for row in strip.chunks_mut(row_bytes) {
                for i in 1..row.len() {
                    row[i] = row[i].wrapping_add(row[i - 1]);
                }
            }
        }

        indices.extend_from_slice(&strip);
    }

    if indices.len() < row_bytes * height as usize {
        return None;
    }

    let mut rgb = RgbImage::new(width, height);
    for (i, pixel) in rgb.pixels_mut().enumerate() {
        let index = indices[i] as usize;
        // u16 (0..=65535) -> u8: take the high byte, which is what PIL and
        // libtiff effectively do for 8-bit output.
        let r = (colormap[index] >> 8) as u8;
        let g = (colormap[256 + index] >> 8) as u8;
        let b = (colormap[512 + index] >> 8) as u8;
        *pixel = image::Rgb([r, g, b]);
    }

    Some(DynamicImage::ImageRgb8(rgb))
}

#[cfg(all(test, feature = "live-data"))]
mod tests {
    use super::*;
    use base64::Engine;
    use std::path::PathBuf;
    use std::process::Command;

    /// Decodes with PIL (the prototype's reference implementation) and
    /// returns (width, height, raw RGB bytes).
    fn decode_with_pil(path: &Path) -> (u32, u32, Vec<u8>) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/dump_image_rgb.py");
        let output = Command::new("python3")
            .arg(&script)
            .arg(path)
            .output()
            .expect("failed to run PIL reference decode");
        assert!(
            output.status.success(),
            "PIL reference decode failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&output.stdout).expect("bad JSON from PIL helper");
        let width = v["width"].as_u64().unwrap() as u32;
        let height = v["height"].as_u64().unwrap() as u32;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(v["rgb_b64"].as_str().unwrap())
            .unwrap();
        (width, height, bytes)
    }

    fn assert_matches_pil(path: &Path) {
        let ours = try_decode(path)
            .unwrap_or_else(|| panic!("our decoder declined {}", path.display()));
        let ours = ours.to_rgb8();
        let (w, h, pil_bytes) = decode_with_pil(path);

        assert_eq!((ours.width(), ours.height()), (w, h), "{}: dimensions differ", path.display());
        assert_eq!(
            ours.as_raw().len(),
            pil_bytes.len(),
            "{}: RGB buffer length differs",
            path.display()
        );

        // Report *how* different, so a garbled decode (predictor/LZW bug) is
        // distinguishable from a benign off-by-one in colour scaling.
        let mut max_diff = 0u8;
        let mut differing = 0usize;
        for (a, b) in ours.as_raw().iter().zip(pil_bytes.iter()) {
            let d = a.abs_diff(*b);
            if d > 0 {
                differing += 1;
                max_diff = max_diff.max(d);
            }
        }
        assert_eq!(
            differing,
            0,
            "{}: {} of {} bytes differ from PIL (max difference {})",
            path.display(),
            differing,
            pil_bytes.len(),
            max_diff
        );
    }

    /// How a palette TIFF is stored, or None if this file isn't one this
    /// decoder handles. Lets the tests go and find a real example of each
    /// shape rather than naming particular files, which belong to one
    /// collection on one machine.
    fn palette_shape(path: &Path) -> Option<(u32, u32)> {
        let file = File::open(path).ok()?;
        let mut dec = Decoder::new(BufReader::new(file)).ok()?;
        if dec.get_tag_u32(Tag::PhotometricInterpretation).ok()? != PHOTOMETRIC_PALETTE {
            return None;
        }
        let compression = dec.get_tag_u32(Tag::Compression).unwrap_or(COMPRESSION_NONE);
        let predictor = dec.get_tag_u32(Tag::Predictor).unwrap_or(PREDICTOR_NONE);
        Some((compression, predictor))
    }

    /// Searches the collection for a palette TIFF stored the given way.
    fn find_palette_tiff(compression: u32) -> Option<PathBuf> {
        let images = crate::test_support::images_dir();
        // Palette TIFFs are a minority of the files here, so this looks at a
        // good number before giving up.
        let mut found = None;
        crate::test_support::find_files(&images, usize::MAX, |p| {
            if found.is_some() {
                return false;
            }
            let is_tiff = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
                .unwrap_or(false);
            if is_tiff {
                if let Some((c, _)) = palette_shape(p) {
                    if c == compression {
                        found = Some(p.to_path_buf());
                    }
                }
            }
            false
        });
        found
    }

    /// Corrupt and truncated images must be declined, never crash.
    ///
    /// Everything this decoder reads -- dimensions, strip positions, how many
    /// rows each strip holds -- comes out of the file itself, and the release
    /// build stops the whole application on a panic. A scanner writing a file
    /// when a drive fills, or a partly-copied folder, is enough to produce
    /// one of these.
    #[test]
    fn damaged_images_are_declined_rather_than_crashing() {
        let source = find_palette_tiff(COMPRESSION_LZW)
            .or_else(|| find_palette_tiff(COMPRESSION_NONE))
            .expect("no palette TIFF found to damage");
        let whole = std::fs::read(&source).expect("could not read it");
        let dir = std::env::temp_dir().join(format!("hs_tiff_damage_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scratch = dir.join("damaged.tif");

        let mut declined = 0;
        let mut decoded = 0;

        // Cut short at a spread of lengths, including inside the header,
        // inside the tag table, and part-way through the pixels.
        for cut in [0, 1, 4, 8, 16, 33, 64, 100, 511, 1000, 4096, whole.len() / 3,
                    whole.len() / 2, whole.len() - 1] {
            let cut = cut.min(whole.len());
            std::fs::write(&scratch, &whole[..cut]).unwrap();
            match try_decode(&scratch) {
                Some(_) => decoded += 1,
                None => declined += 1,
            }
        }

        // Flip bytes at scattered positions, which mostly lands in the header
        // and tag table where the numbers this decoder trusts are stored.
        for position in (0..whole.len().min(4096)).step_by(37) {
            let mut damaged = whole.clone();
            damaged[position] = damaged[position].wrapping_add(0x7f);
            std::fs::write(&scratch, &damaged).unwrap();
            match try_decode(&scratch) {
                Some(_) => decoded += 1,
                None => declined += 1,
            }
        }

        // Reaching here at all is the result: nothing panicked, so nothing
        // would have taken the application down with it.
        //
        // What this does not do is show that each individual bounds check is
        // load-bearing -- removing the one on strip sizes still leaves this
        // passing, because damage placed at regular intervals rarely lands on
        // the few bytes holding those numbers. It is a smoke test over
        // realistic damage, not a demonstration that every guard is needed.
        assert!(declined + decoded > 100, "not enough variations were tried");
        println!("damaged images: {declined} declined, {decoded} still decoded, none crashed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uncompressed_palette_tiff_matches_pil() {
        let path = find_palette_tiff(COMPRESSION_NONE)
            .expect("no uncompressed palette TIFF found in the collection to test against");
        assert_matches_pil(&path);
    }

    #[test]
    fn lzw_palette_tiff_matches_pil() {
        let path = find_palette_tiff(COMPRESSION_LZW)
            .expect("no LZW-compressed palette TIFF found in the collection to test against");
        assert_matches_pil(&path);
    }
}
