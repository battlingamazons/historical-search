//! The look-alike fingerprint: a way to tell that two scans are of the same
//! photograph when they are not the same file.
//!
//! Two scans of one print differ in brightness, fading and yellowing, in how
//! the scanner framed them, in size, in file type, in a few degrees of skew or
//! a quarter turn. None of that changes what is *in* the picture, so the
//! fingerprint keeps only the picture's structure:
//!
//! * the picture is shrunk to a small grey square, and any flat border (white
//!   paper, scanner bed) is painted over with the picture's own edge, so that a
//!   frame is never mistaken for part of the photograph;
//! * a small view of it is taken, a fine blur subtracted from a coarse one
//!   (which removes overall brightness and slow tints and keeps edges and
//!   shapes), the result scaled and clipped so no single bright spot rules,
//!   faded to nothing towards the edges where frames live, and made unit
//!   length;
//! * a stored photograph keeps four such views, whole and then three centre
//!   crops of increasing tightness, so a border of any usual width on either
//!   copy can be cut away before comparing;
//! * a new scan is tried at a grid of slight zooms and shifts, in all four
//!   quarter turns, and the best-fitting pairing counts.
//!
//! The score runs from about 0 to 1, with 1 for the same picture. Its meaning
//! was settled by the program this was ported from, on the museum's own scans:
//! 0.80 and above is a likely duplicate, 0.66 and above a possible match.
//!
//! This is the first of the matching passes. The next ones (a scan cropped
//! more tightly than the stored copy, and a very small crop found through the
//! faces in it) are separate, and build on this.
//!
//! The arithmetic follows the original closely enough that scores agree to
//! about the second decimal place; it is not bit-for-bit, since the picture
//! libraries underneath resize slightly differently.

use image::imageops::FilterType;
use image::{DynamicImage, GrayImage};

/// Side of the grey square a picture is shrunk to.
const SIZE: usize = 96;
/// Edge padding, so that zooming out has something to sample.
const PAD: usize = 48;
const PADDED: usize = SIZE + 2 * PAD;
/// Side of a view; a descriptor holds `T * T` numbers.
const T: usize = 32;
pub const DESC_LEN: usize = T * T;

/// The zoom of each stored view of a photograph.
pub const DB_WINDOWS: [f32; 4] = [1.0, 0.9, 0.8, 0.7];
/// Numbers in all of a photograph's stored views.
pub const STORED_LEN: usize = DB_WINDOWS.len() * DESC_LEN;

/// Zoom IN only: zooming out would drag a frame into view.
const QUERY_SCALES: [f32; 6] = [0.74, 0.8, 0.86, 0.92, 0.96, 1.0];
const QUERY_OFFSETS: [f32; 3] = [-0.04, 0.0, 0.04];
/// A small penalty so that an exact alignment beats a lucky offset one.
const LAM: f32 = 0.25;
const MU: f32 = 0.5;

/// Weights of the fine and coarse blurs.
const SIGMA_FINE: f64 = 0.8;
const SIGMA_COARSE: f64 = 2.2;
/// Each position is weighted by its distance from the centre: full in the
/// middle, fading to zero towards the edge, where borders and frames are.
const TAPER_FLAT: f64 = 0.20;
const TAPER_END: f64 = 0.36;

/// A border may take up to this fraction of each side.
const FRAME_MAX: f64 = 0.22;
/// Grey-level spread below which a line of the edge counts as flat.
const FLAT_TOLERANCE: f32 = 3.0;

/// How many picture sizes of the stored views of each photograph are kept.
pub const VIEWS_PER_PHOTO: usize = DB_WINDOWS.len();

// ------------------------------------------------------------ constants

struct Tables {
    fine: [f32; T * T],
    coarse: [f32; T * T],
    taper: [f32; T * T],
    taper_sum: f32,
}

fn tables() -> &'static Tables {
    static TABLES: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let gauss = |sigma: f64| {
            let mut m = [0f32; T * T];
            for i in 0..T {
                let mut row = [0f64; T];
                let mut sum = 0.0;
                for (j, cell) in row.iter_mut().enumerate() {
                    let d = i as f64 - j as f64;
                    *cell = (-(d * d) / (2.0 * sigma * sigma)).exp();
                    sum += *cell;
                }
                for j in 0..T {
                    m[i * T + j] = (row[j] / sum) as f32;
                }
            }
            m
        };
        let ramp: Vec<f64> = (0..T)
            .map(|i| {
                let r = ((i as f64 + 0.5) / T as f64 - 0.5).abs();
                if r <= TAPER_FLAT {
                    1.0
                } else if r >= TAPER_END {
                    0.0
                } else {
                    (std::f64::consts::FRAC_PI_2 * (r - TAPER_FLAT) / (TAPER_END - TAPER_FLAT)).cos().powi(2)
                }
            })
            .collect();
        let mut taper = [0f32; T * T];
        for y in 0..T {
            for x in 0..T {
                taper[y * T + x] = (ramp[y] * ramp[x]) as f32;
            }
        }
        let taper_sum = taper.iter().sum();
        Tables { fine: gauss(SIGMA_FINE), coarse: gauss(SIGMA_COARSE), taper, taper_sum }
    })
}

// ------------------------------------------------------------ the picture

/// A picture as the grey image the fingerprint is made from.
///
/// Shrunk first to fit 512 pixels, as the original does, which also keeps a
/// very large scan from making everything after it slow. Grey by the same
/// weights as Pillow's (and so the original's) conversion, not the `image`
/// crate's different ones, so a colour photograph is fingerprinted the same.
pub fn gray_for_matching(img: &DynamicImage) -> GrayImage {
    let rgb = if img.width() > 512 || img.height() > 512 {
        img.resize(512, 512, FilterType::Lanczos3).to_rgb8()
    } else {
        img.to_rgb8()
    };
    let mut gray = GrayImage::new(rgb.width(), rgb.height());
    for (src, dst) in rgb.pixels().zip(gray.pixels_mut()) {
        let [r, g, b] = src.0;
        dst.0[0] = ((r as u32 * 19595 + g as u32 * 38470 + b as u32 * 7471 + 0x8000) >> 16) as u8;
    }
    gray
}

/// The grey square, with any flat border painted over, padded for sampling.
fn prepare(gray: &GrayImage) -> Vec<f32> {
    let small = image::imageops::resize(gray, SIZE as u32, SIZE as u32, FilterType::Lanczos3);
    let mut a: Vec<f32> = small.pixels().map(|p| p.0[0] as f32).collect();
    strip_frame(&mut a, SIZE);
    // Padded by repeating the edge.
    let mut p = vec![0f32; PADDED * PADDED];
    for y in 0..PADDED {
        let sy = y.saturating_sub(PAD).min(SIZE - 1);
        for x in 0..PADDED {
            let sx = x.saturating_sub(PAD).min(SIZE - 1);
            p[y * PADDED + x] = a[sy * SIZE + sx];
        }
    }
    p
}

/// How many lines from the top are flat: one colour, as a white border or the
/// scanner bed is. `line(k)` gives the k-th line in from the edge.
fn flat_run(n: usize, line: &dyn Fn(usize) -> Vec<f32>) -> usize {
    let (lo, hi) = ((0.2 * n as f64) as usize, (0.8 * n as f64) as usize);
    let limit = (FRAME_MAX * n as f64) as usize;
    let mut k = 0;
    while k < limit {
        let l = line(k);
        let seg = &l[lo..hi];
        let median = median(seg);
        let mut dev: Vec<f32> = seg.iter().map(|v| (v - median).abs()).collect();
        if percentile(&mut dev, 80.0) < FLAT_TOLERANCE {
            k += 1;
        } else {
            break;
        }
    }
    k
}

fn median(values: &[f32]) -> f32 {
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// The q-th percentile, interpolating linearly between neighbours.
fn percentile(values: &mut [f32], q: f64) -> f32 {
    values.sort_by(|a, b| a.total_cmp(b));
    let pos = q / 100.0 * (values.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    let frac = (pos - lo as f64) as f32;
    values[lo] + (values[hi] - values[lo]) * frac
}

/// Paints over flat borders, so only the photograph itself is described.
///
/// A border is overwritten by repeating the first real line outward, so the
/// picture keeps its position and no artificial edge is left to be matched.
fn strip_frame(a: &mut [f32], n: usize) {
    let src = a.to_vec();
    let at = |y: usize, x: usize| src[y * n + x];
    let top = flat_run(n, &|k| (0..n).map(|x| at(k, x)).collect());
    let bottom = flat_run(n, &|k| (0..n).map(|x| at(n - 1 - k, x)).collect());
    let left = flat_run(n, &|k| (0..n).map(|y| at(y, k)).collect());
    let right = flat_run(n, &|k| (0..n).map(|y| at(y, n - 1 - k)).collect());
    if top >= 2 {
        for y in 0..top {
            for x in 0..n {
                a[y * n + x] = a[top * n + x];
            }
        }
    }
    if bottom >= 2 {
        for y in n - bottom..n {
            for x in 0..n {
                a[y * n + x] = a[(n - bottom - 1) * n + x];
            }
        }
    }
    if left >= 2 {
        for y in 0..n {
            for x in 0..left {
                a[y * n + x] = a[y * n + left];
            }
        }
    }
    if right >= 2 {
        for y in 0..n {
            for x in n - right..n {
                a[y * n + x] = a[y * n + n - right - 1];
            }
        }
    }
}

/// A quarter turn anticlockwise.
fn rot90(p: &[f32], n: usize) -> Vec<f32> {
    let mut out = vec![0f32; n * n];
    for i in 0..n {
        for j in 0..n {
            out[i * n + j] = p[j * n + (n - 1 - i)];
        }
    }
    out
}

// ------------------------------------------------------------ one view

/// A view of the padded picture zoomed by `s` and shifted by (`dx`, `dy`) of
/// its width: sampled down to a small square, then made into a unit-length
/// description of its structure. All zeros for a view with nothing in it.
fn window(p: &[f32], s: f32, dx: f32, dy: f32) -> [f32; DESC_LEN] {
    let zeros = [0f32; DESC_LEN];
    let size = SIZE as f64;
    let sc = size * s as f64 / T as f64;
    let c = PAD as f64 + size / 2.0 + dx as f64 * size - sc * T as f64 / 2.0;
    let f = PAD as f64 + size / 2.0 + dy as f64 * size - sc * T as f64 / 2.0;

    let mut w = [0f32; DESC_LEN];
    for y in 0..T {
        for x in 0..T {
            w[y * T + x] = bilinear(p, sc * (x as f64 + 0.5) + c, sc * (y as f64 + 0.5) + f);
        }
    }
    let mean = w.iter().map(|&v| v as f64).sum::<f64>() / DESC_LEN as f64;
    let var = w.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / DESC_LEN as f64;
    if var.sqrt() < 1.5 {
        return zeros; // blank or featureless: matches nothing
    }

    let t = tables();
    let fine = sandwich(&t.fine, &w);
    let coarse = sandwich(&t.coarse, &w);
    let mut b = [0f32; DESC_LEN];
    for i in 0..DESC_LEN {
        b[i] = fine[i] - coarse[i];
    }
    let mut mags: Vec<f32> = b.iter().map(|v| v.abs()).collect();
    let sd = 1.4826 * median_in_place(&mut mags) + 1e-6;
    for (v, &weight) in b.iter_mut().zip(&t.taper) {
        *v = (*v / sd).clamp(-3.0, 3.0) * weight;
    }
    let centre = b.iter().sum::<f32>() / t.taper_sum;
    for (v, &weight) in b.iter_mut().zip(&t.taper) {
        *v -= centre * weight;
    }
    let norm = b.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm <= 1e-4 {
        return zeros;
    }
    for v in b.iter_mut() {
        *v /= norm;
    }
    b
}

fn median_in_place(v: &mut [f32]) -> f32 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    (v[n / 2 - 1] + v[n / 2]) / 2.0
}

/// Bilinear sample at (`x`, `y`), the pixel centres being at half-integers; a
/// point outside the picture reads as 0. The same convention as Pillow's
/// affine transform, which the original used.
fn bilinear(p: &[f32], x: f64, y: f64) -> f32 {
    let n = PADDED as f64;
    if x < 0.0 || x >= n || y < 0.0 || y >= n {
        return 0.0;
    }
    let (x, y) = (x - 0.5, y - 0.5);
    let (x0, y0) = (x.floor(), y.floor());
    let (dx, dy) = (x - x0, y - y0);
    let last = PADDED as i64 - 1;
    let cx = |v: f64| (v as i64).clamp(0, last) as usize;
    let (xa, xb, ya, yb) = (cx(x0), cx(x0 + 1.0), cx(y0), cx(y0 + 1.0));
    let at = |yy: usize, xx: usize| p[yy * PADDED + xx] as f64;
    let top = at(ya, xa) + (at(ya, xb) - at(ya, xa)) * dx;
    let bottom = at(yb, xa) + (at(yb, xb) - at(yb, xa)) * dx;
    (top + (bottom - top) * dy) as f32
}

/// `m * w * m^T` for 32 x 32 matrices.
fn sandwich(m: &[f32; DESC_LEN], w: &[f32; DESC_LEN]) -> [f32; DESC_LEN] {
    let mut mw = [0f32; DESC_LEN];
    for i in 0..T {
        for k in 0..T {
            let a = m[i * T + k];
            for j in 0..T {
                mw[i * T + j] += a * w[k * T + j];
            }
        }
    }
    let mut out = [0f32; DESC_LEN];
    for i in 0..T {
        for j in 0..T {
            let mut sum = 0f32;
            for k in 0..T {
                sum += mw[i * T + k] * m[j * T + k];
            }
            out[i * T + j] = sum;
        }
    }
    out
}

// ------------------------------------------------------------ what is stored

/// What is stored for a photograph in the collection: its four views.
pub fn describe(gray: &GrayImage) -> Vec<f32> {
    let p = prepare(gray);
    DB_WINDOWS.iter().flat_map(|&s| window(&p, s, 0.0, 0.0)).collect()
}

/// Views are scored sixteen at a time, so their number is rounded up to this.
const BLOCK: usize = 16;
const POSITIONS: usize = 4 * QUERY_SCALES.len() * QUERY_OFFSETS.len() * QUERY_OFFSETS.len();
const BLOCKS: usize = POSITIONS.div_ceil(BLOCK);

/// A new scan, tried at every position it could line up at.
pub struct Query {
    /// `count()` views, one after another.
    views: Vec<f32>,
    scales: Vec<f32>,
    shifts: Vec<f32>,
    /// The same views laid out for fast scoring: block by block, and within a
    /// block number by number with the sixteen views side by side, so that one
    /// number of a stored view meets sixteen views in a single sweep.
    blocked: Vec<f32>,
    /// What each stored view's score is reduced by, for each of the views of a
    /// scan; infinite for the padding at the end, which can then never win.
    penalty: Vec<f32>,
}

impl Query {
    pub fn count(&self) -> usize {
        self.scales.len()
    }
}

/// Every position a new scan is tried at: four quarter turns, each at several
/// zooms and small shifts.
pub fn query(gray: &GrayImage) -> Query {
    let mut p = prepare(gray);
    let mut q = Query {
        views: Vec::new(),
        scales: Vec::new(),
        shifts: Vec::new(),
        blocked: Vec::new(),
        penalty: Vec::new(),
    };
    for turn in 0..4 {
        if turn > 0 {
            p = rot90(&p, PADDED);
        }
        for &s in &QUERY_SCALES {
            for &dx in &QUERY_OFFSETS {
                for &dy in &QUERY_OFFSETS {
                    q.views.extend_from_slice(&window(&p, s, dx, dy));
                    q.scales.push(s);
                    q.shifts.push(dx.abs() + dy.abs());
                }
            }
        }
    }
    q.lay_out();
    q
}

impl Query {
    fn lay_out(&mut self) {
        let count = self.count();
        debug_assert_eq!(count, POSITIONS);
        self.blocked = vec![0f32; BLOCKS * DESC_LEN * BLOCK];
        for v in 0..count {
            let (block, lane) = (v / BLOCK, v % BLOCK);
            for k in 0..DESC_LEN {
                self.blocked[(block * DESC_LEN + k) * BLOCK + lane] = self.views[v * DESC_LEN + k];
            }
        }
        self.penalty = vec![f32::INFINITY; DB_WINDOWS.len() * BLOCKS * BLOCK];
        for (w, &window_scale) in DB_WINDOWS.iter().enumerate() {
            for v in 0..count {
                self.penalty[w * BLOCKS * BLOCK + v] =
                    LAM * (self.scales[v] / window_scale).ln().abs() + MU * self.shifts[v];
            }
        }
    }
}

/// How well a new scan matches one stored photograph (`stored` is what
/// `describe` made): the best pairing of any stored view with any position of
/// the scan, less a small penalty for being zoomed or shifted to get there.
pub fn score(q: &Query, stored: &[f32]) -> f32 {
    debug_assert_eq!(stored.len(), STORED_LEN);
    let mut best = f32::NEG_INFINITY;
    for (k, &window_scale) in DB_WINDOWS.iter().enumerate() {
        let d = &stored[k * DESC_LEN..(k + 1) * DESC_LEN];
        for v in 0..q.count() {
            let view = &q.views[v * DESC_LEN..(v + 1) * DESC_LEN];
            let mut dot = 0f32;
            for i in 0..DESC_LEN {
                dot += d[i] * view[i];
            }
            let penalty = LAM * (q.scales[v] / window_scale).ln().abs() + MU * q.shifts[v];
            best = best.max(dot - penalty);
        }
    }
    best
}

/// The same as `score`, worked out the fast way: a block of sixteen views
/// against each of the four stored views at once, and every product kept in
/// registers until a block is done. Result is the same to rounding.
fn score_fast(q: &Query, stored: &[f32]) -> f32 {
    let mut best = f32::NEG_INFINITY;
    for block in 0..BLOCKS {
        let views = &q.blocked[block * DESC_LEN * BLOCK..(block + 1) * DESC_LEN * BLOCK];
        let mut acc = [[0f32; BLOCK]; 4];
        for (k, view_k) in views.as_chunks::<BLOCK>().0.iter().enumerate() {
            for (w, row) in acc.iter_mut().enumerate() {
                let a = stored[w * DESC_LEN + k];
                for (cell, &v) in row.iter_mut().zip(view_k.iter()) {
                    *cell += a * v;
                }
            }
        }
        for (w, row) in acc.iter().enumerate() {
            let pen = &q.penalty[(w * BLOCKS + block) * BLOCK..(w * BLOCKS + block + 1) * BLOCK];
            for (cell, p) in row.iter().zip(pen) {
                best = best.max(cell - p);
            }
        }
    }
    best
}

/// Scores a new scan against every stored photograph, in order. `stored` holds
/// each photograph's views as half-precision numbers, one after another
/// (`STORED_LEN` each). Spread across the machine's processors.
pub fn score_all(q: &Query, stored: &[u16]) -> Vec<f32> {
    let n = stored.len() / STORED_LEN;
    let mut scores = vec![0f32; n];
    let threads = std::thread::available_parallelism().map(|t| t.get()).unwrap_or(1).min(n.max(1));
    let per = n.div_ceil(threads.max(1)).max(1);
    std::thread::scope(|scope| {
        for (chunk_index, out) in scores.chunks_mut(per).enumerate() {
            scope.spawn(move || {
                let mut photo = vec![0f32; STORED_LEN];
                for (i, slot) in out.iter_mut().enumerate() {
                    let at = (chunk_index * per + i) * STORED_LEN;
                    for (dst, &h) in photo.iter_mut().zip(&stored[at..at + STORED_LEN]) {
                        *dst = f16_to_f32(h);
                    }
                    *slot = score_fast(q, &photo);
                }
            });
        }
    });
    scores
}

// ------------------------------------------------------- compact storage

/// A 32-bit float as the 16-bit "half" form, rounded to nearest. Descriptors
/// are stored this way: half the room, and the lost digits are far below what
/// a score can show.
pub fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00; // too large: infinity
    }
    if e <= 0 {
        if e < -10 {
            return sign; // too small: zero
        }
        let m = (mant | 0x80_0000) >> (1 - e);
        let rounded = (m + 0x1000 - 1 + ((m >> 13) & 1)) >> 13;
        return sign | rounded as u16;
    }
    let m = mant + 0xfff + ((mant >> 13) & 1);
    if m & 0x80_0000 != 0 {
        // The rounding carried into the exponent.
        let e = e + 1;
        return if e >= 0x1f { sign | 0x7c00 } else { sign | ((e as u16) << 10) };
    }
    sign | ((e as u16) << 10) | (m >> 13) as u16
}

pub fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // A subnormal half is a normal float.
            let shift = mant.leading_zeros() - 21;
            sign | ((127 - 15 - shift + 1) << 23) | (((mant << shift) & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | ((exp + 127 - 15) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// A photograph's stored views as bytes, for the database.
pub fn pack(stored: &[f32]) -> Vec<u8> {
    stored.iter().flat_map(|&v| f32_to_f16(v).to_le_bytes()).collect()
}

/// The reverse of `pack`; `None` for bytes that are not a whole set of views.
pub fn unpack(bytes: &[u8]) -> Option<Vec<f32>> {
    (bytes.len() == STORED_LEN * 2)
        .then(|| bytes.as_chunks::<2>().0.iter().map(|c| f16_to_f32(u16::from_le_bytes(*c))).collect())
}

#[cfg(test)]
pub(crate) mod test_pictures {
    use image::GrayImage;

    /// A made-up "photograph": soft shapes of different brightness on a
    /// gradient, from a seed, so that two seeds give two unrelated pictures.
    pub fn picture(seed: u32, w: u32, h: u32) -> GrayImage {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(12345);
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32
        };
        let shapes: Vec<(f32, f32, f32, f32)> =
            (0..14).map(|_| (next() * w as f32, next() * h as f32, 8.0 + next() * 40.0, next() * 255.0)).collect();
        let tilt = next() * 0.4;
        GrayImage::from_fn(w, h, |x, y| {
            let mut v = 90.0 + tilt * (x as f32 - y as f32) * 0.3;
            for &(cx, cy, r, level) in &shapes {
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                if d < r {
                    v = v * 0.2 + level * 0.8;
                }
            }
            image::Luma([v.clamp(0.0, 255.0) as u8])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_pictures::picture;
    use super::*;

    fn similarity(a: &GrayImage, b: &GrayImage) -> f32 {
        score(&query(a), &describe(b))
    }

    #[test]
    fn a_picture_matches_itself() {
        let a = picture(1, 220, 160);
        let s = similarity(&a, &a);
        assert!(s > 0.99, "{s}");
    }

    #[test]
    fn brightness_and_contrast_do_not_matter() {
        let a = picture(2, 220, 160);
        let faded = GrayImage::from_fn(a.width(), a.height(), |x, y| {
            image::Luma([(a.get_pixel(x, y).0[0] as f32 * 0.55 + 70.0) as u8])
        });
        assert!(similarity(&faded, &a) > 0.95, "{}", similarity(&faded, &a));
    }

    #[test]
    fn a_different_picture_does_not_match() {
        let a = picture(3, 220, 160);
        let b = picture(4, 220, 160);
        let s = similarity(&a, &b);
        assert!(s < 0.5, "{s}");
    }

    #[test]
    fn a_smaller_copy_matches() {
        let a = picture(5, 400, 300);
        let small = image::imageops::resize(&a, 150, 112, FilterType::Triangle);
        let s = similarity(&small, &a);
        assert!(s > 0.9, "{s}");
    }

    #[test]
    fn a_quarter_turn_matches() {
        let a = picture(6, 220, 160);
        let turned = image::imageops::rotate90(&a);
        let s = similarity(&turned, &a);
        assert!(s > 0.9, "{s}");
        let back = image::imageops::rotate270(&a);
        assert!(similarity(&back, &a) > 0.9);
    }

    #[test]
    fn a_scanner_border_does_not_stop_a_match() {
        let a = picture(7, 220, 160);
        let mut framed = GrayImage::from_pixel(220 + 60, 160 + 44, image::Luma([250]));
        image::imageops::replace(&mut framed, &a, 30, 22);
        let s = similarity(&framed, &a);
        assert!(s > 0.85, "{s}");
    }

    #[test]
    fn two_unrelated_pictures_with_the_same_border_do_not_match() {
        // The reason frames are painted over: a shared border must never make
        // two different photographs look alike.
        let frame = |seed| {
            let a = picture(seed, 220, 160);
            let mut framed = GrayImage::from_pixel(300, 210, image::Luma([248]));
            image::imageops::replace(&mut framed, &a, 40, 25);
            framed
        };
        let s = similarity(&frame(8), &frame(9));
        assert!(s < 0.5, "{s}");
    }

    #[test]
    fn noise_and_a_little_blur_do_not_stop_a_match() {
        let a = picture(10, 300, 220);
        let mut state = 7u32;
        let noisy = GrayImage::from_fn(a.width(), a.height(), |x, y| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((state >> 16) % 21) as f32 - 10.0;
            image::Luma([(a.get_pixel(x, y).0[0] as f32 + n).clamp(0.0, 255.0) as u8])
        });
        let blurred = image::imageops::blur(&noisy, 1.2);
        let s = similarity(&blurred, &a);
        assert!(s > 0.9, "{s}");
    }

    #[test]
    fn a_slightly_trimmed_copy_still_matches() {
        let a = picture(11, 300, 220);
        let trimmed = image::imageops::crop_imm(&a, 9, 7, 282, 206).to_image();
        let s = similarity(&trimmed, &a);
        assert!(s > 0.85, "{s}");
    }

    #[test]
    fn a_blank_picture_matches_nothing_not_even_itself() {
        let blank = GrayImage::from_pixel(200, 150, image::Luma([128]));
        let other = picture(12, 200, 150);
        assert!(describe(&blank).iter().all(|&v| v == 0.0));
        assert!(similarity(&blank, &other) <= 0.0 + 1e-6);
        assert!(similarity(&other, &blank) <= 0.0 + 1e-6);
    }

    #[test]
    fn a_descriptor_has_four_unit_length_views() {
        let d = describe(&picture(13, 220, 160));
        assert_eq!(d.len(), STORED_LEN);
        for view in d.chunks(DESC_LEN) {
            let norm = view.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-3 || norm == 0.0, "{norm}");
        }
    }

    #[test]
    fn a_query_covers_four_turns_six_zooms_and_nine_shifts() {
        let q = query(&picture(14, 220, 160));
        assert_eq!(q.count(), 4 * 6 * 9);
        assert_eq!(q.views.len(), q.count() * DESC_LEN);
    }

    #[test]
    fn four_quarter_turns_come_back_to_the_start() {
        let p: Vec<f32> = (0..PADDED * PADDED).map(|i| (i % 251) as f32).collect();
        let mut r = p.clone();
        for _ in 0..4 {
            r = rot90(&r, PADDED);
        }
        assert_eq!(r, p);
        // And one turn is anticlockwise: the top-right corner goes to the top-left.
        let once = rot90(&p, PADDED);
        assert_eq!(once[0], p[PADDED - 1]);
    }

    #[test]
    fn a_flat_border_is_painted_over_with_the_picture_s_own_edge() {
        let n = 96;
        let mut a = vec![0f32; n * n];
        for y in 0..n {
            for x in 0..n {
                a[y * n + x] = if y < 10 { 255.0 } else { ((x * 5 + y * 3) % 200) as f32 };
            }
        }
        let first_real_row: Vec<f32> = a[10 * n..11 * n].to_vec();
        strip_frame(&mut a, n);
        assert_eq!(&a[0..n], &first_real_row[..], "the top border now repeats the first real line");
        assert_eq!(&a[9 * n..10 * n], &first_real_row[..]);
    }

    #[test]
    fn a_picture_with_no_border_is_left_alone() {
        let n = 96;
        let a: Vec<f32> = (0..n * n).map(|i| ((i * 37) % 251) as f32).collect();
        let mut b = a.clone();
        strip_frame(&mut b, n);
        assert_eq!(a, b);
    }

    #[test]
    fn the_blur_rows_each_add_up_to_one_and_the_taper_is_symmetric() {
        let t = tables();
        for i in 0..T {
            let fine: f32 = t.fine[i * T..(i + 1) * T].iter().sum();
            let coarse: f32 = t.coarse[i * T..(i + 1) * T].iter().sum();
            assert!((fine - 1.0).abs() < 1e-5 && (coarse - 1.0).abs() < 1e-5);
        }
        for y in 0..T {
            for x in 0..T {
                assert!((t.taper[y * T + x] - t.taper[(T - 1 - y) * T + (T - 1 - x)]).abs() < 1e-6);
            }
        }
        assert_eq!(t.taper[0], 0.0, "nothing counts at the very edge");
        assert!(t.taper[(T / 2) * T + T / 2] > 0.99, "everything counts in the middle");
    }

    #[test]
    fn the_median_and_percentile_agree_with_the_usual_definitions() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        let mut v = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&mut v, 80.0), 4.2);
    }

    #[test]
    fn half_precision_round_trips_closely() {
        for &v in &[0.0f32, 1.0, -1.0, 0.5, 0.123456, -0.0314159, 0.00001, 65504.0] {
            let back = f16_to_f32(f32_to_f16(v));
            assert!((back - v).abs() <= v.abs() * 1.0e-3 + 1.0e-6, "{v} -> {back}");
        }
        assert_eq!(f16_to_f32(f32_to_f16(f32::INFINITY)), f32::INFINITY);
        assert!(f16_to_f32(f32_to_f16(f32::NAN)).is_nan());
        // Every half-precision number survives a trip through f32 unchanged.
        for h in 0..=u16::MAX {
            let v = f16_to_f32(h);
            if v.is_nan() {
                continue;
            }
            assert_eq!(f32_to_f16(v), h, "{h:#06x} -> {v}");
        }
        assert_eq!(f32_to_f16(1.0), 0x3c00, "the standard encoding of one");
        assert_eq!(f32_to_f16(-2.0), 0xc000);
    }

    #[test]
    fn stored_and_reloaded_views_score_the_same_to_two_places() {
        let a = picture(15, 220, 160);
        let d = describe(&a);
        let reloaded = unpack(&pack(&d)).unwrap();
        let q = query(&a);
        assert!((score(&q, &d) - score(&q, &reloaded)).abs() < 0.005);
    }

    #[test]
    fn bytes_that_are_not_a_whole_set_of_views_are_refused() {
        assert!(unpack(&[0u8; 10]).is_none());
        assert!(unpack(&vec![0u8; STORED_LEN * 2]).is_some());
    }

    #[test]
    fn a_colour_picture_is_made_grey_by_the_usual_weights() {
        let rgb = image::RgbImage::from_pixel(10, 10, image::Rgb([200, 100, 50]));
        let g = gray_for_matching(&DynamicImage::ImageRgb8(rgb));
        // 0.299 * 200 + 0.587 * 100 + 0.114 * 50 = 124.2
        assert_eq!(g.get_pixel(0, 0).0[0], 124);
    }

    #[test]
    fn a_large_picture_is_shrunk_to_fit_before_anything_else() {
        let big = DynamicImage::ImageLuma8(GrayImage::from_pixel(2000, 1000, image::Luma([100])));
        let g = gray_for_matching(&big);
        assert_eq!((g.width(), g.height()), (512, 256));
    }

    #[test]
    fn the_fast_scorer_agrees_with_the_plain_one() {
        let pictures: Vec<GrayImage> = (20..28).map(|seed| picture(seed, 200, 150)).collect();
        let stored: Vec<Vec<f32>> = pictures.iter().map(describe).collect();
        let q = query(&image::imageops::rotate90(&pictures[3]));
        let half: Vec<u16> = stored.iter().flat_map(|d| d.iter().map(|&v| f32_to_f16(v))).collect();
        let fast = score_all(&q, &half);
        for (i, d) in stored.iter().enumerate() {
            let plain = score(&q, d);
            assert!((fast[i] - plain).abs() < 0.005, "photo {i}: fast {} plain {plain}", fast[i]);
        }
        let best = fast.iter().cloned().enumerate().max_by(|a, b| a.1.total_cmp(&b.1)).unwrap().0;
        assert_eq!(best, 3, "the turned copy finds its original");
    }

    #[test]
    fn scoring_an_empty_collection_gives_nothing() {
        let q = query(&picture(30, 120, 90));
        assert!(score_all(&q, &[]).is_empty());
    }

    #[test]
    fn the_padding_after_the_last_view_can_never_win() {
        let q = query(&picture(31, 120, 90));
        for w in 0..DB_WINDOWS.len() {
            for v in q.count()..BLOCKS * BLOCK {
                assert!(q.penalty[w * BLOCKS * BLOCK + v].is_infinite());
            }
        }
    }
}
