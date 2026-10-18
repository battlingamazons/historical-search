//! Pulling the page pictures out of a scanned PDF, so they can be read by
//! text recognition.
//!
//! A scanned PDF is, page by page, one picture with nothing else on the page
//! -- so there is nothing to draw: the picture is already in the file, usually
//! as the very JPEG the scanner made. Taking it out whole is faster than
//! drawing the page again, and it keeps the scanner's full resolution.
//!
//! What is understood: JPEG pictures (taken out untouched, which is nearly all
//! of what scanners and libraries produce), uncompressed or Flate-compressed
//! greyscale and colour pictures, and Group 4 fax-compressed black-and-white
//! pictures. What is not, and is reported page by page rather than skipped in
//! silence: JBIG2 and JPEG 2000 pictures, and the rarer colour arrangements.
//!
//! Like reading a PDF's text, this runs in a separate process (see `pdf.rs`):
//! it loads the whole file, and a damaged one can do anything.

use lopdf::{Dictionary, Document, Object};
use std::path::{Path, PathBuf};

pub struct PageImage {
    pub page: u32,
    pub file: PathBuf,
    /// The picture's resolution, worked out from its size against the page's.
    /// Text recognition reads small print badly when it has to guess this.
    pub dpi: Option<u32>,
}

pub struct Extracted {
    pub total_pages: usize,
    pub images: Vec<PageImage>,
    /// Pages with no picture that could be taken out, and why.
    pub skipped: Vec<(u32, String)>,
}

/// Takes the main picture off every page of `pdf` and writes each into `out`.
pub fn extract_here(pdf: &Path, out: &Path) -> Result<Extracted, String> {
    let doc = Document::load(pdf).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let pages = doc.get_pages();
    let mut result = Extracted { total_pages: pages.len(), images: Vec::new(), skipped: Vec::new() };

    for (&number, &page_id) in &pages {
        let Some((width_px, picture)) = main_picture(&doc, page_id) else {
            // A page with no picture is a blank or a real-text page; nothing
            // to recognise either way.
            continue;
        };
        match write_picture(&doc, picture, out, number) {
            Ok(file) => {
                let dpi = page_width_points(&doc, page_id)
                    .filter(|pt| *pt > 0.0)
                    .map(|pt| ((width_px as f64) / (pt / 72.0)).round() as u32)
                    .filter(|dpi| (70..=1200).contains(dpi));
                result.images.push(PageImage { page: number, file, dpi });
            }
            Err(reason) => result.skipped.push((number, reason)),
        }
    }
    Ok(result)
}

/// The largest picture on a page, with its width in pixels.
fn main_picture(doc: &Document, page_id: lopdf::ObjectId) -> Option<(i64, &lopdf::Stream)> {
    let (direct, referenced) = doc.get_page_resources(page_id).ok()?;
    let mut resource_dicts: Vec<&Dictionary> = direct.into_iter().collect();
    resource_dicts.extend(referenced.iter().filter_map(|id| doc.get_dictionary(*id).ok()));

    let mut best: Option<(i64, i64, &lopdf::Stream)> = None;
    for resources in resource_dicts {
        let Some(xobjects) = resources.get(b"XObject").ok().and_then(|o| dict_of(doc, o)) else {
            continue;
        };
        for (_, value) in xobjects.iter() {
            let Ok((_, object)) = doc.dereference(value) else { continue };
            let Ok(stream) = object.as_stream() else { continue };
            if stream.dict.get(b"Subtype").ok().and_then(|o| o.as_name().ok()) != Some(b"Image") {
                continue;
            }
            let (Some(w), Some(h)) = (int_of(doc, &stream.dict, b"Width"), int_of(doc, &stream.dict, b"Height"))
            else {
                continue;
            };
            if best.map(|(bw, bh, _)| w * h > bw * bh).unwrap_or(true) {
                best = Some((w, h, stream));
            }
        }
    }
    best.map(|(w, _, s)| (w, s))
}

fn dict_of<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    doc.dereference(object).ok().and_then(|(_, o)| o.as_dict().ok())
}

fn int_of(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<i64> {
    let value = dict.get(key).ok()?;
    let (_, value) = doc.dereference(value).ok()?;
    match value {
        Object::Integer(i) => Some(*i),
        Object::Real(r) => Some(*r as i64),
        _ => None,
    }
}

fn number_of(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

/// The page's width in points, looking up through its parents because a page
/// need not carry its own MediaBox.
fn page_width_points(doc: &Document, page_id: lopdf::ObjectId) -> Option<f64> {
    let mut id = page_id;
    for _ in 0..32 {
        let dict = doc.get_dictionary(id).ok()?;
        if let Ok(Object::Array(b)) = dict.get(b"MediaBox").and_then(|o| doc.dereference(o).map(|(_, o)| o)) {
            let n: Vec<f64> = b.iter().filter_map(number_of).collect();
            if n.len() == 4 {
                return Some((n[2] - n[0]).abs());
            }
        }
        id = dict.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

/// The names of a stream's filters, in order.
fn filter_names(doc: &Document, dict: &Dictionary) -> Vec<Vec<u8>> {
    let Ok(filter) = dict.get(b"Filter") else { return Vec::new() };
    match doc.dereference(filter).map(|(_, o)| o) {
        Ok(Object::Name(n)) => vec![n.clone()],
        Ok(Object::Array(items)) => items
            .iter()
            .filter_map(|o| doc.dereference(o).ok().and_then(|(_, o)| o.as_name().ok().map(|n| n.to_vec())))
            .collect(),
        _ => Vec::new(),
    }
}

/// The decode parameters belonging to the filter at `index`.
fn decode_parms<'a>(doc: &'a Document, dict: &'a Dictionary, index: usize) -> Option<&'a Dictionary> {
    let parms = dict.get(b"DecodeParms").or_else(|_| dict.get(b"DP")).ok()?;
    match doc.dereference(parms).map(|(_, o)| o) {
        Ok(Object::Dictionary(d)) => Some(d),
        Ok(Object::Array(items)) => items.get(index).and_then(|o| dict_of(doc, o)),
        _ => None,
    }
}

fn write_picture(doc: &Document, stream: &lopdf::Stream, out: &Path, page: u32) -> Result<PathBuf, String> {
    let dict = &stream.dict;
    let filters = filter_names(doc, dict);
    let names: Vec<&[u8]> = filters.iter().map(|f| f.as_slice()).collect();
    let (width, height) = (
        int_of(doc, dict, b"Width").unwrap_or(0).max(0) as u32,
        int_of(doc, dict, b"Height").unwrap_or(0).max(0) as u32,
    );
    if width == 0 || height == 0 {
        return Err("the picture has no size".into());
    }

    match names.as_slice() {
        // The scanner's own JPEG, untouched.
        [b"DCTDecode"] | [b"DCT"] => {
            let path = out.join(format!("p{page:05}.jpg"));
            std::fs::write(&path, &stream.content).map_err(|e| e.to_string())?;
            Ok(path)
        }
        [b"CCITTFaxDecode"] | [b"CCF"] => {
            let path = out.join(format!("p{page:05}.tif"));
            let parms = decode_parms(doc, dict, 0);
            let tiff = group4_tiff(doc, parms, width, height, &stream.content)?;
            std::fs::write(&path, tiff).map_err(|e| e.to_string())?;
            Ok(path)
        }
        [b"JBIG2Decode"] => Err("the page picture is JBIG2-compressed, which is not supported yet".into()),
        [b"JPXDecode"] => Err("the page picture is JPEG 2000-compressed, which is not supported yet".into()),
        [] | [b"FlateDecode"] | [b"Fl"] => {
            let parms = if names.is_empty() { None } else { decode_parms(doc, dict, 0) };
            let raw = if names.is_empty() {
                stream.content.clone()
            } else {
                stream.decompressed_content().map_err(|e| format!("could not decompress the picture: {e}"))?
            };
            let path = out.join(format!("p{page:05}.png"));
            write_raw_png(doc, dict, parms, raw, width, height, &path)?;
            Ok(path)
        }
        other => Err(format!(
            "the page picture uses a compression this app does not read ({})",
            other.iter().map(|n| String::from_utf8_lossy(n).to_string()).collect::<Vec<_>>().join(" + ")
        )),
    }
}

/// Wraps Group 4 fax data in the smallest TIFF that can hold it. The text
/// recognition library reads TIFF directly, and the compressed data goes in
/// as it is, so nothing here has to decode it.
fn group4_tiff(
    doc: &Document,
    parms: Option<&Dictionary>,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    let get_int = |key: &[u8]| parms.and_then(|p| int_of(doc, p, key));
    let k = get_int(b"K").unwrap_or(0);
    if k >= 0 {
        return Err("the page picture uses one-dimensional fax compression, which is not supported".into());
    }
    let truthy = |key: &[u8]| {
        parms
            .and_then(|p| p.get(key).ok())
            .and_then(|o| doc.dereference(o).ok())
            .map(|(_, o)| matches!(o, Object::Boolean(true)))
            .unwrap_or(false)
    };
    if truthy(b"EncodedByteAlign") {
        return Err("the page picture's fax rows are byte-aligned, which is not supported".into());
    }
    let columns = get_int(b"Columns").unwrap_or(1728) as u32;
    let rows = get_int(b"Rows").map(|r| r as u32).filter(|r| *r > 0).unwrap_or(height);
    // In a PDF, 0 bits are black unless BlackIs1; in a TIFF that is "BlackIsZero".
    let photometric: u16 = if truthy(b"BlackIs1") { 0 } else { 1 };
    let _ = width; // the data's own column count governs

    let entries = 10u16;
    let ifd_len = 2 + entries as usize * 12 + 4;
    let data_offset = 8 + ifd_len;
    let mut out = Vec::with_capacity(data_offset + data.len());
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&entries.to_le_bytes());
    let mut entry = |tag: u16, kind: u16, value: u32| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        // A SHORT sits in the low half of the four value bytes.
        out.extend_from_slice(&value.to_le_bytes());
    };
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    entry(256, LONG, columns); // ImageWidth
    entry(257, LONG, rows); // ImageLength
    entry(258, SHORT, 1); // BitsPerSample
    entry(259, SHORT, 4); // Compression: CCITT Group 4
    entry(262, SHORT, photometric as u32);
    entry(273, LONG, data_offset as u32); // StripOffsets
    entry(277, SHORT, 1); // SamplesPerPixel
    entry(278, LONG, rows); // RowsPerStrip
    entry(279, LONG, data.len() as u32); // StripByteCounts
    entry(293, LONG, 0); // T6Options
    out.extend_from_slice(&0u32.to_le_bytes()); // no further IFD
    out.extend_from_slice(data);
    Ok(out)
}

/// Writes uncompressed pixel data as a PNG. Handles 8-bit and 1-bit grey and
/// 8-bit colour, with or without the PNG-style row prediction scanners use.
fn write_raw_png(
    doc: &Document,
    dict: &Dictionary,
    parms: Option<&Dictionary>,
    raw: Vec<u8>,
    width: u32,
    height: u32,
    path: &Path,
) -> Result<(), String> {
    let bpc = int_of(doc, dict, b"BitsPerComponent").unwrap_or(1);
    let is_mask = dict
        .get(b"ImageMask")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .map(|(_, o)| matches!(o, Object::Boolean(true)))
        .unwrap_or(false);
    let components = if is_mask { 1 } else { component_count(doc, dict)? };
    let bpc = if is_mask { 1 } else { bpc };
    if !matches!((components, bpc), (1, 1) | (1, 8) | (3, 8)) {
        return Err(format!(
            "the page picture is {components}-component, {bpc}-bit, which is not supported yet"
        ));
    }

    let row_bytes = (width as usize * components * bpc as usize).div_ceil(8);
    let predictor = parms.and_then(|p| int_of(doc, p, b"Predictor")).unwrap_or(1);
    let pixels = match predictor {
        1 => raw,
        10..=15 => undo_png_prediction(&raw, row_bytes, (components * bpc as usize).div_ceil(8))?,
        other => return Err(format!("the page picture uses row prediction {other}, which is not supported")),
    };
    if pixels.len() < row_bytes * height as usize {
        return Err("the page picture's data is shorter than its size says".into());
    }

    // A /Decode of [1 0] means the sample values are reversed.
    let inverted = dict
        .get(b"Decode")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_array().ok())
        .map(|a| a.first().and_then(number_of) == Some(1.0))
        .unwrap_or(false);

    let image = match (components, bpc) {
        (1, 1) => {
            let mut grey = Vec::with_capacity((width * height) as usize);
            for y in 0..height as usize {
                let row = &pixels[y * row_bytes..(y + 1) * row_bytes];
                for x in 0..width as usize {
                    let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                    let white = (bit == 1) != inverted;
                    grey.push(if white { 255 } else { 0 });
                }
            }
            image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(width, height, grey).ok_or("bad size")?)
        }
        (1, _) => {
            let mut grey = pixels[..(width * height) as usize].to_vec();
            if inverted {
                grey.iter_mut().for_each(|v| *v = 255 - *v);
            }
            image::DynamicImage::ImageLuma8(image::GrayImage::from_raw(width, height, grey).ok_or("bad size")?)
        }
        _ => {
            let mut rgb = pixels[..(width * height * 3) as usize].to_vec();
            if inverted {
                rgb.iter_mut().for_each(|v| *v = 255 - *v);
            }
            image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(width, height, rgb).ok_or("bad size")?)
        }
    };
    image.save_with_format(path, image::ImageFormat::Png).map_err(|e| e.to_string())
}

/// How many values make up one pixel, from the picture's colour space.
fn component_count(doc: &Document, dict: &Dictionary) -> Result<usize, String> {
    let Ok(space) = dict.get(b"ColorSpace") else { return Ok(1) };
    let (_, space) = doc.dereference(space).map_err(|e| e.to_string())?;
    let describe = |name: &[u8]| -> Result<usize, String> {
        match name {
            b"DeviceGray" | b"G" | b"CalGray" => Ok(1),
            b"DeviceRGB" | b"RGB" | b"CalRGB" => Ok(3),
            other => Err(format!(
                "the page picture's colour space ({}) is not supported yet",
                String::from_utf8_lossy(other)
            )),
        }
    };
    match space {
        Object::Name(n) => describe(n),
        Object::Array(items) => match items.first().and_then(|o| o.as_name().ok()) {
            Some(b"ICCBased") => {
                let profile = items.get(1).and_then(|o| doc.dereference(o).ok()).and_then(|(_, o)| o.as_stream().ok());
                match profile.and_then(|s| int_of(doc, &s.dict, b"N")) {
                    Some(1) => Ok(1),
                    Some(3) => Ok(3),
                    other => Err(format!("the page picture's colour profile has {other:?} components, not supported")),
                }
            }
            Some(name) => describe(name),
            None => Err("the page picture's colour space could not be read".into()),
        },
        _ => Err("the page picture's colour space could not be read".into()),
    }
}

/// Reverses the PNG-style row filters PDF applies to compressed pictures.
fn undo_png_prediction(data: &[u8], row_bytes: usize, bpp: usize) -> Result<Vec<u8>, String> {
    let stride = row_bytes + 1;
    if row_bytes == 0 || data.len() < stride {
        return Err("the picture's row-filtered data is too short".into());
    }
    let rows = data.len() / stride;
    let mut out = vec![0u8; rows * row_bytes];
    for y in 0..rows {
        let kind = data[y * stride];
        let src = &data[y * stride + 1..(y + 1) * stride];
        for x in 0..row_bytes {
            let left = if x >= bpp { out[y * row_bytes + x - bpp] } else { 0 };
            let up = if y > 0 { out[(y - 1) * row_bytes + x] } else { 0 };
            let up_left = if y > 0 && x >= bpp { out[(y - 1) * row_bytes + x - bpp] } else { 0 };
            let value = match kind {
                0 => src[x],
                1 => src[x].wrapping_add(left),
                2 => src[x].wrapping_add(up),
                3 => src[x].wrapping_add(((left as u16 + up as u16) / 2) as u8),
                4 => src[x].wrapping_add(paeth(left, up, up_left)),
                other => return Err(format!("the picture uses an unknown row filter ({other})")),
            };
            out[y * row_bytes + x] = value;
        }
    }
    Ok(out)
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a_i, b_i, c_i) = (a as i32, b as i32, c as i32);
    let p = a_i + b_i - c_i;
    let (pa, pb, pc) = ((p - a_i).abs(), (p - b_i).abs(), (p - c_i).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::fixture::make_image_pdf;

    fn extract(pdf: Vec<u8>, name: &str) -> (Extracted, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hs_scan_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("in.pdf");
        std::fs::write(&file, pdf).unwrap();
        (extract_here(&file, &dir.join("out")).unwrap(), dir)
    }

    #[test]
    fn a_jpeg_page_is_taken_out_untouched() {
        let jpeg = b"\xff\xd8\xff\xe0 not really a jpeg but opaque to the extractor \xff\xd9";
        let (got, _) = extract(make_image_pdf(1700, 2200, "/Filter /DCTDecode /ColorSpace /DeviceRGB /BitsPerComponent 8", jpeg), "jpeg");
        assert_eq!(got.total_pages, 1);
        assert_eq!(got.images.len(), 1);
        assert_eq!(std::fs::read(&got.images[0].file).unwrap(), jpeg.to_vec());
        // 1700 pixels across a 612-point (8.5 inch) page is 200 dpi.
        assert_eq!(got.images[0].dpi, Some(200));
    }

    #[test]
    fn raw_grey_pixels_become_a_png() {
        let pixels = vec![200u8; 40 * 30];
        let (got, _) = extract(make_image_pdf(40, 30, "/ColorSpace /DeviceGray /BitsPerComponent 8", &pixels), "gray");
        let png = image::open(&got.images[0].file).unwrap();
        assert_eq!((png.width(), png.height()), (40, 30));
    }

    #[test]
    fn one_bit_pictures_are_expanded_and_can_be_reversed() {
        // 8 pixels across, 2 rows: row one all white bits, row two all black.
        let bits = [0xFFu8, 0x00];
        let (got, _) = extract(make_image_pdf(8, 2, "/ColorSpace /DeviceGray /BitsPerComponent 1", &bits), "bits");
        let png = image::open(&got.images[0].file).unwrap().to_luma8();
        assert_eq!((png.get_pixel(0, 0).0[0], png.get_pixel(0, 1).0[0]), (255, 0));

        let (got, _) = extract(make_image_pdf(8, 2, "/ColorSpace /DeviceGray /BitsPerComponent 1 /Decode [1 0]", &bits), "bitsinv");
        let png = image::open(&got.images[0].file).unwrap().to_luma8();
        assert_eq!((png.get_pixel(0, 0).0[0], png.get_pixel(0, 1).0[0]), (0, 255));
    }

    #[test]
    fn pictures_that_cannot_be_taken_out_are_reported_not_dropped() {
        for (entries, wanted) in [
            ("/Filter /JBIG2Decode /ColorSpace /DeviceGray /BitsPerComponent 1", "JBIG2"),
            ("/Filter /JPXDecode", "JPEG 2000"),
            ("/ColorSpace /DeviceCMYK /BitsPerComponent 8", "DeviceCMYK"),
        ] {
            let (got, _) = extract(make_image_pdf(10, 10, entries, &[0u8; 400]), "unsupported");
            assert!(got.images.is_empty());
            assert_eq!(got.skipped.len(), 1, "{entries}");
            assert!(got.skipped[0].1.contains(wanted), "{entries}: {:?}", got.skipped);
        }
    }

    #[test]
    fn a_pdf_with_only_text_has_no_pictures_to_take_out() {
        let (got, _) = extract(crate::pdf::fixture::make_pdf(&["Just text on this page"]), "text");
        assert_eq!((got.images.len(), got.skipped.len()), (0, 0));
    }

    #[test]
    fn png_row_filters_are_undone() {
        // Two rows of three grey pixels: the first stored raw, the second as
        // "up" differences from it.
        let data = [0, 10, 20, 30, 2, 1, 1, 1];
        assert_eq!(undo_png_prediction(&data, 3, 1).unwrap(), vec![10, 20, 30, 11, 21, 31]);
        // "Sub": each byte is stored as the difference from the one to its left.
        let sub = [1, 5, 1, 1];
        assert_eq!(undo_png_prediction(&sub, 3, 1).unwrap(), vec![5, 6, 7]);
    }

    #[test]
    fn the_paeth_predictor_picks_the_nearest() {
        assert_eq!(paeth(10, 20, 10), 20);
        assert_eq!(paeth(10, 10, 10), 10);
        assert_eq!(paeth(0, 0, 0), 0);
    }
}
