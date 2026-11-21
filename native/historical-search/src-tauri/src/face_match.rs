//! Finding a very small crop through the faces in it.
//!
//! The crop pass (`crop_match.rs`) copes with a copy trimmed by up to a quarter
//! on each side. A copy that is only a small piece of the photograph -- one
//! face from a group -- is beyond it: too little of the picture is left. But a
//! face is the same pixels wherever it is cut out, so two scans of one print
//! hold faces that describe almost identically (typically 0.9 or more, where
//! two different people stay under about 0.5). Such a pair says where the crop
//! sits in the other photograph and how big it is; the pictures are then
//! compared there, pixel by pixel, and only if they agree is the copy
//! reported. A pair of faces alone is never enough.
//!
//! This follows the program it was ported from, constant for constant.

use crate::faces::{Found, EMB_LEN};
use image::RgbImage;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// Faces this alike (cosine) are probably the same pixels.
pub const FACE_SAME: f32 = 0.75;
/// At most this many different photographs are looked at closely for one scan.
pub const FACE_K: usize = 12;
/// Photographs that already score this well need no further check.
pub const FACE_SKIP: f32 = 0.90;
/// How the crop might be scaled, relative to the faces' own sizes.
const RELS: [f64; 5] = [0.85, 0.92, 1.0, 1.08, 1.18];
/// A comparison is made on pictures no larger than this on the long side.
const WORKING_SIDE: f64 = 640.0;

// ------------------------------------------------------------ pictures

/// A grey picture as numbers 0 to 255.
#[derive(Debug, Clone)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub d: Vec<f32>,
}

impl Gray {
    /// Grey by the weights Pillow uses, as the original does.
    pub fn from_rgb(rgb: &RgbImage) -> Gray {
        let d = rgb
            .pixels()
            .map(|p| {
                let [r, g, b] = p.0;
                ((r as u32 * 19595 + g as u32 * 38470 + b as u32 * 7471 + 0x8000) >> 16) as f32
            })
            .collect();
        Gray { w: rgb.width() as usize, h: rgb.height() as usize, d }
    }

    /// Scaled by `f` in each direction, each new pixel being the average of the
    /// area of the old picture it covers (what OpenCV calls INTER_AREA).
    fn scaled(&self, f: f64) -> Gray {
        let nw = ((self.w as f64 * f).round() as usize).max(1);
        let nh = ((self.h as f64 * f).round() as usize).max(1);
        let along = |len_old: usize, len_new: usize| -> Vec<Vec<(usize, f32)>> {
            let step = len_old as f64 / len_new as f64;
            (0..len_new)
                .map(|i| {
                    let (a, b) = (i as f64 * step, (i + 1) as f64 * step);
                    let (first, last) = (a.floor() as usize, ((b.ceil() as usize).min(len_old)).max(1));
                    let mut parts = Vec::new();
                    let mut total = 0.0;
                    for k in first..last {
                        let overlap = (b.min(k as f64 + 1.0) - a.max(k as f64)).max(0.0);
                        if overlap > 0.0 {
                            parts.push((k, overlap as f32));
                            total += overlap as f32;
                        }
                    }
                    for p in parts.iter_mut() {
                        p.1 /= total;
                    }
                    parts
                })
                .collect()
        };
        let (wx, wy) = (along(self.w, nw), along(self.h, nh));
        // Across, then down.
        let mut mid = vec![0f32; nw * self.h];
        for y in 0..self.h {
            for (x, parts) in wx.iter().enumerate() {
                mid[y * nw + x] = parts.iter().map(|&(k, w)| self.d[y * self.w + k] * w).sum();
            }
        }
        let mut d = vec![0f32; nw * nh];
        for (y, parts) in wy.iter().enumerate() {
            for x in 0..nw {
                d[y * nw + x] = parts.iter().map(|&(k, w)| mid[k * nw + x] * w).sum();
            }
        }
        Gray { w: nw, h: nh, d }
    }
}

// ------------------------------------------------------------ template matching

fn good_size(mut n: usize) -> usize {
    loop {
        let mut m = n;
        for p in [2, 3, 5] {
            while m.is_multiple_of(p) {
                m /= p;
            }
        }
        if m == 1 {
            return n;
        }
        n += 1;
    }
}

fn fft2(buf: &mut [Complex<f32>], w: usize, h: usize, inverse: bool) {
    let mut planner = FftPlanner::<f32>::new();
    let (row, col) = if inverse {
        (planner.plan_fft_inverse(w), planner.plan_fft_inverse(h))
    } else {
        (planner.plan_fft_forward(w), planner.plan_fft_forward(h))
    };
    for r in buf.chunks_exact_mut(w) {
        row.process(r);
    }
    let mut column = vec![Complex::new(0f32, 0f32); h];
    for x in 0..w {
        for y in 0..h {
            column[y] = buf[y * w + x];
        }
        col.process(&mut column);
        for y in 0..h {
            buf[y * w + x] = column[y];
        }
    }
}

/// The best normalised correlation of `tmpl` inside `img`, counting only the
/// positions (left, top) in `x0..x1` by `y0..y1` (OpenCV's TM_CCOEFF_NORMED).
fn best_correlation(img: &Gray, tmpl: &Gray, x0: usize, x1: usize, y0: usize, y1: usize) -> f32 {
    let (tw, th) = (tmpl.w, tmpl.h);
    // Only the part of the picture those positions can touch.
    let (rw, rh) = ((x1 - x0) + tw - 1, (y1 - y0) + th - 1);
    let (fw, fh) = (good_size(rw), good_size(rh));
    let n = (tw * th) as f64;
    let mean = tmpl.d.iter().map(|&v| v as f64).sum::<f64>() / n;
    let centred: Vec<f32> = tmpl.d.iter().map(|&v| (v as f64 - mean) as f32).collect();
    let t_norm = centred.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>().sqrt();
    if t_norm < 1e-6 {
        return 0.0;
    }

    let mut a = vec![Complex::new(0f32, 0f32); fw * fh];
    for y in 0..rh {
        for x in 0..rw {
            a[y * fw + x].re = img.d[(y0 + y) * img.w + x0 + x];
        }
    }
    let mut b = vec![Complex::new(0f32, 0f32); fw * fh];
    for y in 0..th {
        for x in 0..tw {
            b[y * fw + x].re = centred[y * tw + x];
        }
    }
    fft2(&mut a, fw, fh, false);
    fft2(&mut b, fw, fh, false);
    for (p, q) in a.iter_mut().zip(&b) {
        *p *= q.conj();
    }
    fft2(&mut a, fw, fh, true);
    let scale = 1.0 / (fw * fh) as f32;

    // Sums over each patch, from running totals.
    let stride = rw + 1;
    let mut s1 = vec![0f64; stride * (rh + 1)];
    let mut s2 = vec![0f64; stride * (rh + 1)];
    for y in 0..rh {
        let (mut r1, mut r2) = (0f64, 0f64);
        for x in 0..rw {
            let v = img.d[(y0 + y) * img.w + x0 + x] as f64;
            r1 += v;
            r2 += v * v;
            s1[(y + 1) * stride + x + 1] = s1[y * stride + x + 1] + r1;
            s2[(y + 1) * stride + x + 1] = s2[y * stride + x + 1] + r2;
        }
    }
    let patch = |s: &Vec<f64>, x: usize, y: usize| {
        s[(y + th) * stride + x + tw] - s[y * stride + x + tw] - s[(y + th) * stride + x] + s[y * stride + x]
    };
    let mut best = f32::MIN;
    for y in 0..(y1 - y0) {
        for x in 0..(x1 - x0) {
            let sum = patch(&s1, x, y);
            let var = patch(&s2, x, y) - sum * sum / n;
            let value = if var <= 1e-9 {
                0.0
            } else {
                let numerator = (a[y * fw + x].re * scale) as f64;
                (numerator / (t_norm * var.sqrt())) as f32
            };
            best = best.max(value);
        }
    }
    best
}

/// The best fit of `tmpl` inside `img` among the positions near `expect`.
fn correlation_near(img: &Gray, tmpl: &Gray, expect: (f64, f64), tol: f64) -> f32 {
    if tmpl.h < 12 || tmpl.w < 12 || tmpl.h > img.h || tmpl.w > img.w {
        return 0.0;
    }
    let (res_w, res_h) = (img.w - tmpl.w + 1, img.h - tmpl.h + 1);
    let lo = |e: f64| ((e - tol) as i64).max(0) as usize;
    let hi = |e: f64, limit: usize| (((e + tol) as i64 + 1).max(0) as usize).min(limit);
    let (x0, x1) = (lo(expect.0), hi(expect.0, res_w));
    let (y0, y1) = (lo(expect.1), hi(expect.1, res_h));
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let v = best_correlation(img, tmpl, x0, x1, y0, y1);
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

// ------------------------------------------------------------ placing a crop

/// A face found in both the scan and a stored photograph, and how alike.
#[derive(Debug, Clone, Copy)]
pub struct Pair {
    pub scan_box: [f32; 4],
    pub stored_box: [f32; 4],
    pub similarity: f32,
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// How well the two pictures agree once their faces are lined up: the best
/// normalised correlation, -1 to 1. Boxes are fractions of each picture.
pub fn place_score(scan: &Gray, stored: &Gray, pairs: &[Pair]) -> f32 {
    let (wq, hq) = (scan.w as f64, scan.h as f64);
    let (ws, hs) = (stored.w as f64, stored.h as f64);
    let b = |a: &[f32; 4]| a.map(|v| v as f64);
    // Stored pixels per scan pixel, by the faces' sizes.
    let ratios: Vec<f64> = pairs
        .iter()
        .map(|p| {
            let (q, s) = (b(&p.scan_box), b(&p.stored_box));
            ((s[2] * ws / (q[2] * wq)) * (s[3] * hs / (q[3] * hq))).sqrt()
        })
        .collect();
    let r0 = median(ratios);
    // The most alike pair gives the point to line up on.
    let mut best_pair = &pairs[0];
    for p in pairs {
        if p.similarity > best_pair.similarity {
            best_pair = p;
        }
    }
    let (q, s) = (b(&best_pair.scan_box), b(&best_pair.stored_box));
    let qc = ((q[0] + q[2] / 2.0) * wq, (q[1] + q[3] / 2.0) * hq);
    let sc = ((s[0] + s[2] / 2.0) * ws, (s[1] + s[3] / 2.0) * hs);
    let fw = s[2] * ws;

    let mut stored_small: Option<(f64, Gray)> = None;
    let mut scan_small: Option<(f64, Gray)> = None;
    let mut at = |rel: f64| -> f32 {
        let r = r0 * rel;
        if wq * r <= ws + 1.0 && hq * r <= hs + 1.0 {
            // The scan is the smaller piece: find it inside the stored photograph.
            let w = (WORKING_SIDE / hs.max(ws)).min(1.0);
            let img = &stored_small.get_or_insert_with(|| (w, stored.scaled(w))).1;
            let tmpl = scan.scaled(r * w);
            correlation_near(img, &tmpl, (sc.0 * w - qc.0 * r * w, sc.1 * w - qc.1 * r * w), 1.5 * fw * w + 4.0)
        } else if ws / r <= wq + 1.0 && hs / r <= hq + 1.0 {
            // The stored photograph is the smaller piece.
            let w = (WORKING_SIDE / hq.max(wq)).min(1.0);
            let img = &scan_small.get_or_insert_with(|| (w, scan.scaled(w))).1;
            let tmpl = stored.scaled(w / r);
            correlation_near(img, &tmpl, (qc.0 * w - sc.0 * w / r, qc.1 * w - sc.1 * w / r), 1.5 * fw * w / r + 4.0)
        } else {
            0.0
        }
    };
    let mut best = (f32::MIN, 0.0f64);
    for rel in RELS {
        let v = at(rel);
        if v > best.0 || (v == best.0 && rel > best.1) {
            best = (v, rel);
        }
    }
    let (mut n, rel) = best;
    for f in [0.97, 0.985, 1.015, 1.03] {
        n = n.max(at(rel * f));
    }
    n
}

/// A correlation on the 0 to 1 scale the other scores use: nothing below 0.55.
pub fn report(n: f32) -> f32 {
    if n < 0.55 {
        0.0
    } else {
        0.60 + ((n - 0.55) / 0.35).min(1.0) * 0.40
    }
}

// ------------------------------------------------------------ choosing what to look at

/// Every face in the collection, with the photograph it belongs to.
#[derive(Default)]
pub struct StoredFaces {
    pub owner: Vec<usize>,
    pub rot: Vec<u8>,
    pub bbox: Vec<[f32; 4]>,
    /// `EMB_LEN` numbers for each face.
    pub emb: Vec<f32>,
}

impl StoredFaces {
    pub fn add(&mut self, owner: usize, found: &Found) {
        for f in &found.faces {
            self.owner.push(owner);
            self.rot.push(found.rot);
            self.bbox.push(f.bbox);
            self.emb.extend_from_slice(&f.emb);
        }
    }
    pub fn len(&self) -> usize {
        self.owner.len()
    }
    pub fn is_empty(&self) -> bool {
        self.owner.is_empty()
    }
}

/// A stored photograph to look at closely, and the faces it shares with the scan.
pub struct Pick {
    pub photo: usize,
    /// Which way up its faces were found.
    pub rot: u8,
    pub pairs: Vec<Pair>,
    /// Every photograph that had the same faces, in the order ranked.
    pub rank: usize,
}

/// Which stored photographs hold the same faces as the scan, best first, and
/// the pairs of faces. Photographs that already score `FACE_SKIP` or better
/// (`scores`), and the scan's own file (`own`), are left out.
pub fn candidates(scan: &Found, stored: &StoredFaces, scores: &[f32], own: Option<usize>) -> Vec<Pick> {
    use std::collections::HashMap;
    // Per photograph: the pairs, and which of the scan's faces were matched.
    let mut hits: HashMap<usize, (Vec<Pair>, Vec<usize>, u8)> = HashMap::new();
    let mut order: Vec<usize> = Vec::new();
    for m in 0..stored.len() {
        let owner = stored.owner[m];
        if Some(owner) == own || scores[owner] >= FACE_SKIP {
            continue;
        }
        let se = &stored.emb[m * EMB_LEN..(m + 1) * EMB_LEN];
        for (j, q) in scan.faces.iter().enumerate() {
            let sim: f32 = se.iter().zip(&q.emb).map(|(a, b)| a * b).sum();
            if sim >= FACE_SAME {
                let entry = hits.entry(owner).or_insert_with(|| {
                    order.push(owner);
                    (Vec::new(), Vec::new(), stored.rot[m])
                });
                entry.0.push(Pair { scan_box: q.bbox, stored_box: stored.bbox[m], similarity: sim });
                if !entry.1.contains(&j) {
                    entry.1.push(j);
                }
            }
        }
    }
    let mut ranked = order;
    ranked.sort_by(|a, b| {
        let (ha, hb) = (&hits[a], &hits[b]);
        let (sa, sb): (f32, f32) = (ha.0.iter().map(|p| p.similarity).sum(), hb.0.iter().map(|p| p.similarity).sum());
        hb.1.len().cmp(&ha.1.len()).then(sb.total_cmp(&sa))
    });
    ranked
        .into_iter()
        .take(FACE_K)
        .enumerate()
        .map(|(rank, photo)| {
            let (pairs, _, rot) = hits.remove(&photo).unwrap();
            Pick { photo, rot, pairs, rank }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::test_pictures::picture;
    use crate::faces::Face;

    fn gray_of(img: &image::GrayImage) -> Gray {
        Gray { w: img.width() as usize, h: img.height() as usize, d: img.pixels().map(|p| p.0[0] as f32).collect() }
    }

    #[test]
    fn scaling_by_area_averages_what_it_covers() {
        let g = Gray { w: 4, h: 2, d: vec![0., 4., 8., 12., 0., 4., 8., 12.] };
        let half = g.scaled(0.5);
        assert_eq!((half.w, half.h), (2, 1));
        assert_eq!(half.d, vec![2.0, 10.0]);
        // Not an even fraction: three become two, so each new pixel takes 1.5 of the old.
        let g = Gray { w: 3, h: 1, d: vec![0., 6., 12.] };
        let s = g.scaled(2.0 / 3.0);
        assert!((s.d[0] - 2.0).abs() < 1e-4 && (s.d[1] - 10.0).abs() < 1e-4, "{:?}", s.d);
        // Larger: each old pixel covers more than one new one.
        let up = Gray { w: 2, h: 1, d: vec![10., 20.] }.scaled(2.0);
        assert_eq!((up.w, up.h), (4, 2));
        assert_eq!(up.d, vec![10., 10., 20., 20., 10., 10., 20., 20.]);
    }

    #[test]
    fn a_piece_cut_from_a_picture_is_found_where_it_was_cut() {
        let big = picture(5, 300, 220);
        let piece = image::imageops::crop_imm(&big, 90, 60, 120, 100).to_image();
        let (img, tmpl) = (gray_of(&big), gray_of(&piece));
        let found = correlation_near(&img, &tmpl, (90.0, 60.0), 10.0);
        assert!(found > 0.99, "{found}");
        // Looked for in the wrong place, it is not there.
        let elsewhere = correlation_near(&img, &tmpl, (10.0, 10.0), 5.0);
        assert!(elsewhere < 0.6, "{elsewhere}");
    }

    #[test]
    fn the_fast_correlation_agrees_with_working_it_out_directly() {
        let big = gray_of(&picture(9, 90, 70));
        let tmpl = gray_of(&image::imageops::crop_imm(&picture(9, 90, 70), 20, 15, 30, 25).to_image());
        let (tw, th) = (tmpl.w, tmpl.h);
        let n = (tw * th) as f64;
        let tm = tmpl.d.iter().map(|&v| v as f64).sum::<f64>() / n;
        let tn = tmpl.d.iter().map(|&v| (v as f64 - tm).powi(2)).sum::<f64>().sqrt();
        let (x0, x1, y0, y1) = (10usize, 40usize, 5usize, 35usize);
        let mut best = f64::MIN;
        for y in y0..y1 {
            for x in x0..x1 {
                let mut patch = Vec::new();
                for j in 0..th {
                    for i in 0..tw {
                        patch.push(big.d[(y + j) * big.w + x + i] as f64);
                    }
                }
                let pm = patch.iter().sum::<f64>() / n;
                let num: f64 = patch.iter().zip(&tmpl.d).map(|(p, t)| (p - pm) * (*t as f64 - tm)).sum();
                let pn = patch.iter().map(|p| (p - pm).powi(2)).sum::<f64>().sqrt();
                best = best.max(if pn < 1e-9 { 0.0 } else { num / (tn * pn) });
            }
        }
        let fast = best_correlation(&big, &tmpl, x0, x1, y0, y1) as f64;
        assert!((fast - best).abs() < 2e-3, "fast {fast} direct {best}");
    }

    #[test]
    fn a_small_crop_of_a_bigger_picture_is_placed_by_its_faces_and_scores_well() {
        // The stored photograph, and a scan cut from it and shrunk, with one
        // "face" at the same place in each (the faces themselves are given).
        let big = picture(7, 640, 480);
        let piece = image::imageops::crop_imm(&big, 200, 120, 240, 200).to_image();
        let (stored, scan) = (gray_of(&big), gray_of(&piece));
        // A face 40 wide at (300, 180) in the stored photograph is at (100, 60) in the scan.
        let pair = Pair {
            stored_box: [300.0 / 640.0, 180.0 / 480.0, 40.0 / 640.0, 40.0 / 480.0],
            scan_box: [100.0 / 240.0, 60.0 / 200.0, 40.0 / 240.0, 40.0 / 200.0],
            similarity: 0.95,
        };
        let n = place_score(&scan, &stored, &[pair]);
        assert!(n > 0.95, "{n}");
        assert!(report(n) >= 0.9);
        // The same faces, but the stored photograph is another one: the pixels
        // there do not agree. (These made-up pictures are all soft blobs, so
        // some chance likeness remains; real photographs leave far less.)
        let other = gray_of(&picture(8, 640, 480));
        let chance = place_score(&scan, &other, &[pair]);
        assert!(chance < n - 0.1, "true {n} against chance {chance}");
    }

    #[test]
    fn the_report_scale_runs_from_nothing_to_one() {
        assert_eq!(report(0.54), 0.0);
        assert!((report(0.55) - 0.60).abs() < 1e-6);
        assert!((report(0.725) - 0.80).abs() < 1e-6);
        assert_eq!(report(0.90), 1.0);
    }

    /// A face whose description points along one axis: two on different axes
    /// have nothing in common (cosine 0), two on the same axis are identical.
    fn face(axis: usize, bbox: [f32; 4]) -> Face {
        let mut emb = vec![0f32; EMB_LEN];
        emb[axis] = 1.0;
        Face { score: 1.0, bbox, emb }
    }

    #[test]
    fn only_photographs_with_the_same_faces_are_looked_at_and_the_best_first() {
        let scan = Found { rot: 0, faces: vec![face(0, [0.1, 0.1, 0.1, 0.1]), face(1, [0.5, 0.5, 0.1, 0.1])] };
        let mut stored = StoredFaces::default();
        // Photograph 0: one face like the scan's first (cosine 1.0).
        stored.add(0, &Found { rot: 0, faces: vec![face(0, [0.2, 0.2, 0.1, 0.1])] });
        // Photograph 1: both of the scan's faces.
        stored.add(1, &Found { rot: 1, faces: vec![face(0, [0.3, 0.3, 0.1, 0.1]), face(1, [0.6, 0.6, 0.1, 0.1])] });
        // Photograph 2: a different face.
        stored.add(2, &Found { rot: 0, faces: vec![face(2, [0.2, 0.2, 0.1, 0.1])] });
        // Photograph 3: the same as the first, but already well matched.
        stored.add(3, &Found { rot: 0, faces: vec![face(0, [0.2, 0.2, 0.1, 0.1])] });
        let scores = [0.1, 0.1, 0.1, 0.95];
        let picks = candidates(&scan, &stored, &scores, None);
        assert_eq!(picks.iter().map(|p| p.photo).collect::<Vec<_>>(), vec![1, 0], "two faces beat one; 2 differs; 3 needs no help");
        assert_eq!(picks[0].rot, 1);
        assert_eq!(picks[0].pairs.len(), 2);
        // The scan's own file is never its own match.
        let picks = candidates(&scan, &stored, &scores, Some(1));
        assert_eq!(picks.iter().map(|p| p.photo).collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn no_more_than_the_limit_are_picked() {
        let scan = Found { rot: 0, faces: vec![face(0, [0.1, 0.1, 0.1, 0.1])] };
        let mut stored = StoredFaces::default();
        for i in 0..40 {
            stored.add(i, &Found { rot: 0, faces: vec![face(0, [0.2, 0.2, 0.1, 0.1])] });
        }
        assert_eq!(candidates(&scan, &stored, &[0.0; 40], None).len(), FACE_K);
    }
}
