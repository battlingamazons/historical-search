//! Describing a picture, or a phrase, as 512 numbers that mean "what it looks like".
//!
//! This is OpenAI's CLIP (the ViT-B/16 model, MIT licence). It has two halves
//! that were trained together: one reads a picture, the other reads text, and
//! both write a point in the same 512-dimensional space. A picture and a
//! phrase that describe the same thing land close together, so "men standing
//! in front of a barn" can be looked up among pictures that have no words
//! attached to them, and one picture can be looked up among the others.
//!
//! The models are run here with `tract`, a Rust library, as the face models
//! are, so nothing needs installing and nothing leaves the computer. They are
//! large (about 300 MB), so they are not carried in the installer: the person
//! turns the feature on and the files are fetched once (see `clip_download.rs`).

use crate::clip_tokenizer;
use image::{imageops, RgbImage};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tract_onnx::prelude::*;

/// Length of a description.
pub const EMB_LEN: usize = 512;
/// The model reads pictures of exactly this many pixels across and down.
const SIDE: u32 = 224;
/// What CLIP's training pictures averaged and varied by, per colour.
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];

/// The two files, and where they are kept once downloaded.
pub const IMAGE_MODEL: &str = "vision_model_fp16.onnx";
pub const TEXT_MODEL: &str = "text_model_fp16.onnx";

type Net = Arc<TypedRunnableModel>;

static MODEL_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static IMAGE_NET: Mutex<Option<Net>> = Mutex::new(None);
static TEXT_NET: Mutex<Option<Net>> = Mutex::new(None);

/// Where the downloaded model files are kept. Anything already loaded from
/// somewhere else is dropped.
pub fn set_model_dir(dir: &Path) {
    *MODEL_DIR.lock().unwrap() = Some(dir.to_path_buf());
    *IMAGE_NET.lock().unwrap() = None;
    *TEXT_NET.lock().unwrap() = None;
}

pub fn model_dir() -> Option<PathBuf> {
    MODEL_DIR.lock().unwrap().clone()
}

fn file(name: &str) -> Option<PathBuf> {
    let p = model_dir()?.join(name);
    p.is_file().then_some(p)
}

/// Whether both model files are in place (not whether they are sound: that is
/// settled when they are downloaded, by checksum).
pub fn installed() -> bool {
    file(IMAGE_MODEL).is_some() && file(TEXT_MODEL).is_some()
}

fn load(name: &str, fact: InferenceFact) -> Option<Net> {
    let path = file(name)?;
    let result = (|| -> TractResult<Net> {
        // The published files describe every size as "any", which the fixed
        // sizes given here would contradict, so what they say is ignored.
        let m = tract_onnx::onnx().with_ignore_value_info(true).model_for_path(path)?;
        m.with_input_fact(0, fact)?.into_optimized()?.into_runnable()
    })();
    result.ok()
}

/// A loaded model, loaded once it is wanted; a failure is not remembered, so a
/// file that arrives later is picked up.
fn net(slot: &Mutex<Option<Net>>, name: &str, fact: impl FnOnce() -> InferenceFact) -> Option<Net> {
    let mut slot = slot.lock().unwrap();
    if slot.is_none() {
        *slot = load(name, fact());
    }
    slot.clone()
}

fn normalised(v: Vec<f32>) -> Option<Vec<f32>> {
    if v.len() != EMB_LEN {
        return None;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    (norm.is_finite() && norm > 0.0).then(|| v.into_iter().map(|x| x / norm).collect())
}

/// The picture as the model wants it: the short side shrunk to 224, the middle
/// 224 x 224 kept, colours scaled to the range it was trained on.
pub fn prepare(rgb: &RgbImage) -> tract_ndarray::Array4<f32> {
    let (w, h) = rgb.dimensions();
    let (nw, nh) = if w <= h {
        (SIDE, ((SIDE as u64 * h as u64) / w.max(1) as u64).max(SIDE as u64) as u32)
    } else {
        (((SIDE as u64 * w as u64) / h.max(1) as u64).max(SIDE as u64) as u32, SIDE)
    };
    let small = imageops::resize(rgb, nw, nh, imageops::FilterType::CatmullRom);
    let (left, top) = ((nw - SIDE) / 2, (nh - SIDE) / 2);
    let mut input = tract_ndarray::Array4::<f32>::zeros((1, 3, SIDE as usize, SIDE as usize));
    for y in 0..SIDE {
        for x in 0..SIDE {
            let p = small.get_pixel(left + x, top + y);
            for c in 0..3 {
                input[[0, c, y as usize, x as usize]] = (p[c] as f32 / 255.0 - MEAN[c]) / STD[c];
            }
        }
    }
    input
}

/// 512 numbers, of length one, describing what the picture looks like. `None`
/// when the models are not there or cannot run.
pub fn embed_image(rgb: &RgbImage) -> Option<Vec<f32>> {
    let net = net(&IMAGE_NET, IMAGE_MODEL, || f32::fact([1, 3, SIDE as usize, SIDE as usize]).into())?;
    let out = net.run(tvec!(prepare(rgb).into_tensor().into())).ok()?;
    normalised(out[0].to_plain_array_view::<f32>().ok()?.iter().copied().collect())
}

/// The same for a phrase, in the same space as the pictures.
pub fn embed_text(phrase: &str) -> Option<Vec<f32>> {
    let net = net(&TEXT_NET, TEXT_MODEL, || i64::fact([1, clip_tokenizer::CONTEXT]).into())?;
    let ids = clip_tokenizer::tokenize(phrase).to_vec();
    let input = tract_ndarray::Array2::from_shape_vec((1, clip_tokenizer::CONTEXT), ids).ok()?;
    let out = net.run(tvec!(input.into_tensor().into())).ok()?;
    normalised(out[0].to_plain_array_view::<f32>().ok()?.iter().copied().collect())
}

/// How alike two descriptions are, from -1 to 1 (both are of length one).
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A description stored as bytes (little-endian f32s), and back.
pub fn to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn from_bytes(b: &[u8]) -> Option<Vec<f32>> {
    (b.len() == EMB_LEN * 4).then(|| b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// The model folder is shared by the whole program, so tests that change it
/// (here and in the modules that use the models) take turns.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::sync::{Mutex, MutexGuard};
    static TURN: Mutex<()> = Mutex::new(());
    pub fn one_at_a_time() -> MutexGuard<'static, ()> {
        TURN.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    use super::tests_support::one_at_a_time;

    #[test]
    fn a_description_survives_being_stored() {
        let v: Vec<f32> = (0..EMB_LEN).map(|i| i as f32 * 0.25 - 3.0).collect();
        assert_eq!(from_bytes(&to_bytes(&v)).unwrap(), v);
        assert!(from_bytes(&[1, 2, 3]).is_none());
    }

    #[test]
    fn nothing_is_described_when_the_models_are_not_there() {
        let _turn = one_at_a_time();
        // A folder with no model files in it.
        let dir = std::env::temp_dir().join(format!("hs-clip-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        set_model_dir(&dir);
        assert!(!installed());
        assert!(embed_image(&RgbImage::new(10, 10)).is_none());
        assert!(embed_text("a dog").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_picture_is_prepared_at_the_size_the_model_reads() {
        for (w, h) in [(640u32, 480u32), (300, 900), (224, 224), (50, 40), (3000, 100)] {
            let t = prepare(&RgbImage::from_pixel(w, h, Rgb([255, 255, 255])));
            assert_eq!(t.shape(), &[1, 3, 224, 224], "{w}x{h}");
        }
        // White, scaled the way the model expects.
        let t = prepare(&RgbImage::from_pixel(64, 64, Rgb([255, 255, 255])));
        assert!((t[[0, 0, 5, 5]] - (1.0 - MEAN[0]) / STD[0]).abs() < 1e-4);
    }

    /// Needs the real model files, which are downloaded and not kept in the
    /// repository: point `HS_TEST_CLIP_DIR` at the folder holding them.
    #[cfg(feature = "live-data")]
    mod with_models {
        use super::*;

        fn use_models() -> std::sync::MutexGuard<'static, ()> {
            let turn = one_at_a_time();
            let dir = std::env::var("HS_TEST_CLIP_DIR")
                .expect("set HS_TEST_CLIP_DIR to the folder holding the two CLIP model files (see test-env.sh)");
            set_model_dir(Path::new(&dir));
            assert!(installed(), "the two model files are not in {dir}");
            turn
        }

        /// A picture of horizontal red and blue bands, and one of green dots.
        fn pictures() -> (RgbImage, RgbImage) {
            let bands = RgbImage::from_fn(320, 240, |_, y| if (y / 30) % 2 == 0 { Rgb([200, 30, 30]) } else { Rgb([30, 30, 200]) });
            let dots = RgbImage::from_fn(320, 240, |x, y| if (x / 20 + y / 20) % 2 == 0 { Rgb([20, 160, 40]) } else { Rgb([240, 240, 230]) });
            (bands, dots)
        }

        #[test]
        fn descriptions_have_the_right_length_and_are_of_length_one() {
            let _turn = use_models();
            let (bands, _) = pictures();
            for v in [embed_image(&bands).unwrap(), embed_text("a photograph of a barn").unwrap()] {
                assert_eq!(v.len(), EMB_LEN);
                assert!((v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-3);
            }
        }

        #[test]
        fn the_same_picture_is_described_the_same_and_a_different_one_differently() {
            let _turn = use_models();
            let (bands, dots) = pictures();
            let (a, b, c) = (embed_image(&bands).unwrap(), embed_image(&bands).unwrap(), embed_image(&dots).unwrap());
            assert!(similarity(&a, &b) > 0.9999);
            assert!(similarity(&a, &c) < 0.95, "two unlike pictures came out alike: {}", similarity(&a, &c));
        }

        /// Measurement, not a pass/fail check of behaviour: how closely this
        /// reproduces the descriptions the Python prototype stored for 200 of
        /// the collection's pictures. Prints only numbers.
        /// `cargo test --release --lib clip_agrees_with_the_prototype -- --ignored --nocapture`
        #[test]
        #[ignore]
        fn clip_agrees_with_the_prototype() {
            let _turn = use_models();
            let home = std::env::var("HOME").unwrap();
            let dir = Path::new(&home).join("photo-search");
            let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
            let rows = meta.as_array().unwrap().len();
            let npy = std::fs::read(dir.join("vecs.npy")).unwrap();
            let data = &npy[npy.len() - rows * EMB_LEN * 4..];
            let mut sims = Vec::new();
            for i in (0..rows).step_by(rows / 200) {
                let thumb = dir.join(meta[i]["thumb"].as_str().unwrap());
                let Ok(img) = image::open(&thumb) else { continue };
                let stored = normalised(from_bytes(&data[i * EMB_LEN * 4..(i + 1) * EMB_LEN * 4]).unwrap()).unwrap();
                sims.push(similarity(&embed_image(&img.to_rgb8()).unwrap(), &stored));
            }
            sims.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = sims.iter().sum::<f32>() / sims.len() as f32;
            println!("pictures {} mean similarity {:.5} lowest {:.5} median {:.5}", sims.len(), mean, sims[0], sims[sims.len() / 2]);
            assert!(mean > 0.99);
        }

        #[test]
        fn words_land_nearer_a_picture_that_matches_them() {
            let _turn = use_models();
            // Plain colour fields are the one thing a test can draw, and CLIP
            // reads them well enough to tell "red" from "green".
            let red = embed_image(&RgbImage::from_pixel(224, 224, Rgb([220, 20, 20]))).unwrap();
            let green = embed_image(&RgbImage::from_pixel(224, 224, Rgb([20, 200, 40]))).unwrap();
            let say_red = embed_text("a solid red picture").unwrap();
            let say_green = embed_text("a solid green picture").unwrap();
            assert!(similarity(&say_red, &red) > similarity(&say_red, &green));
            assert!(similarity(&say_green, &green) > similarity(&say_green, &red));
        }
    }
}
