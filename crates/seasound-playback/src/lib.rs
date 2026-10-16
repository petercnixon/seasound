//! Playback. This crate never parses a container. Callers hand it a [`PcmSource`].

mod core;
mod device;
mod engine;
mod resample;

pub use core::{CoreStats, PlayerCore, PullResult, SegmentJob};
pub use engine::{Command, Engine, Opener, Snapshot};

#[cfg(feature = "cpal-output")]
pub use device::device_names;
