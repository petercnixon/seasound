//! The SeaSound window. Playback and file parsing live in the other crates.

mod app;

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([720.0, 460.0])
            .with_title("SeaSound"),
        ..Default::default()
    };
    eframe::run_native(
        "SeaSound",
        options,
        Box::new(|creation| Ok(Box::new(app::SeaSoundApp::new(creation)))),
    )
}
