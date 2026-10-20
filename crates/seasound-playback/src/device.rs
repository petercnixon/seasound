//! Audio output. The callback only reads the ring, ramps volume, and writes the device.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rtrb::{Consumer, Producer, RingBuffer};

use seasound_model::Error;

/// What the decode thread can do to the device without touching the callback.
pub trait AudioOut {
    fn sample_rate(&self) -> u32;
    fn free_frames(&self) -> usize;
    /// Accepts every frame when `stereo.len() / 2 <= free_frames()`.
    fn push(&mut self, stereo: &[f32]) -> usize;
    fn clear(&mut self);
    fn set_paused(&mut self, paused: bool);
    fn set_gain(&mut self, gain: f32);
    fn played(&self) -> u64;
    fn device_name(&self) -> String;
    fn devices(&self) -> Vec<String>;
    fn set_device(&mut self, name: Option<&str>) -> Result<(), Error>;

    /// Why the device could not be opened, when playback has no other error.
    fn last_error(&self) -> Option<String> {
        None
    }
}

/// Records what was played. Used by tests, and when no sound device is available.
pub struct MemoryOutput {
    samples: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    played: u64,
    gain: f32,
    paused: bool,
}

impl MemoryOutput {
    pub fn new(rate: u32) -> Self {
        Self {
            samples: Arc::new(Mutex::new(Vec::new())),
            rate,
            played: 0,
            gain: 1.0,
            paused: false,
        }
    }

    pub fn samples_handle(&self) -> Arc<Mutex<Vec<f32>>> {
        Arc::clone(&self.samples)
    }
}

impl AudioOut for MemoryOutput {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn free_frames(&self) -> usize {
        8192
    }

    fn push(&mut self, stereo: &[f32]) -> usize {
        let frames = stereo.len() / 2;
        if !self.paused {
            if let Ok(mut samples) = self.samples.lock() {
                samples.extend_from_slice(&stereo[..frames * 2]);
            }
            self.played += frames as u64;
        }
        frames
    }

    fn clear(&mut self) {}

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn set_gain(&mut self, gain: f32) {
        self.gain = gain;
    }

    fn played(&self) -> u64 {
        let _ = self.gain;
        self.played
    }

    fn device_name(&self) -> String {
        "Test output".into()
    }

    fn devices(&self) -> Vec<String> {
        vec!["Test output".into()]
    }

    fn set_device(&mut self, _name: Option<&str>) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(feature = "cpal-output")]
mod cpal_out {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, Stream, StreamConfig};

    struct Shared {
        consumer: Mutex<Consumer<f32>>,
        gain: AtomicU32,
        paused: AtomicBool,
        played: AtomicU64,
        channels: usize,
        /// Gain already applied. It has to survive from one callback to the next,
        /// because `gain` is the target and a fresh read of it would skip the ramp.
        applied: AtomicU32,
    }

    pub struct CpalOutput {
        producer: Option<Producer<f32>>,
        shared: Option<Arc<Shared>>,
        stream: Option<Stream>,
        rate: u32,
        name: String,
        devices: Vec<String>,
        error: Mutex<Option<String>>,
    }

    impl CpalOutput {
        pub fn new() -> Self {
            let mut output = Self {
                producer: None,
                shared: None,
                stream: None,
                rate: 44100,
                name: String::new(),
                devices: Vec::new(),
                error: Mutex::new(None),
            };
            output.devices = list_devices();
            if let Err(err) = output.open_default() {
                *output.error.lock().unwrap() = Some(err.message().to_string());
            }
            output
        }

        fn open_default(&mut self) -> Result<(), Error> {
            self.open_named(None)
        }

        fn open_named(&mut self, name: Option<&str>) -> Result<(), Error> {
            let host = cpal::default_host();
            let device = match name {
                Some(name) => host
                    .output_devices()
                    .map_err(|err| Error::new(err.to_string()))?
                    .find(|device| device.name().ok().as_deref() == Some(name))
                    .ok_or_else(|| Error::new(format!("output device \"{name}\" was not found")))?,
                None => host
                    .default_output_device()
                    .ok_or_else(|| Error::new("no output device"))?,
            };
            let device_name = device.name().unwrap_or_else(|_| "Output".into());
            let supported = device
                .default_output_config()
                .map_err(|err| Error::new(err.to_string()))?;
            let rate = supported.sample_rate().0;
            let channels = supported.channels() as usize;
            let format = supported.sample_format();
            let config: StreamConfig = supported.into();
            let slots = (rate as usize / 5).saturating_mul(2).max(2048);
            let (producer, consumer) = RingBuffer::<f32>::new(slots);
            let shared = Arc::new(Shared {
                consumer: Mutex::new(consumer),
                gain: AtomicU32::new(1.0f32.to_bits()),
                paused: AtomicBool::new(false),
                played: AtomicU64::new(0),
                channels: channels.max(1),
                applied: AtomicU32::new(0),
            });
            let stream = build_stream(&device, &config, format, Arc::clone(&shared), rate)?;
            stream.play().map_err(|err| Error::new(err.to_string()))?;
            self.stream = Some(stream);
            self.producer = Some(producer);
            self.shared = Some(shared);
            self.rate = rate;
            self.name = device_name;
            if let Ok(mut slot) = self.error.lock() {
                *slot = None;
            }
            Ok(())
        }
    }

    impl AudioOut for CpalOutput {
        fn sample_rate(&self) -> u32 {
            self.rate
        }

        fn free_frames(&self) -> usize {
            self.producer.as_ref().map(|producer| producer.slots() / 2).unwrap_or(0)
        }

        fn push(&mut self, stereo: &[f32]) -> usize {
            let Some(producer) = self.producer.as_mut() else {
                return 0;
            };
            let frames = stereo.len() / 2;
            let samples = frames * 2;
            let space = producer.slots();
            let take = samples.min(space);
            let take = take - (take % 2);
            if take == 0 {
                return 0;
            }
            let Ok(mut chunk) = producer.write_chunk(take) else {
                return 0;
            };
            let (first, second) = chunk.as_mut_slices();
            first.copy_from_slice(&stereo[..first.len()]);
            if !second.is_empty() {
                second.copy_from_slice(&stereo[first.len()..first.len() + second.len()]);
            }
            chunk.commit_all();
            take / 2
        }

        fn clear(&mut self) {
            let Some(shared) = &self.shared else {
                return;
            };
            if let Ok(mut consumer) = shared.consumer.lock() {
                while consumer.slots() > 0 {
                    let n = consumer.slots();
                    if let Ok(chunk) = consumer.read_chunk(n) {
                        chunk.commit_all();
                    } else {
                        break;
                    }
                }
            }
        }

        fn set_paused(&mut self, paused: bool) {
            if let Some(shared) = &self.shared {
                shared.paused.store(paused, Ordering::Relaxed);
            }
        }

        fn set_gain(&mut self, gain: f32) {
            if let Some(shared) = &self.shared {
                shared.gain.store(gain.to_bits(), Ordering::Relaxed);
            }
        }

        fn played(&self) -> u64 {
            self.shared.as_ref().map(|shared| shared.played.load(Ordering::Relaxed)).unwrap_or(0)
        }

        fn device_name(&self) -> String {
            if self.name.is_empty() {
                "No device".into()
            } else {
                self.name.clone()
            }
        }

        fn last_error(&self) -> Option<String> {
            self.error.lock().ok().and_then(|guard| guard.clone())
        }

        fn devices(&self) -> Vec<String> {
            self.devices.clone()
        }

        fn set_device(&mut self, name: Option<&str>) -> Result<(), Error> {
            self.devices = list_devices();
            self.open_named(name)
        }
    }

    fn list_devices() -> Vec<String> {
        let host = cpal::default_host();
        host.output_devices()
            .map(|devices| devices.filter_map(|device| device.name().ok()).collect())
            .unwrap_or_default()
    }

    fn build_stream(
        device: &cpal::Device,
        config: &StreamConfig,
        format: SampleFormat,
        shared: Arc<Shared>,
        rate: u32,
    ) -> Result<Stream, Error> {
        let err_shared = Arc::clone(&shared);
        let error = move |err: cpal::StreamError| {
            let _ = err_shared;
            eprintln!("audio device: {err}");
        };
        match format {
            SampleFormat::F32 => device
                .build_output_stream(
                    config,
                    move |data: &mut [f32], _| render(data, &shared, rate, write_f32),
                    error,
                    None,
                )
                .map_err(|err| Error::new(err.to_string())),
            SampleFormat::I16 => device
                .build_output_stream(
                    config,
                    move |data: &mut [i16], _| render(data, &shared, rate, write_i16),
                    error,
                    None,
                )
                .map_err(|err| Error::new(err.to_string())),
            other => Err(Error::new(format!("unsupported sample format {other:?}"))),
        }
    }

    fn render<T>(data: &mut [T], shared: &Shared, rate: u32, write: fn(&mut T, f32)) {
        data.iter_mut().for_each(|sample| write(sample, 0.0));
        if shared.paused.load(Ordering::Relaxed) {
            return;
        }
        let channels = shared.channels.max(1);
        let frames = data.len() / channels;
        if frames == 0 {
            return;
        }
        let Ok(mut consumer) = shared.consumer.try_lock() else {
            return;
        };
        let available = consumer.slots() / 2;
        let take = available.min(frames);
        if take == 0 {
            return;
        }
        let Ok(chunk) = consumer.read_chunk(take * 2) else {
            return;
        };
        let (first, second) = chunk.as_slices();
        let desired = f32::from_bits(shared.gain.load(Ordering::Relaxed));
        let mut gain = f32::from_bits(shared.applied.load(Ordering::Relaxed));
        let step = 1.0 / (rate as f32 * 0.010).max(1.0);
        let mut cursor = 0usize;
        let mut feed = |frame: &[f32], data: &mut [T]| {
            let diff = desired - gain;
            if diff.abs() <= step {
                gain = desired;
            } else {
                gain += step.copysign(diff);
            }
            let left = (frame[0] * gain).clamp(-1.0, 1.0);
            let right = (frame[1] * gain).clamp(-1.0, 1.0);
            let base = cursor * channels;
            if channels == 1 {
                write(&mut data[base], 0.5 * (left + right));
            } else {
                write(&mut data[base], left);
                write(&mut data[base + 1], right);
            }
            cursor += 1;
        };
        for frame in first.chunks_exact(2) {
            feed(frame, data);
        }
        for frame in second.chunks_exact(2) {
            feed(frame, data);
        }
        chunk.commit_all();
        shared.applied.store(gain.to_bits(), Ordering::Relaxed);
        shared.played.fetch_add(take as u64, Ordering::Relaxed);
    }

    fn write_f32(sample: &mut f32, value: f32) {
        *sample = value;
    }

    fn write_i16(sample: &mut i16, value: f32) {
        *sample = (value * i16::MAX as f32) as i16;
    }
}

#[cfg(feature = "cpal-output")]
pub use cpal_out::CpalOutput;

#[cfg(feature = "cpal-output")]
pub fn device_names() -> Vec<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    host.output_devices()
        .map(|devices| devices.filter_map(|device| device.name().ok()).collect())
        .unwrap_or_default()
}
