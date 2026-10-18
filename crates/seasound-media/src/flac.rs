//! FLAC metadata blocks. Audio frames are never read.
//!
//! The cuesheet block is type 5 in RFC 9639. Its track offset points at the first index,
//! which may be INDEX 00 (the pregap). Playback starts at INDEX 01.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::io::MediaSource;

use crate::cue::{cue_album_title, CueTrack};

#[derive(Clone, Debug, Default)]
pub struct FlacMeta {
    pub sample_rate: u32,
    pub channels: u16,
    pub total_samples: u64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_number: u32,
    pub disc: u32,
    pub cuesheet_text: Option<String>,
    pub cue_tracks: Vec<CueTrack>,
    /// Sample offset of the lead-out track, when the cuesheet has one.
    pub lead_out: Option<u64>,
}

pub fn read_flac_meta(path: &Path) -> Option<FlacMeta> {
    let mut file = File::open(path).ok()?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    if &magic != b"fLaC" {
        return None;
    }
    let mut meta = FlacMeta::default();
    loop {
        let mut header = [0u8; 4];
        file.read_exact(&mut header).ok()?;
        let is_last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let length = (u32::from(header[1]) << 16) | (u32::from(header[2]) << 8) | u32::from(header[3]);
        if length > 16 * 1024 * 1024 {
            return None;
        }
        let mut body = vec![0u8; length as usize];
        file.read_exact(&mut body).ok()?;
        match kind {
            0 => {
                if let Some((rate, channels, samples)) = parse_streaminfo(&body) {
                    meta.sample_rate = rate;
                    meta.channels = channels;
                    meta.total_samples = samples;
                }
            }
            4 => parse_vorbis_comment(&body, &mut meta),
            5 => {
                if let Ok(tracks) = parse_cuesheet_block(&body) {
                    meta.lead_out = cuesheet_lead_out(&body);
                    meta.cue_tracks = tracks;
                }
            }
            _ => {}
        }
        if is_last {
            break;
        }
    }
    // The comment walker leaves the file positioned after metadata. Touching the
    // position keeps the seek import used if a future reader needs it.
    let _ = file.seek(SeekFrom::Start(0));
    if meta.album.is_empty() {
        if let Some(text) = &meta.cuesheet_text {
            meta.album = cue_album_title(text);
        }
    }
    Some(meta)
}

fn parse_streaminfo(body: &[u8]) -> Option<(u32, u16, u64)> {
    if body.len() < 18 {
        return None;
    }
    // 16+16+24+24 bits of block and frame sizes, then a packed 64-bit field.
    let packed = u64::from_be_bytes(body[10..18].try_into().ok()?);
    let sample_rate = (packed >> 44) as u32;
    let channels = (((packed >> 41) & 0x07) + 1) as u16;
    let total = packed & ((1 << 36) - 1);
    if sample_rate == 0 {
        return None;
    }
    Some((sample_rate, channels, total))
}

fn parse_vorbis_comment(body: &[u8], meta: &mut FlacMeta) {
    if body.len() < 8 {
        return;
    }
    let vendor_len = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
    let mut pos = 4 + vendor_len;
    if pos + 4 > body.len() {
        return;
    }
    let count = u32::from_le_bytes(body[pos..pos + 4].try_into().unwrap());
    pos += 4;
    for _ in 0..count {
        if pos + 4 > body.len() {
            break;
        }
        let len = u32::from_le_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if pos + len > body.len() {
            break;
        }
        let item = &body[pos..pos + len];
        pos += len;
        let Ok(item) = std::str::from_utf8(item) else {
            continue;
        };
        let Some((key, value)) = item.split_once('=') else {
            continue;
        };
        match key.to_ascii_uppercase().as_str() {
            "TITLE" if meta.title.is_empty() => meta.title = value.to_string(),
            "ARTIST" if meta.artist.is_empty() => meta.artist = value.to_string(),
            "ALBUM" if meta.album.is_empty() => meta.album = value.to_string(),
            "ALBUMARTIST" | "ALBUM ARTIST" if meta.album_artist.is_empty() => {
                meta.album_artist = value.to_string();
            }
            "TRACKNUMBER" if meta.track_number == 0 => {
                meta.track_number = value
                    .split('/')
                    .next()
                    .and_then(|number| number.trim().parse().ok())
                    .unwrap_or(0);
            }
            "DISCNUMBER" if meta.disc == 0 => {
                meta.disc = value
                    .split('/')
                    .next()
                    .and_then(|number| number.trim().parse().ok())
                    .unwrap_or(0);
            }
            "CUESHEET" if meta.cuesheet_text.is_none() => {
                meta.cuesheet_text = Some(value.to_string());
            }
            _ => {}
        }
    }
}

/// Parse a FLAC `CUESHEET` metadata block body (the header byte is already removed).
pub fn parse_cuesheet_block(body: &[u8]) -> Result<Vec<CueTrack>, String> {
    // 128 catalog + 8 lead-in + 1 flag byte + 258 reserved + 1 count.
    let header = 128 + 8 + 1 + 258;
    if body.len() < header + 1 {
        return Err("cuesheet block is truncated".into());
    }
    let count = body[header] as usize;
    if count == 0 || count > 100 {
        return Err(format!("cuesheet track count {count} is not usable"));
    }
    let mut pos = header + 1;
    let mut raw: Vec<RawTrack> = Vec::with_capacity(count);
    for _ in 0..count {
        if pos + 36 > body.len() {
            return Err("cuesheet track is truncated".into());
        }
        let offset = u64::from_be_bytes(body[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let number = body[pos];
        pos += 1;
        pos += 12; // ISRC
        pos += 1 + 13; // flags and reserved
        if pos >= body.len() {
            return Err("cuesheet track is truncated".into());
        }
        let nidx = body[pos] as usize;
        pos += 1;
        let mut indexes = Vec::with_capacity(nidx);
        for _ in 0..nidx {
            if pos + 12 > body.len() {
                return Err("cuesheet index is truncated".into());
            }
            let index_offset = u64::from_be_bytes(body[pos..pos + 8].try_into().unwrap());
            pos += 8;
            let index_number = body[pos];
            pos += 1;
            pos += 3;
            indexes.push((index_number, index_offset));
        }
        raw.push(RawTrack {
            number,
            offset,
            indexes,
        });
    }

    let mut tracks = Vec::new();
    for track in raw.into_iter().filter(|track| track.number != 170 && track.number != 255) {
        let audible = audible_start(&track);
        tracks.push(CueTrack {
            number: u32::from(track.number),
            title: format!("Track {:02}", track.number),
            performer: String::new(),
            start_frame: audible,
        });
    }
    if tracks.is_empty() {
        return Err("cuesheet has no musical tracks".into());
    }
    Ok(tracks)
}

struct RawTrack {
    number: u8,
    offset: u64,
    indexes: Vec<(u8, u64)>,
}

fn audible_start(track: &RawTrack) -> u64 {
    if let Some((_, offset)) = track.indexes.iter().find(|(number, _)| *number == 1) {
        track.offset.saturating_add(*offset)
    } else if let Some((_, offset)) = track.indexes.first() {
        track.offset.saturating_add(*offset)
    } else {
        track.offset
    }
}

/// The lead-out offset is the end of the last musical track. It is stored by appending
/// nothing: callers pass the file length. When the lead-out is earlier than the file
/// length it is the more accurate end, so it is returned separately.
pub fn cuesheet_lead_out(body: &[u8]) -> Option<u64> {
    // Reparse is cheap (a cuesheet is a few kilobytes) and keeps the track parser honest.
    let header = 128 + 8 + 1 + 258;
    if body.len() < header + 1 {
        return None;
    }
    let count = body[header] as usize;
    let mut pos = header + 1;
    for _ in 0..count {
        if pos + 36 > body.len() {
            return None;
        }
        let offset = u64::from_be_bytes(body[pos..pos + 8].try_into().ok()?);
        pos += 8;
        let number = body[pos];
        pos += 1;
        pos += 12 + 1 + 13;
        if pos >= body.len() {
            return None;
        }
        let nidx = body[pos] as usize;
        pos += 1;
        pos += nidx * 12;
        if number == 170 || number == 255 {
            return Some(offset);
        }
    }
    None
}

/// Open a file for Symphonia. A FLAC file's cuesheet blocks are shown to it as padding.
///
/// Symphonia rejects the whole stream when a cuesheet breaks a CD-DA rule, for example a
/// non-zero lead-in with the CD-DA flag clear, or an offset that is not a multiple of 588.
/// Sheets made up for a joined album often do. The cue rows come from `read_flac_meta`, so
/// Symphonia never needs the block. Only the block type byte is changed, and only in memory.
pub fn open_media_source(path: &Path) -> std::io::Result<Box<dyn MediaSource>> {
    let mut file = File::open(path)?;
    let patches = cuesheet_type_bytes(&mut file).unwrap_or_default();
    file.seek(SeekFrom::Start(0))?;
    if patches.is_empty() {
        return Ok(Box::new(file));
    }
    Ok(Box::new(MaskedFlac {
        file,
        pos: 0,
        patches,
    }))
}

/// Byte offsets of the header type byte of every cuesheet block.
fn cuesheet_type_bytes(file: &mut File) -> Option<Vec<u64>> {
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    if &magic != b"fLaC" {
        return None;
    }
    let mut offsets = Vec::new();
    let mut pos = 4u64;
    loop {
        let mut header = [0u8; 4];
        file.read_exact(&mut header).ok()?;
        let length = (u64::from(header[1]) << 16) | (u64::from(header[2]) << 8) | u64::from(header[3]);
        if header[0] & 0x7f == 5 {
            offsets.push(pos);
        }
        if header[0] & 0x80 != 0 {
            break;
        }
        pos += 4 + length;
        file.seek(SeekFrom::Start(pos)).ok()?;
    }
    Some(offsets)
}

struct MaskedFlac {
    file: File,
    pos: u64,
    patches: Vec<u64>,
}

impl Read for MaskedFlac {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        for &offset in &self.patches {
            if offset >= self.pos && offset < self.pos + n as u64 {
                let byte = &mut buf[(offset - self.pos) as usize];
                // Keep the last-block flag. Type 1 is padding.
                *byte = (*byte & 0x80) | 1;
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for MaskedFlac {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = self.file.seek(to)?;
        Ok(self.pos)
    }
}

impl MediaSource for MaskedFlac {
    fn is_seekable(&self) -> bool {
        self.file.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.file.byte_len()
    }
}

/// End of the last cue track: the lead-out if it sits inside the file, otherwise the file.
pub fn cue_end(lead_out: Option<u64>, presentation_frames: u64) -> u64 {
    match lead_out {
        Some(offset) if offset > 0 && offset <= presentation_frames => offset,
        _ => presentation_frames,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_u64(buf: &mut Vec<u8>, value: u64) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    fn push_track(buf: &mut Vec<u8>, offset: u64, number: u8, indexes: &[(u8, u64)]) {
        push_u64(buf, offset);
        buf.push(number);
        buf.extend_from_slice(&[0u8; 12]);
        buf.push(0); // flags
        buf.extend_from_slice(&[0u8; 13]);
        buf.push(indexes.len() as u8);
        for (number, offset) in indexes {
            push_u64(buf, *offset);
            buf.push(*number);
            buf.extend_from_slice(&[0u8; 3]);
        }
    }

    fn sheet() -> Vec<u8> {
        let mut body = vec![0u8; 128 + 8 + 1 + 258];
        body.push(3);
        // Track 1 offset is INDEX 00. INDEX 01 is 588 samples later.
        push_track(&mut body, 0, 1, &[(0, 0), (1, 588)]);
        push_track(&mut body, 44100, 2, &[(1, 0)]);
        push_track(&mut body, 88200, 170, &[]);
        body
    }

    #[test]
    fn cuesheet_blocks_read_as_padding_and_nothing_else_changes() {
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0x00, 0, 0, 2, 0xaa, 0xbb]);
        bytes.extend_from_slice(&[0x05, 0, 0, 1, 0xcc]);
        bytes.extend_from_slice(&[0x85, 0, 0, 1, 0xdd]);
        bytes.extend_from_slice(&[0xff, 0xf8, 0x05]);
        let path = std::env::temp_dir().join(format!("seasound-mask-{}.flac", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let mut source = open_media_source(&path).unwrap();
        let mut seen = Vec::new();
        // One byte at a time, so each patch lands at a different read position.
        let mut byte = [0u8; 1];
        while source.read(&mut byte).unwrap() == 1 {
            seen.push(byte[0]);
        }
        source.seek(SeekFrom::Start(15)).unwrap();
        source.read_exact(&mut byte).unwrap();
        std::fs::remove_file(&path).unwrap();
        let mut expected = bytes.clone();
        expected[10] = 0x01;
        expected[15] = 0x81;
        assert_eq!(seen, expected);
        assert_eq!(byte[0], 0x81);
    }

    #[test]
    fn index_01_not_the_pregap_and_lead_out_is_excluded() {
        let body = sheet();
        let tracks = parse_cuesheet_block(&body).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].number, 1);
        assert_eq!(tracks[0].start_frame, 588);
        assert_eq!(tracks[1].start_frame, 44100);
        assert_eq!(cuesheet_lead_out(&body), Some(88200));
        assert_eq!(cue_end(Some(88200), 100_000), 88200);
        assert_eq!(cue_end(Some(88200), 1000), 1000);
    }
}
