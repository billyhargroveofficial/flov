#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    match flov_lib::control::run_if_requested() {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            eprintln!("flov control error: {e:#}");
            std::process::exit(1);
        }
    }
    // Headless service must be selected before any Tauri/GTK/Wayland,
    // microphone, or hotkey initialization, so it is dispatched here
    // instead of inside flov_lib::run().
    match flov_lib::headless::run_if_requested() {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            eprintln!("flov headless error: {e:#}");
            std::process::exit(1);
        }
    }
    flov_lib::run();
}
