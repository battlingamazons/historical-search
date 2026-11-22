//! Fetching the two picture-search models, once, when the person asks for them.
//!
//! They are about 300 MB together, so they are not carried in the installer.
//! This is the only thing in the program that uses the internet, and it does so
//! only when the person has turned picture search on and pressed the button.
//!
//! Each file comes from a fixed revision of a public Hugging Face repository
//! (`Xenova/clip-vit-base-patch16`, OpenAI's CLIP, MIT licence, converted to
//! ONNX with half-precision weights) and is checked against its SHA-256 before
//! it is kept. A file that is not exactly the one expected -- cut short,
//! changed, or replaced -- is thrown away and never used.
//!
//! A download that is interrupted leaves a partial file, and the next attempt
//! carries on from where it stopped.

use crate::clip::{IMAGE_MODEL, TEXT_MODEL};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// A file to fetch, and exactly what it must turn out to be.
#[derive(Debug, Clone)]
pub struct ModelFile {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

/// The repository revision the files below belong to.
const REVISION: &str = "342fdf2f67aded64d138ff074745fb4a5d2bba5f";

/// The files, as published.
pub fn model_files() -> Vec<ModelFile> {
    let url = |name: &str| format!("https://huggingface.co/Xenova/clip-vit-base-patch16/resolve/{REVISION}/onnx/{name}");
    vec![
        ModelFile {
            name: IMAGE_MODEL.into(),
            url: url(IMAGE_MODEL),
            size: 172_768_684,
            sha256: "3e261924f7d66d04f10bb131f7f212162d9ee82199544771aa2dd318bb7e2b7b".into(),
        },
        ModelFile {
            name: TEXT_MODEL.into(),
            url: url(TEXT_MODEL),
            size: 127_339_794,
            sha256: "9e423931894ecbfc018894006883082edf010fce63f9cba02ffb6659582d8101".into(),
        },
    ]
}

/// Bytes to fetch in all, for a progress bar.
pub fn total_size() -> u64 {
    model_files().iter().map(|f| f.size).sum()
}

fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Fetches whatever of `files` is not already in `dir`, telling `progress` how
/// many bytes of all of them are in place. Stops quietly with an error when
/// `cancel` is set. Files already there are trusted only because they were
/// checked when they arrived.
pub fn download(
    files: &[ModelFile],
    dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("could not make the folder for the picture-search files: {e}"))?;
    let total: u64 = files.iter().map(|f| f.size).sum();
    let mut done_before = 0u64;
    progress(0, total);
    for file in files {
        let target = dir.join(&file.name);
        let present = fs::metadata(&target).map(|m| m.len() == file.size).unwrap_or(false);
        if !present {
            fetch_one(file, dir, done_before, total, progress, cancel)?;
        }
        done_before += file.size;
        progress(done_before, total);
    }
    Ok(())
}

fn fetch_one(
    file: &ModelFile,
    dir: &Path,
    done_before: u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<(), String> {
    let part = dir.join(format!("{}.part", file.name));
    let target = dir.join(&file.name);
    let mut have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > file.size {
        let _ = fs::remove_file(&part);
        have = 0;
    }
    if have < file.size {
        let mut request = ureq::get(&file.url);
        if have > 0 {
            request = request.header("Range", format!("bytes={have}-"));
        }
        let response = request
            .call()
            .map_err(|e| format!("could not download the picture-search files ({e}). Check the internet connection and try again."))?;
        // A server that ignores the range sends the whole file again.
        let resumed = response.status().as_u16() == 206;
        let mut out = if resumed {
            OpenOptions::new().append(true).open(&part)
        } else {
            have = 0;
            File::create(&part)
        }
        .map_err(|e| format!("could not write the picture-search files: {e}"))?;
        let mut body = response.into_body().into_reader();
        let mut buf = vec![0u8; 256 * 1024];
        let mut last_reported = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("The download was stopped. It will carry on from here next time.".into());
            }
            let n = body.read(&mut buf).map_err(|e| format!("the download was interrupted ({e}). Try again to carry on."))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| format!("could not write the picture-search files: {e}"))?;
            have += n as u64;
            if have - last_reported >= 512 * 1024 {
                last_reported = have;
                progress(done_before + have.min(file.size), total);
            }
        }
        out.flush().map_err(|e| e.to_string())?;
    }
    let size = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if size != file.size {
        // Cut short: keep what there is, to carry on from.
        return Err("the download ended early. Try again to carry on.".into());
    }
    let sum = sha256_of(&part).map_err(|e| format!("could not check the downloaded file: {e}"))?;
    if sum != file.sha256 {
        let _ = fs::remove_file(&part);
        return Err("a downloaded file was not the one expected, so it was not kept. Try again; if this keeps happening, the files may have been changed at their source.".into());
    }
    fs::rename(&part, &target).map_err(|e| format!("could not save the picture-search files: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// A web server for one file on this computer, which honours a range, and
    /// counts the requests made of it.
    fn serve(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                counted.fetch_add(1, Ordering::SeqCst);
                let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
                let mut start = 0usize;
                while let Some(Ok(line)) = lines.next() {
                    if line.is_empty() {
                        break;
                    }
                    if let Some(r) = line.to_lowercase().strip_prefix("range: bytes=") {
                        start = r.trim_end_matches('-').parse().unwrap_or(0);
                    }
                }
                let part = &body[start.min(body.len())..];
                let head = if start > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        part.len(),
                        start,
                        body.len() - 1,
                        body.len()
                    )
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", part.len())
                };
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(part);
            }
        });
        (url, hits)
    }

    fn sum(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hs-clip-dl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn payload() -> Vec<u8> {
        (0..3_000_000u32).map(|i| (i % 251) as u8).collect()
    }

    fn spec(url: String, body: &[u8]) -> Vec<ModelFile> {
        vec![ModelFile { name: "m.onnx".into(), url, size: body.len() as u64, sha256: sum(body) }]
    }

    #[test]
    fn the_published_files_are_pinned_and_named() {
        let files = model_files();
        assert_eq!(files.len(), 2);
        for f in &files {
            assert!(f.url.contains(REVISION), "{} is not pinned to a revision", f.name);
            assert_eq!(f.sha256.len(), 64);
            assert!(f.url.ends_with(&f.name));
        }
        assert_eq!(total_size(), 172_768_684 + 127_339_794);
    }

    #[test]
    fn a_file_is_fetched_checked_and_kept() {
        let body = payload();
        let (url, _) = serve(body.clone());
        let dir = scratch("ok");
        let mut seen = Vec::new();
        download(&spec(url, &body), &dir, &mut |d, t| seen.push((d, t)), &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(dir.join("m.onnx")).unwrap(), body);
        assert!(!dir.join("m.onnx.part").exists());
        assert_eq!(seen.last(), Some(&(body.len() as u64, body.len() as u64)));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "progress went backwards");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_that_is_not_the_one_expected_is_not_kept() {
        let body = payload();
        let (url, _) = serve(body.clone());
        let mut files = spec(url, &body);
        files[0].sha256 = sum(b"something else");
        let dir = scratch("bad");
        let err = download(&files, &dir, &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("not the one expected"), "{err}");
        assert!(!dir.join("m.onnx").exists());
        assert!(!dir.join("m.onnx.part").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_interrupted_download_carries_on_from_where_it_stopped() {
        let body = payload();
        let (url, _) = serve(body.clone());
        let dir = scratch("resume");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("m.onnx.part"), &body[..1_000_000]).unwrap();
        download(&spec(url, &body), &dir, &mut |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(dir.join("m.onnx")).unwrap(), body);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_partial_file_is_caught_not_trusted() {
        let body = payload();
        let (url, _) = serve(body.clone());
        let dir = scratch("damaged");
        fs::create_dir_all(&dir).unwrap();
        let mut wrong = body[..1_000_000].to_vec();
        wrong[500] ^= 0xff;
        fs::write(dir.join("m.onnx.part"), &wrong).unwrap();
        assert!(download(&spec(url, &body), &dir, &mut |_, _| {}, &AtomicBool::new(false)).is_err());
        assert!(!dir.join("m.onnx").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn files_already_in_place_are_not_fetched_again() {
        let body = payload();
        let (url, hits) = serve(body.clone());
        let dir = scratch("have");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("m.onnx"), &body).unwrap();
        download(&spec(url, &body), &dir, &mut |_, _| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stopping_is_honoured_and_nothing_wrong_is_kept() {
        let body = payload();
        let (url, _) = serve(body.clone());
        let dir = scratch("cancel");
        let stop = AtomicBool::new(true);
        let err = download(&spec(url, &body), &dir, &mut |_, _| {}, &stop).unwrap_err();
        assert!(err.contains("stopped"), "{err}");
        assert!(!dir.join("m.onnx").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreachable_server_says_so_plainly() {
        // Nothing listens on this port.
        let files = spec("http://127.0.0.1:9/file".into(), b"x");
        let dir = scratch("down");
        let err = download(&files, &dir, &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("internet connection"), "{err}");
        let _ = fs::remove_dir_all(dir);
    }

    /// The real thing, over the internet (about 300 MB, into a temporary
    /// folder): `cargo test --release --lib download_the_real_files -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn download_the_real_files() {
        let dir = scratch("real");
        let started = std::time::Instant::now();
        let mut last = 0;
        download(&model_files(), &dir, &mut |d, t| {
            if d / 50_000_000 != last / 50_000_000 {
                println!("  {} of {} MB", d / 1_000_000, t / 1_000_000);
            }
            last = d;
        }, &AtomicBool::new(false))
        .unwrap();
        println!("downloaded and checked in {:?}", started.elapsed());
        for f in model_files() {
            assert_eq!(sha256_of(&dir.join(&f.name)).unwrap(), f.sha256);
        }
        let _ = fs::remove_dir_all(dir);
    }
}
