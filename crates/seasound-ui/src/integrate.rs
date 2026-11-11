//! File-manager commands for the window that is listening.
//!
//! Linux gets a desktop entry with an Enqueue action. Windows gets Open with
//! SeaSound and Enqueue in SeaSound. macOS is left unregistered: the same
//! arguments work from a terminal, and Finder needs an app bundle.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

const BASE_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "mp4", "wav", "aiff", "aif", "alac", "ogg", "oga", "cue", "m3u",
    "m3u8",
];

struct PngIcon {
    size: &'static str,
    bytes: &'static [u8],
}

fn png_icons() -> &'static [PngIcon] {
    &[
        PngIcon {
            size: "16x16",
            bytes: include_bytes!("../../../icons/seasound-16.png"),
        },
        PngIcon {
            size: "32x32",
            bytes: include_bytes!("../../../icons/seasound-32.png"),
        },
        PngIcon {
            size: "48x48",
            bytes: include_bytes!("../../../icons/seasound-48.png"),
        },
        PngIcon {
            size: "128x128",
            bytes: include_bytes!("../../../icons/seasound-128.png"),
        },
        PngIcon {
            size: "256x256",
            bytes: include_bytes!("../../../icons/seasound-256.png"),
        },
    ]
}

const SVG_ICON: &[u8] = include_bytes!("../../../icons/seasound.svg");

pub(crate) fn install(exe: &Path, extra_extensions: &[String]) {
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    install_freedesktop(exe, extra_extensions);
    #[cfg(windows)]
    install_windows(exe, extra_extensions);
    #[cfg(not(any(
        windows,
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        let _ = (exe, extra_extensions);
    }
}

#[cfg_attr(
    not(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )),
    allow(dead_code)
)]
fn install_freedesktop(exe: &Path, extra_extensions: &[String]) {
    let Some(base) = directories::BaseDirs::new() else {
        return;
    };
    if let Err(err) = write_freedesktop(base.data_dir(), exe, extra_extensions, true) {
        eprintln!("Could not install the SeaSound menu entry: {err}");
    }
}

fn write_freedesktop(
    data_home: &Path,
    exe: &Path,
    extra_extensions: &[String],
    refresh: bool,
) -> io::Result<bool> {
    let Some(exe_text) = exe.to_str() else {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "program path is not text",
        ));
    };
    if exe_text.contains('\n') || exe_text.contains('\r') {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "program path breaks the desktop entry",
        ));
    }
    let extensions = shell_extensions(extra_extensions);
    let mime = all_mimes(&extensions);
    let mut changed = write_icons(data_home)?;
    let package = data_home.join("mime/packages/seasound.xml");
    if let Some(xml) = mime_xml(&extensions) {
        changed |= write_if_changed(&package, xml.as_bytes())?;
    } else {
        changed |= remove_if_present(&package)?;
    }
    let desktop = desktop_entry(exe_text, &mime);
    let desktop_path = data_home.join("applications/seasound.desktop");
    changed |= write_if_changed(&desktop_path, desktop.as_bytes())?;
    // GNOME Files lists applications in Open With. It does not list a desktop
    // file's actions, so Enqueue is its own entry. NoDisplay hides it from the
    // app grid and still leaves it in that Open With list.
    let enqueue = enqueue_desktop_entry(exe_text, &mime);
    let enqueue_path = data_home.join("applications/seasound-enqueue.desktop");
    changed |= write_if_changed(&enqueue_path, enqueue.as_bytes())?;
    if changed && refresh {
        refresh_databases(data_home);
    }
    Ok(changed)
}

fn write_icons(data_home: &Path) -> io::Result<bool> {
    let mut changed = false;
    for icon in png_icons() {
        let path = data_home.join(format!("icons/hicolor/{}/apps/seasound.png", icon.size));
        changed |= write_if_changed(&path, icon.bytes)?;
    }
    let svg = data_home.join("icons/hicolor/scalable/apps/seasound.svg");
    changed |= write_if_changed(&svg, SVG_ICON)?;
    Ok(changed)
}

fn refresh_databases(data_home: &Path) {
    refresh("update-desktop-database", &data_home.join("applications"));
    refresh("update-mime-database", &data_home.join("mime"));
}

fn refresh(command: &str, arg: &Path) {
    let _ = std::process::Command::new(command)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn desktop_entry(exe: &str, mime: &str) -> String {
    let open = desktop_exec(exe, " %F");
    let enqueue = desktop_exec(exe, " --enqueue %F");
    format!(
        "\
[Desktop Entry]
Type=Application
Name=SeaSound
GenericName=Music Player
Comment=Play audio files in a playlist
Exec={open}
TryExec={exe}
Icon=seasound
Terminal=false
Categories=AudioVideo;Audio;Player;
MimeType={mime}
StartupNotify=false
StartupWMClass=seasound
Actions=enqueue;

[Desktop Action enqueue]
Name=Enqueue in SeaSound
Exec={enqueue}
"
    )
}

/// A separate application so GNOME's Open With list can offer enqueue.
/// `NoDisplay` keeps it out of the app grid. Files still lists it.
/// The action on the main entry is what KDE and similar desktops show.
fn enqueue_desktop_entry(exe: &str, mime: &str) -> String {
    let enqueue = desktop_exec(exe, " --enqueue %F");
    format!(
        "\
[Desktop Entry]
Type=Application
Name=Enqueue in SeaSound
Comment=Add files to the current SeaSound playlist
Exec={enqueue}
TryExec={exe}
Icon=seasound
Terminal=false
NoDisplay=true
Categories=AudioVideo;Audio;Player;
MimeType={mime}
StartupNotify=false
StartupWMClass=seasound
"
    )
}

fn desktop_exec(exe: &str, trailing: &str) -> String {
    let mut escaped = String::with_capacity(exe.len());
    for ch in exe.chars() {
        match ch {
            '%' => escaped.push_str("%%"),
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            other => escaped.push(other),
        }
    }
    format!("\"{escaped}\"{trailing}")
}

fn all_mimes(extensions: &[String]) -> String {
    let mut mime = mime_list(extensions);
    for ext in extensions {
        if mimes_for(ext).is_empty() {
            mime.push_str(&extra_mime(ext));
            mime.push(';');
        }
    }
    mime
}

fn mime_list(extensions: &[String]) -> String {
    let mut mimes = Vec::new();
    for ext in extensions {
        for mime in mimes_for(ext) {
            if !mimes.contains(mime) {
                mimes.push(*mime);
            }
        }
    }
    let mut text = mimes.join(";");
    if !text.is_empty() {
        text.push(';');
    }
    text
}

fn mimes_for(ext: &str) -> &'static [&'static str] {
    match ext {
        "mp3" => &["audio/mpeg"],
        "flac" => &["audio/flac", "audio/x-flac"],
        "m4a" => &["audio/mp4", "audio/x-m4a", "audio/m4a"],
        "m4b" => &["audio/mp4", "audio/x-m4b"],
        "mp4" => &["audio/mp4"],
        "wav" => &["audio/wav", "audio/x-wav", "audio/vnd.wave"],
        "aiff" | "aif" => &["audio/aiff", "audio/x-aiff"],
        "alac" => &["audio/alac", "audio/x-alac"],
        "ogg" | "oga" => &[
            "audio/ogg",
            "audio/vorbis",
            "audio/x-vorbis+ogg",
            "application/ogg",
        ],
        "cue" => &["application/x-cue"],
        "m3u" => &["audio/x-mpegurl", "audio/mpegurl"],
        "m3u8" => &["audio/x-mpegurl", "application/vnd.apple.mpegurl"],
        "wv" => &["audio/x-wavpack"],
        _ => &[],
    }
}

fn extra_mime(ext: &str) -> String {
    format!("audio/x-seasound-{ext}")
}

fn mime_xml(extensions: &[String]) -> Option<String> {
    let mut body = String::new();
    for ext in extensions {
        if !mimes_for(ext).is_empty() {
            continue;
        }
        let mime = extra_mime(ext);
        body.push_str(&format!(
            "  <mime-type type=\"{mime}\">\n    <comment>SeaSound audio</comment>\n    <glob pattern=\"*.{ext}\"/>\n  </mime-type>\n"
        ));
    }
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\">\n{body}</mime-info>\n"
    ))
}

fn shell_extensions(extra: &[String]) -> Vec<String> {
    let mut exts: Vec<String> = BASE_EXTENSIONS
        .iter()
        .map(|ext| (*ext).to_string())
        .collect();
    for ext in extra {
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        if !usable_extension(&ext) || exts.iter().any(|have| have == &ext) {
            continue;
        }
        exts.push(ext);
    }
    exts
}

fn usable_extension(ext: &str) -> bool {
    !ext.is_empty() && ext.len() <= 16 && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
}

#[cfg(any(windows, test))]
struct RegValue {
    key: String,
    name: String,
    value: String,
}

#[cfg(any(windows, test))]
fn reg(key: &str, name: &str, value: &str) -> RegValue {
    RegValue {
        key: key.to_string(),
        name: name.to_string(),
        value: value.to_string(),
    }
}

#[cfg(any(windows, test))]
fn windows_registration(exe: &str, extensions: &[String]) -> Vec<RegValue> {
    let open = windows_open_command(exe);
    let enqueue = windows_enqueue_command(exe);
    let icon = icon_value(exe);
    let mut rows = Vec::new();
    let app = r"Software\Classes\Applications\seasound.exe";
    rows.push(reg(app, "", "SeaSound"));
    rows.push(reg(app, "FriendlyAppName", "SeaSound"));
    rows.push(reg(
        &format!(r"{app}\shell\open"),
        "MultiSelectModel",
        "Player",
    ));
    rows.push(reg(&format!(r"{app}\shell\open\command"), "", &open));
    rows.push(reg(
        &format!(r"{app}\shell\enqueue"),
        "",
        "Enqueue in SeaSound",
    ));
    rows.push(reg(
        &format!(r"{app}\shell\enqueue"),
        "MultiSelectModel",
        "Player",
    ));
    rows.push(reg(&format!(r"{app}\shell\enqueue\command"), "", &enqueue));
    if let Some(icon) = &icon {
        rows.push(reg(&format!(r"{app}\shell\open"), "Icon", icon));
        rows.push(reg(&format!(r"{app}\shell\enqueue"), "Icon", icon));
    }
    for ext in extensions {
        let open_key =
            format!(r"Software\Classes\SystemFileAssociations\.{ext}\shell\seasound.open");
        let enqueue_key =
            format!(r"Software\Classes\SystemFileAssociations\.{ext}\shell\seasound.enqueue");
        rows.push(reg(&open_key, "", "Open with SeaSound"));
        rows.push(reg(&open_key, "MultiSelectModel", "Player"));
        rows.push(reg(&format!(r"{open_key}\command"), "", &open));
        rows.push(reg(&enqueue_key, "", "Enqueue in SeaSound"));
        rows.push(reg(&enqueue_key, "MultiSelectModel", "Player"));
        rows.push(reg(&format!(r"{enqueue_key}\command"), "", &enqueue));
        if let Some(icon) = &icon {
            rows.push(reg(&open_key, "Icon", icon));
            rows.push(reg(&enqueue_key, "Icon", icon));
        }
        rows.push(reg(
            &format!(r"Software\Classes\.{ext}\OpenWithList\seasound.exe"),
            "",
            "",
        ));
    }
    rows
}

#[cfg(any(windows, test))]
fn windows_open_command(exe: &str) -> String {
    format!(r#"{} "%1""#, quote_windows_arg(exe))
}

#[cfg(any(windows, test))]
fn windows_enqueue_command(exe: &str) -> String {
    format!(r#"{} --enqueue "%1""#, quote_windows_arg(exe))
}

#[cfg(any(windows, test))]
fn quote_windows_arg(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\\\""))
}

#[cfg(any(windows, test))]
fn icon_value(exe: &str) -> Option<String> {
    if exe.contains('"') || exe.contains(',') {
        return None;
    }
    Some(format!("\"{exe}\",0"))
}

#[cfg(windows)]
fn install_windows(exe: &Path, extra_extensions: &[String]) {
    let Some(exe) = exe.to_str() else {
        eprintln!("Could not register SeaSound in the file menu: the program path is not text.");
        return;
    };
    if exe.contains('"') || exe.contains('\n') || exe.contains('\r') {
        eprintln!(
            "Could not register SeaSound in the file menu: the program path cannot be quoted."
        );
        return;
    }
    let rows = windows_registration(exe, &shell_extensions(extra_extensions));
    if let Err(err) = apply_registration(&rows) {
        eprintln!("Could not register SeaSound in the file menu: {err}");
    }
}

#[cfg(windows)]
fn apply_registration(rows: &[RegValue]) -> io::Result<()> {
    for row in rows {
        let key = create_key(&row.key)?;
        set_sz(&key, &row.name, &row.value)?;
    }
    Ok(())
}

#[cfg(windows)]
struct RegKey(*mut std::ffi::c_void);

#[cfg(windows)]
impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe { reg_close_key(self.0) };
    }
}

#[cfg(windows)]
fn create_key(sub: &str) -> io::Result<RegKey> {
    use std::ffi::c_void;
    let sub = wide(sub);
    let mut out: *mut c_void = std::ptr::null_mut();
    let rc = unsafe {
        reg_create_key_ex_w(
            hkey_current_user(),
            sub.as_ptr(),
            0,
            std::ptr::null(),
            0,
            0x0002_0006,
            std::ptr::null_mut(),
            &mut out,
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(RegKey(out))
}

#[cfg(windows)]
fn set_sz(key: &RegKey, name: &str, value: &str) -> io::Result<()> {
    let name = wide(name);
    let data = wide(value);
    let bytes = (data.len() * 2) as u32;
    let rc = unsafe {
        reg_set_value_ex_w(
            key.0,
            name.as_ptr(),
            0,
            1,
            data.as_ptr() as *const u8,
            bytes,
        )
    };
    if rc != 0 {
        Err(io::Error::from_raw_os_error(rc))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn hkey_current_user() -> *mut std::ffi::c_void {
    // HKEY_CURRENT_USER is 0x80000001, sign-extended to a pointer.
    (-2147483647isize) as *mut std::ffi::c_void
}

#[cfg(windows)]
unsafe fn reg_create_key_ex_w(
    key: *mut std::ffi::c_void,
    sub: *const u16,
    reserved: u32,
    class: *const u16,
    options: u32,
    access: u32,
    security: *mut std::ffi::c_void,
    result: *mut *mut std::ffi::c_void,
    disposition: *mut u32,
) -> i32 {
    extern "system" {
        fn RegCreateKeyExW(
            key: *mut std::ffi::c_void,
            sub: *const u16,
            reserved: u32,
            class: *const u16,
            options: u32,
            access: u32,
            security: *mut std::ffi::c_void,
            result: *mut *mut std::ffi::c_void,
            disposition: *mut u32,
        ) -> i32;
    }
    RegCreateKeyExW(
        key,
        sub,
        reserved,
        class,
        options,
        access,
        security,
        result,
        disposition,
    )
}

#[cfg(windows)]
unsafe fn reg_set_value_ex_w(
    key: *mut std::ffi::c_void,
    name: *const u16,
    reserved: u32,
    kind: u32,
    data: *const u8,
    bytes: u32,
) -> i32 {
    extern "system" {
        fn RegSetValueExW(
            key: *mut std::ffi::c_void,
            name: *const u16,
            reserved: u32,
            kind: u32,
            data: *const u8,
            bytes: u32,
        ) -> i32;
    }
    RegSetValueExW(key, name, reserved, kind, data, bytes)
}

#[cfg(windows)]
unsafe fn reg_close_key(key: *mut std::ffi::c_void) {
    extern "system" {
        fn RegCloseKey(key: *mut std::ffi::c_void) -> i32;
    }
    RegCloseKey(key);
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    if let Ok(existing) = fs::read(path) {
        if existing == bytes {
            return Ok(false);
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_beside(path);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(true)
}

fn temp_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("seasound");
    path.with_file_name(format!(".{name}.seasound-tmp"))
}

fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_entry_opens_and_enqueues() {
        let text = desktop_entry("/home/me/sea sound", "audio/mpeg;");
        assert!(text.contains("Exec=\"/home/me/sea sound\" %F\n"));
        assert!(text.contains("Name=Enqueue in SeaSound\n"));
        assert!(text.contains("Exec=\"/home/me/sea sound\" --enqueue %F\n"));
        assert!(text.contains("MimeType=audio/mpeg;"));
        let enqueue = enqueue_desktop_entry("/home/me/sea sound", "audio/mpeg;");
        assert!(enqueue.contains("Name=Enqueue in SeaSound\n"));
        assert!(enqueue.contains("Exec=\"/home/me/sea sound\" --enqueue %F\n"));
        assert!(enqueue.contains("MimeType=audio/mpeg;"));
        assert!(enqueue.contains("NoDisplay=true\n"));
        assert!(!text.contains("inode/directory"));
        assert_eq!(
            desktop_exec("/opt/sea%sound", " %F"),
            "\"/opt/sea%%sound\" %F"
        );
    }

    #[test]
    fn freedesktop_files_are_written_once() {
        let dir = std::env::temp_dir().join(format!(
            "seasound-desktop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let exe = Path::new("/usr/bin/seasound");
        assert!(write_freedesktop(&dir, exe, &["wv".into(), "xyz".into()], false).unwrap());
        let desktop = fs::read_to_string(dir.join("applications/seasound.desktop")).unwrap();
        let enqueue =
            fs::read_to_string(dir.join("applications/seasound-enqueue.desktop")).unwrap();
        assert!(enqueue.contains("--enqueue"));
        assert!(enqueue.contains("Name=Enqueue in SeaSound"));
        assert!(desktop.contains("audio/x-wavpack"));
        assert!(desktop.contains("audio/x-seasound-xyz"));
        assert!(desktop.contains("audio/mpeg"));
        assert!(desktop.contains("application/x-cue"));
        let icon = fs::read(dir.join("icons/hicolor/48x48/apps/seasound.png")).unwrap();
        assert!(icon.starts_with(b"\x89PNG"));
        let xml = fs::read_to_string(dir.join("mime/packages/seasound.xml")).unwrap();
        assert!(xml.contains("*.xyz"));
        assert!(!xml.contains("*.wv"));
        assert!(!write_freedesktop(&dir, exe, &["wv".into(), "xyz".into()], false).unwrap());
        assert!(write_freedesktop(&dir, exe, &[], false).unwrap());
        assert!(!dir.join("mime/packages/seasound.xml").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_registration_quotes_the_program_and_names_both_commands() {
        let exe = r"C:\Program Files\SeaSound\seasound.exe";
        let rows = windows_registration(
            exe,
            &shell_extensions(&["wv".into(), "MP3".into(), ".flac".into(), "bad ext".into()]),
        );
        let open = windows_open_command(exe);
        let enqueue = windows_enqueue_command(exe);
        assert_eq!(open, r#""C:\Program Files\SeaSound\seasound.exe" "%1""#);
        assert_eq!(
            enqueue,
            r#""C:\Program Files\SeaSound\seasound.exe" --enqueue "%1""#
        );
        assert!(rows.iter().any(|row| row.value == "Open with SeaSound"));
        assert!(rows.iter().any(|row| row.value == "Enqueue in SeaSound"));
        assert!(rows
            .iter()
            .any(|row| row.name == "MultiSelectModel" && row.value == "Player"));
        for row in &rows {
            if row.key.ends_with(r"\command") {
                if row.key.contains("enqueue") {
                    assert_eq!(row.value, enqueue);
                } else {
                    assert_eq!(row.value, open);
                }
            }
        }
        assert!(rows.iter().any(|row| row.key.contains(r".wv\")));
        assert!(rows.iter().any(|row| row.key.contains(r".cue\")));
        assert!(!rows.iter().any(|row| row.key.contains("bad")));
        let mp3_labels = rows
            .iter()
            .filter(|row| {
                row.key.contains(r".mp3\shell\seasound.enqueue")
                    && row.name.is_empty()
                    && row.value == "Enqueue in SeaSound"
            })
            .count();
        assert_eq!(mp3_labels, 1);
    }
}
