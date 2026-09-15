#![deny(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    clinch_desktop::run();
}
