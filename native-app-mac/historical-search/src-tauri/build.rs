fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["pick_folder", "download_original"])),
    )
    .expect("failed to run tauri-build");
}
