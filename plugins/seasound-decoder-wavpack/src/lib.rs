//! WavPack decoder. The whole file is decoded into memory on open. wavicle
//! exposes a whole-stream decoder, and seek is then an index into that buffer.
//! Lossless mono and stereo only. Hybrid, DSD, correction files, and more than
//! two channels become an error.

use std::ffi::{c_char, c_void};
use std::path::Path;

use seasound_plugin_api::{
    write_error, write_text, DecodeRequest, DecoderVtable, PluginInfo, ProbeResult, ABI_VERSION,
    DECODER_V1_SIZE, KIND_DECODER, OK, TEXT_CAP,
};
use wavicle::{decode_stream, Error as WvError, Scope, StreamInfo};

const ID: &str = "wavpack\0";
const NAME: &str = "WavPack\0";
const VERSION: &str = "0.1.0\0";

#[repr(C)]
struct ExtList {
    items: [*const c_char; 2],
}

unsafe impl Sync for ExtList {}

static EXTENSIONS: ExtList = ExtList {
    items: [c"wv".as_ptr(), std::ptr::null()],
};

#[no_mangle]
pub extern "C" fn seasound_plugin_info() -> *const PluginInfo {
    static INFO: PluginInfo = PluginInfo {
        abi: ABI_VERSION,
        kind: KIND_DECODER,
        id: ID.as_ptr().cast(),
        name: NAME.as_ptr().cast(),
        version: VERSION.as_ptr().cast(),
    };
    &INFO
}

#[no_mangle]
pub extern "C" fn seasound_decoder_v1() -> *const DecoderVtable {
    static VTABLE: DecoderVtable = DecoderVtable {
        size: DECODER_V1_SIZE,
        extensions: EXTENSIONS.items.as_ptr(),
        probe: Some(probe),
        open: Some(open),
        close: Some(close),
        sample_rate: Some(sample_rate),
        channels: Some(channels),
        presentation_frames: Some(presentation_frames),
        pull: Some(pull),
        seek: Some(seek),
    };
    &VTABLE
}

struct Stream {
    pcm: Vec<f32>,
    channels: u16,
    sample_rate: u32,
    frames: u64,
    pos: u64,
}

unsafe extern "C" fn probe(
    path: *const c_char,
    out: *mut ProbeResult,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    match probe_inner(path) {
        Ok(result) => {
            if !out.is_null() {
                unsafe { *out = result };
            }
            OK
        }
        Err(message) => {
            write_error(err, err_len, &message);
            1
        }
    }
}

fn probe_inner(path: *const c_char) -> Result<ProbeResult, String> {
    let path = c_path(path)?;
    let bytes = std::fs::read(&path).map_err(|err| format!("could not read the file: {err}"))?;
    let info = StreamInfo::scan(&bytes).map_err(explain)?;
    if !info.lossless {
        return Err("WavPack hybrid and lossy files are not supported".into());
    }
    if info.channels == 0 || info.channels > 2 {
        return Err("WavPack files with more than two channels are not supported".into());
    }
    let frames = info
        .total_samples
        .filter(|frames| *frames > 0)
        .ok_or_else(|| "the file has no duration".to_string())?;
    if info.sample_rate == 0 {
        return Err("the file has no sample rate".into());
    }
    let mut result = ProbeResult {
        sample_rate: info.sample_rate,
        channels: info.channels as u16,
        _pad: 0,
        presentation_frames: frames,
        track_number: 0,
        disc: 0,
        title: [0; TEXT_CAP],
        artist: [0; TEXT_CAP],
        album: [0; TEXT_CAP],
        format_label: [0; 64],
    };
    let title = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Untitled");
    write_text(&mut result.title, title);
    write_text(&mut result.format_label, "WavPack");
    Ok(result)
}

unsafe extern "C" fn open(
    path: *const c_char,
    params: *const DecodeRequest,
    err: *mut c_char,
    err_len: usize,
) -> *mut c_void {
    let params = if params.is_null() {
        DecodeRequest {
            delay: 0,
            padding: 0,
            presentation_frames: 0,
        }
    } else {
        unsafe { *params }
    };
    match open_inner(path, params) {
        Ok(stream) => Box::into_raw(Box::new(stream)).cast(),
        Err(message) => {
            write_error(err, err_len, &message);
            std::ptr::null_mut()
        }
    }
}

fn open_inner(path: *const c_char, params: DecodeRequest) -> Result<Stream, String> {
    let path = c_path(path)?;
    let bytes = std::fs::read(&path).map_err(|err| format!("could not read the file: {err}"))?;
    let decoded = decode_stream(&bytes).map_err(explain)?;
    if decoded.channels == 0 || decoded.channels > 2 {
        return Err("WavPack files with more than two channels are not supported".into());
    }
    if decoded.sample_rate == 0 {
        return Err("the file has no sample rate".into());
    }
    let channels = decoded.channels as usize;
    let file_frames = decoded.samples.len() / channels;
    let delay = params.delay as usize;
    if delay > file_frames {
        return Err("encoder delay is longer than the file".into());
    }
    let available = file_frames - delay;
    let padding = (params.padding as usize).min(available);
    let audible = available - padding;
    let frames = if params.presentation_frames == 0 {
        audible
    } else {
        audible.min(params.presentation_frames as usize)
    };
    if frames == 0 {
        return Err("the file has no duration".into());
    }
    let scale = if decoded.is_float {
        1.0
    } else {
        let bits = decoded.bits_per_sample.clamp(2, 32);
        (1u32 << (bits - 1)) as f32
    };
    let start = delay * channels;
    let end = start + frames * channels;
    let pcm = if decoded.is_float {
        decoded.samples[start..end]
            .iter()
            .map(|sample| f32::from_bits(*sample as u32))
            .collect()
    } else {
        decoded.samples[start..end]
            .iter()
            .map(|sample| *sample as f32 / scale)
            .collect()
    };
    Ok(Stream {
        pcm,
        channels: decoded.channels as u16,
        sample_rate: decoded.sample_rate,
        frames: frames as u64,
        pos: 0,
    })
}

unsafe extern "C" fn close(stream: *mut c_void) {
    if stream.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(stream.cast::<Stream>()) });
}

unsafe extern "C" fn sample_rate(stream: *mut c_void) -> u32 {
    stream_ref(stream).map(|stream| stream.sample_rate).unwrap_or(0)
}

unsafe extern "C" fn channels(stream: *mut c_void) -> u16 {
    stream_ref(stream).map(|stream| stream.channels).unwrap_or(0)
}

unsafe extern "C" fn presentation_frames(stream: *mut c_void) -> u64 {
    stream_ref(stream).map(|stream| stream.frames).unwrap_or(0)
}

unsafe extern "C" fn pull(
    stream: *mut c_void,
    interleaved: *mut f32,
    frame_cap: usize,
    frames_out: *mut usize,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let Some(stream) = stream_mut(stream) else {
        write_error(err, err_len, "the decoder is closed");
        return 1;
    };
    if frames_out.is_null() {
        write_error(err, err_len, "the host passed no frame count");
        return 1;
    }
    let channels = stream.channels as usize;
    let remain = stream.frames.saturating_sub(stream.pos) as usize;
    let frames = frame_cap.min(remain);
    if frames > 0 && !interleaved.is_null() {
        let start = stream.pos as usize * channels;
        let end = start + frames * channels;
        unsafe {
            std::ptr::copy_nonoverlapping(stream.pcm[start..end].as_ptr(), interleaved, end - start);
        }
    }
    stream.pos += frames as u64;
    unsafe { *frames_out = frames };
    OK
}

unsafe extern "C" fn seek(
    stream: *mut c_void,
    frame: u64,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let Some(stream) = stream_mut(stream) else {
        write_error(err, err_len, "the decoder is closed");
        return 1;
    };
    stream.pos = frame.min(stream.frames);
    OK
}

fn stream_ref<'a>(stream: *mut c_void) -> Option<&'a Stream> {
    if stream.is_null() {
        None
    } else {
        Some(unsafe { &*stream.cast::<Stream>() })
    }
}

fn stream_mut<'a>(stream: *mut c_void) -> Option<&'a mut Stream> {
    if stream.is_null() {
        None
    } else {
        Some(unsafe { &mut *stream.cast::<Stream>() })
    }
}

fn c_path(path: *const c_char) -> Result<std::path::PathBuf, String> {
    if path.is_null() {
        return Err("the path is missing".into());
    }
    let text = unsafe { std::ffi::CStr::from_ptr(path) }
        .to_str()
        .map_err(|_| "the path is not UTF-8".to_string())?;
    Ok(Path::new(text).to_path_buf())
}

fn explain(err: WvError) -> String {
    match err {
        WvError::OutOfScope(Scope::Hybrid) => {
            "WavPack hybrid and lossy files are not supported".into()
        }
        WvError::OutOfScope(Scope::Dsd) => "WavPack DSD is not supported".into(),
        WvError::OutOfScope(Scope::CorrectionFile) => {
            "WavPack correction files are not supported".into()
        }
        WvError::OutOfScope(Scope::MoreThanTwoChannels | Scope::MultichannelSpanning) => {
            "WavPack files with more than two channels are not supported".into()
        }
        WvError::UnsupportedVersion(_) => "this WavPack version is not supported".into(),
        other => other.to_string(),
    }
}
