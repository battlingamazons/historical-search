//! Finding a scan that is a differently cropped copy of a stored photograph.
//!
//! The look-alike fingerprint (`fingerprint.rs`) copes with a modest, centred
//! difference in framing. Two scans of one print can differ much more than
//! that: one edge trimmed harder than another, a corner cut off, one copy
//! being only the left two thirds of the other. This pass asks a different
//! question: is one picture a *window cut out of* the other?
//!
//! Stage 1 is cheap and runs against every photograph. A stored photograph
//! keeps 64 coarse windows of itself, trimmed by 0, 12 or 24 per cent on each
//! side independently. The new scan is tried whole (nudged a little) against
//! each of those, and cut into 625 windows of its own (trimmed by up to 24 per
//! cent a side) against the whole stored photograph, in all four quarter
//! turns. Every trim costs a little, so the least cropping that fits wins.
//!
//! Stage 2 looks closely at the 150 best candidates only. Each keeps a small
//! picture of itself (192 square), from which the windows are cut afresh at
//! higher resolution; the best few starting positions are tried, and the 40
//! most promising are lined up precisely by sliding every window edge, of both
//! pictures, until the fit stops improving. Only a precise fit counts, which
//! keeps unrelated photographs from being reported.
//!
//! The scores are put on the same 0 to 1 scale as the look-alike scores, so a
//! cropped copy is listed alongside the others: below 0.80 of the precise fit
//! nothing is reported; 0.80 maps to 0.60 and 0.95 or more to 1.0.

use crate::fingerprint::{self, f16_to_f32};
use image::imageops::FilterType;
use image::GrayImage;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// Side of the stored picture, and of the edge padding round it.
pub const CSZ: usize = 192;
const CPAD: usize = 96;
const CPADDED: usize = CSZ + 2 * CPAD;
/// Side of the coarse descriptors (stage 1) and the fine ones (stage 2).
const CT: usize = 16;
const FT: usize = 48;
/// Candidates looked at more closely, and of those, how many are lined up
/// precisely.
pub const CROP_K: usize = 150;
const CROP_REFINE: usize = 40;
/// Penalty per unit of cropping: coarse, then precise.
const CA1: f32 = 0.2;
const CA2: f32 = 0.15;
/// A window must keep at least this much of each side.
const CMIN: f64 = 0.45;

const TAPER_FLAT: f64 = 0.20;
const TAPER_END: f64 = 0.36;

/// Coarse windows kept for each stored photograph.
pub const DB_BOXES: usize = 64;
/// Half-precision numbers stored for each photograph's coarse windows.
pub const CROP_STORED_LEN: usize = DB_BOXES * CT * CT;

/// A window of a picture: left, right, top, bottom, as fractions of it.
type Window = [f64; 4];

// ------------------------------------------------------------ the windows

fn axis(starts: &[f64], ends: &[f64]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for &a in starts {
        for &b in ends {
            if b - a >= 0.5 {
                out.push((a, b));
            }
        }
    }
    out
}

fn boxes(axis: &[(f64, f64)]) -> Vec<Window> {
    let mut out = Vec::new();
    for &(a, b) in axis {
        for &(c, d) in axis {
            out.push([a, b, c, d]);
        }
    }
    out
}

fn trim(b: &Window) -> f32 {
    (b[0] + (1.0 - b[1]) + b[2] + (1.0 - b[3])) as f32
}

struct Boxes {
    /// Kept per stored photograph.
    db: Vec<Window>,
    /// Tried on the new scan.
    query: Vec<Window>,
    /// The whole new scan, nudged a little.
    nudged: Vec<Window>,
    trim_db: Vec<f32>,
    /// The trim of each query window, for all four turns.
    trim_query_x4: Vec<f32>,
}

const NUDGED_PER_TURN: usize = 16;
const QUERY_PER_TURN: usize = 625;

fn windows() -> &'static Boxes {
    static B: OnceLock<Boxes> = OnceLock::new();
    B.get_or_init(|| {
        let ax_db: Vec<(f64, f64)> =
            axis(&[0.0, 0.12, 0.24], &[1.0, 0.88, 0.76]).into_iter().filter(|&p| p != (0.24, 0.76)).collect();
        let ax_q = axis(&[0.0, 0.06, 0.12, 0.18, 0.24], &[1.0, 0.94, 0.88, 0.82, 0.76]);
        let ax_j: Vec<(f64, f64)> = [(-0.04, 1.0), (0.0, 1.0), (0.04, 1.0), (0.0, 0.94)]
            .iter()
            .map(|&(shift, scale)| (0.5 + shift - scale / 2.0, 0.5 + shift + scale / 2.0))
            .collect();
        let db = boxes(&ax_db);
        let query = boxes(&ax_q);
        let nudged = boxes(&ax_j);
        assert_eq!((db.len(), query.len(), nudged.len()), (DB_BOXES, QUERY_PER_TURN, NUDGED_PER_TURN));
        let trim_db = db.iter().map(trim).collect();
        let trim_query_x4 = (0..4).flat_map(|_| query.iter().map(trim)).collect();
        Boxes { db, query, nudged, trim_db, trim_query_x4 }
    })
}

/// The window that is the whole picture, which is the first of the stored ones.
const FULL: usize = 0;

// ------------------------------------------------------------ one descriptor

struct Cfg {
    fine: Vec<f32>,
    coarse: Vec<f32>,
    taper: Vec<f32>,
    taper_sum: f32,
}

fn gauss_matrix(n: usize, sigma: f64) -> Vec<f32> {
    let mut m = vec![0f32; n * n];
    for i in 0..n {
        let row: Vec<f64> = (0..n)
            .map(|j| {
                let d = i as f64 - j as f64;
                (-(d * d) / (2.0 * sigma * sigma)).exp()
            })
            .collect();
        let sum: f64 = row.iter().sum();
        for j in 0..n {
            m[i * n + j] = (row[j] / sum) as f32;
        }
    }
    m
}

fn make_cfg(t: usize) -> Cfg {
    let scale = t as f64 / 32.0;
    let ramp: Vec<f64> = (0..t)
        .map(|i| {
            let r = ((i as f64 + 0.5) / t as f64 - 0.5).abs();
            if r <= TAPER_FLAT {
                1.0
            } else if r >= TAPER_END {
                0.0
            } else {
                (std::f64::consts::FRAC_PI_2 * (r - TAPER_FLAT) / (TAPER_END - TAPER_FLAT)).cos().powi(2)
            }
        })
        .collect();
    let mut taper = vec![0f32; t * t];
    for y in 0..t {
        for x in 0..t {
            taper[y * t + x] = (ramp[y] * ramp[x]) as f32;
        }
    }
    let taper_sum = taper.iter().sum();
    Cfg { fine: gauss_matrix(t, 0.8 * scale), coarse: gauss_matrix(t, 2.2 * scale), taper, taper_sum }
}

fn cfg(t: usize) -> &'static Cfg {
    static COARSE: OnceLock<Cfg> = OnceLock::new();
    static FINE: OnceLock<Cfg> = OnceLock::new();
    match t {
        CT => COARSE.get_or_init(|| make_cfg(CT)),
        _ => FINE.get_or_init(|| make_cfg(FT)),
    }
}

/// `m * w * m^T` for t x t matrices.
fn sandwich(m: &[f32], w: &[f32], t: usize) -> Vec<f32> {
    let mut mw = vec![0f32; t * t];
    for i in 0..t {
        for k in 0..t {
            let a = m[i * t + k];
            let (row, src) = (&mut mw[i * t..(i + 1) * t], &w[k * t..(k + 1) * t]);
            for (r, &s) in row.iter_mut().zip(src) {
                *r += a * s;
            }
        }
    }
    let mut out = vec![0f32; t * t];
    for i in 0..t {
        let mi = &mw[i * t..(i + 1) * t];
        for j in 0..t {
            out[i * t + j] = mi.iter().zip(&m[j * t..(j + 1) * t]).map(|(a, b)| a * b).sum();
        }
    }
    out
}

fn bilinear(p: &[f32], x: f64, y: f64) -> f32 {
    let n = CPADDED as f64;
    if x < 0.0 || x >= n || y < 0.0 || y >= n {
        return 0.0;
    }
    let (x, y) = (x - 0.5, y - 0.5);
    let (x0, y0) = (x.floor(), y.floor());
    let (dx, dy) = (x - x0, y - y0);
    let last = CPADDED as i64 - 1;
    let c = |v: f64| (v as i64).clamp(0, last) as usize;
    let (xa, xb, ya, yb) = (c(x0), c(x0 + 1.0), c(y0), c(y0 + 1.0));
    let at = |yy: usize, xx: usize| p[yy * CPADDED + xx] as f64;
    let top = at(ya, xa) + (at(ya, xb) - at(ya, xa)) * dx;
    let bottom = at(yb, xa) + (at(yb, xb) - at(yb, xa)) * dx;
    (top + (bottom - top) * dy) as f32
}

/// A stored-size picture, padded, from which windows of any shape are cut.
struct Big {
    p: Vec<f32>,
}

impl Big {
    /// From the grey picture: shrunk to a square, flat borders painted over,
    /// padded by repeating the edge.
    fn from_gray(gray: &GrayImage) -> Big {
        let small = image::imageops::resize(gray, CSZ as u32, CSZ as u32, FilterType::Lanczos3);
        let mut a: Vec<f32> = small.pixels().map(|p| p.0[0] as f32).collect();
        fingerprint::strip_frame(&mut a, CSZ);
        Big::from_square(&a)
    }

    fn from_square(a: &[f32]) -> Big {
        let mut p = vec![0f32; CPADDED * CPADDED];
        for y in 0..CPADDED {
            let sy = y.saturating_sub(CPAD).min(CSZ - 1);
            for x in 0..CPADDED {
                let sx = x.saturating_sub(CPAD).min(CSZ - 1);
                p[y * CPADDED + x] = a[sy * CSZ + sx];
            }
        }
        Big { p }
    }

    fn turned(&self, quarter_turns: usize) -> Big {
        let mut p = self.p.clone();
        for _ in 0..quarter_turns {
            p = fingerprint::rot90(&p, CPADDED);
        }
        Big { p }
    }

    /// The picture without its padding, as bytes.
    fn square_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CSZ * CSZ);
        for y in 0..CSZ {
            for x in 0..CSZ {
                out.push(self.p[(y + CPAD) * CPADDED + x + CPAD] as u8);
            }
        }
        out
    }

    /// A unit-length description of the structure inside `b`, sampled to
    /// `t` x `t`; all zeros for a window with nothing in it.
    fn window(&self, b: &Window, t: usize) -> Vec<f32> {
        let zeros = vec![0f32; t * t];
        let (csz, pad, tf) = (CSZ as f64, CPAD as f64, t as f64);
        let (scx, scy) = (csz * (b[1] - b[0]) / tf, csz * (b[3] - b[2]) / tf);
        let cx = pad + csz * (b[0] + b[1]) / 2.0 - scx * tf / 2.0;
        let cy = pad + csz * (b[2] + b[3]) / 2.0 - scy * tf / 2.0;
        let mut w = vec![0f32; t * t];
        for y in 0..t {
            for x in 0..t {
                w[y * t + x] = bilinear(&self.p, scx * (x as f64 + 0.5) + cx, scy * (y as f64 + 0.5) + cy);
            }
        }
        let n = (t * t) as f64;
        let mean = w.iter().map(|&v| v as f64).sum::<f64>() / n;
        let var = w.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n;
        if var.sqrt() < 1.5 {
            return zeros;
        }
        let c = cfg(t);
        let fine = sandwich(&c.fine, &w, t);
        let coarse = sandwich(&c.coarse, &w, t);
        let mut d: Vec<f32> = fine.iter().zip(&coarse).map(|(a, b)| a - b).collect();
        let mut mags: Vec<f32> = d.iter().map(|v| v.abs()).collect();
        let sd = 1.4826 * fingerprint::median_in_place(&mut mags) + 1e-6;
        for (v, &weight) in d.iter_mut().zip(&c.taper) {
            *v = (*v / sd).clamp(-3.0, 3.0) * weight;
        }
        let centre = d.iter().sum::<f32>() / c.taper_sum;
        for (v, &weight) in d.iter_mut().zip(&c.taper) {
            *v -= centre * weight;
        }
        let norm = d.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm <= 1e-4 {
            return zeros;
        }
        for v in d.iter_mut() {
            *v /= norm;
        }
        d
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut lanes = [0f32; 8];
    for (x, y) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0) {
        for k in 0..8 {
            lanes[k] += x[k] * y[k];
        }
    }
    lanes.iter().sum()
}

fn lookup() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| (0..=u16::MAX).map(f16_to_f32).collect())
}

// ------------------------------------------------------------ what is stored

/// What is kept for a photograph, for cropped-copy matching.
pub struct Stored {
    /// The coarse windows, as half-precision numbers (`CROP_STORED_LEN` of them).
    pub windows: Vec<u8>,
    /// A small picture of it, compressed, from which windows are cut afresh.
    pub picture: Vec<u8>,
}

pub fn describe(gray: &GrayImage) -> Stored {
    let big = Big::from_gray(gray);
    let b = windows();
    let flat: Vec<f32> = b.db.iter().flat_map(|w| big.window(w, CT)).collect();
    Stored {
        windows: fingerprint::pack(&flat),
        picture: miniz_oxide::deflate::compress_to_vec_zlib(&big.square_bytes(), 3),
    }
}

fn picture_from_stored(bytes: &[u8]) -> Option<Big> {
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(bytes).ok()?;
    if raw.len() != CSZ * CSZ {
        return None;
    }
    let a: Vec<f32> = raw.iter().map(|&v| v as f32).collect();
    Some(Big::from_square(&a))
}

// ------------------------------------------------------------ the new scan

/// A new scan, prepared once to be tried against every stored photograph.
pub struct Query {
    /// The scan in each of the four quarter turns.
    turns: Vec<Big>,
    /// Its nudged whole views, 16 per turn, as unit vectors.
    nudged: Vec<f32>,
    /// Its windows, 625 per turn.
    cut: Vec<f32>,
}

pub fn query(gray: &GrayImage) -> Query {
    let big = Big::from_gray(gray);
    let b = windows();
    let turns: Vec<Big> = (0..4).map(|t| if t == 0 { Big { p: big.p.clone() } } else { big.turned(t) }).collect();
    let nudged = turns.iter().flat_map(|t| b.nudged.iter().flat_map(|w| t.window(w, CT))).collect();
    let cut = turns.iter().flat_map(|t| b.query.iter().flat_map(|w| t.window(w, CT))).collect();
    Query { turns, nudged, cut }
}

pub(crate) fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    let threads = std::thread::available_parallelism().map(|t| t.get()).unwrap_or(1).min(items.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                let r = f(item);
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|r| r.expect("every item is done")).collect()
}

/// Stage 1: how well each stored photograph fits the scan as a crop of it or a
/// window of it. `stored` holds every photograph's coarse windows, one after
/// another (`CROP_STORED_LEN` each). One score per photograph.
pub fn coarse_scores(q: &Query, stored: &[u16]) -> Vec<f32> {
    let n = stored.len() / CROP_STORED_LEN;
    let b = windows();
    let lut = lookup();
    let d = CT * CT;
    let mut scores = vec![-9f32; n];
    let threads = std::thread::available_parallelism().map(|t| t.get()).unwrap_or(1).min(n.max(1));
    let per = n.div_ceil(threads.max(1)).max(1);
    std::thread::scope(|scope| {
        for (chunk_index, out) in scores.chunks_mut(per).enumerate() {
            scope.spawn(move || {
                let mut photo = vec![0f32; CROP_STORED_LEN];
                for (i, slot) in out.iter_mut().enumerate() {
                    let at = (chunk_index * per + i) * CROP_STORED_LEN;
                    for (dst, &h) in photo.iter_mut().zip(&stored[at..at + CROP_STORED_LEN]) {
                        *dst = lut[h as usize];
                    }
                    let mut best = -9f32;
                    // The whole scan fits a window of the stored photograph.
                    for w in 0..DB_BOXES {
                        let dw = &photo[w * d..(w + 1) * d];
                        let m = q.nudged.chunks_exact(d).map(|v| dot(dw, v)).fold(-9f32, f32::max);
                        best = best.max(m - CA1 * b.trim_db[w]);
                    }
                    // A window of the scan fits the whole stored photograph.
                    let full = &photo[FULL * d..(FULL + 1) * d];
                    for (v, tr) in q.cut.chunks_exact(d).zip(&b.trim_query_x4) {
                        best = best.max(dot(full, v) - CA1 * tr);
                    }
                    *slot = best;
                }
            });
        }
    });
    scores
}

// ------------------------------------------------------------ stage 2

/// Where a window of the scan was laid on a window of the stored photograph.
struct Layout {
    turn: usize,
    scan: Window,
    stored: Window,
}

/// The `n` best (score, index) pairs, best first.
fn best_few(scores: impl Iterator<Item = f32>, n: usize) -> Vec<(f32, usize)> {
    let mut top: Vec<(f32, usize)> = Vec::with_capacity(n + 1);
    for (i, s) in scores.enumerate() {
        if top.len() < n || s > top[top.len() - 1].0 {
            let at = top.partition_point(|&(v, _)| v >= s);
            top.insert(at, (s, i));
            top.truncate(n);
        }
    }
    top
}

/// The most promising ways of laying the scan on one stored photograph.
fn promising_layouts(q: &Query, windows_of_stored: &[f32], n: usize) -> Vec<(f32, Layout)> {
    let b = windows();
    let d = CT * CT;
    let nudged_rows = q.nudged.len() / d;
    let a = (0..nudged_rows * DB_BOXES).map(|k| {
        let (r, w) = (k / DB_BOXES, k % DB_BOXES);
        dot(&q.nudged[r * d..(r + 1) * d], &windows_of_stored[w * d..(w + 1) * d]) - CA1 * b.trim_db[w]
    });
    let full = &windows_of_stored[FULL * d..(FULL + 1) * d];
    let c = q.cut.chunks_exact(d).zip(&b.trim_query_x4).map(|(v, tr)| dot(v, full) - CA1 * tr);
    let mut out: Vec<(f32, Layout)> = Vec::new();
    for (s, k) in best_few(a, n) {
        let (r, w) = (k / DB_BOXES, k % DB_BOXES);
        out.push((s, Layout { turn: r / NUDGED_PER_TURN, scan: b.nudged[r % NUDGED_PER_TURN], stored: b.db[w] }));
    }
    for (s, k) in best_few(c, n) {
        out.push((s, Layout { turn: k / QUERY_PER_TURN, scan: b.query[k % QUERY_PER_TURN], stored: b.db[FULL] }));
    }
    out.sort_by(|x, y| y.0.total_cmp(&x.0));
    out.truncate(n);
    out
}

/// Lines two pictures up precisely: slides the window edges of both, ever more
/// finely, until the fit stops improving. Returns the fit.
fn refine(scan: &Big, stored: &Big, start: &Layout) -> f32 {
    let ok = |b: &Window| {
        b[1] - b[0] >= CMIN && b[3] - b[2] >= CMIN && b[0].min(b[2]) >= -0.08 && b[1].max(b[3]) <= 1.08
    };
    let (mut qb, mut sb) = (start.scan, start.stored);
    let (mut vq, mut vs) = (scan.window(&qb, FT), stored.window(&sb, FT));
    let mut cur = dot(&vq, &vs) - CA2 * (trim(&qb) + trim(&sb));
    for step in [0.04, 0.02, 0.01] {
        for _ in 0..3 {
            let mut moved = false;
            for edge in 0..8 {
                let e = edge % 4;
                for d in [-step, step] {
                    let (mine, other_box, other_v, pic) =
                        if edge < 4 { (&mut qb, &sb, &vs, scan) } else { (&mut sb, &qb, &vq, stored) };
                    let mut nb = *mine;
                    nb[e] += d;
                    if !ok(&nb) {
                        continue;
                    }
                    let nv = pic.window(&nb, FT);
                    let val = dot(&nv, other_v) - CA2 * (trim(&nb) + trim(other_box));
                    if val > cur + 1e-4 {
                        cur = val;
                        mine[e] = nb[e];
                        moved = true;
                        if edge < 4 {
                            vq = nv;
                        } else {
                            vs = nv;
                        }
                    }
                }
            }
            if !moved {
                break;
            }
        }
    }
    cur
}

/// A precise fit on the 0 to 1 scale the other scores use: nothing below 0.80.
pub fn report(fit: f32) -> f32 {
    if fit < 0.80 {
        0.0
    } else {
        0.60 + ((fit - 0.80) / 0.15).min(1.0) * 0.40
    }
}

/// A stored photograph that stage 1 found promising, to be looked at closely.
pub struct Candidate {
    /// Which photograph, as the caller numbers them.
    pub index: usize,
    /// Its coarse windows, half-precision, `CROP_STORED_LEN` of them.
    pub windows: Vec<u16>,
    /// Its stored picture, compressed.
    pub picture: Vec<u8>,
}

/// Stage 2: the closer look. Returns each candidate's score on the 0 to 1
/// scale (0 where no precise fit was found), by its `index`.
pub fn close_look(q: &Query, candidates: &[Candidate]) -> Vec<(usize, f32)> {
    let lut = lookup();
    // Each candidate's best starting position.
    let starts: Vec<Option<(f32, Big, Layout)>> = parallel_map(candidates, |c| {
        let big = picture_from_stored(&c.picture)?;
        if c.windows.len() != CROP_STORED_LEN {
            return None;
        }
        let w: Vec<f32> = c.windows.iter().map(|&h| lut[h as usize]).collect();
        let mut best: Option<(f32, Layout)> = None;
        for (_coarse, layout) in promising_layouts(q, &w, 8) {
            let raw = dot(&q.turns[layout.turn].window(&layout.scan, FT), &big.window(&layout.stored, FT))
                - CA2 * (trim(&layout.scan) + trim(&layout.stored));
            if best.as_ref().is_none_or(|(r, _)| raw > *r) {
                best = Some((raw, layout));
            }
        }
        best.map(|(raw, layout)| (raw, big, layout))
    });
    let mut order: Vec<usize> = (0..candidates.len()).filter(|&i| starts[i].is_some()).collect();
    order.sort_by(|&a, &b| starts[b].as_ref().unwrap().0.total_cmp(&starts[a].as_ref().unwrap().0));
    order.truncate(CROP_REFINE);
    let fits = parallel_map(&order, |&i| {
        let (_, big, layout) = starts[i].as_ref().unwrap();
        report(refine(&q.turns[layout.turn], big, layout))
    });
    order.iter().zip(fits).map(|(&i, r)| (candidates[i].index, r)).collect()
}

/// The `k` best candidates from stage 1, by index, best first.
pub fn shortlist(scores: &[f32], k: usize) -> Vec<usize> {
    best_few(scores.iter().copied(), k.min(scores.len())).into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::test_pictures::picture;
    use image::imageops::{crop_imm, resize};

    fn flat_windows(pics: &[GrayImage]) -> (Vec<u16>, Vec<Stored>) {
        let stored: Vec<Stored> = pics.iter().map(describe).collect();
        let mut all = Vec::new();
        for s in &stored {
            all.extend(s.windows.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)));
        }
        (all, stored)
    }

    /// Runs both stages for a scan against some pictures; the report for each.
    fn reports(pics: &[GrayImage], scan: &GrayImage) -> Vec<f32> {
        let (all, stored) = flat_windows(pics);
        let q = query(scan);
        let coarse = coarse_scores(&q, &all);
        let cands: Vec<Candidate> = shortlist(&coarse, CROP_K)
            .into_iter()
            .map(|i| Candidate {
                index: i,
                windows: all[i * CROP_STORED_LEN..(i + 1) * CROP_STORED_LEN].to_vec(),
                picture: stored[i].picture.clone(),
            })
            .collect();
        let mut out = vec![0f32; pics.len()];
        for (i, r) in close_look(&q, &cands) {
            out[i] = r;
        }
        out
    }

    fn collection() -> Vec<GrayImage> {
        (1..=8).map(|s| picture(s, 320, 240)).collect()
    }

    #[test]
    fn the_windows_are_the_ones_the_original_uses() {
        let b = windows();
        assert_eq!((b.db.len(), b.query.len(), b.nudged.len()), (64, 625, 16));
        assert_eq!(b.db[FULL], [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(b.trim_db[FULL], 0.0);
        assert_eq!(b.trim_query_x4.len(), 4 * 625);
    }

    #[test]
    fn what_is_stored_has_the_expected_size_and_reads_back() {
        let s = describe(&picture(3, 300, 200));
        assert_eq!(s.windows.len(), CROP_STORED_LEN * 2);
        let back = picture_from_stored(&s.picture).expect("the stored picture reads back");
        assert_eq!(back.p.len(), CPADDED * CPADDED);
        assert!(s.picture.len() < CSZ * CSZ, "it is compressed");
        assert!(picture_from_stored(&[1, 2, 3]).is_none());
    }

    #[test]
    fn the_same_picture_fits_itself_exactly() {
        let pics = collection();
        let r = reports(&pics, &pics[4]);
        assert!(r[4] >= 0.95, "{r:?}");
        for (i, &v) in r.iter().enumerate() {
            if i != 4 {
                assert!(v < 0.60, "unrelated picture {i} reported at {v}");
            }
        }
    }

    #[test]
    fn a_copy_cut_from_the_left_and_top_is_found() {
        let pics = collection();
        let src = &pics[2];
        // Without the left 22 per cent and the top 18 per cent.
        let cropped = crop_imm(src, 70, 43, 250, 197).to_image();
        let r = reports(&pics, &cropped);
        assert!(r[2] >= 0.60, "the cropped copy is found: {r:?}");
        for (i, &v) in r.iter().enumerate() {
            if i != 2 {
                assert!(v < r[2], "picture {i} is not better than the true one: {r:?}");
            }
        }
    }

    #[test]
    fn a_cropped_smaller_turned_copy_is_found() {
        let pics = collection();
        let cropped = crop_imm(&pics[5], 0, 20, 230, 200).to_image();
        let small = resize(&cropped, 150, 117, FilterType::Triangle);
        let turned = image::imageops::rotate270(&small);
        let r = reports(&pics, &turned);
        assert!(r[5] >= 0.60, "{r:?}");
    }

    #[test]
    fn a_scan_that_has_more_than_the_stored_copy_is_found_too() {
        // The stored copy is the tighter one this time.
        let mut pics = collection();
        let full = pics[1].clone();
        pics[1] = crop_imm(&full, 40, 30, 240, 180).to_image();
        let r = reports(&pics, &full);
        assert!(r[1] >= 0.60, "{r:?}");
    }

    #[test]
    fn an_unrelated_picture_reports_nothing() {
        let pics = collection();
        let r = reports(&pics, &picture(777, 320, 240));
        assert!(r.iter().all(|&v| v < 0.60), "{r:?}");
    }

    #[test]
    fn a_blank_scan_matches_nothing() {
        let pics = collection();
        let r = reports(&pics, &GrayImage::from_pixel(200, 200, image::Luma([128])));
        assert!(r.iter().all(|&v| v == 0.0), "{r:?}");
    }

    #[test]
    fn the_report_scale_runs_from_nothing_to_one() {
        assert_eq!(report(0.79), 0.0);
        assert!((report(0.80) - 0.60).abs() < 1e-6);
        assert!((report(0.875) - 0.80).abs() < 1e-6);
        assert_eq!(report(0.95), 1.0);
        assert_eq!(report(1.2), 1.0);
    }

    #[test]
    fn the_shortlist_is_the_best_in_order() {
        assert_eq!(shortlist(&[0.1, 0.9, 0.5, 0.7], 2), vec![1, 3]);
        assert_eq!(shortlist(&[0.1, 0.9], 5), vec![1, 0]);
        assert!(shortlist(&[], 3).is_empty());
    }

    #[test]
    fn candidates_with_damaged_data_are_skipped_not_fatal() {
        let pics = collection();
        let (all, stored) = flat_windows(&pics);
        let q = query(&pics[0]);
        let cands = vec![
            Candidate { index: 0, windows: all[..CROP_STORED_LEN].to_vec(), picture: stored[0].picture.clone() },
            Candidate { index: 1, windows: vec![0; 5], picture: stored[1].picture.clone() },
            Candidate { index: 2, windows: all[..CROP_STORED_LEN].to_vec(), picture: vec![9, 9] },
        ];
        let got = close_look(&q, &cands);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 0);
    }
}
