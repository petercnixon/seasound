//! The SeaSound window. Playback and file parsing live in the other crates.

mod app;
mod integrate;
mod remote;
mod theme;

pub fn run() -> eframe::Result {
    let launch = match remote::parse_args(std::env::args_os()) {
        Ok(launch) => launch,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };
    if launch.help {
        println!("{}", remote::help_text());
        return Ok(());
    }
    let remote = match remote::attach(app::config_dir().as_deref(), &launch) {
        Ok(Some(remote)) => remote,
        Ok(None) => return Ok(()),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };
    let mut options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([720.0, 460.0])
            .with_title("SeaSound")
            .with_icon(window_icon()),
        ..Default::default()
    };
    // winit 0.30 does not emit file-drop events on Wayland. XWayland does.
    options.event_loop_builder = Some(Box::new(|builder| {
        prefer_x11_when_xwayland_is_available(builder);
    }));
    eframe::run_native(
        "SeaSound",
        options,
        Box::new(move |creation| Ok(Box::new(app::SeaSoundApp::new(creation, remote)))),
    )
}

fn window_icon() -> eframe::egui::IconData {
    let image = image::load_from_memory(include_bytes!("../../../icons/seasound-256.png"))
        .expect("icons/seasound-256.png")
        .into_rgba8();
    let (width, height) = image.dimensions();
    eframe::egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn prefer_x11_when_xwayland_is_available(
    builder: &mut eframe::EventLoopBuilder<eframe::UserEvent>,
) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_is_a_square_rgba_image() {
        let icon = window_icon();
        assert_eq!(icon.width, 256);
        assert_eq!(icon.height, 256);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }
}
