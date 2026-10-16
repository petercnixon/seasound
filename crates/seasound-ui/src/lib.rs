//! The SeaSound window. Playback and file parsing live in the other crates.

mod app;

pub fn run() -> eframe::Result {
    let mut options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([720.0, 460.0])
            .with_title("SeaSound"),
        ..Default::default()
    };
    // winit 0.30 does not emit file-drop events on Wayland. XWayland does.
    options.event_loop_builder = Some(Box::new(|builder| {
        prefer_x11_when_xwayland_is_available(builder);
    }));
    eframe::run_native(
        "SeaSound",
        options,
        Box::new(|creation| Ok(Box::new(app::SeaSoundApp::new(creation)))),
    )
}

fn prefer_x11_when_xwayland_is_available(builder: &mut eframe::EventLoopBuilder<eframe::UserEvent>) {
    #[cfg(all(
        unix,
        not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "android",
            target_os = "redox"
        ))
    ))]
    {
        use winit::platform::x11::EventLoopBuilderExtX11 as _;
        if xwayland_available() {
            builder.with_x11();
        }
    }
    #[cfg(not(all(
        unix,
        not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "android",
            target_os = "redox"
        ))
    )))]
    {
        let _ = builder;
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "android",
        target_os = "redox"
    ))
))]
fn xwayland_available() -> bool {
    let wayland = std::env::var_os("WAYLAND_DISPLAY")
        .filter(|value| !value.is_empty())
        .is_some();
    let display = std::env::var_os("DISPLAY")
        .filter(|value| !value.is_empty())
        .is_some();
    if !wayland || !display {
        return false;
    }
    match std::env::var("WINIT_UNIX_BACKEND") {
        Ok(backend) if backend.eq_ignore_ascii_case("wayland") => false,
        _ => true,
    }
}
