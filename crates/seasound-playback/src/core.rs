//! Pull presentation audio for one segment, then the next, without inserting a gap.
//!
//! A contiguous range on the same decoder is read straight through. Anything else
//! seeks. Sample rate conversion keeps its filter when the rate does not change.

use seasound_model::{ChannelMap, Error, PcmSource, SourceId, SourceRead};

use crate::resample::RateConvert;

#[derive(Clone, Copy, Debug)]
pub struct SegmentJob {
    pub source_id: SourceId,
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct PullResult {
    pub frames: usize,
    pub source_start: u64,
    pub source_frames: u64,
    pub sample_rate: u32,
    /// The decoder has no buffered audio yet. `frames == 0` here is not the end of the segment.
    pub pending: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreStats {
    pub opens: u64,
    pub seeks: u64,
}

struct Active {
    id: SourceId,
    source: Box<dyn PcmSource>,
    position: u64,
    rate: u32,
    channels: u16,
    map: ChannelMap,
}

pub struct PlayerCore {
    output_rate: u32,
    active: Option<Active>,
    waiting: Option<(SourceId, Box<dyn PcmSource>)>,
    resample: RateConvert,
    raw: Vec<f32>,
    stereo: Vec<f32>,
    converted: Vec<f32>,
    overflow: Vec<f32>,
    stats: CoreStats,
    force_seek: bool,
}

impl PlayerCore {
    pub fn new(output_rate: u32) -> Self {
        let output_rate = output_rate.max(1);
        Self {
            output_rate,
            active: None,
            waiting: None,
            resample: RateConvert::new(output_rate),
            raw: Vec::new(),
            stereo: Vec::new(),
            converted: Vec::new(),
            overflow: Vec::new(),
            stats: CoreStats::default(),
            force_seek: false,
        }
    }

    pub fn stats(&self) -> CoreStats {
        self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = CoreStats::default();
    }

    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    pub fn set_output_rate(&mut self, rate: u32) {
        if rate == 0 || rate == self.output_rate {
            return;
        }
        self.output_rate = rate;
        self.resample.set_output_rate(rate);
        self.overflow.clear();
    }

    pub fn has_source(&self, id: SourceId) -> bool {
        self.active.as_ref().is_some_and(|active| active.id == id)
            || self.waiting.as_ref().is_some_and(|(waiting, _)| *waiting == id)
    }

    pub fn source_id(&self) -> Option<SourceId> {
        self.active.as_ref().map(|active| active.id)
    }

    pub fn position(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.position)
    }

    /// The next `pull` for this id adopts `source` instead of the current decoder.
    pub fn store_prefetch(&mut self, id: SourceId, source: Box<dyn PcmSource>) {
        self.waiting = Some((id, source));
    }

    /// The next pull seeks even when the decoder is already at the target.
    pub fn force_seek(&mut self) {
        self.force_seek = true;
        self.overflow.clear();
    }

    pub fn discard_output(&mut self) {
        self.overflow.clear();
    }

    /// Drop a decoder that failed. A later play of this file has to open it again.
    ///
    /// A prefetch for a different file stays. The resampler output from the failed
    /// file is discarded so it is not played as the start of the next one.
    pub fn forget(&mut self, id: SourceId) {
        if self.active.as_ref().is_some_and(|active| active.id == id) {
            self.active = None;
            self.overflow.clear();
            self.force_seek = true;
        }
        if self.waiting.as_ref().is_some_and(|(waiting, _)| *waiting == id) {
            self.waiting = None;
        }
    }

    /// Drop the open decoder. The next pull has to be given a new one.
    ///
    /// Call this when the playlist is replaced. Source ids are not a guarantee
    /// on their own if a caller rebuilds a playlist with an id that is still open.
    pub fn release(&mut self) {
        self.active = None;
        self.waiting = None;
        self.overflow.clear();
        self.force_seek = false;
        self.resample.reset();
    }

    pub fn job_done(&self, job: SegmentJob) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.id == job.source_id && active.position >= job.end)
    }

    pub fn pull(
        &mut self,
        job: SegmentJob,
        out: &mut [f32],
        flush_tail: bool,
    ) -> Result<PullResult, Error> {
        if out.len() < 2 || job.end <= job.start {
            return Ok(PullResult {
                frames: 0,
                source_start: job.start,
                source_frames: 0,
                sample_rate: self.output_rate,
                pending: false,
            });
        }
        self.prepare(job)?;
        let source_start = self.active.as_ref().map(|active| active.position).unwrap_or(job.start);
        let source_rate = self.active.as_ref().map(|active| active.rate).unwrap_or(self.output_rate);
        let mut written = 0usize;
        let capacity = out.len() / 2;
        self.spill_overflow(out, &mut written, capacity);

        while written < capacity {
            let position = self.active.as_ref().map(|active| active.position).unwrap_or(job.end);
            if position >= job.end {
                break;
            }
            let room = capacity - written;
            let rate = self.active.as_ref().map(|active| active.rate).unwrap_or(1).max(1);
            let want = source_frames_for(room, rate, self.output_rate, job.end - position);
            let Some(got) = self.read_source(want)? else {
                let position_now = self
                    .active
                    .as_ref()
                    .map(|active| active.position)
                    .unwrap_or(source_start);
                return Ok(PullResult {
                    frames: written,
                    source_start,
                    source_frames: position_now.saturating_sub(source_start),
                    sample_rate: source_rate,
                    pending: true,
                });
            };
            if got == 0 {
                if let Some(active) = self.active.as_mut() {
                    active.position = job.end;
                }
                break;
            }
            let input_rate = self.active.as_ref().map(|active| active.rate).unwrap_or(rate);
            let stereo = self.stereo[..got * 2].to_vec();
            self.converted.clear();
            self.resample.push(input_rate, &stereo, &mut self.converted);
            self.take_converted(out, &mut written, capacity);
        }

        let position = self.active.as_ref().map(|active| active.position).unwrap_or(job.end);
        if position >= job.end && flush_tail {
            self.converted.clear();
            self.resample.flush(&mut self.converted);
            self.take_converted(out, &mut written, capacity);
        }

        let position_now = self.active.as_ref().map(|active| active.position).unwrap_or(source_start);
        Ok(PullResult {
            frames: written,
            source_start,
            source_frames: position_now.saturating_sub(source_start),
            sample_rate: source_rate,
            pending: false,
        })
    }

    fn spill_overflow(&mut self, out: &mut [f32], written: &mut usize, capacity: usize) {
        if self.overflow.is_empty() || *written >= capacity {
            return;
        }
        let have = self.overflow.len() / 2;
        let room = capacity - *written;
        let take = have.min(room);
        let samples = take * 2;
        out[*written * 2..*written * 2 + samples].copy_from_slice(&self.overflow[..samples]);
        self.overflow.drain(..samples);
        *written += take;
    }

    fn take_converted(&mut self, out: &mut [f32], written: &mut usize, capacity: usize) {
        let frames = self.converted.len() / 2;
        if frames == 0 {
            return;
        }
        let room = capacity - *written;
        let take = frames.min(room);
        if take > 0 {
            let samples = take * 2;
            out[*written * 2..*written * 2 + samples].copy_from_slice(&self.converted[..samples]);
            *written += take;
            if frames > take {
                self.overflow.extend_from_slice(&self.converted[samples..]);
            }
        } else {
            self.overflow.extend_from_slice(&self.converted);
        }
    }

    fn prepare(&mut self, job: SegmentJob) -> Result<(), Error> {
        let wrong = self
            .active
            .as_ref()
            .map(|active| active.id != job.source_id)
            .unwrap_or(true);
        if wrong {
            let Some((id, source)) = self.waiting.take() else {
                return Err(Error::new("no decoder is ready for this track"));
            };
            if id != job.source_id {
                return Err(Error::new("the prefetched decoder is for a different file"));
            }
            let rate = source.sample_rate().max(1);
            let channels = source.channels().max(1);
            let map = source.channel_map();
            self.stats.opens += 1;
            let mut active = Active {
                id,
                source,
                position: 0,
                rate,
                channels,
                map,
            };
            if job.start != 0 {
                active.source.seek_presentation(job.start)?;
                active.position = job.start;
                self.stats.seeks += 1;
                self.resample.reset();
                self.overflow.clear();
            } else if self.resample.input_rate() != 0 && self.resample.input_rate() != rate {
                self.resample.reset();
            }
            active.position = job.start;
            self.active = Some(active);
            self.force_seek = false;
            return Ok(());
        }

        let force = self.force_seek;
        self.force_seek = false;
        let active = self.active.as_mut().expect("active source");
        if active.position >= job.end && !force {
            return Ok(());
        }
        let at_start = active.position == job.start;
        let inside = active.position >= job.start && active.position < job.end;
        if at_start || (inside && !force) {
            return Ok(());
        }
        active.source.seek_presentation(job.start)?;
        active.position = job.start;
        self.stats.seeks += 1;
        self.resample.reset();
        self.overflow.clear();
        Ok(())
    }

    /// `None` means the decoder is not ready yet. That is not the end of the file.
    fn read_source(&mut self, frames: usize) -> Result<Option<usize>, Error> {
        let Some(active) = self.active.as_ref() else {
            return Ok(Some(0));
        };
        if frames == 0 {
            return Ok(Some(0));
        }
        let channels = active.channels as usize;
        let map = active.map;
        let samples = frames * channels;
        if self.raw.len() < samples {
            self.raw.resize(samples, 0.0);
        }
        let mut raw = std::mem::take(&mut self.raw);
        let read = {
            let active = self.active.as_mut().expect("active source");
            active.source.pull_available(&mut raw[..samples])?
        };
        self.raw = raw;
        let got = match read {
            SourceRead::Pending => return Ok(None),
            SourceRead::Frames(got) => got,
        };
        if got == 0 {
            return Ok(Some(0));
        }
        if let Some(active) = self.active.as_mut() {
            active.position += got as u64;
        }
        downmix(&self.raw[..got * channels], map, &mut self.stereo);
        Ok(Some(got))
    }
}

fn source_frames_for(output_frames: usize, input_rate: u32, output_rate: u32, remaining: u64) -> usize {
    let estimate = if input_rate == output_rate {
        output_frames
    } else {
        let ratio = f64::from(output_rate.max(1)) / f64::from(input_rate.max(1));
        ((output_frames as f64) / ratio).ceil() as usize + 64
    };
    estimate.min(remaining as usize).clamp(1, 4096)
}

fn downmix(interleaved: &[f32], map: ChannelMap, stereo: &mut Vec<f32>) {
    stereo.clear();
    match map {
        ChannelMap::Mono => {
            stereo.reserve(interleaved.len() * 2);
            for sample in interleaved {
                stereo.push(*sample);
                stereo.push(*sample);
            }
        }
        ChannelMap::Stereo => stereo.extend_from_slice(interleaved),
        ChannelMap::Surround51 => {
            for frame in interleaved.chunks_exact(6) {
                let center = frame[2] * 0.707;
                let left = frame[0] + center + frame[4] * 0.5;
                let right = frame[1] + center + frame[5] * 0.5;
                stereo.push(left);
                stereo.push(right);
            }
        }
        ChannelMap::Other(channels) => {
            let channels = channels.max(1) as usize;
            for frame in interleaved.chunks_exact(channels) {
                let mean = frame.iter().sum::<f32>() / channels as f32;
                stereo.push(mean);
                stereo.push(mean);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seasound_model::{PcmSource, SourceRead};

    struct Ramp {
        rate: u32,
        len: u64,
        pos: u64,
        channels: u16,
        constant: Option<f32>,
    }

    impl Ramp {
        fn frames(len: u64) -> Self {
            Self {
                rate: 44100,
                len,
                pos: 0,
                channels: 2,
                constant: None,
            }
        }

        fn constant(len: u64, rate: u32, value: f32) -> Self {
            Self {
                rate,
                len,
                pos: 0,
                channels: 2,
                constant: Some(value),
            }
        }
    }

    impl PcmSource for Ramp {
        fn sample_rate(&self) -> u32 {
            self.rate
        }
        fn channels(&self) -> u16 {
            self.channels
        }
        fn channel_map(&self) -> ChannelMap {
            if self.channels == 1 {
                ChannelMap::Mono
            } else {
                ChannelMap::Stereo
            }
        }
        fn presentation_frames(&self) -> u64 {
            self.len
        }
        fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
            let channels = self.channels as usize;
            let max_frames = interleaved.len() / channels;
            let left = self.len.saturating_sub(self.pos) as usize;
            let n = max_frames.min(left);
            for index in 0..n {
                let value = self.constant.unwrap_or((self.pos + index as u64) as f32);
                for channel in 0..channels {
                    interleaved[index * channels + channel] = value;
                }
            }
            self.pos += n as u64;
            Ok(n)
        }
        fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
            self.pos = frame.min(self.len);
            Ok(())
        }
    }

    fn drain(core: &mut PlayerCore, job: SegmentJob, flush: bool) -> Vec<f32> {
        let mut out = Vec::new();
        loop {
            let mut buf = vec![0f32; 256];
            let pulled = core.pull(job, &mut buf, flush).unwrap();
            if pulled.frames == 0 {
                break;
            }
            out.extend_from_slice(&buf[..pulled.frames * 2]);
            if core.job_done(job) && pulled.frames < 128 {
                break;
            }
        }
        out
    }

    fn left(stereo: &[f32]) -> Vec<f32> {
        stereo.iter().step_by(2).copied().collect()
    }

    #[test]
    fn two_sources_concatenate_without_a_gap() {
        let mut core = PlayerCore::new(44100);
        core.store_prefetch(SourceId(1), Box::new(Ramp::frames(1000)));
        let first = drain(
            &mut core,
            SegmentJob {
                source_id: SourceId(1),
                start: 0,
                end: 1000,
            },
            false,
        );
        core.store_prefetch(SourceId(2), Box::new(Ramp::frames(500)));
        let second = drain(
            &mut core,
            SegmentJob {
                source_id: SourceId(2),
                start: 0,
                end: 500,
            },
            true,
        );
        let samples = left(&[first, second].concat());
        let expected: Vec<f32> = (0..1000).chain(0..500).map(|frame| frame as f32).collect();
        assert_eq!(samples, expected);
        assert_eq!(core.stats().opens, 2);
        assert_eq!(core.stats().seeks, 0);
    }

    #[test]
    fn contiguous_segments_do_not_seek_or_reopen() {
        let mut core = PlayerCore::new(44100);
        core.store_prefetch(SourceId(1), Box::new(Ramp::frames(90)));
        let mut all = Vec::new();
        for (start, end) in [(0, 30), (30, 60), (60, 90)] {
            let job = SegmentJob {
                source_id: SourceId(1),
                start,
                end,
            };
            let part = drain(&mut core, job, false);
            all.extend_from_slice(&part);
        }
        let samples = left(&all);
        let expected: Vec<f32> = (0..90).map(|frame| frame as f32).collect();
        assert_eq!(samples, expected);
        assert_eq!(core.stats().opens, 1);
        assert_eq!(core.stats().seeks, 0);
    }

    #[test]
    fn a_gap_in_the_same_file_seeks_once() {
        let mut core = PlayerCore::new(44100);
        core.store_prefetch(SourceId(1), Box::new(Ramp::frames(100)));
        let head = drain(
            &mut core,
            SegmentJob {
                source_id: SourceId(1),
                start: 0,
                end: 10,
            },
            true,
        );
        let tail = drain(
            &mut core,
            SegmentJob {
                source_id: SourceId(1),
                start: 50,
                end: 60,
            },
            true,
        );
        let samples = left(&[head, tail].concat());
        let expected: Vec<f32> = (0..10).chain(50..60).map(|frame| frame as f32).collect();
        assert_eq!(samples, expected);
        assert_eq!(core.stats().opens, 1);
        assert_eq!(core.stats().seeks, 1);
    }

    #[test]
    fn a_rate_change_does_not_open_a_silent_hole() {
        let mut core = PlayerCore::new(44100);
        core.store_prefetch(SourceId(1), Box::new(Ramp::constant(2000, 22050, 0.8)));
        let first = drain(
            &mut core,
            SegmentJob {
                source_id: SourceId(1),
                start: 0,
                end: 2000,
            },
            false,
        );
        core.store_prefetch(SourceId(2), Box::new(Ramp::constant(2000, 22050, 0.8)));
        let second = drain(
            &mut core,
            SegmentJob {
                source_id: SourceId(2),
                start: 0,
                end: 2000,
            },
            true,
        );
        let samples = left(&[first, second].concat());
        assert!(samples.len() > 2000, "upsampling should grow the stream");
        let mut run = 0usize;
        let mut worst = 0usize;
        for sample in &samples {
            if sample.abs() < 0.2 {
                run += 1;
                worst = worst.max(run);
            } else {
                run = 0;
            }
        }
        assert!(
            worst < 32,
            "silent run of {worst} frames at the rate change, first samples {:?}",
            &samples[..samples.len().min(16)]
        );
        let mean = samples.iter().sum::<f32>() / samples.len() as f32;
        assert!(mean > 0.7, "mean level {mean}");
    }

    struct Stall {
        inner: Ramp,
        stalls: usize,
    }

    impl PcmSource for Stall {
        fn sample_rate(&self) -> u32 {
            self.inner.rate
        }
        fn channels(&self) -> u16 {
            self.inner.channels
        }
        fn channel_map(&self) -> ChannelMap {
            if self.inner.channels == 1 {
                ChannelMap::Mono
            } else {
                ChannelMap::Stereo
            }
        }
        fn presentation_frames(&self) -> u64 {
            self.inner.len
        }
        fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
            self.inner.pull(interleaved)
        }
        fn pull_available(&mut self, interleaved: &mut [f32]) -> Result<SourceRead, Error> {
            if self.stalls > 0 {
                self.stalls -= 1;
                return Ok(SourceRead::Pending);
            }
            Ok(SourceRead::Frames(self.pull(interleaved)?))
        }
        fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
            self.inner.seek_presentation(frame)
        }
    }

    #[test]
    fn a_pending_read_does_not_finish_the_segment() {
        let mut core = PlayerCore::new(44100);
        core.store_prefetch(
            SourceId(1),
            Box::new(Stall {
                inner: Ramp::frames(10),
                stalls: 1,
            }),
        );
        let job = SegmentJob {
            source_id: SourceId(1),
            start: 0,
            end: 10,
        };
        let mut buf = vec![0f32; 64];
        let first = core.pull(job, &mut buf, true).unwrap();
        assert!(first.pending);
        assert_eq!(first.frames, 0);
        assert!(!core.job_done(job));
        let second = core.pull(job, &mut buf, true).unwrap();
        assert!(!second.pending);
        assert_eq!(second.frames, 10);
        assert!(core.job_done(job));
    }
}
