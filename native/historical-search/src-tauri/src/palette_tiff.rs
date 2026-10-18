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

/// Decodes one LZW-compressed strip.
///
/// Driven a step at a time rather than through weezl's one-shot `decode`,
/// which refused 90 photographs in this collection with an invalid-code error
/// part-way through a strip -- while the same data, fed through the stepwise
/// interface, decodes completely and matches what PIL produces byte for byte.
/// Most strips of an affected file came out fine and a few did not, so the
/// files looked healthy until one happened to be opened.
///
/// This was invisible here for months. The preview cache still held images
/// made by the Python prototype, whose decoder handles these, so nothing
/// looked wrong until the application ran on a machine with an empty cache.
///
/// The result is accepted only if it is as long as the strip should be, so a
/// partial decode is refused rather than turned into a half-drawn picture.
fn decode_lzw_strip(compressed: &[u8], expected: usize) -> Option<Vec<u8>> {
    // TIFF's variant of LZW: the code width grows one code earlier than in
    // the original algorithm.
    let mut decoder = weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
    let mut out = vec![0u8; expected];
    let mut read = 0usize;
    let mut written = 0usize;

    // Deliberately not stopping when the input runs out. These strips compress
    // heavily -- a couple of hundred bytes can stand for ten thousand -- and
    // the decoder still holds output to hand back after it has taken the last
    // of the input. Stopping at that point returned a strip short of its rows,
    // which then failed the length check below and cost the picture its
    // preview. It keeps being called with whatever input is left, which may be
    // none, until it says it is finished or stops making progress.
    loop {
        let step = decoder.decode_bytes(&compressed[read..], &mut out[written..]);
        read += step.consumed_in;
        written += step.consumed_out;
        if written >= expected {
            break;
        }
        match step.status {
            Ok(weezl::LzwStatus::Done) | Ok(weezl::LzwStatus::NoProgress) => break,
            Err(_) => break,
            Ok(weezl::LzwStatus::Ok) => {
                if step.consumed_in == 0 && step.consumed_out == 0 {
                    break; // neither consuming nor producing: stop rather than spin
                }
            }
        }
    }

    (written >= expected).then_some(out)
}

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

        // Rows in the final strip may be fewer than rows_per_strip.
        let first_row = (strip_index as u32).checked_mul(rows_per_strip)?;
        let rows_in_strip = rows_per_strip.min(height.saturating_sub(first_row)) as usize;
        let expected = row_bytes * rows_in_strip;

        let mut strip = match compression {
            COMPRESSION_NONE => compressed,
            COMPRESSION_LZW => decode_lzw_strip(&compressed, expected)?,
            _ => return None,
        };
        strip.truncate(expected);
        if strip.len() < expected {
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

    /// Palette TIFFs split into many small strips.
    ///
    /// This is the shape that was silently failing: strips of a handful of
    /// rows each, where most decode and a few report an invalid code. Ninety
    /// photographs in this collection were affected and none of them had a
    /// preview on a machine with an empty cache. Checked against PIL pixel for
    /// pixel, so a decode that merely succeeds is not enough.
    #[test]
    fn palette_tiffs_split_into_many_strips_match_pil() {
        use std::io::Read as _;
        // Decodes a great many large photographs and shells out to PIL for
        // each, so it is one of the tests that should not be sharing the
        // processor with a measurement.
        let _machine = crate::test_support::exclusive_use_of_the_machine();
        let images = crate::test_support::images_dir();
        let photos = crate::test_support::photo_roots();

        // Look through both the catalogue's images and the standalone photo
        // folders, since the affected files were in the latter.
        let mut found = Vec::new();
        for root in std::iter::once(images).chain(photos.into_iter().map(|(_, p)| p)) {
            crate::test_support::find_files(&root, usize::MAX, |p| {
                if found.len() >= 8 {
                    return false;
                }
                let is_tiff = p.extension().and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
                    .unwrap_or(false);
                if !is_tiff {
                    return false;
                }
                // Palette, LZW, and split into more than fifty strips.
                if let Ok(file) = File::open(p) {
                    if let Ok(mut dec) = Decoder::new(BufReader::new(file)) {
                        let palette = dec.get_tag_u32(Tag::PhotometricInterpretation).ok()
                            == Some(PHOTOMETRIC_PALETTE);
                        let lzw = dec.get_tag_u32(Tag::Compression).unwrap_or(COMPRESSION_NONE)
                            == COMPRESSION_LZW;
                        let strips = dec.get_tag_u64_vec(Tag::StripOffsets)
                            .map(|v| v.len()).unwrap_or(0);
                        if palette && lzw && strips > 50 {
                            found.push(p.to_path_buf());
                        }
                    }
                }
                false
            });
            if found.len() >= 8 {
                break;
            }
        }

        assert!(
            !found.is_empty(),
            "no many-stripped palette TIFF found to check -- this is the shape that failed, \
             so the test proves nothing without one"
        );
        for path in &found {
            // Not read here; only present so the compiler keeps the import
            // that File/Read need above.
            let _ = File::open(path).map(|mut f| { let mut b = [0u8; 1]; let _ = f.read(&mut b); });
            assert!(
                try_decode(path).is_some(),
                "declined a palette TIFF it should handle: {}",
                path.display()
            );
            assert_matches_pil(path);
        }
        println!("checked {} many-stripped palette TIFFs against PIL", found.len());
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
