//! Read audio files into playlist sources, and decode them on the presentation timeline.
//!
//! The playback crate never parses a container. It receives a [`PcmSource`] whose `pull`
//! already skips encoder delay and stops before encoder padding.

mod chapters;
mod cue;
mod decode;
mod flac;
mod gapless;
mod plugins;
mod probe;

pub use decode::open_decoder;
pub use plugins::{install_plugins, plugin_extensions, plugin_statuses, set_plugin_enabled, PluginStatus};
pub use probe::{load_paths, load_paths_each, probe_file};

pub use chapters::segments_from_chapters;
pub use cue::{parse_cue_text, segments_from_cue};
pub use flac::parse_cuesheet_block;
pub use gapless::{parse_itunsmpb, parse_lame_delay, Mp3Info};
