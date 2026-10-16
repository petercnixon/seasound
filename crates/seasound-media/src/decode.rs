//! Symphonia decoder on the presentation timeline.
//!
//! `enable_gapless` stays off. Delay and padding passed in by the probe are applied
//! here, once, so cue points and chapter points are presentation frames.

use std::fs::File;
use std::path::Path;

use seasound_model::{ChannelMap, DecodeParams, Error, PcmSource};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

pub fn open_decoder(path: &Path, params: DecodeParams) -> Result<Box<dyn PcmSource>, Error> {
    let source = SymphoniaSource::open(path, params)?;
    Ok(Box::new(source))
}

struct SymphoniaSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u16,
    map: ChannelMap,
    time_base: Option<TimeBase>,
    delay: u64,
    presentation_frames: u64,
    /// Next decoded frame index. This counts from the start of the file, including delay.
    decoded_frame: u64,
    /// Drop decoded frames until this index. Equals `delay` at the start, and
    /// `delay + target` after a seek.
    skip_until: u64,
    pending: Vec<f32>,
    pending_pos: usize,
    packet_buf: Option<SampleBuffer<f32>>,
    eof: bool,
}

impl SymphoniaSource {
    fn open(path: &Path, params: DecodeParams) -> Result<Self, Error> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
            hint.with_extension(ext);
        }
        let mut format_opts = FormatOptions::default();
        format_opts.enable_gapless = false;
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &format_opts, &MetadataOptions::default())
            .map_err(|err| Error::new(format!("could not read {}: {err}", path.display())))?;
        let format = probed.format;
        let tracks: Vec<_> = format.tracks().to_vec();
        let mut chosen = None;
        for track in &tracks {
            let opts = DecoderOptions::default();
            if let Ok(decoder) = symphonia::default::get_codecs().make(&track.codec_params, &opts) {
                chosen = Some((track.clone(), decoder));
                break;
            }
        }
        let Some((track, decoder)) = chosen else {
            return Err(Error::new(format!(
                "no supported audio stream in {}",
                path.display()
            )));
        };
        let codec = track.codec_params.codec;
        if codec == CODEC_TYPE_OPUS {
            return Err(Error::new(
                "Opus is not supported yet. The pre-skip in this Symphonia version is incomplete.",
            ));
        }
        if let Some(extra) = track.codec_params.extra_data.as_deref() {
            if is_he_aac(extra) {
                return Err(Error::new(
                    "HE-AAC is not supported. SeaSound decodes AAC-LC.",
                ));
            }
        }
        let sample_rate = track
            .codec_params
            .sample_rate
            .ok_or_else(|| Error::new("the file has no sample rate"))?;
        let channels = track
            .codec_params
            .channels
            .map(|channels| channels.count() as u16)
            .filter(|count| *count > 0)
            .unwrap_or(2);
        let map = channel_map(channels);
        let presentation_frames = if params.presentation_frames == 0 {
            u64::MAX / 4
        } else {
            params.presentation_frames
        };
        Ok(Self {
            format,
            decoder,
            track_id: track.id,
            sample_rate,
            channels,
            map,
            time_base: track.codec_params.time_base,
            delay: params.delay,
            presentation_frames,
            decoded_frame: 0,
            skip_until: params.delay,
            pending: Vec::new(),
            pending_pos: 0,
            packet_buf: None,
            eof: false,
        })
    }

    fn frame_to_ts(&self, frame: u64) -> u64 {
        let Some(time_base) = self.time_base else {
            return frame;
        };
        let rate = u64::from(self.sample_rate).max(1);
        let seconds = frame / rate;
        let frac = (frame % rate) as f64 / rate as f64;
        time_base.calc_timestamp(Time { seconds, frac })
    }

    fn ts_to_frame(&self, ts: u64) -> u64 {
        let Some(time_base) = self.time_base else {
            return ts;
        };
        let time = time_base.calc_time(ts);
        let rate = f64::from(self.sample_rate);
        let frames = time.seconds as f64 * rate + time.frac * rate;
        if frames.is_finite() && frames > 0.0 {
            frames.round() as u64
        } else {
            0
        }
    }

    fn refill(&mut self) -> Result<bool, Error> {
        if self.eof {
            return Ok(false);
        }
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(SymError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.eof = true;
                    return Ok(false);
                }
                Err(err) => {
                    let text = err.to_string();
                    if text.to_ascii_lowercase().contains("end of") {
                        self.eof = true;
                        return Ok(false);
                    }
                    return Err(Error::new(text));
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let frames = decoded.frames().max(1) as u64;
                    let samples = frames * (spec.channels.count().max(1) as u64);
                    let need_new = self
                        .packet_buf
                        .as_ref()
                        .map(|buf| (buf.capacity() as u64) < samples)
                        .unwrap_or(true);
                    if need_new {
                        self.packet_buf = Some(SampleBuffer::<f32>::new(frames, spec));
                    }
                    if let Some(buf) = self.packet_buf.as_mut() {
                        buf.copy_interleaved_ref(decoded);
                        self.pending.extend_from_slice(buf.samples());
                    }
                    return Ok(true);
                }
                Err(SymError::DecodeError(_)) => continue,
                Err(SymError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(SymError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.eof = true;
                    return Ok(false);
                }
                Err(err) => return Err(Error::new(err.to_string())),
            }
        }
    }
}

impl PcmSource for SymphoniaSource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn channel_map(&self) -> ChannelMap {
        self.map
    }

    fn presentation_frames(&self) -> u64 {
        self.presentation_frames
    }

    fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
        let channels = self.channels as usize;
        if channels == 0 {
            return Ok(0);
        }
        let max_frames = interleaved.len() / channels;
        let mut filled = 0usize;
        while filled < max_frames {
            if self.pending_pos + channels > self.pending.len() {
                self.compact_pending();
                if !self.refill()? && self.pending_pos + channels > self.pending.len() {
                    break;
                }
                if self.pending_pos + channels > self.pending.len() {
                    continue;
                }
            }
            let decoded = self.decoded_frame;
            self.decoded_frame += 1;
            let start = self.pending_pos;
            self.pending_pos += channels;
            if decoded < self.skip_until {
                continue;
            }
            let presentation = decoded - self.delay;
            if presentation >= self.presentation_frames {
                self.eof = true;
                break;
            }
            let dest = filled * channels;
            interleaved[dest..dest + channels]
                .copy_from_slice(&self.pending[start..start + channels]);
            filled += 1;
        }
        Ok(filled)
    }

    fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
        let frame = frame.min(self.presentation_frames.saturating_sub(1));
        let target = frame + self.delay;
        let ts = self.frame_to_ts(target);
        let seeked = self
            .format
            .seek(SeekMode::Accurate, SeekTo::TimeStamp { ts, track_id: self.track_id })
            .or_else(|_| {
                let rate = u64::from(self.sample_rate).max(1);
                let time = Time {
                    seconds: target / rate,
                    frac: (target % rate) as f64 / rate as f64,
                };
                self.format.seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time,
                        track_id: Some(self.track_id),
                    },
                )
            })
            .map_err(|err| Error::new(format!("seek failed: {err}")))?;
        self.decoder.reset();
        self.pending.clear();
        self.pending_pos = 0;
        self.packet_buf = None;
        self.eof = false;
        self.decoded_frame = self.ts_to_frame(seeked.actual_ts);
        self.skip_until = target;
        // A seek that lands past the target cannot be rewound inside this packet.
        // Accurate mode lands on or before the timestamp. If it does not, start there.
        if self.decoded_frame > target {
            self.skip_until = self.decoded_frame;
        }
        Ok(())
    }
}

impl SymphoniaSource {
    fn compact_pending(&mut self) {
        if self.pending_pos == 0 {
            return;
        }
        self.pending.drain(..self.pending_pos);
        self.pending_pos = 0;
    }
}

fn channel_map(channels: u16) -> ChannelMap {
    match channels {
        1 => ChannelMap::Mono,
        2 => ChannelMap::Stereo,
        6 => ChannelMap::Surround51,
        n => ChannelMap::Other(n),
    }
}

/// AudioSpecificConfig: the first five bits are the audio object type.
/// 2 is AAC-LC. 5 is HE-AAC. 29 is HE-AAC v2.
fn is_he_aac(extra: &[u8]) -> bool {
    if extra.is_empty() {
        return false;
    }
    let aot = extra[0] >> 3;
    aot == 5 || aot == 29
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gapless::read_mp3_info;
    use std::path::PathBuf;

    #[test]
    fn trimmed_mp3_matches_the_untrimmed_stream_after_the_delay() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/Gapless MP3 Album/101. Fatboy Slim - Gangster Trippin'.mp3");
        if !path.is_file() {
            return;
        }
        let info = read_mp3_info(&path);
        let delay = info.trim.delay;
        assert_eq!(delay, 576, "the example album is LAME with a 576-frame delay");
        let count = 2048u64;
        let mut raw = open_decoder(
            &path,
            DecodeParams {
                delay: 0,
                padding: 0,
                presentation_frames: delay + count,
            },
        )
        .expect("raw decoder");
        let mut trimmed = open_decoder(
            &path,
            DecodeParams {
                delay,
                padding: info.trim.padding,
                presentation_frames: count,
            },
        )
        .expect("trimmed decoder");
        let channels = raw.channels() as usize;
        assert_eq!(trimmed.channels() as usize, channels);
        let mut raw_buf = vec![0f32; (delay as usize + count as usize) * channels];
        let mut got = 0usize;
        while got < raw_buf.len() / channels {
            let n = raw
                .pull(&mut raw_buf[got * channels..])
                .expect("raw pull");
            if n == 0 {
                break;
            }
            got += n;
        }
        assert!(got >= delay as usize + 64, "decoded {got} frames");
        let mut trimmed_buf = vec![0f32; count as usize * channels];
        let mut got_trimmed = 0usize;
        while got_trimmed < count as usize {
            let n = trimmed
                .pull(&mut trimmed_buf[got_trimmed * channels..])
                .expect("trimmed pull");
            if n == 0 {
                break;
            }
            got_trimmed += n;
        }
        let compare = got_trimmed.min(got.saturating_sub(delay as usize));
        assert!(compare >= 1024, "only compared {compare} frames");
        let raw_slice = &raw_buf[delay as usize * channels..(delay as usize + compare) * channels];
        let trimmed_slice = &trimmed_buf[..compare * channels];
        for (index, (left, right)) in raw_slice.iter().zip(trimmed_slice).enumerate() {
            let delta = (left - right).abs();
            assert!(
                delta < 1e-5,
                "sample {index} differs by {delta}: raw {left} trimmed {right}"
            );
        }
    }
}
