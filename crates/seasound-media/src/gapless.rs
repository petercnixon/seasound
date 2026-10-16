//! Encoder delay and padding.
//!
//! Symphonia is asked to return every decoded frame. SeaSound drops delay and padding
//! itself, so a cue range and a gapless splice share one presentation timeline.
//! Trim is applied only when a file actually carries it. A guessed delay would cut music.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Delay, trailing padding, and the audible sample count when the tag states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trim {
    pub delay: u64,
    pub padding: u64,
    pub valid: Option<u64>,
}

impl Trim {
    pub fn none() -> Self {
        Self {
            delay: 0,
            padding: 0,
            valid: None,
        }
    }

    pub fn presentation(&self, decoded_frames: Option<u64>) -> u64 {
        if let Some(valid) = self.valid {
            return valid;
        }
        decoded_frames
            .unwrap_or(0)
            .saturating_sub(self.delay)
            .saturating_sub(self.padding)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mp3Info {
    pub trim: Trim,
    /// PCM frames the Xing/Info header says the file decodes to, including delay and padding.
    pub decoded_frames: Option<u64>,
    pub sample_rate: Option<u32>,
}

/// Read gapless info from an MP3.
///
/// The LAME tag lives in the first audio frame, after any ID3v2 tag. Its delay field is
/// three bytes at offset 21 from the `LAME` or `Lavf` marker: 12 bits of delay, then 12
/// bits of padding. Symphonia ignores this tag when the tag CRC is wrong, which is common,
/// so the bytes are read here.
pub fn read_mp3_info(path: &Path) -> Mp3Info {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            return Mp3Info {
                trim: Trim::none(),
                decoded_frames: None,
                sample_rate: None,
            }
        }
    };
    let mut header = [0u8; 10];
    if file.read(&mut header).ok() != Some(10) {
        return Mp3Info {
            trim: Trim::none(),
            decoded_frames: None,
            sample_rate: None,
        };
    }
    let mut window = vec![0u8; 256 * 1024];
    if &header[0..3] == b"ID3" {
        let size = synchsafe(&header[6..10]);
        // The 10-byte header is already consumed, so the file is at the tag body.
        let mut trim = Trim::none();
        if size > 0 && size < 8 * 1024 * 1024 {
            let mut tag = vec![0u8; size];
            if file.read_exact(&mut tag).is_ok() {
                trim = find_itunsmpb(&tag).unwrap_or(Trim::none());
            } else {
                let _ = file.seek(SeekFrom::Start(10 + size as u64));
            }
        } else {
            let _ = file.seek(SeekFrom::Start(10 + size as u64));
        }
        let n = file.read(&mut window).unwrap_or(0);
        window.truncate(n);
        return finish_mp3(&window, trim);
    }
    let n = file.read(&mut window).unwrap_or(0);
    window.truncate(n);
    // Those first 10 bytes were audio. Put them back in front of the rest.
    let mut full = header.to_vec();
    full.extend_from_slice(&window);
    if full.len() > 256 * 1024 {
        full.truncate(256 * 1024);
    }
    finish_mp3(&full, Trim::none())
}

fn finish_mp3(audio: &[u8], id3_trim: Trim) -> Mp3Info {
    let lame = parse_mp3_frame(audio);
    let trim = if lame.trim.delay > 0 || lame.trim.padding > 0 {
        // LAME describes this encode. Prefer it over an ID3 comment when both exist
        // and the comment does not carry a sample count that matches the frame header.
        if let Some(valid) = id3_trim.valid {
            if let Some(decoded) = lame.decoded_frames {
                let from_id3 = id3_trim.delay + valid + id3_trim.padding;
                if from_id3.abs_diff(decoded) <= 1152 && lame.trim.delay == 0 {
                    return Mp3Info {
                        trim: id3_trim,
                        decoded_frames: lame.decoded_frames,
                        sample_rate: lame.sample_rate,
                    };
                }
            }
        }
        lame.trim
    } else if id3_trim.delay > 0 || id3_trim.padding > 0 || id3_trim.valid.is_some() {
        id3_trim
    } else {
        Trim::none()
    };
    Mp3Info {
        trim,
        decoded_frames: lame.decoded_frames,
        sample_rate: lame.sample_rate,
    }
}

/// Parse a LAME/Lavf delay field. `bytes` may be a whole frame or a slice starting at the marker.
pub fn parse_lame_delay(bytes: &[u8]) -> Option<Trim> {
    let marker = find_subsequence(bytes, b"LAME")
        .or_else(|| find_subsequence(bytes, b"Lavf"))?;
    let start = marker + 21;
    if start + 3 > bytes.len() {
        return None;
    }
    let b0 = bytes[start];
    let b1 = bytes[start + 1];
    let b2 = bytes[start + 2];
    let delay = (u32::from(b0) << 4) | u32::from(b1 >> 4);
    let padding = (u32::from(b1 & 0x0f) << 8) | u32::from(b2);
    // Real encoder delays are a few thousand samples. A random hit on the letters is not.
    if delay > 16384 || padding > 16384 * 4 {
        return None;
    }
    if delay == 0 && padding == 0 {
        return None;
    }
    Some(Trim {
        delay: u64::from(delay),
        padding: u64::from(padding),
        valid: None,
    })
}

fn parse_mp3_frame(audio: &[u8]) -> Mp3Info {
    let Some(offset) = find_frame(audio) else {
        return Mp3Info {
            trim: Trim::none(),
            decoded_frames: None,
            sample_rate: None,
        };
    };
    let header = &audio[offset..];
    let sample_rate = mpeg_sample_rate(header);
    let spf = samples_per_frame(header);
    let trim = parse_lame_delay(header).unwrap_or(Trim::none());
    let decoded_frames = xing_frames(header).map(|frames| frames.saturating_mul(spf));
    Mp3Info {
        trim,
        decoded_frames,
        sample_rate,
    }
}

fn find_frame(audio: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 4 < audio.len() {
        if audio[i] == 0xff && audio[i + 1] & 0xe0 == 0xe0 && valid_header(&audio[i..i + 4]) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn valid_header(header: &[u8]) -> bool {
    let version = (header[1] >> 3) & 0x03;
    let layer = (header[1] >> 1) & 0x03;
    let bitrate = (header[2] >> 4) & 0x0f;
    let sample = (header[2] >> 2) & 0x03;
    version != 0x01 && layer != 0x00 && bitrate != 0x00 && bitrate != 0x0f && sample != 0x03
}

fn mpeg_sample_rate(header: &[u8]) -> Option<u32> {
    let version = (header[1] >> 3) & 0x03;
    let index = ((header[2] >> 2) & 0x03) as usize;
    let table: &[u32] = match version {
        0b11 => &[44100, 48000, 32000],
        0b10 => &[22050, 24000, 16000],
        0b00 => &[11025, 12000, 8000],
        _ => return None,
    };
    table.get(index).copied()
}

fn samples_per_frame(header: &[u8]) -> u64 {
    let version = (header[1] >> 3) & 0x03;
    let layer = (header[1] >> 1) & 0x03;
    match (version == 0b11, layer) {
        (_, 0b11) => 384,  // Layer I
        (true, _) => 1152, // MPEG-1 Layer II/III
        _ => 576,
    }
}

/// Xing/Info frame count, when the header carries one.
fn xing_frames(frame: &[u8]) -> Option<u64> {
    let side = xing_offset(frame)?;
    if side + 16 > frame.len() {
        return None;
    }
    let tag = &frame[side..side + 4];
    if tag != b"Xing" && tag != b"Info" {
        return None;
    }
    let flags = u32::from_be_bytes(frame[side + 4..side + 8].try_into().ok()?);
    if flags & 0x0001 == 0 {
        return None;
    }
    let frames = u32::from_be_bytes(frame[side + 8..side + 12].try_into().ok()?);
    if frames == 0 {
        None
    } else {
        Some(u64::from(frames))
    }
}

fn xing_offset(header: &[u8]) -> Option<usize> {
    if header.len() < 4 {
        return None;
    }
    let mpeg1 = (header[1] >> 3) & 0x03 == 0b11;
    let mono = (header[3] >> 6) & 0x03 == 0b11;
    Some(match (mpeg1, mono) {
        (true, false) => 36,
        (true, true) => 21,
        (false, false) => 21,
        (false, true) => 13,
    })
}

/// Apple's gapless comment: ` 00000000 <priming> <remainder> <valid samples>`, all hex.
///
/// The marker often sits inside an MP4 freeform atom, so the bytes before the hex
/// fields are atom headers. Only whitespace-separated hex tokens are considered, and
/// the reserved field of eight zeros is the anchor.
pub fn parse_itunsmpb(text: &str) -> Option<Trim> {
    let tokens: Vec<&str> = text
        .split_whitespace()
        .filter(|token| {
            let len = token.len();
            (8..=16).contains(&len) && token.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .collect();
    for window in tokens.windows(4) {
        if window[0].len() != 8 || window[1].len() != 8 || window[2].len() != 8 {
            continue;
        }
        if window[0].bytes().any(|byte| byte != b'0') {
            continue;
        }
        let priming = match u64::from_str_radix(window[1], 16) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let padding = match u64::from_str_radix(window[2], 16) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let valid = match u64::from_str_radix(window[3], 16) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if priming > 16384 * 4 || padding > 16384 * 8 || valid == 0 {
            continue;
        }
        return Some(Trim {
            delay: priming,
            padding,
            valid: Some(valid),
        });
    }
    None
}

pub fn find_itunsmpb(bytes: &[u8]) -> Option<Trim> {
    let marker = find_subsequence(bytes, b"iTunSMPB")?;
    let tail = &bytes[marker..marker.saturating_add(512).min(bytes.len())];
    let text = String::from_utf8_lossy(tail);
    parse_itunsmpb(&text)
}

fn synchsafe(bytes: &[u8]) -> usize {
    ((bytes[0] as usize & 0x7f) << 21)
        | ((bytes[1] as usize & 0x7f) << 14)
        | ((bytes[2] as usize & 0x7f) << 7)
        | (bytes[3] as usize & 0x7f)
}

pub(crate) fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// First edit-list media time of the audio track, in samples.
///
/// This is the priming offset QuickTime stores when an iTunSMPB atom is absent.
/// A missing edit list is not an error.
pub fn read_mp4_itunsmpb(path: &Path) -> Option<Trim> {
    let moov = read_moov(path)?;
    find_itunsmpb(&moov)
}

pub fn read_mp4_priming(path: &Path) -> Option<u64> {
    let moov = read_moov(path)?;
    let mut priming = None;
    walk_atoms(&moov, |name, body| {
        if name == "trak" {
            if let Some(samples) = audio_track_priming(body) {
                priming = Some(samples);
            }
        }
    });
    priming.filter(|samples| *samples > 0 && *samples < 16384 * 4)
}

fn audio_track_priming(trak_body: &[u8]) -> Option<u64> {
    let mut is_audio = false;
    let mut timescale = None;
    let mut media_time = None;
    walk_atoms(trak_body, |name, body| {
        if name == "hdlr" && body.len() >= 12 && &body[8..12] == b"soun" {
            is_audio = true;
        }
        if name == "mdhd" {
            timescale = mdhd_timescale(body);
        }
        if name == "elst" {
            media_time = elst_media_time(body);
        }
    });
    if !is_audio {
        return None;
    }
    let media_time = media_time?;
    if media_time <= 0 {
        return None;
    }
    // For AAC and ALAC the media timescale is the sample rate, so media time is samples.
    let _ = timescale;
    Some(media_time as u64)
}

fn mdhd_timescale(body: &[u8]) -> Option<u32> {
    if body.is_empty() {
        return None;
    }
    let version = body[0];
    if version == 1 {
        if body.len() < 24 {
            return None;
        }
        Some(u32::from_be_bytes(body[20..24].try_into().ok()?))
    } else {
        if body.len() < 16 {
            return None;
        }
        Some(u32::from_be_bytes(body[12..16].try_into().ok()?))
    }
}

fn elst_media_time(body: &[u8]) -> Option<i64> {
    if body.len() < 8 {
        return None;
    }
    let version = body[0];
    let count = u32::from_be_bytes(body[4..8].try_into().ok()?) as usize;
    if count == 0 {
        return None;
    }
    if version == 1 {
        if body.len() < 8 + 20 {
            return None;
        }
        Some(i64::from_be_bytes(body[16..24].try_into().ok()?))
    } else {
        if body.len() < 8 + 12 {
            return None;
        }
        Some(i64::from(i32::from_be_bytes(body[12..16].try_into().ok()?)))
    }
}

fn read_moov(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut pos = 0u64;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos)).ok()?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header).ok()?;
        let mut size = u32::from_be_bytes(header[0..4].try_into().ok()?) as u64;
        let name = &header[4..8];
        let mut header_len = 8u64;
        if size == 1 {
            let mut large = [0u8; 8];
            file.read_exact(&mut large).ok()?;
            size = u64::from_be_bytes(large);
            header_len = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header_len || pos.saturating_add(size) > len {
            return None;
        }
        if name == b"moov" {
            let body_len = (size - header_len) as usize;
            // A metadata atom. Refuse anything that would pull in media data.
            if body_len > 64 * 1024 * 1024 {
                return None;
            }
            let mut body = vec![0u8; body_len];
            file.read_exact(&mut body).ok()?;
            return Some(body);
        }
        pos += size;
    }
    None
}

fn walk_atoms(data: &[u8], mut visit: impl FnMut(&str, &[u8])) {
    fn rec(data: &[u8], visit: &mut impl FnMut(&str, &[u8])) {
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as u64;
            let name = match std::str::from_utf8(&data[pos + 4..pos + 8]) {
                Ok(name) => name,
                Err(_) => break,
            };
            let mut header = 8usize;
            let size = if size32 == 1 {
                if pos + 16 > data.len() {
                    break;
                }
                header = 16;
                u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap())
            } else if size32 == 0 {
                (data.len() - pos) as u64
            } else {
                size32
            };
            if size < header as u64 || pos + size as usize > data.len() {
                break;
            }
            let body = &data[pos + header..pos + size as usize];
            let container = matches!(
                name,
                "moov" | "trak" | "mdia" | "minf" | "stbl" | "edts" | "udta"
            );
            if container {
                visit(name, body);
                rec(body, visit);
            } else if name == "meta" && body.len() > 4 {
                // Fullbox: version and flags, then child atoms.
                visit(name, body);
                rec(&body[4..], visit);
            } else {
                visit(name, body);
            }
            pos += size as usize;
        }
    }
    rec(data, &mut visit);
}

/// Pick one trim description.
///
/// iTunSMPB wins when its three counts add up to the decoded length, because that is
/// the presentation length Apple players use. Otherwise a LAME tag wins. An edit-list
/// priming value is the fallback and does not invent padding.
pub fn choose_trim(lame: Option<Trim>, itun: Option<Trim>, elst_delay: Option<u64>, decoded: Option<u64>) -> Trim {
    if let Some(itun) = itun {
        if let (Some(valid), Some(decoded)) = (itun.valid, decoded) {
            let sum = itun.delay + valid + itun.padding;
            if sum.abs_diff(decoded) <= 2048 {
                return itun;
            }
        } else if itun.valid.is_some() && lame.is_none() {
            return itun;
        }
    }
    if let Some(lame) = lame {
        if lame.delay > 0 || lame.padding > 0 {
            return lame;
        }
    }
    if let Some(itun) = itun {
        if itun.delay > 0 || itun.padding > 0 || itun.valid.is_some() {
            return itun;
        }
    }
    if let Some(delay) = elst_delay {
        return Trim {
            delay,
            padding: 0,
            valid: None,
        };
    }
    Trim::none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lame_delay_from_the_info_tag_layout() {
        // Bytes taken from a LAME 3.93 Info tag. Delay 576, padding 1560.
        let bytes = b"LAME3.93 \x01\xdd\x00\x00\x00\x00\x00\x00\x00\x00\x02\xff$\x06\x18";
        let trim = parse_lame_delay(bytes).unwrap();
        assert_eq!(trim.delay, 576);
        assert_eq!(trim.padding, 1560);
    }

    #[test]
    fn itunsmpb_hex_fields() {
        let trim = parse_itunsmpb(" 00000000 00000840 00000190 000000000BF53230").unwrap();
        assert_eq!(trim.delay, 0x840);
        assert_eq!(trim.padding, 0x190);
        assert_eq!(trim.valid, Some(0x0BF5_3230));
    }

    #[test]
    fn itunsmpb_ignores_the_mp4_atom_header_before_the_hex() {
        // name atom payload, then a data atom, then the comment. Taken from a real M4A.
        let bytes = b"iTunSMPB\x00\x00\x00\x84data\x00\x00\x00\x01\x00\x00\x00\x00 00000000 00000840 00000190 000000000BF53230 00000000";
        let trim = find_itunsmpb(bytes).unwrap();
        assert_eq!(trim.delay, 0x840);
        assert_eq!(trim.padding, 0x190);
        assert_eq!(trim.valid, Some(0x0BF5_3230));
    }

    #[test]
    fn itunsmpb_rejects_empty_and_huge_values() {
        assert!(parse_itunsmpb("00000000 00000000 00000000 0000000000000000").is_none());
        assert!(parse_itunsmpb("not a tag").is_none());
    }

    #[test]
    fn choose_prefers_itunsmpb_when_it_matches_the_decoded_length() {
        let lame = Trim {
            delay: 576,
            padding: 100,
            valid: None,
        };
        let itun = Trim {
            delay: 2112,
            padding: 400,
            valid: Some(10000),
        };
        let decoded = 2112 + 10000 + 400;
        let chosen = choose_trim(Some(lame), Some(itun), Some(2112), Some(decoded));
        assert_eq!(chosen, itun);
    }

    #[test]
    fn choose_uses_lame_when_it_is_the_only_description() {
        let lame = Trim {
            delay: 576,
            padding: 1560,
            valid: None,
        };
        let chosen = choose_trim(Some(lame), None, None, Some(100_000));
        assert_eq!(chosen.delay, 576);
        assert_eq!(chosen.presentation(Some(100_000)), 100_000 - 576 - 1560);
    }
}
