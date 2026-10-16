//! C ABI for SeaSound decoder plugins.
//!
//! A plugin is a shared library. It exports [`seasound_plugin_info`] and, for a
//! decoder, [`seasound_decoder_v1`]. The host and the plugin both compile this
//! crate, so the `#[repr(C)]` layouts match. Strings that cross the boundary are
//! NUL-terminated UTF-8. The host owns every error buffer it passes in.
//!
//! The shared library stays loaded for the life of the process. A stream pointer
//! from `open` is only used on the decode thread, and `close` runs before the
//! library could be unloaded.

use std::ffi::{c_char, CStr};
use std::os::raw::c_void;

/// ABI version exported by [`PluginInfo::abi`].
pub const ABI_VERSION: u32 = 1;

/// [`PluginInfo::kind`] value for a decoder plugin.
pub const KIND_DECODER: u32 = 1;

/// Success. Any other `i32` is a failure, with text in the host error buffer.
pub const OK: i32 = 0;

/// Bytes available for one tag or label, including the trailing NUL.
pub const TEXT_CAP: usize = 512;

/// Plugin identity. Pointers refer to memory owned by the plugin.
#[repr(C)]
pub struct PluginInfo {
    pub abi: u32,
    pub kind: u32,
    pub id: *const c_char,
    pub name: *const c_char,
    pub version: *const c_char,
}

/// The pointers are immutable and point at the plugin's own static strings.
unsafe impl Sync for PluginInfo {}

/// Facts the host needs to build one playlist row. Tags are NUL-terminated.
#[repr(C)]
pub struct ProbeResult {
    pub sample_rate: u32,
    pub channels: u16,
    pub _pad: u16,
    pub presentation_frames: u64,
    pub track_number: u32,
    pub disc: u32,
    pub title: [u8; TEXT_CAP],
    pub artist: [u8; TEXT_CAP],
    pub album: [u8; TEXT_CAP],
    pub format_label: [u8; 64],
}

/// Presentation timeline. `delay` frames at the start of the file are skipped,
/// and `presentation_frames` is how many audible frames follow. WavPack leaves
/// delay and padding at zero.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DecodeRequest {
    pub delay: u64,
    pub padding: u64,
    pub presentation_frames: u64,
}

/// Decoder vtable. `size` is `size_of::<DecoderVtable>()` so a newer host can
/// ignore fields an older plugin does not have.
#[repr(C)]
pub struct DecoderVtable {
    pub size: u32,
    /// NUL-terminated list of extensions without a dot, itself NUL-terminated.
    pub extensions: *const *const c_char,
    pub probe: Option<
        unsafe extern "C" fn(
            path: *const c_char,
            out: *mut ProbeResult,
            err: *mut c_char,
            err_len: usize,
        ) -> i32,
    >,
    pub open: Option<
        unsafe extern "C" fn(
            path: *const c_char,
            params: *const DecodeRequest,
            err: *mut c_char,
            err_len: usize,
        ) -> *mut c_void,
    >,
    pub close: Option<unsafe extern "C" fn(stream: *mut c_void)>,
    pub sample_rate: Option<unsafe extern "C" fn(stream: *mut c_void) -> u32>,
    pub channels: Option<unsafe extern "C" fn(stream: *mut c_void) -> u16>,
    pub presentation_frames: Option<unsafe extern "C" fn(stream: *mut c_void) -> u64>,
    pub pull: Option<
        unsafe extern "C" fn(
            stream: *mut c_void,
            interleaved: *mut f32,
            frame_cap: usize,
            frames_out: *mut usize,
            err: *mut c_char,
            err_len: usize,
        ) -> i32,
    >,
    pub seek: Option<
        unsafe extern "C" fn(
            stream: *mut c_void,
            frame: u64,
            err: *mut c_char,
            err_len: usize,
        ) -> i32,
    >,
}

/// Function pointers and the extension list are immutable for the life of the library.
unsafe impl Send for DecoderVtable {}
unsafe impl Sync for DecoderVtable {}

/// `size` of the v1 vtable. A plugin that reports less than this is rejected.
pub const DECODER_V1_SIZE: u32 = std::mem::size_of::<DecoderVtable>() as u32;

/// Write a short UTF-8 message into a host-owned buffer. Always NUL-terminates
/// when `len > 0`.
pub fn write_error(buf: *mut c_char, len: usize, message: &str) {
    if buf.is_null() || len == 0 {
        return;
    }
    let bytes = message.as_bytes();
    let n = bytes.len().min(len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast(), n);
        *buf.add(n) = 0;
    }
}

/// Read a NUL-terminated buffer the host just asked a plugin to fill.
pub fn read_error(buf: *const c_char) -> String {
    if buf.is_null() {
        return "the plugin failed".to_string();
    }
    unsafe { CStr::from_ptr(buf) }
        .to_string_lossy()
        .into_owned()
}

pub fn write_text(dest: &mut [u8], text: &str) {
    dest.fill(0);
    let bytes = text.as_bytes();
    let n = bytes.len().min(dest.len().saturating_sub(1));
    dest[..n].copy_from_slice(&bytes[..n]);
}

pub fn read_text(src: &[u8]) -> String {
    let end = src.iter().position(|byte| *byte == 0).unwrap_or(src.len());
    String::from_utf8_lossy(&src[..end]).into_owned()
}

pub fn read_cstr<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    unsafe { CStr::from_ptr(ptr) }.to_str().unwrap_or("")
}
