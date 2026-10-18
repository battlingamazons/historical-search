fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "search",
                "get_item",
                "get_config",
                "save_config",
                "validate_path",
                "get_status",
                "start_refresh",
                "pick_folder",
                "download_image",
                "open_document",
                "find_pages",
                "read_document_range",
                "reveal_document",
                "get_diagnostics",
                "save_diagnostics",
                "log_from_ui",
            ]),
        ),
    )
    .expect("failed to run tauri-build");
}
