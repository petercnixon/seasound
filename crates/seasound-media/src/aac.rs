//! AAC timing follows the core Symphonia decodes, not the MP4 sample entry.
//!
//! HE-AAC's container rate is the rate after spectral band replication, usually
//! twice the AAC-LC core. The decoder emits the core. Playing those samples at
//! the container rate runs the file fast. The channel count lives in the
//! AudioSpecificConfig. The MP4 demuxer does not copy it onto the codec
//! parameters, so a missing count is not "mono".

/// What an `AudioSpecificConfig` says about the AAC-LC core Symphonia emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacConfig {
    /// Sample rate of the decoded core, in Hz.
    pub core_rate: u32,
    /// Channels of the decoded core. Zero means a program config element.
    pub core_channels: u16,
    /// Rate after SBR. Equals `core_rate` when the stream is plain AAC-LC.
    pub output_rate: u32,
    /// Spectral band replication is signalled.
    pub sbr: bool,
    /// Parametric stereo is signalled. The core is then mono.
    pub ps: bool,
    /// Core audio object type. 2 is AAC-LC.
    pub object_type: u8,
}

/// Parse an MPEG-4 `AudioSpecificConfig`.
///
/// Audio object type 5 is HE-AAC and 29 is HE-AAC v2. In that hierarchical
/// form the first sample rate is the core rate. Backward-compatible files
/// start with AAC-LC (object type 2) and signal SBR later, after the sync
/// word `0x2b7`. The first five bits alone do not tell those apart.
pub fn parse(extra: &[u8]) -> Option<AacConfig> {
    if extra.len() < 2 {
        return None;
    }
    let mut bits = Bits::new(extra);
    let mut object_type = read_object_type(&mut bits)?;
    let core_rate = read_sample_rate(&mut bits)?;
    if core_rate == 0 {
        return None;
    }
    let channel_index = bits.read(4)?;
    let mut channels = channel_count(channel_index);
    let mut sbr = false;
    let mut ps = false;
    let mut output_rate = core_rate;
    if object_type == 5 || object_type == 29 {
        sbr = true;
        ps = object_type == 29;
        output_rate = read_sample_rate(&mut bits)?;
        object_type = read_object_type(&mut bits)?;
        if object_type == 22 {
            channels = channel_count(bits.read(4)?);
        }
    } else if let Some(extension) = sbr_extension(&mut bits, object_type, channels) {
        if extension.sbr {
            sbr = true;
            if extension.output_rate > 0 {
                output_rate = extension.output_rate;
            }
        }
        ps = extension.ps;
    }
    if output_rate == 0 {
        output_rate = core_rate;
    }
    Some(AacConfig {
        core_rate,
        core_channels: channels,
        output_rate,
        sbr,
        ps,
        object_type,
    })
}

/// Convert a count in `from` Hz ticks into `to` Hz samples.
pub fn scale_count(count: u64, from: u32, to: u32) -> u64 {
    if from == 0 || to == 0 || from == to {
        return count;
    }
    count.saturating_mul(u64::from(to)) / u64::from(from)
}

struct Extension {
    sbr: bool,
    ps: bool,
    output_rate: u32,
}

/// Backward-compatible SBR/PS after `GASpecificConfig`.
fn sbr_extension(bits: &mut Bits<'_>, object_type: u8, channels: u16) -> Option<Extension> {
    let saved = *bits;
    if skip_ga_specific(bits, object_type, channels).is_none() {
        *bits = saved;
        return None;
    }
    if bits.left() < 16 {
        return Some(Extension {
            sbr: false,
            ps: false,
            output_rate: 0,
        });
    }
    if bits.read(11)? != 0x2b7 {
        return Some(Extension {
            sbr: false,
            ps: false,
            output_rate: 0,
        });
    }
    let extension_type = read_object_type(bits)?;
    if extension_type != 5 {
        return Some(Extension {
            sbr: false,
            ps: false,
            output_rate: 0,
        });
    }
    let present = bits.read(1)? == 1;
    if !present {
        return Some(Extension {
            sbr: false,
            ps: false,
            output_rate: 0,
        });
    }
    let output_rate = read_sample_rate(bits)?;
    let mut ps = false;
    if bits.left() >= 12 && bits.read(11)? == 0x548 {
        ps = bits.read(1)? == 1;
    }
    Some(Extension {
        sbr: true,
        ps,
        output_rate,
    })
}

fn skip_ga_specific(bits: &mut Bits<'_>, object_type: u8, channels: u16) -> Option<()> {
    let ga = matches!(
        object_type,
        1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23
    );
    if !ga {
        return None;
    }
    let _frame_length = bits.read(1)?;
    let depends = bits.read(1)?;
    if depends == 1 {
        let _delay = bits.read(14)?;
    }
    let extension = bits.read(1)?;
    if channels == 0 {
        return None;
    }
    if object_type == 6 || object_type == 20 {
        let _layer = bits.read(3)?;
    }
    if extension == 1 {
        if matches!(object_type, 17 | 19 | 20 | 23) {
            let _section = bits.read(1)?;
            let _scalefactors = bits.read(1)?;
            let _spectral = bits.read(1)?;
        }
        if object_type == 22 {
            let _subframes = bits.read(5)?;
            let _layer_length = bits.read(11)?;
        }
        if bits.read(1)? == 1 {
            return None;
        }
    }
    Some(())
}

fn channel_count(index: u32) -> u16 {
    const TABLE: [u16; 8] = [0, 1, 2, 3, 4, 5, 6, 8];
    TABLE.get(index as usize).copied().unwrap_or(0)
}

fn read_object_type(bits: &mut Bits<'_>) -> Option<u8> {
    let object_type = bits.read(5)?;
    if object_type == 31 {
        Some(32 + bits.read(6)? as u8)
    } else {
        Some(object_type as u8)
    }
}

fn read_sample_rate(bits: &mut Bits<'_>) -> Option<u32> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let index = bits.read(4)?;
    if index == 0xf {
        return bits.read(24);
    }
    RATES.get(index as usize).copied().filter(|rate| *rate > 0)
}

#[derive(Clone, Copy)]
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn left(&self) -> usize {
        self.data.len().saturating_mul(8).saturating_sub(self.pos)
    }

    fn read(&mut self, n: usize) -> Option<u32> {
        if n == 0 || n > 24 || self.left() < n {
            return None;
        }
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self.data[self.pos / 8];
            let shift = 7 - (self.pos % 8);
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.pos += 1;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_lc_stereo_and_mono() {
        let stereo = parse(&[0x12, 0x10]).unwrap();
        assert_eq!(stereo.object_type, 2);
        assert_eq!(stereo.core_rate, 44_100);
        assert_eq!(stereo.core_channels, 2);
        assert!(!stereo.sbr);
        assert_eq!(stereo.output_rate, 44_100);

        let mono = parse(&[0x12, 0x08]).unwrap();
        assert_eq!(mono.core_channels, 1);
        assert_eq!(mono.core_rate, 44_100);
        assert!(!mono.sbr);
    }

    #[test]
    fn backward_compatible_he_aac_uses_the_core_rate() {
        // Fraunhofer stereo test, explicit SBR, backward compatible.
        // The first five bits are object type 2, not 5.
        let cfg = parse(&[0x13, 0x90, 0x56, 0xe5, 0xa0]).unwrap();
        assert_eq!(cfg.object_type, 2);
        assert_eq!(cfg.core_rate, 22_050);
        assert_eq!(cfg.core_channels, 2);
        assert!(cfg.sbr);
        assert!(!cfg.ps);
        assert_eq!(cfg.output_rate, 44_100);
    }

    #[test]
    fn backward_compatible_he_aac_v2_core_is_mono() {
        let cfg = parse(&[0x13, 0x88, 0x56, 0xe5, 0xa5, 0x48, 0x80]).unwrap();
        assert_eq!(cfg.object_type, 2);
        assert_eq!(cfg.core_rate, 22_050);
        assert_eq!(cfg.core_channels, 1);
        assert!(cfg.sbr && cfg.ps);
        assert_eq!(cfg.output_rate, 44_100);
    }

    #[test]
    fn hierarchical_he_aac_v2_core_rate_comes_first() {
        // AOT 29, core 22050 Hz mono, extension 44100 Hz, core AOT 2.
        let cfg = parse(&[0xeb, 0x8a, 0x08, 0x00]).unwrap();
        assert_eq!(cfg.object_type, 2);
        assert_eq!(cfg.core_rate, 22_050);
        assert_eq!(cfg.output_rate, 44_100);
        assert_eq!(cfg.core_channels, 1);
        assert!(cfg.sbr && cfg.ps);
    }

    #[test]
    fn padding_after_a_short_config_is_not_sbr() {
        let cfg = parse(&[0x12, 0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(!cfg.sbr);
        assert_eq!(cfg.core_channels, 1);
        assert_eq!(cfg.core_rate, 44_100);
    }

    #[test]
    fn scale_count_halves_a_dual_rate_timeline() {
        assert_eq!(scale_count(1_447_936, 44_100, 22_050), 723_968);
        assert_eq!(scale_count(3_274, 44_100, 22_050), 1_637);
        assert_eq!(scale_count(100, 44_100, 44_100), 100);
    }
}
