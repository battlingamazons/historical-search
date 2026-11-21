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

pub fn try_decode(path: &Path) -> Option<DynamicImage> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext == "pdf" {
        first_page_picture(path)
    } else if RAW.contains(&ext.as_str()) {
        largest_embedded_jpeg(&std::fs::read(path).ok()?)
    } else if HEIF.contains(&ext.as_str()) {
        via_system(path)
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

#[cfg(not(target_os = "macos"))]
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
