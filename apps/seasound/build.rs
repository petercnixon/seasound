fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let icon = std::path::Path::new(&manifest_dir).join("../../icons/seasound.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    // The pinned taskbar shortcut reads the icon from the executable. The
    // window icon set at runtime does not fill that in.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().expect("icon path is utf-8"));
    if let Err(err) = res.compile() {
        panic!("failed to embed the Windows icon: {err}");
    }
}
