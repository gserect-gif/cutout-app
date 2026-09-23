// Prevents an additional console window on Windows in release builds, without
// affecting other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    bgremover_lib::run();
}
