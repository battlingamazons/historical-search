//! Finding the faces in a photograph, and describing each with 128 numbers.
//!
//! This is for the copies the ordinary matching cannot see: a crop so tight
//! that hardly any of the photograph is left -- one face out of a group, say.
//! The same face in two scans of one print is the same pixels, so a face that
//! matches very closely says *where* to look in the other photograph and how
//! big the crop is; `face_match.rs` then compares the pixels to be sure.
//!
//! Two small models do the work, both run here with `tract`, a Rust library,
//! so nothing needs installing and nothing leaves the computer:
//!
//! * YuNet (MIT licence) finds faces and five landmarks on each;
//! * SFace (Apache-2.0) describes an aligned face as 128 numbers.
//!
//! Both are the models the program this was ported from uses, with the same
//! settings, so the same faces are found. Scans are often lying sideways, so
//! when no face is found a photograph is tried the other three ways up.

use image::imageops;
use image::RgbImage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tract_onnx::prelude::*;

/// Photographs are shrunk to this before looking for faces.
pub const DETECT_SIDE: u32 = 1600;
/// Faces narrower than this many pixels, at that size, are ignored.
const MIN_FACE: f32 = 20.0;
/// How sure the detector must be; and how sure for a face found only after
/// turning the photograph, which is stricter because turning invites mistakes.
const DET_SCORE: f32 = 0.6;
const ROT_SCORE: f32 = 0.85;
/// Boxes overlapping more than this are taken to be the same face.
const NMS_OVERLAP: f32 = 0.3;
/// The detector works on sizes that are a multiple of this.
const DIVISOR: usize = 32;
const STRIDES: [usize; 3] = [8, 16, 32];
/// Length of a face's description.
pub const EMB_LEN: usize = 128;

type Net = Arc<TypedRunnableModel>;

// ------------------------------------------------------------ the models

static MODEL_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where the model files are, once the application knows: they travel with it.
pub fn set_model_dir(dir: &Path) {
    *MODEL_DIR.lock().unwrap() = Some(dir.to_path_buf());
}

fn model_file(name: &str) -> Option<PathBuf> {
    let configured = MODEL_DIR.lock().unwrap().clone();
    // Beside the source, for a build run from the project folder.
    let beside_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models");
    configured
        .into_iter()
        .chain(std::iter::once(beside_source))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Whether both models can be found and read. Without them the face check is
/// simply left out; everything else works as it does.
pub fn available() -> bool {
    // Worked out once, when first asked: the application says where the models
    // are before anything asks.
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| detector_for(DIVISOR, DIVISOR).is_some() && recogniser().is_some())
}

fn load(name: &str, shape: [usize; 4]) -> Option<Net> {
    let path = model_file(name)?;
    let result = (|| -> TractResult<Net> {
        let mut m = tract_onnx::onnx().model_for_path(path)?;
        // The file records the size of every stage for a 640 x 640 picture,
        // which would stop it taking any other; the sizes are worked out afresh.
        for node in m.nodes.iter_mut() {
            for out in node.outputs.iter_mut() {
                out.fact = InferenceFact::default();
            }
        }
        m = m.with_input_fact(0, f32::fact(shape).into())?;
        m.into_optimized()?.into_runnable()
    })();
    result.ok()
}

/// The detector for pictures of this (padded) size; built once per size.
fn detector_for(w: usize, h: usize) -> Option<Net> {
    type Nets = Mutex<HashMap<(usize, usize), Option<Net>>>;
    static NETS: OnceLock<Nets> = OnceLock::new();
    let mut nets = NETS.get_or_init(Default::default).lock().unwrap();
    if nets.len() > 96 {
        nets.clear(); // sizes vary; do not keep them all
    }
    nets.entry((w, h)).or_insert_with(|| load("yunet.onnx", [1, 3, h, w])).clone()
}

fn recogniser() -> Option<Net> {
    static NET: OnceLock<Option<Net>> = OnceLock::new();
    NET.get_or_init(|| load("sface.onnx", [1, 3, 112, 112])).clone()
}

// ------------------------------------------------------------ one face

/// A face and how it was found.
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    pub score: f32,
    /// Left, top, width, height, as fractions of the photograph (as turned).
    pub bbox: [f32; 4],
    /// Unit length.
    pub emb: Vec<f32>,
}

/// The faces in a photograph, and which way up it had to be turned to find them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Found {
    /// 0 as it is, 1 a quarter turn clockwise, 2 a quarter turn the other way, 3 upside down.
    pub rot: u8,
    pub faces: Vec<Face>,
}

/// The photograph turned as `rot` says (0 leaves it as it is).
pub fn turn(rgb: &RgbImage, rot: u8) -> RgbImage {
    match rot {
        1 => imageops::rotate90(rgb),
        2 => imageops::rotate270(rgb),
        3 => imageops::rotate180(rgb),
        _ => rgb.clone(),
    }
}

/// A raw detection: left, top, width, height, five landmarks (x, y), score.
type Raw = [f32; 15];

fn detect(rgb: &RgbImage, min_score: f32) -> Option<Vec<Raw>> {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let (pad_w, pad_h) = (w.div_ceil(DIVISOR) * DIVISOR, h.div_ceil(DIVISOR) * DIVISOR);
    let net = detector_for(pad_w, pad_h)?;
    // Blue, green, red, 0 to 255, the picture in the top left of a black canvas.
    let mut input = tract_ndarray::Array4::<f32>::zeros((1, 3, pad_h, pad_w));
    for (x, y, p) in rgb.enumerate_pixels() {
        let (x, y) = (x as usize, y as usize);
        input[[0, 0, y, x]] = p.0[2] as f32;
        input[[0, 1, y, x]] = p.0[1] as f32;
        input[[0, 2, y, x]] = p.0[0] as f32;
    }
    let out = net.run(tvec!(Tensor::from(input).into())).ok()?;
    let view = |i: usize| out[i].to_plain_array_view::<f32>().ok();
    let mut found: Vec<Raw> = Vec::new();
    for (k, &stride) in STRIDES.iter().enumerate() {
        let (cls, obj, bbox, kps) = (view(k)?, view(k + 3)?, view(k + 6)?, view(k + 9)?);
        let (cols, rows) = (pad_w / stride, pad_h / stride);
        let s = stride as f32;
        for r in 0..rows {
            for c in 0..cols {
                let i = r * cols + c;
                let score = (cls[[0, i, 0]].clamp(0.0, 1.0) * obj[[0, i, 0]].clamp(0.0, 1.0)).sqrt();
                if score < min_score {
                    continue;
                }
                let cx = (c as f32 + bbox[[0, i, 0]]) * s;
                let cy = (r as f32 + bbox[[0, i, 1]]) * s;
                let bw = bbox[[0, i, 2]].exp() * s;
                let bh = bbox[[0, i, 3]].exp() * s;
                let mut f = [0f32; 15];
                f[0] = cx - bw / 2.0;
                f[1] = cy - bh / 2.0;
                f[2] = bw;
                f[3] = bh;
                for n in 0..5 {
                    f[4 + 2 * n] = (kps[[0, i, 2 * n]] + c as f32) * s;
                    f[5 + 2 * n] = (kps[[0, i, 2 * n + 1]] + r as f32) * s;
                }
                f[14] = score;
                found.push(f);
            }
        }
    }
    Some(non_max_suppression(found))
}

fn overlap(a: &Raw, b: &Raw) -> f32 {
    let (x0, y0) = (a[0].max(b[0]), a[1].max(b[1]));
    let (x1, y1) = ((a[0] + a[2]).min(b[0] + b[2]), (a[1] + a[3]).min(b[1] + b[3]));
    let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    let union = a[2] * a[3] + b[2] * b[3] - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Of boxes on one face, the most confident; best first.
fn non_max_suppression(mut found: Vec<Raw>) -> Vec<Raw> {
    found.sort_by(|a, b| b[14].total_cmp(&a[14]));
    found.truncate(5000);
    let mut kept: Vec<Raw> = Vec::new();
    for f in found {
        if kept.iter().all(|k| overlap(&f, k) <= NMS_OVERLAP) {
            kept.push(f);
        }
    }
    kept
}

fn big_enough(found: Vec<Raw>) -> Vec<Raw> {
    found.into_iter().filter(|f| f[2] >= MIN_FACE && f[3] >= MIN_FACE).collect()
}

// ------------------------------------------------------------ describing a face

/// Where the five landmarks go in the 112 x 112 picture a face is described from.
const STANDARD: [[f64; 2]; 5] =
    [[38.2946, 51.6963], [73.5318, 51.5014], [56.0252, 71.7366], [41.5493, 92.3655], [70.7299, 92.2041]];

/// The transform (a, b, tx, ty) taking the landmarks as found onto the standard ones, by
/// the least-squares turn, scale and shift: x' = a*x - b*y + tx, y' = b*x + a*y + ty.
fn alignment(found: &[[f64; 2]; 5]) -> [f64; 4] {
    let mean = |pts: &[[f64; 2]; 5]| {
        let (sx, sy) = pts.iter().fold((0.0, 0.0), |(x, y), p| (x + p[0], y + p[1]));
        (sx / 5.0, sy / 5.0)
    };
    let (mx, my) = mean(found);
    let (nx, ny) = mean(&STANDARD);
    let (mut re, mut im, mut var) = (0.0, 0.0, 0.0);
    for (p, q) in found.iter().zip(&STANDARD) {
        let (zx, zy) = (p[0] - mx, p[1] - my);
        let (wx, wy) = (q[0] - nx, q[1] - ny);
        re += zx * wx + zy * wy;
        im += zx * wy - zy * wx;
        var += zx * zx + zy * zy;
    }
    let (a, b) = if var > 1e-12 { (re / var, im / var) } else { (1.0, 0.0) };
    [a, b, nx - (a * mx - b * my), ny - (b * mx + a * my)]
}

/// The face cut out of the photograph, straightened and scaled to 112 x 112 by
/// the landmarks, as whole numbers 0 to 255: red, green, blue, row by row. (The
/// detector wants blue first; this model wants red first.)
fn align_crop(rgb: &RgbImage, landmarks: &[[f64; 2]; 5]) -> Vec<f32> {
    let [a, b, tx, ty] = alignment(landmarks);
    let det = a * a + b * b;
    let (w, h) = (rgb.width() as i64, rgb.height() as i64);
    let mut out = vec![0f32; 3 * 112 * 112];
    let pixel = |x: i64, y: i64, ch: usize| -> f64 {
        if x < 0 || y < 0 || x >= w || y >= h {
            0.0
        } else {
            rgb.get_pixel(x as u32, y as u32).0[ch] as f64
        }
    };
    for y in 0..112usize {
        for x in 0..112usize {
            // Back from the straightened picture to the photograph.
            let (dx, dy) = (x as f64 - tx, y as f64 - ty);
            let sx = (a * dx + b * dy) / det;
            let sy = (-b * dx + a * dy) / det;
            let (x0, y0) = (sx.floor(), sy.floor());
            let (fx, fy) = (sx - x0, sy - y0);
            let (x0, y0) = (x0 as i64, y0 as i64);
            for ch in 0..3 {
                let top = pixel(x0, y0, ch) * (1.0 - fx) + pixel(x0 + 1, y0, ch) * fx;
                let bottom = pixel(x0, y0 + 1, ch) * (1.0 - fx) + pixel(x0 + 1, y0 + 1, ch) * fx;
                // Whole numbers, as the picture it is made into would hold.
                out[ch * 112 * 112 + y * 112 + x] = (top * (1.0 - fy) + bottom * fy).round() as f32;
            }
        }
    }
    out
}

fn describe(net: &Net, rgb: &RgbImage, raw: &Raw) -> Option<Vec<f32>> {
    let mut lm = [[0f64; 2]; 5];
    for (n, p) in lm.iter_mut().enumerate() {
        *p = [raw[4 + 2 * n] as f64, raw[5 + 2 * n] as f64];
    }
    let crop = align_crop(rgb, &lm);
    let input = tract_ndarray::Array4::from_shape_vec((1, 3, 112, 112), crop).ok()?;
    let out = net.run(tvec!(Tensor::from(input).into())).ok()?;
    let mut emb: Vec<f32> = out[0].to_plain_array_view::<f32>().ok()?.iter().copied().collect();
    if emb.len() != EMB_LEN {
        return None;
    }
    let norm = emb.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
    for v in emb.iter_mut() {
        *v /= norm;
    }
    Some(emb)
}

/// The faces in a photograph already shrunk to `DETECT_SIDE`. None if the
/// models are not there; a photograph with no face gives an empty list.
pub fn find(rgb: &RgbImage) -> Option<Found> {
    let recogniser = recogniser()?;
    let mut best = big_enough(detect(rgb, DET_SCORE)?);
    let (mut best_img, mut best_rot): (Option<RgbImage>, u8) = (None, 0);
    if best.is_empty() {
        // Scans are often lying sideways: try the other three ways up.
        for rot in 1..=3u8 {
            let turned = turn(rgb, rot);
            let got = big_enough(detect(&turned, ROT_SCORE)?);
            if got.len() > best.len() {
                best = got;
                best_img = Some(turned);
                best_rot = rot;
            }
        }
    }
    if best.is_empty() {
        return Some(Found::default());
    }
    let image = best_img.as_ref().unwrap_or(rgb);
    let (w, h) = (image.width() as f32, image.height() as f32);
    let mut faces = Vec::new();
    for raw in &best {
        let emb = describe(&recogniser, image, raw)?;
        faces.push(Face { score: raw[14], bbox: [raw[0] / w, raw[1] / h, raw[2] / w, raw[3] / h], emb });
    }
    Some(Found { rot: best_rot, faces })
}

/// A photograph shrunk, if it is larger, to fit `DETECT_SIDE`.
pub fn shrink_for_faces(img: &image::DynamicImage) -> RgbImage {
    if img.width() > DETECT_SIDE || img.height() > DETECT_SIDE {
        img.resize(DETECT_SIDE, DETECT_SIDE, imageops::FilterType::Lanczos3).to_rgb8()
    } else {
        img.to_rgb8()
    }
}

// ------------------------------------------------------------ keeping them

/// What is kept for a photograph: which way up, how many faces, and for each
/// its box and its description.
pub fn pack(found: &Found) -> Vec<u8> {
    let mut out = vec![found.rot, found.faces.len().min(255) as u8];
    for f in found.faces.iter().take(255) {
        for v in f.bbox.iter().chain(f.emb.iter()) {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

pub fn unpack(bytes: &[u8]) -> Option<Found> {
    let (&rot, rest) = bytes.split_first()?;
    let (&n, rest) = rest.split_first()?;
    let each = (4 + EMB_LEN) * 4;
    if rest.len() != n as usize * each {
        return None;
    }
    let faces = rest
        .chunks_exact(each)
        .map(|c| {
            let v: Vec<f32> = c.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            Face { score: 1.0, bbox: [v[0], v[1], v[2], v[3]], emb: v[4..].to_vec() }
        })
        .collect();
    Some(Found { rot, faces })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_are_found_and_run() {
        assert!(available(), "the two model files are in src-tauri/models");
    }

    #[test]
    fn a_picture_with_no_face_gives_none_and_costs_nothing_to_keep() {
        let img = RgbImage::from_pixel(320, 240, image::Rgb([120, 130, 140]));
        let found = find(&img).expect("the models run");
        assert!(found.faces.is_empty());
        assert_eq!(pack(&found), vec![0, 0]);
        assert_eq!(unpack(&pack(&found)).unwrap(), Found { rot: 0, faces: vec![] });
    }

    #[test]
    fn what_is_kept_for_faces_reads_back_the_same() {
        let face = Face { score: 1.0, bbox: [0.1, 0.2, 0.3, 0.4], emb: (0..EMB_LEN).map(|i| i as f32 / 100.0).collect() };
        let found = Found { rot: 2, faces: vec![face.clone(), face] };
        let bytes = pack(&found);
        assert_eq!(bytes.len(), 2 + 2 * (4 + EMB_LEN) * 4);
        assert_eq!(unpack(&bytes), Some(found));
        assert_eq!(unpack(&[2, 1, 0, 0]), None, "a short record is not read as faces");
        assert_eq!(unpack(&[]), None);
    }

    #[test]
    fn overlapping_boxes_on_one_face_leave_the_most_confident() {
        let mk = |x: f32, score: f32| {
            let mut r = [0f32; 15];
            r[0] = x;
            r[2] = 100.0;
            r[3] = 100.0;
            r[14] = score;
            r
        };
        let kept = non_max_suppression(vec![mk(0.0, 0.7), mk(10.0, 0.9), mk(300.0, 0.8)]);
        assert_eq!(kept.iter().map(|k| k[14]).collect::<Vec<_>>(), vec![0.9, 0.8]);
    }

    #[test]
    fn the_alignment_of_landmarks_already_in_place_changes_nothing() {
        let [a, b, tx, ty] = alignment(&STANDARD);
        assert!((a - 1.0).abs() < 1e-9 && b.abs() < 1e-9 && tx.abs() < 1e-9 && ty.abs() < 1e-9);
        // The same landmarks, twice the size and shifted: scaled back by one half.
        let mut big = STANDARD;
        for p in big.iter_mut() {
            *p = [p[0] * 2.0 + 10.0, p[1] * 2.0 - 5.0];
        }
        let [a, b, ..] = alignment(&big);
        assert!((a - 0.5).abs() < 1e-9 && b.abs() < 1e-9);
    }
}
