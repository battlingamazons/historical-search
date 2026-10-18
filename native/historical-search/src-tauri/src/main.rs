// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Before anything else: when the application starts a copy of itself to
    // read one PDF, that copy must do only that and never open a window.
    historical_search_lib::pdf::run_worker_if_asked();
    historical_search_lib::sheets::run_worker_if_asked();
    historical_search_lib::run()
}
