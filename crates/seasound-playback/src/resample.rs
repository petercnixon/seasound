//! Sample-rate conversion for the decode thread.
//!
//! Equal rates are copied. A rate change builds a sinc resampler and drops its
//! startup delay so the splice does not begin with silence. The same pair of
//! rates keeps that filter, including across two files, so the boundary is not
//! a new fade-in.

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

pub struct RateConvert {
    output_rate: u32,
    input_rate: u32,
    resampler: Option<SincFixedIn<f32>>,
    pending_l: Vec<f32>,
    pending_r: Vec<f32>,
    skip: usize,
    chunk: usize,
}

impl RateConvert {
    pub fn new(output_rate: u32) -> Self {
        Self {
            output_rate: output_rate.max(1),
            input_rate: 0,
            resampler: None,
            pending_l: Vec::new(),
            pending_r: Vec::new(),
            skip: 0,
            chunk: 512,
        }
    }

    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    pub fn set_output_rate(&mut self, rate: u32) {
        if rate == 0 || rate == self.output_rate {
            return;
        }
        self.output_rate = rate;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.resampler = None;
        self.pending_l.clear();
        self.pending_r.clear();
        self.skip = 0;
        self.input_rate = 0;
    }

    /// Append converted stereo frames to `out`.
    pub fn push(&mut self, input_rate: u32, stereo: &[f32], out: &mut Vec<f32>) {
        if stereo.is_empty() {
            return;
        }
        if input_rate == 0 || input_rate == self.output_rate {
            if self.resampler.is_some() {
                self.flush(out);
                self.resampler = None;
                self.pending_l.clear();
                self.pending_r.clear();
                self.skip = 0;
                self.input_rate = 0;
            }
            out.extend_from_slice(stereo);
            return;
        }
        if self.input_rate != input_rate {
            if self.resampler.is_some() {
                self.flush(out);
            }
            self.install(input_rate);
        }
        for frame in stereo.chunks_exact(2) {
            self.pending_l.push(frame[0]);
            self.pending_r.push(frame[1]);
        }
        self.emit_ready(out);
    }

    /// Emit a partial final chunk. Used when the next audio is not continuous.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        self.emit_ready(out);
        let have = self.pending_l.len();
        if have == 0 || self.resampler.is_none() {
            return;
        }
        let last_l = self.pending_l.last().copied().unwrap_or(0.0);
        let last_r = self.pending_r.last().copied().unwrap_or(0.0);
        self.pending_l.resize(self.chunk, last_l);
        self.pending_r.resize(self.chunk, last_r);
        let before = out.len();
        self.emit_ready(out);
        let produced = (out.len() - before) / 2;
        let ratio = f64::from(self.output_rate) / f64::from(self.input_rate.max(1));
        let keep = ((have as f64) * ratio).round() as usize;
        if produced > keep {
            let drop_samples = (produced - keep) * 2;
            out.truncate(out.len().saturating_sub(drop_samples));
        }
        self.pending_l.clear();
        self.pending_r.clear();
    }

    fn install(&mut self, input_rate: u32) {
        self.pending_l.clear();
        self.pending_r.clear();
        self.input_rate = input_rate;
        let ratio = f64::from(self.output_rate) / f64::from(input_rate.max(1));
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        match SincFixedIn::<f32>::new(ratio, 2.0, params, self.chunk, 2) {
            Ok(resampler) => {
                self.skip = resampler.output_delay();
                self.chunk = resampler.input_frames_next().max(1);
                self.resampler = Some(resampler);
            }
            Err(_) => {
                self.resampler = None;
                self.skip = 0;
            }
        }
    }

    fn emit_ready(&mut self, out: &mut Vec<f32>) {
        loop {
            let (need, max_out) = {
                let Some(resampler) = self.resampler.as_ref() else {
                    return;
                };
                (
                    resampler.input_frames_next().max(1),
                    resampler.output_frames_max().max(1),
                )
            };
            if self.pending_l.len() < need {
                return;
            }
            let in_l = self.pending_l[..need].to_vec();
            let in_r = self.pending_r[..need].to_vec();
            let mut out_l = vec![0f32; max_out];
            let mut out_r = vec![0f32; max_out];
            let processed = {
                let Some(resampler) = self.resampler.as_mut() else {
                    return;
                };
                resampler.process_into_buffer(
                    &[&in_l[..], &in_r[..]],
                    &mut [&mut out_l[..], &mut out_r[..]],
                    None,
                )
            };
            let Ok((used_in, used_out)) = processed else {
                return;
            };
            let used_in = used_in.min(self.pending_l.len());
            if used_in == 0 {
                return;
            }
            self.pending_l.drain(..used_in);
            self.pending_r.drain(..used_in);
            let mut start = 0usize;
            if self.skip > 0 {
                let drop = self.skip.min(used_out);
                self.skip -= drop;
                start = drop;
            }
            for index in start..used_out.min(out_l.len()) {
                out.push(out_l[index]);
                out.push(out_r[index]);
            }
        }
    }
}
