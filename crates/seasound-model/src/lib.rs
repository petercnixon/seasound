//! Shared types for SeaSound. This crate does not open files or talk to a sound device.

mod playlist;

pub use playlist::{
    load_saved_playlist, playlist_from_descriptions, playlist_matching, reuse_source_id, Back,
    Cursor, PlayState, Playlist, PlaylistEntry, Repeat, RowView, SavedPlaylist, SavedPosition,
    SavedRow, Step,
};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Recoverable failure. Library paths return this instead of panicking.
#[derive(Debug, Clone)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::new(value.to_string())
    }
}

impl From<String> for Error {
    fn from(value: String) -> Self {
        Error::new(value)
    }
}

/// Stable id for one probed file inside a playlist.
///
/// Ids are never reused in this process. A decoder is remembered by id, so a
/// new playlist that started again at 1 would keep playing the previous file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId(pub u64);

impl SourceId {
    pub fn fresh() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// How interleaved samples are arranged in a [`PcmSource`] buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelMap {
    Mono,
    Stereo,
    /// L, R, C, LFE, Ls, Rs. LFE is dropped by the mixer.
    Surround51,
    Other(u16),
}

/// Container or codec family, used for display and for choosing a metadata reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioFormat {
    Mp3,
    Aac,
    Flac,
    Alac,
    Wav,
    Aiff,
    Vorbis,
    Other,
}

impl AudioFormat {
    pub fn label(self) -> &'static str {
        match self {
            AudioFormat::Mp3 => "MP3",
            AudioFormat::Aac => "AAC",
            AudioFormat::Flac => "FLAC",
            AudioFormat::Alac => "ALAC",
            AudioFormat::Wav => "WAV",
            AudioFormat::Aiff => "AIFF",
            AudioFormat::Vorbis => "Vorbis",
            AudioFormat::Other => "Audio",
        }
    }
}

/// One playable range on a file's presentation timeline.
///
/// `start_frame` is inclusive and `end_frame` is exclusive. Both are in PCM frames
/// after encoder delay has been removed, so cue tracks and chapters share one clock
/// with gapless trim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub number: u32,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub start_frame: u64,
    pub end_frame: u64,
}

impl Segment {
    pub fn frames(&self) -> u64 {
        self.end_frame.saturating_sub(self.start_frame)
    }
}

/// Embedded image, kept out of the playback thread's playlist copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cover {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// One audio file after probing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub id: SourceId,
    pub path: PathBuf,
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub channels: u16,
    /// Average file bitrate in bits per second. Zero when the size or duration is unknown.
    #[serde(default)]
    pub bitrate_bps: u32,
    /// Bits per coded sample for PCM and lossless codecs. Zero when the file does not say.
    #[serde(default)]
    pub bits_per_sample: u16,
    /// Encoder delay, in PCM frames. Zero when the file has no trim metadata.
    pub delay: u64,
    /// Encoder padding at the end, in PCM frames.
    pub padding: u64,
    /// Audible length after trim.
    pub presentation_frames: u64,
    pub artist: String,
    pub album: String,
    pub title: String,
    pub disc: u32,
    pub track_number: u32,
    pub segments: Vec<Segment>,
    /// Shown above the rows when this file contributed more than one segment.
    pub group_title: Option<String>,
    /// The file cannot be decoded. The row stays so the failure is visible.
    pub error: Option<String>,
    /// The file plays, but cue or chapter data was ignored.
    pub warning: Option<String>,
    /// Name shown for a format the built-in enum does not list. Empty for those.
    #[serde(default)]
    pub format_label: String,
    #[serde(skip)]
    pub cover: Option<Cover>,
}

impl Source {
    pub fn format_name(&self) -> &str {
        if self.format_label.is_empty() {
            self.format.label()
        } else {
            &self.format_label
        }
    }

    pub fn playable(&self) -> bool {
        self.error.is_none()
            && self.sample_rate > 0
            && self.presentation_frames > 0
            && !self.segments.is_empty()
    }
}

/// Parameters the decoder uses to turn file samples into the presentation timeline.
#[derive(Clone, Copy, Debug)]
pub struct DecodeParams {
    pub delay: u64,
    pub padding: u64,
    pub presentation_frames: u64,
}

impl From<&Source> for DecodeParams {
    fn from(source: &Source) -> Self {
        Self {
            delay: source.delay,
            padding: source.padding,
            presentation_frames: source.presentation_frames,
        }
    }
}

/// What [`PcmSource::pull_available`] got from the decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceRead {
    /// Frames written into the buffer. Zero means the presentation has ended.
    Frames(usize),
    /// Nothing is queued yet. The presentation has not ended.
    Pending,
}

/// Decoded audio on the presentation timeline.
///
/// `pull` returns frames that the listener should hear. Encoder delay and padding
/// are already removed. `seek_presentation(0)` is the first audible frame.
pub trait PcmSource: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn channel_map(&self) -> ChannelMap;
    fn presentation_frames(&self) -> u64;

    /// Write up to `interleaved.len() / channels` frames.
    /// Returns the number of frames written. Zero means the presentation has ended.
    fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error>;

    /// Like [`pull`](Self::pull), but a buffered decoder can report [`SourceRead::Pending`]
    /// instead of blocking or pretending the presentation has ended.
    fn pull_available(&mut self, interleaved: &mut [f32]) -> Result<SourceRead, Error> {
        Ok(SourceRead::Frames(self.pull(interleaved)?))
    }

    fn seek_presentation(&mut self, frame: u64) -> Result<(), Error>;
}

/// Format a frame count as `m:ss` or `h:mm:ss`.
pub fn format_time(frames: u64, sample_rate: u32) -> String {
    if sample_rate == 0 {
        return "0:00".to_string();
    }
    let total = frames / sample_rate as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
