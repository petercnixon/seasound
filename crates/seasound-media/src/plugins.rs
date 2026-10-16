//! Load decoder plugins and adapt them to [`PcmSource`].
//!
//! Directories are scanned in order. The first plugin id wins. A later file
//! with the same id is kept in the list as an error so the Plugins menu can
//! show it. Built-in extensions are never claimed here.

use std::ffi::{c_char, c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use libloading::Library;
use seasound_model::{ChannelMap, Error, PcmSource};
use seasound_plugin_api::{
    read_cstr, read_error, read_text, DecoderVtable, DecodeRequest, PluginInfo, ProbeResult,
    ABI_VERSION, DECODER_V1_SIZE, KIND_DECODER, OK,
};

const ERR_CAP: usize = 512;

struct DecoderPlugin {
    _lib: Library,
    vtable: DecoderVtable,
}

struct Slot {
    id: String,
    name: String,
    version: String,
    extensions: Vec<String>,
    enabled: bool,
    error: Option<String>,
    file_name: String,
    decoder: Option<Arc<DecoderPlugin>>,
}

struct Registry {
    slots: Vec<Slot>,
}

impl Registry {
    fn empty() -> Self {
        Self { slots: Vec::new() }
    }
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::empty()))
}

/// One row in the Plugins menu.
#[derive(Clone, Debug)]
pub struct PluginStatus {
    pub id: String,
    pub name: String,
    pub version: String,
    pub extensions: Vec<String>,
    pub enabled: bool,
    pub error: Option<String>,
    pub file_name: String,
}

/// Replace the loaded set. `extra_dirs` are searched after `SEASOUND_PLUGIN_DIR`
/// and the `plugins` directory next to the executable.
pub fn install_plugins(extra_dirs: &[PathBuf], disabled: &[String]) {
    load_plugin_dirs(&plugin_dirs(extra_dirs), disabled);
}

/// Load only the directories given. Tests use this so the executable directory
/// and `SEASOUND_PLUGIN_DIR` do not leak into the registry.
pub(crate) fn load_plugin_dirs(dirs: &[PathBuf], disabled: &[String]) {
    let mut slots = Vec::new();
    for dir in dirs {
        load_dir(dir, &mut slots);
    }
    for slot in &mut slots {
        if slot.error.is_none() && disabled.iter().any(|id| id == &slot.id) {
            slot.enabled = false;
        }
    }
    *registry().lock().expect("plugin registry") = Registry { slots };
}

pub fn plugin_statuses() -> Vec<PluginStatus> {
    registry()
        .lock()
        .expect("plugin registry")
        .slots
        .iter()
        .map(status_of)
        .collect()
}

pub fn plugin_extensions() -> Vec<String> {
    let registry = registry().lock().expect("plugin registry");
    let mut out = Vec::new();
    for slot in &registry.slots {
        if slot.enabled && slot.error.is_none() {
            for ext in &slot.extensions {
                if !out.iter().any(|have| have == ext) {
                    out.push(ext.clone());
                }
            }
        }
    }
    out
}

pub fn set_plugin_enabled(id: &str, enabled: bool) {
    let mut registry = registry().lock().expect("plugin registry");
    for slot in &mut registry.slots {
        if slot.id == id && slot.error.is_none() && slot.decoder.is_some() {
            slot.enabled = enabled;
        }
    }
}

pub(crate) fn claims(extension: &str) -> bool {
    let registry = registry().lock().expect("plugin registry");
    registry.slots.iter().any(|slot| usable(slot, extension))
}

pub(crate) enum Claim {
    None,
    Off { name: String },
    Ready(ProbeFacts),
    Failed(String),
}

pub(crate) struct ProbeFacts {
    pub sample_rate: u32,
    pub channels: u16,
    pub presentation_frames: u64,
    pub track_number: u32,
    pub disc: u32,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub format_label: String,
}

/// Probe a path whose extension belongs to a plugin. `None` means no plugin
/// listed that extension, so the built-in probe should run.
pub(crate) fn probe_claim(path: &Path) -> Claim {
    let Some(ext) = extension(path) else {
        return Claim::None;
    };
    let registry = registry().lock().expect("plugin registry");
    let Some(slot) = registry.slots.iter().find(|slot| lists(slot, &ext)) else {
        return Claim::None;
    };
    if slot.error.is_some() || slot.decoder.is_none() {
        return Claim::Failed(slot.error.clone().unwrap_or_else(|| "the plugin failed to load".into()));
    }
    if !slot.enabled {
        return Claim::Off {
            name: slot.name.clone(),
        };
    }
    let decoder = slot.decoder.clone().expect("checked");
    drop(registry);
    match call_probe(&decoder, path) {
        Ok(facts) => Claim::Ready(facts),
        Err(err) => Claim::Failed(err.message().to_string()),
    }
}

pub(crate) fn open_claimed(path: &Path, params: DecodeRequest) -> Result<Option<Box<dyn PcmSource>>, Error> {
    let Some(ext) = extension(path) else {
        return Ok(None);
    };
    let registry = registry().lock().expect("plugin registry");
    let Some(slot) = registry.slots.iter().find(|slot| lists(slot, &ext)) else {
        return Ok(None);
    };
    if let Some(error) = &slot.error {
        return Err(Error::new(error.clone()));
    }
    if !slot.enabled {
        return Err(Error::new(format!("{} is turned off", slot.name)));
    }
    let decoder = slot
        .decoder
        .clone()
        .ok_or_else(|| Error::new("the plugin failed to load"))?;
    drop(registry);
    let stream = call_open(&decoder, path, params)?;
    Ok(Some(Box::new(stream)))
}

fn plugin_dirs(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("SEASOUND_PLUGIN_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.join("plugins"));
        }
    }
    dirs.extend(extra.iter().cloned());
    dirs
}

fn load_dir(dir: &Path, slots: &mut Vec<Slot>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_plugin_file(path))
        .collect();
    files.sort();
    for path in files {
        let slot = load_file(&path, slots);
        slots.push(slot);
    }
}

fn is_plugin_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("so" | "dll" | "dylib") => true,
        _ => false,
    }
}

fn load_file(path: &Path, existing: &[Slot]) -> Slot {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".into());
    let lib = match unsafe { Library::new(path) } {
        Ok(lib) => lib,
        Err(err) => {
            return failed(&file_name, &file_name, format!("could not load {file_name}: {err}"));
        }
    };
    let info = match read_info(&lib) {
        Ok(info) => info,
        Err(err) => return failed(&file_name, &file_name, err),
    };
    if existing.iter().any(|slot| slot.id == info.id && slot.error.is_none()) {
        return failed(
            &info.id,
            &file_name,
            format!("{} is already loaded", info.name),
        );
    }
    if info.abi != ABI_VERSION {
        return Slot {
            id: info.id,
            name: info.name,
            version: info.version,
            extensions: Vec::new(),
            enabled: false,
            error: Some(format!(
                "plugin ABI {} is not supported (SeaSound speaks ABI {ABI_VERSION})",
                info.abi
            )),
            file_name,
            decoder: None,
        };
    }
    if info.kind != KIND_DECODER {
        return failed(&info.id, &file_name, "only decoder plugins are supported");
    }
    let vtable = match read_vtable(&lib) {
        Ok(vtable) => vtable,
        Err(err) => return failed(&info.id, &file_name, err),
    };
    let extensions = match read_extensions(&vtable) {
        Ok(extensions) => extensions,
        Err(err) => return failed(&info.id, &file_name, err),
    };
    Slot {
        id: info.id,
        name: info.name,
        version: info.version,
        extensions,
        enabled: true,
        error: None,
        file_name,
        decoder: Some(Arc::new(DecoderPlugin { _lib: lib, vtable })),
    }
}

struct InfoOwned {
    abi: u32,
    kind: u32,
    id: String,
    name: String,
    version: String,
}

fn read_info(lib: &Library) -> Result<InfoOwned, String> {
    let symbol: libloading::Symbol<unsafe extern "C" fn() -> *const PluginInfo> =
        unsafe { lib.get(b"seasound_plugin_info") }
            .map_err(|_| "not a SeaSound plugin".to_string())?;
    let info = unsafe { symbol() };
    if info.is_null() {
        return Err("the plugin returned no identity".into());
    }
    let info = unsafe { &*info };
    let id = read_cstr(info.id).to_string();
    if id.is_empty() {
        return Err("the plugin has no id".into());
    }
    let name = {
        let name = read_cstr(info.name).to_string();
        if name.is_empty() {
            id.clone()
        } else {
            name
        }
    };
    Ok(InfoOwned {
        abi: info.abi,
        kind: info.kind,
        id,
        name,
        version: read_cstr(info.version).to_string(),
    })
}

fn read_vtable(lib: &Library) -> Result<DecoderVtable, String> {
    let symbol: libloading::Symbol<unsafe extern "C" fn() -> *const DecoderVtable> =
        unsafe { lib.get(b"seasound_decoder_v1") }
            .map_err(|_| "the plugin has no decoder".to_string())?;
    let vtable = unsafe { symbol() };
    if vtable.is_null() {
        return Err("the plugin returned no decoder".into());
    }
    let vtable = unsafe { &*vtable };
    if vtable.size < DECODER_V1_SIZE {
        return Err("the decoder vtable is too small".into());
    }
    if vtable.probe.is_none()
        || vtable.open.is_none()
        || vtable.close.is_none()
        || vtable.pull.is_none()
        || vtable.seek.is_none()
        || vtable.sample_rate.is_none()
        || vtable.channels.is_none()
        || vtable.presentation_frames.is_none()
    {
        return Err("the decoder vtable is incomplete".into());
    }
    Ok(DecoderVtable {
        size: vtable.size,
        extensions: vtable.extensions,
        probe: vtable.probe,
        open: vtable.open,
        close: vtable.close,
        sample_rate: vtable.sample_rate,
        channels: vtable.channels,
        presentation_frames: vtable.presentation_frames,
        pull: vtable.pull,
        seek: vtable.seek,
    })
}

fn read_extensions(vtable: &DecoderVtable) -> Result<Vec<String>, String> {
    if vtable.extensions.is_null() {
        return Err("the plugin lists no file extensions".into());
    }
    let mut out = Vec::new();
    let mut index = 0usize;
    loop {
        let ptr = unsafe { *vtable.extensions.add(index) };
        if ptr.is_null() {
            break;
        }
        let ext = read_cstr(ptr).to_ascii_lowercase();
        if !ext.is_empty() {
            out.push(ext);
        }
        index += 1;
        if index > 64 {
            break;
        }
    }
    if out.is_empty() {
        return Err("the plugin lists no file extensions".into());
    }
    Ok(out)
}

fn failed(id: &str, file_name: &str, error: impl Into<String>) -> Slot {
    Slot {
        id: id.to_string(),
        name: file_name.to_string(),
        version: String::new(),
        extensions: Vec::new(),
        enabled: false,
        error: Some(error.into()),
        file_name: file_name.to_string(),
        decoder: None,
    }
}

fn status_of(slot: &Slot) -> PluginStatus {
    PluginStatus {
        id: slot.id.clone(),
        name: slot.name.clone(),
        version: slot.version.clone(),
        extensions: slot.extensions.clone(),
        enabled: slot.enabled,
        error: slot.error.clone(),
        file_name: slot.file_name.clone(),
    }
}

fn lists(slot: &Slot, extension: &str) -> bool {
    slot.extensions.iter().any(|ext| ext == extension)
}

fn usable(slot: &Slot, extension: &str) -> bool {
    slot.enabled && slot.error.is_none() && slot.decoder.is_some() && lists(slot, extension)
}

fn call_probe(plugin: &DecoderPlugin, path: &Path) -> Result<ProbeFacts, Error> {
    let path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| Error::new("the path contains a NUL byte"))?;
    let mut out = ProbeResult {
        sample_rate: 0,
        channels: 0,
        _pad: 0,
        presentation_frames: 0,
        track_number: 0,
        disc: 0,
        title: [0; seasound_plugin_api::TEXT_CAP],
        artist: [0; seasound_plugin_api::TEXT_CAP],
        album: [0; seasound_plugin_api::TEXT_CAP],
        format_label: [0; 64],
    };
    let mut err = [0u8; ERR_CAP];
    let probe = plugin.vtable.probe.expect("checked");
    let code = unsafe {
        probe(
            path.as_ptr(),
            &mut out,
            err.as_mut_ptr().cast::<c_char>(),
            err.len(),
        )
    };
    if code != OK {
        return Err(Error::new(read_error(err.as_ptr().cast())));
    }
    if out.sample_rate == 0 || out.channels == 0 || out.presentation_frames == 0 {
        return Err(Error::new("the plugin probe returned no duration"));
    }
    Ok(ProbeFacts {
        sample_rate: out.sample_rate,
        channels: out.channels,
        presentation_frames: out.presentation_frames,
        track_number: out.track_number,
        disc: out.disc,
        title: read_text(&out.title),
        artist: read_text(&out.artist),
        album: read_text(&out.album),
        format_label: read_text(&out.format_label),
    })
}

fn call_open(plugin: &Arc<DecoderPlugin>, path: &Path, params: DecodeRequest) -> Result<PluginPcm, Error> {
    let path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| Error::new("the path contains a NUL byte"))?;
    let mut err = [0u8; ERR_CAP];
    let open = plugin.vtable.open.expect("checked");
    let stream = unsafe {
        open(
            path.as_ptr(),
            &params,
            err.as_mut_ptr().cast::<c_char>(),
            err.len(),
        )
    };
    if stream.is_null() {
        return Err(Error::new(read_error(err.as_ptr().cast())));
    }
    let sample_rate = unsafe { (plugin.vtable.sample_rate.expect("checked"))(stream) };
    let channels = unsafe { (plugin.vtable.channels.expect("checked"))(stream) };
    let frames = unsafe { (plugin.vtable.presentation_frames.expect("checked"))(stream) };
    if sample_rate == 0 || channels == 0 {
        unsafe { (plugin.vtable.close.expect("checked"))(stream) };
        return Err(Error::new("the plugin opened a file with no sample rate"));
    }
    Ok(PluginPcm {
        plugin: Arc::clone(plugin),
        stream,
        sample_rate,
        channels,
        frames,
    })
}

struct PluginPcm {
    plugin: Arc<DecoderPlugin>,
    stream: *mut c_void,
    sample_rate: u32,
    channels: u16,
    frames: u64,
}

unsafe impl Send for PluginPcm {}

impl Drop for PluginPcm {
    fn drop(&mut self) {
        if self.stream.is_null() {
            return;
        }
        let close = self.plugin.vtable.close.expect("checked");
        unsafe { close(self.stream) };
        self.stream = std::ptr::null_mut();
    }
}

impl PcmSource for PluginPcm {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn channel_map(&self) -> ChannelMap {
        match self.channels {
            1 => ChannelMap::Mono,
            2 => ChannelMap::Stereo,
            6 => ChannelMap::Surround51,
            n => ChannelMap::Other(n),
        }
    }

    fn presentation_frames(&self) -> u64 {
        self.frames
    }

    fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
        let channels = self.channels as usize;
        if channels == 0 || interleaved.is_empty() {
            return Ok(0);
        }
        let frame_cap = interleaved.len() / channels;
        let mut written = 0usize;
        let mut err = [0u8; ERR_CAP];
        let pull = self.plugin.vtable.pull.expect("checked");
        let code = unsafe {
            pull(
                self.stream,
                interleaved.as_mut_ptr(),
                frame_cap,
                &mut written,
                err.as_mut_ptr().cast::<c_char>(),
                err.len(),
            )
        };
        if code != OK {
            return Err(Error::new(read_error(err.as_ptr().cast())));
        }
        Ok(written)
    }

    fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
        let mut err = [0u8; ERR_CAP];
        let seek = self.plugin.vtable.seek.expect("checked");
        let code = unsafe {
            seek(
                self.stream,
                frame,
                err.as_mut_ptr().cast::<c_char>(),
                err.len(),
            )
        };
        if code != OK {
            return Err(Error::new(read_error(err.as_ptr().cast())));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seasound_model::DecodeParams;
    use std::sync::Mutex as StdMutex;
    use wavicle::{encode_int, EncodeParams};

    static LOCK: StdMutex<()> = StdMutex::new(());

    fn stage(dir: &Path) {
        let debug = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
        for name in ["libseasound_decoder_wavpack.so", "libseasound_plugin_badabi.so"] {
            let direct = debug.join(name);
            let deps = debug.join("deps").join(name);
            let src = if direct.is_file() { direct } else { deps };
            assert!(src.is_file(), "missing {}", src.display());
            std::fs::copy(&src, dir.join(name)).unwrap();
        }
    }

    fn tone() -> (Vec<i32>, PathBuf) {
        let mut samples = Vec::with_capacity(400 * 2);
        for frame in 0..400 {
            samples.push(frame * 20);
            samples.push(8000 - frame * 10);
        }
        let bytes = encode_int(
            EncodeParams {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 16,
            },
            &samples,
        )
        .expect("encode");
        let dir = std::env::temp_dir().join(format!("seasound-wv-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tone.wv");
        std::fs::write(&path, bytes).unwrap();
        (samples, path)
    }

    #[test]
    fn wavpack_plugin_probes_decodes_and_seeks() {
        let _guard = LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("seasound-plugins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        stage(&dir);
        load_plugin_dirs(&[dir], &[]);
        let statuses = plugin_statuses();
        let wavpack = statuses.iter().find(|plugin| plugin.id == "wavpack").unwrap();
        assert!(wavpack.error.is_none(), "{:?}", wavpack.error);
        assert!(wavpack.enabled);
        assert_eq!(wavpack.extensions, vec!["wv".to_string()]);
        let bad = statuses.iter().find(|plugin| plugin.id == "badabi").unwrap();
        assert!(bad.error.as_deref().unwrap_or("").contains("ABI 99"));
        assert!(!bad.enabled);

        let (samples, path) = tone();
        let source = crate::probe_file(&path);
        assert!(source.error.is_none(), "{:?}", source.error);
        assert_eq!(source.format_label, "WavPack");
        assert_eq!(source.sample_rate, 44100);
        assert_eq!(source.channels, 2);
        assert_eq!(source.presentation_frames, 400);
        assert_eq!(source.segments.len(), 1);
        assert_eq!(source.delay, 0);

        let mut decoder = crate::open_decoder(&path, DecodeParams::from(&source)).unwrap();
        let mut interleaved = vec![0.0; 8];
        let frames = decoder.pull(&mut interleaved).unwrap();
        assert_eq!(frames, 4);
        let scale = 32768.0;
        for index in 0..4 {
            let left = samples[index * 2] as f32 / scale;
            let right = samples[index * 2 + 1] as f32 / scale;
            assert!((interleaved[index * 2] - left).abs() < 1e-4);
            assert!((interleaved[index * 2 + 1] - right).abs() < 1e-4);
        }
        decoder.seek_presentation(100).unwrap();
        let frames = decoder.pull(&mut interleaved[..2]).unwrap();
        assert_eq!(frames, 1);
        let left = samples[200] as f32 / scale;
        let right = samples[201] as f32 / scale;
        assert!((interleaved[0] - left).abs() < 1e-4);
        assert!((interleaved[1] - right).abs() < 1e-4);

        set_plugin_enabled("wavpack", false);
        let disabled = crate::probe_file(&path);
        assert!(disabled.error.as_deref().unwrap_or("").contains("turned off"));
        assert!(!disabled.playable());
        load_plugin_dirs(&[], &[]);
    }
}

fn extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}
