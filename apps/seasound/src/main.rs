// A release build is a window, not a console program. Debug builds keep the
// console so a startup error is still visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    if let Err(err) = seasound_ui::run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
