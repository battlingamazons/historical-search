use std::net::TcpStream;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

// Path to the prototype backend, resolved relative to this crate at build time
// (not hardcoded to one machine/username) so it still works after a git clone
// or a move, as long as `prototype/` stays a sibling of `native-app-mac/`.
const PROTOTYPE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../prototype");

struct ServerProcess(Mutex<Option<Child>>);

/// The Windows Python installer only registers `python`, not `python3`
/// (macOS/Linux is the reverse convention: `python3` is the reliable name
/// there, and plain `python` may not exist at all). Try both rather than
/// assume either.
fn spawn_server() -> std::io::Result<Child> {
    let mut last_err = None;
    for candidate in ["python3", "python"] {
        match Command::new(candidate)
            .arg("server.py")
            .current_dir(PROTOTYPE_DIR)
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap())
}

/// Blocks (briefly) until something is actually listening on port 8765, or
/// `timeout` elapses. Without this, the window would try to load the app's
/// URL immediately after *starting* the Python subprocess, but interpreter
/// startup + imports + socket bind isn't instant -- racing that produced a
/// blank window that never recovered on its own, requiring a relaunch (or
/// several) to happen to win the race. Waiting here means the window is only
/// ever created once there's something real to show it.
fn wait_for_server_ready(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let addr = "127.0.0.1:8765".parse().expect("valid socket address");
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    // Must be `async` -- Tauri runs sync commands on the main thread, but
    // `blocking_pick_folder` blocks that same thread waiting for the native
    // dialog, and the dialog itself needs the main thread free to run. A sync
    // fn here deadlocks (main thread waits on itself); async gets scheduled
    // off the main thread, so it can block-wait safely instead.
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog().file().blocking_pick_folder().map(|p| p.to_string())
    })
    .await
    .unwrap_or(None)
}

/// Downloads the image at `url` (our own local backend) to a location the
/// user picks with a native Save dialog. The webview's own download handling
/// isn't reliable here -- clicking a plain link just displayed the image
/// in-place instead of saving it -- so this does the fetch-and-write on the
/// Rust side instead, same as `pick_folder`, and for the same reason (must be
/// `async` so the blocking dialog + blocking HTTP call don't run on the main
/// thread and deadlock against the dialog needing that same thread).
#[tauri::command]
async fn download_original(app: tauri::AppHandle, url: String, suggested_name: String) -> Result<bool, String> {
    use std::io::Read;
    tauri::async_runtime::spawn_blocking(move || -> Result<bool, String> {
        let response = ureq::get(&url).call().map_err(|e| e.to_string())?;
        let filename = response
            .header("Content-Disposition")
            .and_then(|cd| cd.split("filename=\"").nth(1))
            .and_then(|rest| rest.split('"').next())
            .map(|s| s.to_string())
            .unwrap_or(suggested_name);
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;

        let Some(dest) = app.dialog().file().set_file_name(&filename).blocking_save_file() else {
            return Ok(false); // user cancelled the save dialog
        };
        let dest_path = dest.into_path().map_err(|e| e.to_string())?;
        std::fs::write(&dest_path, &bytes).map_err(|e| e.to_string())?;
        Ok(true)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(unix)]
fn stop_any_previous_server() {
    // Make sure nothing is still holding port 8765, regardless of how it was
    // started (matching by command text is unreliable across invocations).
    if let Ok(output) = Command::new("lsof").args(["-ti", ":8765"]).output() {
        for pid in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            let _ = Command::new("kill").args(["-9", pid]).status();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

#[cfg(windows)]
fn stop_any_previous_server() {
    // Same idea as the Unix version above, using the Windows equivalents:
    // netstat to find whatever PID owns port 8765, taskkill to end it.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000; // don't flash a console window

    let Ok(output) = Command::new("netstat")
        .args(["-ano"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if !line.contains(":8765") {
            continue;
        }
        if let Some(pid) = line.split_whitespace().last() {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", pid])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![pick_folder, download_original])
        .manage(ServerProcess(Mutex::new(None)))
        .setup(|app| {
            stop_any_previous_server();
            let child = spawn_server()
                .expect("failed to start the local search backend (is Python 3 installed?)");

            let state = app.state::<ServerProcess>();
            *state.0.lock().unwrap() = Some(child);

            if !wait_for_server_ready(Duration::from_secs(15)) {
                // Backend never came up. Build the window anyway, pointed at
                // the URL, so at least something visible shows up (matching
                // the old behavior) instead of the app silently doing nothing.
                eprintln!("warning: local search backend did not respond within 15s");
            }

            let url = "http://127.0.0.1:8765".parse().expect("valid url");
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url))
                .title("Historical Society Search")
                .inner_size(1000.0, 760.0)
                .build()?;

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                let state = app_handle.state::<ServerProcess>();
                let mut guard = state.0.lock().unwrap();
                if let Some(mut child) = guard.take() {
                    let _ = child.kill();
                }
            }
        });
}
