//! CDRWIN cue sheets. Titles come from here. Sample offsets come from here only when
//! the FLAC file has no binary cuesheet block.

use encoding_rs::WINDOWS_1252;

use seasound_model::Segment;

/// One musical track from a cue sheet, before it is clipped to the file length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CueTrack {
    pub number: u32,
    pub title: String,
    pub performer: String,
    /// INDEX 01, in presentation frames. INDEX 00 is the pregap and is not the start.
    pub start_frame: u64,
}

/// Tracks that belong to one `FILE` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CueFile {
    pub file_name: String,
    pub tracks: Vec<CueTrack>,
}

/// Decode a sidecar cue file. UTF-8 is used when the bytes are valid UTF-8.
/// Otherwise the bytes are Windows-1252, which is what older rippers wrote.
pub fn decode_cue_bytes(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => WINDOWS_1252.decode(bytes).0.into_owned(),
    }
}

/// Parse cue text into one group per `FILE` line.
///
/// Index times are CD frames (`MM:SS:FF` at 75 frames per second). At 44.1 kHz that
/// converts to an exact sample count. Other rates round down to the nearest sample,
/// which matches integer `frames * rate / 75`.
pub fn parse_cue_text(text: &str, sample_rate: u32) -> Result<Vec<CueFile>, String> {
    if sample_rate == 0 {
        return Err("cue sheet needs a sample rate".into());
    }
    let mut files: Vec<CueFile> = Vec::new();
    let mut album_performer = String::new();
    let mut album_title = String::new();
    let mut in_track = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = split_cue(line);
        if parts.is_empty() {
            continue;
        }
        let command = parts.remove(0).to_ascii_uppercase();
        match command.as_str() {
            "REM" | "FLAGS" | "PREGAP" | "POSTGAP" | "SONGWRITER" | "ISRC" | "CATALOG"
            | "CDTEXTFILE" => {}
            "PERFORMER" => {
                let value = join_rest(&parts);
                if in_track {
                    if let Some(track) = files.last_mut().and_then(|file| file.tracks.last_mut()) {
                        track.performer = value;
                    }
                } else {
                    album_performer = value;
                }
            }
            "TITLE" => {
                let value = join_rest(&parts);
                if in_track {
                    if let Some(track) = files.last_mut().and_then(|file| file.tracks.last_mut()) {
                        track.title = value;
                    }
                } else {
                    album_title = value;
                }
            }
            "FILE" => {
                in_track = false;
                let file_name = if parts.len() >= 2 {
                    parts[..parts.len() - 1].join(" ")
                } else {
                    parts.first().cloned().unwrap_or_default()
                };
                files.push(CueFile {
                    file_name: unquote(&file_name),
                    tracks: Vec::new(),
                });
            }
            "TRACK" => {
                in_track = true;
                if files.is_empty() {
                    files.push(CueFile {
                        file_name: String::new(),
                        tracks: Vec::new(),
                    });
                }
                let number = parts
                    .first()
                    .and_then(|token| token.parse::<u32>().ok())
                    .unwrap_or(0);
                if number == 0 {
                    continue;
                }
                let file = files.last_mut().expect("a file was just ensured");
                file.tracks.push(CueTrack {
                    number,
                    title: String::new(),
                    performer: album_performer.clone(),
                    start_frame: 0,
                });
            }
            "INDEX" => {
                let (Some(which), Some(stamp)) = (parts.first(), parts.get(1)) else {
                    continue;
                };
                let Ok(index_number) = which.parse::<u32>() else {
                    continue;
                };
                let Some(frame) = parse_index_time(stamp, sample_rate) else {
                    continue;
                };
                let Some(track) = files.last_mut().and_then(|file| file.tracks.last_mut()) else {
                    continue;
                };
                // INDEX 01 is the audible start. A later index must not move it.
                // INDEX 00 is the pregap. Use it only when the track has no INDEX 01 yet
                // and this is the only index we will see; a following INDEX 01 replaces it.
                if index_number == 1 || (index_number == 0 && track.start_frame == 0) {
                    track.start_frame = frame;
                }
            }
            _ => {}
        }
    }

    for file in &mut files {
        for track in &mut file.tracks {
            if track.title.is_empty() {
                track.title = format!("Track {:02}", track.number);
            }
            if track.performer.is_empty() {
                track.performer = album_performer.clone();
            }
            let _ = &album_title;
        }
    }
    files.retain(|file| !file.tracks.is_empty());
    if files.is_empty() {
        return Err("cue sheet has no tracks".into());
    }
    Ok(files)
}

/// Album title from the header, before the first track. Empty when the sheet has none.
pub fn cue_album_title(text: &str) -> String {
    let mut seen_track = false;
    for raw in text.lines() {
        let line = raw.trim();
        let mut parts = split_cue(line);
        if parts.is_empty() {
            continue;
        }
        let command = parts.remove(0).to_ascii_uppercase();
        if command == "TRACK" {
            seen_track = true;
        }
        if command == "TITLE" && !seen_track {
            return join_rest(&parts);
        }
    }
    String::new()
}

pub fn segments_from_cue(
    tracks: &[CueTrack],
    presentation_frames: u64,
    album: &str,
) -> Vec<Segment> {
    if tracks.is_empty() || presentation_frames == 0 {
        return Vec::new();
    }
    let mut segments = Vec::new();
    for (index, track) in tracks.iter().enumerate() {
        let start = track.start_frame;
        let end = tracks
            .get(index + 1)
            .map(|next| next.start_frame)
            .unwrap_or(presentation_frames);
        let end = end.min(presentation_frames);
        if start >= presentation_frames || end <= start {
            continue;
        }
        segments.push(Segment {
            number: track.number,
            title: track.title.clone(),
            artist: track.performer.clone(),
            album: album.to_string(),
            start_frame: start,
            end_frame: end,
        });
    }
    segments
}

/// Apply titles and performers from a text sheet onto offsets from a binary sheet.
/// Matching is by track number. Binary offsets stay.
pub fn merge_titles(binary: &[CueTrack], text: &[CueTrack]) -> Vec<CueTrack> {
    binary
        .iter()
        .map(|track| {
            let mut track = track.clone();
            if let Some(named) = text.iter().find(|named| named.number == track.number) {
                if !named.title.is_empty() {
                    track.title = named.title.clone();
                }
                if !named.performer.is_empty() {
                    track.performer = named.performer.clone();
                }
            }
            track
        })
        .collect()
}

fn parse_index_time(stamp: &str, sample_rate: u32) -> Option<u64> {
    let mut pieces = stamp.split(':');
    let minutes: u64 = pieces.next()?.parse().ok()?;
    let seconds: u64 = pieces.next()?.parse().ok()?;
    let frames: u64 = pieces.next()?.parse().ok()?;
    if seconds >= 60 || frames >= 75 {
        return None;
    }
    let cd_frames = (minutes * 60 + seconds) * 75 + frames;
    Some(cd_frames.saturating_mul(u64::from(sample_rate)) / 75)
}

fn split_cue(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn join_rest(parts: &[String]) -> String {
    unquote(&parts.join(" "))
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHEET: &str = r#"
PERFORMER "Pink Floyd"
TITLE "The Dark Side of the Moon"
FILE "darkside.flac" WAVE
  TRACK 01 AUDIO
    TITLE "Speak to Me"
    PERFORMER "Pink Floyd"
    INDEX 00 00:00:00
    INDEX 01 00:00:32
  TRACK 02 AUDIO
    TITLE "Breathe"
    INDEX 01 01:05:00
  TRACK 03 AUDIO
    TITLE "On the Run"
    INDEX 00 02:55:00
    INDEX 01 03:00:00
"#;

    #[test]
    fn index_01_is_the_start_and_pregap_belongs_to_the_previous_track() {
        let files = parse_cue_text(SHEET, 44100).unwrap();
        assert_eq!(files.len(), 1);
        let tracks = &files[0].tracks;
        assert_eq!(tracks[0].start_frame, 32 * 588);
        assert_eq!(tracks[1].title, "Breathe");
        assert_eq!(tracks[1].performer, "Pink Floyd");
        // 1:05:00 is 65 seconds, 65 * 75 CD frames.
        assert_eq!(tracks[1].start_frame, 65 * 75 * 588);
        let segments = segments_from_cue(tracks, 20 * 44100 * 60, "The Dark Side of the Moon");
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].end_frame, segments[1].start_frame);
        // Track 2 runs through track 3's pregap, up to track 3 INDEX 01.
        assert_eq!(segments[1].end_frame, tracks[2].start_frame);
        assert_eq!(segments[0].album, "The Dark Side of the Moon");
    }

    #[test]
    fn windows_1252_bytes_round_trip_a_name() {
        let mut bytes = b"TITLE \"".to_vec();
        bytes.push(0xE9); // é in Windows-1252
        bytes.extend_from_slice(b"lan\"\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
        let text = decode_cue_bytes(&bytes);
        let files = parse_cue_text(&text, 44100).unwrap();
        assert!(files[0].tracks[0].title.contains('é') || text.contains('é'));
    }

    #[test]
    fn several_files_stay_separate() {
        let text = r#"
FILE "a.flac" WAVE
  TRACK 01 AUDIO
    TITLE "One"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Two"
    INDEX 01 01:00:00
FILE "b.flac" WAVE
  TRACK 03 AUDIO
    TITLE "Three"
    INDEX 01 00:00:00
"#;
        let files = parse_cue_text(text, 44100).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file_name, "a.flac");
        assert_eq!(files[1].tracks[0].title, "Three");
    }

    #[test]
    fn binary_offsets_keep_text_titles() {
        let binary = vec![CueTrack {
            number: 2,
            title: "Track 02".into(),
            performer: String::new(),
            start_frame: 44100,
        }];
        let text = vec![CueTrack {
            number: 2,
            title: "Breathe".into(),
            performer: "Pink Floyd".into(),
            start_frame: 0,
        }];
        let merged = merge_titles(&binary, &text);
        assert_eq!(merged[0].start_frame, 44100);
        assert_eq!(merged[0].title, "Breathe");
        assert_eq!(merged[0].performer, "Pink Floyd");
    }
}
