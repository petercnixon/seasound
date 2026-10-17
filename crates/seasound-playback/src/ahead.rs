//! Decoded audio kept ahead of the playhead.
//!
//! The playback thread only reads this queue. Opening a file and decoding it
//! run on a feeder thread, so a slow disk or NAS does not stall the output ring.
//! The feeder stops reading once the queue holds [`LEAD_SECS`] of audio.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use seasound_model::{ChannelMap, Error, PcmSource, SourceRead};

/// How much decoded audio to keep queued for the file that is playing, and for the next file.
pub(crate) const LEAD_SECS: u32 = 8;

const CHUNK_FRAMES: usize = 2048;

struct Slot {
    pcm: VecDeque<f32>,
    ended: bool,
    error: Option<String>,
    seek: Option<u64>,
    generation: u64,
    stop: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    cv: Condvar,
}

pub(crate) struct Ahead {
    shared: Arc<Shared>,
    rate: u32,
    channels: u16,
    map: ChannelMap,
    frames: u64,
}

impl Ahead {
    pub(crate) fn boxed(source: Box<dyn PcmSource>) -> Box<dyn PcmSource> {
        Box::new(Self::spawn(source))
    }

    fn spawn(source: Box<dyn PcmSource>) -> Self {
        let rate = source.sample_rate().max(1);
        let channels = source.channels().max(1);
        let map = source.channel_map();
        let frames = source.presentation_frames();
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot {
                pcm: VecDeque::new(),
                ended: false,
                error: None,
                seek: None,
                generation: 0,
                stop: false,
            }),
            cv: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("seasound-ahead".into())
            .spawn(move || feed(source, worker, rate, channels))
            .expect("ahead thread");
        Self {
            shared,
            rate,
            channels,
            map,
            frames,
        }
    }
}

impl Drop for Ahead {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.shared.slot.lock() {
            slot.stop = true;
            self.shared.cv.notify_all();
        }
    }
}

impl PcmSource for Ahead {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn channel_map(&self) -> ChannelMap {
        self.map
    }

    fn presentation_frames(&self) -> u64 {
        self.frames
    }

    fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
        let shared = Arc::clone(&self.shared);
        let mut slot = shared.slot.lock().unwrap();
        loop {
            if let Some(err) = slot.error.clone() {
                return Err(Error::new(err));
            }
            let channels = self.channels as usize;
            let want = interleaved.len() / channels;
            let have = slot.pcm.len() / channels;
            if have > 0 || slot.ended || want == 0 {
                return take(&mut slot, interleaved, channels);
            }
            slot = shared.cv.wait(slot).unwrap();
        }
    }

    fn pull_available(&mut self, interleaved: &mut [f32]) -> Result<SourceRead, Error> {
        let mut slot = self.shared.slot.lock().unwrap();
        if let Some(err) = slot.error.clone() {
            let channels = self.channels as usize;
            if slot.pcm.len() >= channels {
                return take(&mut slot, interleaved, channels).map(SourceRead::Frames);
            }
            return Err(Error::new(err));
        }
        let channels = self.channels as usize;
        let want = interleaved.len() / channels.max(1);
        let have = slot.pcm.len() / channels.max(1);
        if have == 0 && !slot.ended && want > 0 {
            return Ok(SourceRead::Pending);
        }
        let read = take(&mut slot, interleaved, channels)?;
        self.shared.cv.notify_one();
        Ok(SourceRead::Frames(read))
    }

    fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
        let mut slot = self.shared.slot.lock().unwrap();
        slot.pcm.clear();
        slot.ended = false;
        slot.error = None;
        slot.generation = slot.generation.wrapping_add(1);
        slot.seek = Some(frame);
        self.shared.cv.notify_all();
        let shared = Arc::clone(&self.shared);
        slot = shared
            .cv
            .wait_while(slot, |slot| {
                slot.error.is_none() && (slot.seek.is_some() || (slot.pcm.is_empty() && !slot.ended))
            })
            .unwrap();
        if let Some(err) = slot.error.clone() {
            return Err(Error::new(err));
        }
        Ok(())
    }
}

fn take(slot: &mut Slot, interleaved: &mut [f32], channels: usize) -> Result<usize, Error> {
    let channels = channels.max(1);
    let want = interleaved.len() / channels;
    let have = slot.pcm.len() / channels;
    if have == 0 {
        return Ok(0);
    }
    let frames = want.min(have);
    let samples = frames * channels;
    let (head, tail) = slot.pcm.as_slices();
    if head.len() >= samples {
        interleaved[..samples].copy_from_slice(&head[..samples]);
    } else {
        let mid = head.len();
        interleaved[..mid].copy_from_slice(head);
        interleaved[mid..samples].copy_from_slice(&tail[..samples - mid]);
    }
    slot.pcm.drain(..samples);
    Ok(frames)
}

fn feed(mut source: Box<dyn PcmSource>, shared: Arc<Shared>, rate: u32, channels: u16) {
    let channels = channels.max(1) as usize;
    let lead_frames = rate as usize * LEAD_SECS as usize;
    let mut scratch = vec![0f32; CHUNK_FRAMES * channels];
    loop {
        let mut slot = shared.slot.lock().unwrap();
        if slot.stop {
            break;
        }
        if let Some(frame) = slot.seek.take() {
            let generation = slot.generation;
            drop(slot);
            let seeked = source.seek_presentation(frame);
            let mut slot = shared.slot.lock().unwrap();
            if slot.stop {
                break;
            }
            if slot.generation != generation {
                continue;
            }
            if let Err(err) = seeked {
                slot.error = Some(err.message().to_string());
                slot.ended = true;
                shared.cv.notify_all();
                continue;
            }
            slot.ended = false;
            shared.cv.notify_all();
            continue;
        }
        let have = slot.pcm.len() / channels;
        if slot.ended || have >= lead_frames {
            let _slot = shared.cv.wait(slot).unwrap();
            continue;
        }
        let generation = slot.generation;
        drop(slot);

        let pulled = source.pull(&mut scratch);
        let mut slot = shared.slot.lock().unwrap();
        if slot.stop {
            break;
        }
        if slot.generation != generation || slot.seek.is_some() {
            continue;
        }
        match pulled {
            Ok(0) => slot.ended = true,
            Ok(frames) => {
                let samples = frames * channels;
                slot.pcm.extend(scratch[..samples].iter().copied());
            }
            Err(err) => {
                slot.error = Some(err.message().to_string());
                slot.ended = true;
            }
        }
        shared.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Ramp {
        len: u64,
        pos: u64,
    }

    impl PcmSource for Ramp {
        fn sample_rate(&self) -> u32 {
            1000
        }
        fn channels(&self) -> u16 {
            1
        }
        fn channel_map(&self) -> ChannelMap {
            ChannelMap::Mono
        }
        fn presentation_frames(&self) -> u64 {
            self.len
        }
        fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
            let frames = interleaved.len();
            let n = frames.min(self.len.saturating_sub(self.pos) as usize);
            for index in 0..n {
                interleaved[index] = (self.pos + index as u64) as f32;
            }
            self.pos += n as u64;
            Ok(n)
        }
        fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
            self.pos = frame.min(self.len);
            Ok(())
        }
    }

    fn next_frames(ahead: &mut Ahead, frames: usize) -> SourceRead {
        let mut buf = vec![0f32; frames];
        let mut spins = 0;
        loop {
            match ahead.pull_available(&mut buf).unwrap() {
                SourceRead::Pending => {
                    spins += 1;
                    assert!(spins < 1000, "decoder never produced audio");
                    thread::sleep(Duration::from_millis(2));
                }
                other => return other,
            }
        }
    }

    #[test]
    fn the_queue_fills_without_the_caller_reading_the_file() {
        let mut ahead = Ahead::spawn(Box::new(Ramp { len: 50, pos: 0 }));
        match next_frames(&mut ahead, 50) {
            SourceRead::Frames(50) => {}
            other => panic!("expected the whole clip, got {other:?}"),
        }
    }

    #[test]
    fn pending_is_not_the_end_of_the_file() {
        struct Hold {
            inner: Ramp,
            ready: Arc<(Mutex<bool>, Condvar)>,
        }
        impl PcmSource for Hold {
            fn sample_rate(&self) -> u32 {
                self.inner.sample_rate()
            }
            fn channels(&self) -> u16 {
                1
            }
            fn channel_map(&self) -> ChannelMap {
                ChannelMap::Mono
            }
            fn presentation_frames(&self) -> u64 {
                self.inner.len
            }
            fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
                let mut ready = self.ready.0.lock().unwrap();
                while !*ready {
                    ready = self.ready.1.wait(ready).unwrap();
                }
                self.inner.pull(interleaved)
            }
            fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
                self.inner.seek_presentation(frame)
            }
        }

        let ready = Arc::new((Mutex::new(false), Condvar::new()));
        let mut ahead = Ahead::spawn(Box::new(Hold {
            inner: Ramp { len: 4, pos: 0 },
            ready: Arc::clone(&ready),
        }));
        let mut buf = vec![0f32; 4];
        assert_eq!(ahead.pull_available(&mut buf).unwrap(), SourceRead::Pending);
        *ready.0.lock().unwrap() = true;
        ready.1.notify_all();
        match next_frames(&mut ahead, 4) {
            SourceRead::Frames(4) => {}
            other => panic!("expected frames after the stall, got {other:?}"),
        }
    }

    #[test]
    fn seek_drops_queued_audio_and_continues_from_the_new_frame() {
        let mut ahead = Ahead::spawn(Box::new(Ramp { len: 10_000, pos: 0 }));
        let _ = next_frames(&mut ahead, 10);
        ahead.seek_presentation(500).unwrap();
        let mut buf = vec![0f32; 1];
        let mut spins = 0;
        let sample = loop {
            match ahead.pull_available(&mut buf).unwrap() {
                SourceRead::Pending => {
                    spins += 1;
                    assert!(spins < 1000, "seek never refilled");
                    thread::sleep(Duration::from_millis(2));
                }
                SourceRead::Frames(0) => panic!("seek ended the file"),
                SourceRead::Frames(_) => break buf[0],
            }
        };
        assert_eq!(sample, 500.0);
    }
}
