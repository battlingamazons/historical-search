//! Other kinds of file a volunteer might drop on the checker: a scanned PDF, a
//! camera's RAW file, a phone's HEIC picture. None of these are in the
//! collection's photo folders, so this is used only to read the scan itself,
//! and only after the ordinary picture readers have failed.
//!
//! Nothing here draws a page or develops a RAW file. A scanned PDF is a picture
//! wrapped in a page, so the picture is taken out; a RAW file carries a full
//! size JPEG for the camera's own screen, so that is taken out. Both are what
//! the matching needs and far cheaper than rendering. HEIC needs a decoder the
//! application does not carry, so it is read through the system's own where
//! there is one (macOS).

use image::DynamicImage;
use std::path::Path;

/// Files bigger than this are not opened here: they are read whole.
const MAX_FILE: u64 = 400 * 1024 * 1024;

const RAW: &[&str] = &[
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "pef", "srw", "3fr", "erf",
    "kdc", "mrw", "x3f", "iiq",
];
const HEIF: &[&str] = &["heic", "heif", "hif"];
/// AV1 pictures: read through the system, as HEIC is, so no decoder is carried.
const AVIF: &[&str] = &["avif"];
const JPEG2000: &[&str] = &["jp2", "j2k", "j2c", "jpx"];

/// Every kind of file the checker reads, in the photo folders and as a dropped
/// scan, spelled the way a file name ends (lower case).
pub const PICTURE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "tif", "tiff", "bmp", "dib", "gif", "webp", "tga", "pnm", "ppm", "pgm", "pbm",
    "ico", "jp2", "j2k", "j2c", "jpx", "pcx", "psd", "avif", "heic", "heif", "hif", "pdf", "dng", "cr2", "cr3", "crw",
    "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "pef", "srw", "3fr", "erf", "kdc", "mrw", "x3f", "iiq",
];

/// The most pixels read from a picture here, so a damaged header cannot ask
/// for an enormous allocation.
const MAX_PIXELS: u64 = 200_000_000;

pub fn try_decode(path: &Path) -> Option<DynamicImage> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext == "pdf" {
        first_page_picture(path)
    } else if RAW.contains(&ext.as_str()) {
        largest_embedded_jpeg(&std::fs::read(path).ok()?)
    } else if HEIF.contains(&ext.as_str()) || AVIF.contains(&ext.as_str()) {
        via_system(path)
    } else if JPEG2000.contains(&ext.as_str()) {
        jpeg2000_picture(&std::fs::read(path).ok()?)
    } else if ext == "pcx" {
        pcx_picture(&std::fs::read(path).ok()?)
    } else if ext == "psd" {
        psd_picture(&std::fs::read(path).ok()?)
    } else if ext == "ico" {
        ico_picture(&std::fs::read(path).ok()?)
    } else {
        None
    }
}

/// The biggest picture on the first page of a PDF, when it is kept in a form
/// that can be read: JPEG, or plain deflated grey or colour.
fn first_page_picture(path: &Path) -> Option<DynamicImage> {
    let doc = lopdf::Document::load(path).ok()?;
    let page = *doc.get_pages().values().next()?;
    let mut best: Option<DynamicImage> = None;
    for pic in doc.get_page_images(page).ok()? {
        let filters = pic.filters.clone().unwrap_or_default();
        let img = match filters.as_slice() {
            [f] if f == "DCTDecode" => image::load_from_memory_with_format(pic.content, image::ImageFormat::Jpeg).ok(),
            [f] if f == "FlateDecode" => flate_picture(&pic),
            [] => raw_picture(pic.content, &pic),
            _ => None,
        };
        if let Some(img) = img {
            if best.as_ref().is_none_or(|b| img.width() * img.height() > b.width() * b.height()) {
                best = Some(img);
            }
        }
    }
    best
}

fn flate_picture(pic: &lopdf::xobject::PdfImage) -> Option<DynamicImage> {
    // A prediction step would need undoing first; not attempted.
    if pic.origin_dict.get(b"DecodeParms").is_ok() {
        return None;
    }
    let data = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(pic.content, 1 << 30).ok()?;
    raw_picture(&data, pic)
}

fn raw_picture(data: &[u8], pic: &lopdf::xobject::PdfImage) -> Option<DynamicImage> {
    if pic.bits_per_component != Some(8) {
        return None;
    }
    let (w, h) = (u32::try_from(pic.width).ok()?, u32::try_from(pic.height).ok()?);
    match pic.color_space.as_deref()? {
        "DeviceGray" => image::GrayImage::from_raw(w, h, data.get(..(w as usize * h as usize))?.to_vec())
            .map(DynamicImage::ImageLuma8),
        "DeviceRGB" => image::RgbImage::from_raw(w, h, data.get(..(w as usize * h as usize * 3))?.to_vec())
            .map(DynamicImage::ImageRgb8),
        _ => None,
    }
}

/// The largest JPEG that can be read from inside a file; RAW files keep one or
/// more (a tiny thumbnail, and usually a full size preview).
fn largest_embedded_jpeg(bytes: &[u8]) -> Option<DynamicImage> {
    let mut best: Option<DynamicImage> = None;
    let mut tried = 0;
    let mut at = 0;
    while let Some(off) = bytes[at..].windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) {
        let start = at + off;
        at = start + 3;
        if tried >= 12 {
            break;
        }
        tried += 1;
        if let Ok(img) = image::load_from_memory_with_format(&bytes[start..], image::ImageFormat::Jpeg) {
            if img.width().min(img.height()) >= 200
                && best.as_ref().is_none_or(|b| img.width() * img.height() > b.width() * b.height())
            {
                best = Some(img);
            }
        }
    }
    best
}

/// JPEG 2000 (.jp2, .j2k, .jpx): grey, colour, with or without transparency.
fn jpeg2000_picture(data: &[u8]) -> Option<DynamicImage> {
    use hayro_jpeg2000::{ColorSpace, DecodeSettings, DecoderContext, Image};
    let img = Image::new(data, &DecodeSettings::default()).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let cmyk = matches!(img.color_space(), ColorSpace::CMYK);
    let mut ctx = DecoderContext::default();
    let decoded = img.decode(&mut ctx).ok()?;
    let bytes = decoded.data_u8();
    let pixels = (w as usize) * (h as usize);
    if bytes.len() % pixels != 0 {
        return None;
    }
    match (bytes.len() / pixels, cmyk) {
        (1, _) => image::GrayImage::from_raw(w, h, bytes).map(DynamicImage::ImageLuma8),
        (2, _) => image::GrayAlphaImage::from_raw(w, h, bytes).map(DynamicImage::ImageLumaA8),
        (3, _) => image::RgbImage::from_raw(w, h, bytes).map(DynamicImage::ImageRgb8),
        (4, false) => image::RgbaImage::from_raw(w, h, bytes).map(DynamicImage::ImageRgba8),
        (4, true) => {
            let rgb: Vec<u8> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| {
                    let k = u32::from(p[3]);
                    [0, 1, 2].map(|i| (u32::from(p[i]) * (255 - k) / 255) as u8)
                })
                .collect();
            image::RgbImage::from_raw(w, h, rgb).map(DynamicImage::ImageRgb8)
        }
        _ => None,
    }
}

/// PCX, the old paint-program format: colour or palette pictures.
fn pcx_picture(data: &[u8]) -> Option<DynamicImage> {
    let mut reader = pcx::Reader::from_mem(data).ok()?;
    let (w, h) = (u32::from(reader.width()), u32::from(reader.height()));
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let mut rgb = vec![0u8; (w as usize) * (h as usize) * 3];
    reader.read_rgb_pixels(&mut rgb).ok()?;
    image::RgbImage::from_raw(w, h, rgb).map(DynamicImage::ImageRgb8)
}

/// Photoshop: the flattened picture Photoshop saves beside the layers.
///
/// Read here, not by a library, because every number in the file is checked
/// against what is actually there: a damaged file gives nothing rather than a
/// crash. Handles grey, colour, CMYK and palette pictures, 8 or 16 bits, plain
/// or run-length packed (what Photoshop writes by default).
fn psd_picture(data: &[u8]) -> Option<DynamicImage> {
    fn be16(d: &[u8], at: usize) -> Option<usize> {
        Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?) as usize)
    }
    fn be32(d: &[u8], at: usize) -> Option<usize> {
        Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?) as usize)
    }
    if data.get(0..4)? != b"8BPS" {
        return None;
    }
    let big = match be16(data, 4)? {
        1 => false,
        2 => true, // the "large document" form uses wider length fields
        _ => return None,
    };
    let channels = be16(data, 12)?;
    let (h, w) = (be32(data, 14)?, be32(data, 18)?);
    let depth = be16(data, 22)?;
    let mode = be16(data, 24)?;
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_PIXELS || !(1..=56).contains(&channels) {
        return None;
    }
    let wanted = match mode {
        1 | 2 => 1, // grey, palette
        3 => 3,
        4 => 4,
        _ => return None, // bitmap, Lab, multichannel and the like
    };
    if channels < wanted || !(depth == 8 || depth == 16) {
        return None;
    }
    let bytes_per = depth / 8;
    // Colour table, image resources, layers: skipped by their own lengths.
    let mut at = 26;
    let palette_len = be32(data, at)?;
    let palette = data.get(at + 4..at + 4 + palette_len)?;
    at += 4 + palette_len;
    let resources = be32(data, at)?;
    at = at.checked_add(4)?.checked_add(resources)?;
    let layers = if big {
        u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?) as usize
    } else {
        be32(data, at)?
    };
    at = at.checked_add(if big { 8 } else { 4 })?.checked_add(layers)?;
    let compression = be16(data, at)?;
    at += 2;

    // Rows of every channel we need, as 8-bit samples (the high byte of 16).
    let row_len = w.checked_mul(bytes_per)?;
    let mut planes: Vec<Vec<u8>> = Vec::new();
    match compression {
        0 => {
            let plane_len = row_len.checked_mul(h)?;
            for c in 0..wanted {
                let start = at.checked_add(plane_len.checked_mul(c)?)?;
                let raw = data.get(start..start.checked_add(plane_len)?)?;
                planes.push(raw.chunks(bytes_per).map(|s| s[0]).collect());
            }
        }
        1 => {
            let width_of_count = if big { 4 } else { 2 };
            let table = at;
            let mut pos = table.checked_add(channels.checked_mul(h)?.checked_mul(width_of_count)?)?;
            if pos > data.len() {
                return None;
            }
            for c in 0..channels {
                let mut plane = Vec::with_capacity(if c < wanted { w * h } else { 0 });
                for row in 0..h {
                    let slot = table + (c * h + row) * width_of_count;
                    let count = if big { be32(data, slot)? } else { be16(data, slot)? };
                    let packed = data.get(pos..pos.checked_add(count)?)?;
                    pos += count;
                    if c >= wanted {
                        continue; // alpha and spot channels are not needed
                    }
                    let mut line: Vec<u8> = Vec::with_capacity(row_len);
                    let mut i = 0;
                    while i < packed.len() && line.len() < row_len {
                        let n = packed[i] as i8;
                        i += 1;
                        if n >= 0 {
                            let run = n as usize + 1;
                            line.extend_from_slice(packed.get(i..i + run)?);
                            i += run;
                        } else if n != -128 {
                            let run = 1 - n as isize;
                            line.extend(std::iter::repeat_n(*packed.get(i)?, run as usize));
                            i += 1;
                        }
                    }
                    if line.len() < row_len {
                        return None;
                    }
                    line.truncate(row_len);
                    plane.extend(line.chunks(bytes_per).map(|s| s[0]));
                }
                if c < wanted {
                    planes.push(plane);
                }
            }
        }
        _ => return None, // zip-packed: not attempted
    }
    let (w32, h32) = (w as u32, h as u32);
    let n = w * h;
    match mode {
        1 => image::GrayImage::from_raw(w32, h32, planes.remove(0)).map(DynamicImage::ImageLuma8),
        2 => {
            if palette.len() < 768 {
                return None;
            }
            let rgb: Vec<u8> = planes[0]
                .iter()
                .flat_map(|&i| [palette[i as usize], palette[256 + i as usize], palette[512 + i as usize]])
                .collect();
            image::RgbImage::from_raw(w32, h32, rgb).map(DynamicImage::ImageRgb8)
        }
        3 => {
            let rgb: Vec<u8> = (0..n).flat_map(|i| [planes[0][i], planes[1][i], planes[2][i]]).collect();
            image::RgbImage::from_raw(w32, h32, rgb).map(DynamicImage::ImageRgb8)
        }
        _ => {
            // Photoshop stores CMYK upside down: 255 is no ink.
            let mut rgb = Vec::with_capacity(n * 3);
            for i in 0..n {
                let k = u32::from(planes[3][i]);
                for plane in &planes[..3] {
                    rgb.push((u32::from(plane[i]) * k / 255) as u8);
                }
            }
            image::RgbImage::from_raw(w32, h32, rgb).map(DynamicImage::ImageRgb8)
        }
    }
}

/// An icon whose pictures are PNGs inside the .ico: the ordinary reader only
/// takes those when they have transparency, so the largest one is read here.
fn ico_picture(data: &[u8]) -> Option<DynamicImage> {
    let count = u16::from_le_bytes(data.get(4..6)?.try_into().ok()?) as usize;
    let mut best: Option<(usize, &[u8])> = None;
    for i in 0..count.min(256) {
        let e = data.get(6 + i * 16..22 + i * 16)?;
        let side = |b: u8| if b == 0 { 256 } else { b as usize };
        let area = side(e[0]) * side(e[1]);
        let size = u32::from_le_bytes(e[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(e[12..16].try_into().ok()?) as usize;
        let blob = data.get(offset..offset.checked_add(size)?)?;
        if blob.starts_with(&[0x89, b'P', b'N', b'G']) && best.is_none_or(|(a, _)| area > a) {
            best = Some((area, blob));
        }
    }
    image::load_from_memory_with_format(best?.1, image::ImageFormat::Png).ok()
}

#[cfg(target_os = "macos")]
fn via_system(path: &Path) -> Option<DynamicImage> {
    let out = std::env::temp_dir().join(format!("hs-scan-{}.jpg", std::process::id()));
    let ok = std::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", "jpeg"])
        .arg(path)
        .arg("--out")
        .arg(&out)
        .output()
        .ok()
        .is_some_and(|o| o.status.success());
    let img = ok.then(|| image::open(&out).ok()).flatten();
    let _ = std::fs::remove_file(&out);
    img
}

/// Windows: ask the system's picture reader (WIC). It opens HEIC only when
/// the person has Microsoft's HEIF (and, for most phones, HEVC) extension;
/// without it this simply returns None and the caller says it can't read it.
#[cfg(windows)]
fn via_system(path: &Path) -> Option<DynamicImage> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;

    unsafe {
        // Already started on this thread (S_FALSE) or started the other way
        // (RPC_E_CHANGED_MODE) are both fine for our purpose.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let decoder = factory
            .CreateDecoderFromFilename(PCWSTR(wide.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let converter = factory.CreateFormatConverter().ok()?;
        converter
            .Initialize(&frame, &GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        converter.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 || (w as u64) * (h as u64) > 200_000_000 {
            return None;
        }
        let mut pixels = vec![0u8; (w as usize) * (h as usize) * 4];
        converter.CopyPixels(std::ptr::null(), w * 4, &mut pixels).ok()?;
        image::RgbaImage::from_raw(w, h, pixels).map(DynamicImage::ImageRgba8)
    }
}

/// Linux: the desktop's picture loaders (gdk-pixbuf). HEIC and AVIF open when
/// the computer has the loader and decoders for them; otherwise this is None.
#[cfg(target_os = "linux")]
fn via_system(path: &Path) -> Option<DynamicImage> {
    use gdk_pixbuf::Pixbuf;
    let (_, w, h) = Pixbuf::file_info(path)?;
    if w <= 0 || h <= 0 || (w as u64) * (h as u64) > MAX_PIXELS {
        return None;
    }
    let pix = Pixbuf::from_file(path).ok()?;
    let (w, h) = (pix.width() as usize, pix.height() as usize);
    let (channels, stride) = (pix.n_channels() as usize, pix.rowstride() as usize);
    let bytes = pix.pixel_bytes()?;
    if !(channels == 3 || channels == 4) || stride < w * channels || bytes.len() < stride * (h - 1) + w * channels {
        return None;
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in bytes.chunks(stride).take(h) {
        for px in row[..w * channels].chunks(channels) {
            rgb.extend_from_slice(&px[..3]);
        }
    }
    image::RgbImage::from_raw(w as u32, h as u32, rgb).map(DynamicImage::ImageRgb8)
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn via_system(_path: &Path) -> Option<DynamicImage> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::test_pictures::picture;

    fn jpeg_bytes(img: &image::GrayImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-scanfmt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn the_biggest_jpeg_inside_a_raw_file_is_taken() {
        let small = jpeg_bytes(&picture(1, 64, 48));
        let big = jpeg_bytes(&picture(2, 400, 300));
        let mut file = b"II*\0 some camera header ".to_vec();
        file.extend(&small);
        file.extend(b"\0\0padding\0\0");
        file.extend(&big);
        file.extend(b"trailing raw data");
        let p = scratch("shot.nef");
        std::fs::write(&p, &file).unwrap();
        let img = try_decode(&p).expect("a picture");
        assert_eq!((img.width(), img.height()), (400, 300));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_scanned_pdf_gives_up_its_picture() {
        use lopdf::{dictionary, Document, Object, Stream};
        let photo = picture(3, 300, 200);
        let mut doc = Document::with_version("1.5");
        let pages = doc.new_object_id();
        let image = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => 300, "Height" => 200,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8, "Filter" => "DCTDecode",
            },
            jpeg_bytes(&photo),
        ));
        let content = doc.add_object(Stream::new(dictionary! {}, b"q 300 0 0 200 0 0 cm /Im0 Do Q".to_vec()));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => content,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 200.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => image } },
        });
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let p = scratch("scan.pdf");
        doc.save(&p).unwrap();
        let img = try_decode(&p).expect("a picture");
        assert_eq!((img.width(), img.height()), (300, 200));
        let _ = std::fs::remove_file(p);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_heic_picture_is_read_through_the_system() {
        let jpg = scratch("phone.jpg");
        std::fs::write(&jpg, jpeg_bytes(&picture(4, 320, 240))).unwrap();
        let heic = scratch("phone.heic");
        let made = std::process::Command::new("/usr/bin/sips")
            .args(["-s", "format", "heic"]).arg(&jpg).arg("--out").arg(&heic).output().unwrap();
        if !made.status.success() {
            return; // this machine cannot write HEIC; nothing to read back
        }
        let img = try_decode(&heic).expect("a picture");
        assert_eq!((img.width(), img.height()), (320, 240));
    }

    fn picture_rgba(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 7) as u8, (y * 5) as u8, 90, 255])))
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formats").join(name)
    }

    /// The fixtures are 64x48 (the icon 48x48): red top-left, blue top-right,
    /// green bottom-left, black bottom-right, on light grey. Checking the four
    /// corners catches flipped, swapped-channel and wrongly decoded pictures.
    fn assert_the_four_blocks(img: &DynamicImage, tolerance: i32) {
        let rgb = img.to_rgb8();
        let (w, h) = (rgb.width(), rgb.height());
        let at = |fx: f32, fy: f32| {
            let p = rgb.get_pixel((fx * w as f32) as u32, (fy * h as f32) as u32);
            [p[0] as i32, p[1] as i32, p[2] as i32]
        };
        let near = |got: [i32; 3], want: [i32; 3]| got.iter().zip(want).all(|(g, w)| (g - w).abs() <= tolerance);
        assert!(near(at(0.22, 0.25), [220, 30, 30]), "top-left should be red: {:?}", at(0.22, 0.25));
        assert!(near(at(0.75, 0.25), [30, 60, 220]), "top-right should be blue: {:?}", at(0.75, 0.25));
        assert!(near(at(0.22, 0.75), [30, 170, 60]), "bottom-left should be green: {:?}", at(0.22, 0.75));
        assert!(near(at(0.75, 0.75), [20, 20, 20]), "bottom-right should be black: {:?}", at(0.75, 0.75));
        assert!(near(at(0.5, 0.5), [235, 235, 235]), "the middle should be light grey: {:?}", at(0.5, 0.5));
    }

    #[test]
    fn jpeg_2000_pictures_are_read_from_either_encoder_and_either_wrapper() {
        for name in ["shapes.jp2", "shapes.j2k", "shapes_opj.jp2"] {
            let img = try_decode(&fixture(name)).unwrap_or_else(|| panic!("{name} should be readable"));
            assert_eq!((img.width(), img.height()), (64, 48), "{name}");
            assert_the_four_blocks(&img, 30);
        }
    }

    #[test]
    fn pcx_pictures_are_read_in_colour_and_with_a_palette() {
        for name in ["shapes_rgb.pcx", "shapes_pal.pcx"] {
            let img = try_decode(&fixture(name)).unwrap_or_else(|| panic!("{name} should be readable"));
            assert_eq!((img.width(), img.height()), (64, 48), "{name}");
            assert_the_four_blocks(&img, 30);
        }
    }

    #[test]
    fn a_photoshop_file_gives_its_flattened_picture() {
        let img = try_decode(&fixture("shapes.psd")).expect("a picture");
        assert_eq!((img.width(), img.height()), (64, 48));
        assert_the_four_blocks(&img, 6);
    }

    /// A Photoshop file built by hand: `planes` are the channels in order, one
    /// byte per sample (written twice for 16-bit).
    fn make_psd(mode: u16, depth: u16, w: u32, h: u32, planes: &[Vec<u8>], rle: bool, palette: &[u8]) -> Vec<u8> {
        let mut f = b"8BPS".to_vec();
        f.extend(1u16.to_be_bytes());
        f.extend([0u8; 6]);
        f.extend((planes.len() as u16).to_be_bytes());
        f.extend(h.to_be_bytes());
        f.extend(w.to_be_bytes());
        f.extend(depth.to_be_bytes());
        f.extend(mode.to_be_bytes());
        f.extend((palette.len() as u32).to_be_bytes());
        f.extend(palette);
        f.extend(0u32.to_be_bytes()); // image resources
        f.extend(0u32.to_be_bytes()); // layers
        f.extend((rle as u16).to_be_bytes());
        let wide = |row: &[u8]| -> Vec<u8> {
            if depth == 16 { row.iter().flat_map(|&b| [b, 0]).collect() } else { row.to_vec() }
        };
        if !rle {
            for plane in planes {
                f.extend(wide(plane));
            }
        } else {
            let mut table = Vec::new();
            let mut body = Vec::new();
            for plane in planes {
                for row in plane.chunks(w as usize) {
                    let row = wide(row);
                    let mut packed = Vec::new();
                    let mut i = 0;
                    while i < row.len() {
                        let mut run = 1;
                        while i + run < row.len() && row[i + run] == row[i] && run < 128 {
                            run += 1;
                        }
                        if run >= 3 {
                            packed.push((1 - run as i16) as i8 as u8);
                            packed.push(row[i]);
                            i += run;
                        } else {
                            let end = (i + 3).min(row.len());
                            packed.push((end - i - 1) as u8);
                            packed.extend(&row[i..end]);
                            i = end;
                        }
                    }
                    table.extend((packed.len() as u16).to_be_bytes());
                    body.extend(packed);
                }
            }
            f.extend(table);
            f.extend(body);
        }
        f
    }

    #[test]
    fn photoshop_files_are_read_in_each_mode_and_packing() {
        let (w, h) = (6u32, 4u32);
        let n = (w * h) as usize;
        let r: Vec<u8> = (0..n).map(|i| (i * 10) as u8).collect();
        let g: Vec<u8> = (0..n).map(|i| if i % 6 < 3 { 200 } else { 20 }).collect();
        let b: Vec<u8> = vec![77; n];
        for rle in [false, true] {
            for depth in [8u16, 16] {
                let psd = make_psd(3, depth, w, h, &[r.clone(), g.clone(), b.clone()], rle, &[]);
                let img = psd_picture(&psd).unwrap_or_else(|| panic!("rgb rle={rle} depth={depth}")).to_rgb8();
                assert_eq!((img.width(), img.height()), (w, h));
                for i in 0..n {
                    assert_eq!(img.get_pixel(i as u32 % w, i as u32 / w).0, [r[i], g[i], b[i]], "rgb rle={rle} depth={depth} i={i}");
                }
                let grey = psd_picture(&make_psd(1, depth, w, h, &[r.clone()], rle, &[])).expect("grey").to_luma8();
                assert_eq!(grey.get_pixel(2, 1).0[0], r[8], "grey rle={rle} depth={depth}");
            }
            // A fourth channel (transparency) is ignored, not mistaken for ink.
            let with_alpha = psd_picture(&make_psd(3, 8, w, h, &[r.clone(), g.clone(), b.clone(), vec![0; n]], rle, &[])).expect("alpha").to_rgb8();
            assert_eq!(with_alpha.get_pixel(1, 0).0, [r[1], g[1], b[1]]);
            // CMYK is stored upside down: all 255 is white paper, 0 in K is black.
            let cmyk = psd_picture(&make_psd(4, 8, w, h, &[vec![255; n], vec![255; n], vec![255; n], vec![255; n]], rle, &[])).expect("cmyk").to_rgb8();
            assert_eq!(cmyk.get_pixel(0, 0).0, [255, 255, 255]);
            let black = psd_picture(&make_psd(4, 8, w, h, &[vec![255; n], vec![255; n], vec![255; n], vec![0; n]], rle, &[])).expect("cmyk").to_rgb8();
            assert_eq!(black.get_pixel(0, 0).0, [0, 0, 0]);
            // Palette pictures look their colours up.
            let mut table = vec![0u8; 768];
            table[1] = 10; table[256 + 1] = 20; table[512 + 1] = 30;
            let indexed = psd_picture(&make_psd(2, 8, w, h, &[vec![1; n]], rle, &table)).expect("indexed").to_rgb8();
            assert_eq!(indexed.get_pixel(3, 2).0, [10, 20, 30]);
        }
    }

    #[test]
    fn an_icon_file_is_read_by_the_ordinary_picture_reader() {
        let dir = std::env::temp_dir().join(format!("hs-ico-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // PNG pictures inside the icon, which the ordinary reader refuses...
        let img = crate::thumbnails::decode_for_matching(&fixture("shapes.ico"), &dir).expect("a picture");
        assert_eq!((img.width(), img.height()), (48, 48));
        assert_the_four_blocks(&img, 30);
        // ...and one the ordinary reader takes itself.
        let mut plain = std::io::Cursor::new(Vec::new());
        picture_rgba(32, 32).write_to(&mut plain, image::ImageFormat::Ico).unwrap();
        let p = scratch("plain.ico");
        std::fs::write(&p, plain.into_inner()).unwrap();
        let img = crate::thumbnails::decode_for_matching(&p, &dir).expect("a picture");
        assert_eq!((img.width(), img.height()), (32, 32));
    }

    #[test]
    fn an_avif_picture_is_read_through_the_system() {
        // Where the computer has no reader for AVIF (older macOS, Windows
        // without Microsoft's extension, Linux without the loader) there is
        // nothing to check.
        if let Some(img) = try_decode(&fixture("shapes.avif")) {
            assert_eq!((img.width(), img.height()), (64, 48));
            assert_the_four_blocks(&img, 40);
        }
    }

    /// A release build stops dead on a panic, so no damaged file may cause one:
    /// every prefix and a spread of single-byte changes of each file must come
    /// back as a picture or as nothing.
    #[test]
    fn damaged_photoshop_files_of_every_kind_never_crash() {
        let n = 24usize;
        let plane: Vec<u8> = (0..n).map(|i| (i * 9) as u8).collect();
        for rle in [false, true] {
            for (mode, depth, planes) in [(3u16, 8u16, 3usize), (3, 16, 3), (4, 8, 4), (1, 8, 1), (2, 8, 1)] {
                let table = vec![5u8; if mode == 2 { 768 } else { 0 }];
                let good = make_psd(mode, depth, 6, 4, &vec![plane.clone(); planes], rle, &table);
                for cut in 0..good.len() {
                    let _ = psd_picture(&good[..cut]);
                }
                for at in 0..good.len() {
                    for value in [0u8, 0xff, 0x7f, 0x80] {
                        let mut bytes = good.clone();
                        bytes[at] = value;
                        let _ = psd_picture(&bytes);
                    }
                }
            }
        }
    }

    #[test]
    fn damaged_files_never_crash_a_reader() {
        for name in ["shapes.jp2", "shapes.j2k", "shapes_opj.jp2", "shapes_rgb.pcx", "shapes_pal.pcx", "shapes.psd"] {
            let original = std::fs::read(fixture(name)).unwrap();
            let ext = name.rsplit('.').next().unwrap();
            let p = scratch(&format!("damaged.{ext}"));
            let step = (original.len() / 150).max(1);
            for cut in (0..original.len()).step_by(step) {
                std::fs::write(&p, &original[..cut]).unwrap();
                let _ = try_decode(&p);
            }
            for at in (0..original.len()).step_by(step) {
                for value in [0u8, 0xff, 0x80] {
                    let mut bytes = original.clone();
                    bytes[at] = value;
                    std::fs::write(&p, &bytes).unwrap();
                    let _ = try_decode(&p);
                }
            }
        }
    }

    #[test]
    fn the_list_of_picture_types_covers_every_group_this_file_reads() {
        for group in [RAW, HEIF, AVIF, JPEG2000] {
            for ext in group {
                assert!(PICTURE_EXTENSIONS.contains(ext), "{ext} is read but not listed");
            }
        }
        for ext in ["pdf", "pcx", "psd", "ico", "webp", "tga", "pnm"] {
            assert!(PICTURE_EXTENSIONS.contains(&ext), "{ext}");
        }
        assert!(PICTURE_EXTENSIONS.iter().all(|e| *e == e.to_ascii_lowercase()));
    }

    #[test]
    fn other_files_are_not_taken_for_pictures() {
        let p = scratch("notes.txt");
        std::fs::write(&p, b"hello").unwrap();
        assert!(try_decode(&p).is_none());
        let q = scratch("broken.pdf");
        std::fs::write(&q, b"not a pdf").unwrap();
        assert!(try_decode(&q).is_none());
    }
}
