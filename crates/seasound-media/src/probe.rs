//! Turn a path into a [`Source`]. One file becomes one source, with one segment per
//! cue track or chapter, or a single segment for an ordinary file.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use lofty::file::TaggedFileExt;
use lofty::prelude::*;
use lofty::probe::Probe;
use mp4ameta::Tag as Mp4Tag;
use seasound_model::{AudioFormat, Cover, Error, Segment, Source, SourceId};
use symphonia::core::codecs::{
    CodecType, DecoderOptions, CODEC_TYPE_AAC, CODEC_TYPE_ALAC, CODEC_TYPE_FLAC, CODEC_TYPE_MP1,
    CODEC_TYPE_MP2, CODEC_TYPE_MP3, CODEC_TYPE_OPUS, CODEC_TYPE_VORBIS,
};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::chapters::{segments_from_chapters, ChapterMark};
use crate::cue::{
    cue_album_title, decode_cue_bytes, merge_titles, parse_cue_text, segments_from_cue, CueTrack,
};
use crate::flac::{cue_end, read_flac_meta};
use crate::gapless::{self, Trim};

const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "mp4", "wav", "aiff", "aif", "alac", "ogg", "oga",
];

/// Load files, folders, cue sheets, and m3u playlists.
///
/// A folder is sorted by album, disc, track number, then path. An explicit list of
/// files keeps the order the caller passed. Cue tracks stay in cue order inside a file.
/// A cue sheet becomes one source for every audio file it names.
pub fn load_paths(paths: &[PathBuf]) -> Vec<Source> {
    let mut sources = Vec::new();
    for path in paths {
        let Ok(meta) = fs::symlink_metadata(path) else {
            sources.push(error_source(path, "could not read the path"));
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            sources.extend(load_dir(path));
        } else if extension(path) == "cue" {
            sources.extend(probe_cue_file(path));
        } else if is_m3u(path) {
            sources.extend(load_m3u(path));
        } else {
            sources.push(probe_file(path));
        }
    }
    sources
}

pub fn probe_file(path: &Path) -> Source {
    if extension(path) == "cue" {
        return probe_cue_file(path)
            .into_iter()
            .next()
            .unwrap_or_else(|| error_source(path, "the cue sheet did not list a readable audio file"));
    }
    match probe_audio(path, CueMode::Discover) {
        Ok(source) => source,
        Err(err) => error_source(path, err.message()),
    }
}

enum CueMode {
    /// Embedded cue, then a sibling `.cue` of the same name.
    Discover,
}

fn load_dir(dir: &Path) -> Vec<Source> {
    let mut files = Vec::new();
    collect(dir, &mut files, 0);
    files.sort();
    let mut cues = Vec::new();
    let mut audio = Vec::new();
    for path in files {
        match extension(&path).as_str() {
            "cue" => cues.push(path),
            "m3u" | "m3u8" => {}
            ext if AUDIO_EXT.contains(&ext) => audio.push(path),
            _ => {}
        }
    }
    let mut consumed = HashSet::new();
    let mut sources = Vec::new();
    for cue in &cues {
        match expand_cue(cue) {
            Ok((expanded, referenced)) => {
                for path in referenced {
                    consumed.insert(normalize(&path));
                }
                sources.extend(expanded);
            }
            Err(err) => sources.push(error_source(cue, err.message())),
        }
    }
    for path in audio {
        if consumed.contains(&normalize(&path)) {
            continue;
        }
        sources.push(probe_file(&path));
    }
    sources.sort_by(|a, b| {
        let album_a = a.album.to_ascii_lowercase();
        let album_b = b.album.to_ascii_lowercase();
        album_a
            .cmp(&album_b)
            .then(a.disc.cmp(&b.disc))
            .then(a.track_number.cmp(&b.track_number))
            .then(a.path.cmp(&b.path))
    });
    sources
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if meta.is_dir() {
            collect(&path, out, depth + 1);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

fn load_m3u(path: &Path) -> Vec<Source> {
    let Ok(bytes) = fs::read(path) else {
        return vec![error_source(path, "could not read the playlist")];
    };
    let text = decode_cue_bytes(&bytes);
    let parent = path.parent().unwrap_or(Path::new(""));
    let mut sources = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let file = parent.join(line);
        sources.push(probe_file(&file));
    }
    if sources.is_empty() {
        sources.push(error_source(path, "the playlist is empty"));
    }
    sources
}

fn probe_cue_file(path: &Path) -> Vec<Source> {
    expand_cue(path)
        .unwrap_or_else(|err| (vec![error_source(path, err.message())], Vec::new()))
        .0
}

fn expand_cue(path: &Path) -> Result<(Vec<Source>, Vec<PathBuf>), Error> {
    let bytes = fs::read(path)?;
    let text = decode_cue_bytes(&bytes);
    let album = cue_album_title(&text);
    // File names do not depend on the sample rate. 44100 only lists the FILE blocks.
    let listing = parse_cue_text(&text, 44100).map_err(Error::new)?;
    let mut sources = Vec::new();
    let mut referenced = Vec::new();
    for file in &listing {
        let audio = resolve_cue_file(path, &file.file_name).ok_or_else(|| {
            Error::new(format!("cue sheet file \"{}\" was not found", file.file_name))
        })?;
        referenced.push(audio.clone());
        let mut source = probe_audio(&audio, CueMode::Discover)?;
        let parsed = parse_cue_text(&text, source.sample_rate.max(1)).map_err(Error::new)?;
        let matched = parsed
            .iter()
            .find(|candidate| candidate.file_name == file.file_name)
            .or_else(|| parsed.first());
        let Some(matched) = matched else {
            continue;
        };
        let album = if album.is_empty() {
            source.album.clone()
        } else {
            album.clone()
        };
        let segments = segments_from_cue(&matched.tracks, source.presentation_frames, &album);
        if segments.is_empty() {
            source.warning = Some("the cue sheet did not produce any tracks inside the file".into());
        } else {
            apply_segments(&mut source, segments, &album);
        }
        sources.push(source);
    }
    if sources.is_empty() {
        return Err(Error::new("the cue sheet has no readable audio"));
    }
    Ok((sources, referenced))
}

fn resolve_cue_file(cue_path: &Path, name: &str) -> Option<PathBuf> {
    let parent = cue_path.parent().unwrap_or(Path::new(""));
    let direct = parent.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    let file_name = Path::new(name).file_name()?;
    let beside = parent.join(file_name);
    if beside.is_file() {
        return Some(beside);
    }
    let stem = Path::new(file_name).file_stem()?;
    for ext in AUDIO_EXT {
        let candidate = parent.join(stem).with_extension(ext);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn probe_audio(path: &Path, cue: CueMode) -> Result<Source, Error> {
    if !path.is_file() {
        return Err(Error::new(format!("{} does not exist", path.display())));
    }
    let info = stream_info(path)?;
    if info.he_aac {
        return Ok(error_source(
            path,
            "HE-AAC is not supported. SeaSound decodes AAC-LC.",
        ));
    }
    let tags = read_tags(path);
    let flac = if info.format == AudioFormat::Flac {
        read_flac_meta(path)
    } else {
        None
    };

    let trim = match info.format {
        AudioFormat::Mp3 => gapless::read_mp3_info(path).trim,
        AudioFormat::Aac | AudioFormat::Alac => {
            let itun = gapless::read_mp4_itunsmpb(path);
            let elst = gapless::read_mp4_priming(path);
            gapless::choose_trim(None, itun, elst, info.decoded_frames)
        }
        _ => Trim::none(),
    };
    let decoded = match info.format {
        AudioFormat::Mp3 => gapless::read_mp3_info(path).decoded_frames.or(info.decoded_frames),
        AudioFormat::Flac => flac
            .as_ref()
            .map(|meta| meta.total_samples)
            .filter(|samples| *samples > 0)
            .or(info.decoded_frames),
        _ => info.decoded_frames,
    };
    let mut presentation = trim.presentation(decoded);
    if presentation == 0 {
        if let Some(duration) = tags.duration_secs {
            presentation = (duration * f64::from(info.sample_rate.max(1))).round() as u64;
        }
    }
    if presentation == 0 {
        return Err(Error::new("the file has no duration"));
    }

    let sample_rate = if info.sample_rate > 0 {
        info.sample_rate
    } else {
        flac.as_ref().map(|meta| meta.sample_rate).unwrap_or(0)
    };
    if sample_rate == 0 {
        return Err(Error::new("the file has no sample rate"));
    }

    let artist = first_nonempty([
        tags.artist.as_str(),
        flac.as_ref().map(|meta| meta.artist.as_str()).unwrap_or(""),
        tags.album_artist.as_str(),
        flac.as_ref().map(|meta| meta.album_artist.as_str()).unwrap_or(""),
    ]);
    let album = first_nonempty([
        tags.album.as_str(),
        flac.as_ref().map(|meta| meta.album.as_str()).unwrap_or(""),
    ]);
    let title = first_nonempty([
        tags.title.as_str(),
        flac.as_ref().map(|meta| meta.title.as_str()).unwrap_or(""),
        file_stem(path),
    ]);
    let track_number = tags
        .track_number
        .or(flac.as_ref().map(|meta| meta.track_number))
        .unwrap_or(0);
    let disc = tags
        .disc
        .or(flac.as_ref().map(|meta| meta.disc))
        .unwrap_or(0);

    let mut source = Source {
        id: SourceId(0),
        path: path.to_path_buf(),
        format: info.format,
        sample_rate,
        channels: info.channels.max(1),
        delay: trim.delay,
        padding: trim.padding,
        presentation_frames: presentation,
        artist: artist.to_string(),
        album: album.to_string(),
        title: title.to_string(),
        disc,
        track_number,
        segments: Vec::new(),
        group_title: None,
        error: None,
        warning: None,
        cover: tags.cover,
    };

    match cue {
        CueMode::Discover => {
            if let Some(meta) = flac.as_ref() {
                if let Some(segments) = flac_segments(path, meta, &source) {
                    let album = if source.album.is_empty() {
                        meta.album.clone()
                    } else {
                        source.album.clone()
                    };
                    apply_segments(&mut source, segments, &album);
                }
            }
            if source.segments.is_empty() && is_mp4(path) {
                match mp4_segments(path, &source) {
                    Ok(Some(segments)) => {
                        let album = if source.album.is_empty() {
                            source.title.clone()
                        } else {
                            source.album.clone()
                        };
                        apply_segments(&mut source, segments, &album);
                    }
                    Ok(None) => {}
                    Err(err) => source.warning = Some(err),
                }
            }
            if source.segments.is_empty() {
                source.segments = vec![whole_file_segment(&source)];
            }
        }
    }
    if source.track_number == 0 {
        source.track_number = source.segments.first().map(|segment| segment.number).unwrap_or(0);
    }
    Ok(source)
}

fn flac_segments(path: &Path, meta: &crate::flac::FlacMeta, source: &Source) -> Option<Vec<Segment>> {
    let text = meta.cuesheet_text.clone().or_else(|| sibling_cue_text(path));
    let text_tracks = text.as_deref().and_then(|text| {
        parse_cue_text(text, source.sample_rate)
            .ok()
            .and_then(|files| files.into_iter().next())
            .map(|file| file.tracks)
    });
    let tracks: Vec<CueTrack> = if !meta.cue_tracks.is_empty() {
        match text_tracks {
            Some(text) => merge_titles(&meta.cue_tracks, &text),
            None => meta.cue_tracks.clone(),
        }
    } else {
        text_tracks?
    };
    let album = if source.album.is_empty() {
        if meta.album.is_empty() {
            text.as_deref()
                .map(cue_album_title)
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| file_stem(path).to_string())
        } else {
            meta.album.clone()
        }
    } else {
        source.album.clone()
    };
    let end = cue_end(meta.lead_out, source.presentation_frames);
    let mut segments = segments_from_cue(&tracks, end, &album);
    for segment in &mut segments {
        if segment.artist.is_empty() {
            segment.artist = source.artist.clone();
        }
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments)
    }
}

fn sibling_cue_text(path: &Path) -> Option<String> {
    let cue = path.with_extension("cue");
    if !cue.is_file() {
        return None;
    }
    let bytes = fs::read(cue).ok()?;
    Some(decode_cue_bytes(&bytes))
}

fn mp4_segments(path: &Path, source: &Source) -> Result<Option<Vec<Segment>>, String> {
    let tag = Mp4Tag::read_from_path(path).map_err(|err| err.to_string())?;
    let track = tag.chapter_track();
    let list = tag.chapter_list();
    let chosen: Vec<ChapterMark> = if !track.is_empty() {
        track
            .iter()
            .map(|chapter| ChapterMark {
                start: chapter.start,
                title: chapter.title.clone(),
            })
            .collect()
    } else {
        list.iter()
            .map(|chapter| ChapterMark {
                start: chapter.start,
                title: chapter.title.clone(),
            })
            .collect()
    };
    if chosen.is_empty() {
        return Ok(None);
    }
    let artist = source.artist.clone();
    let album = if source.album.is_empty() {
        source.title.clone()
    } else {
        source.album.clone()
    };
    let segments = segments_from_chapters(
        &chosen,
        source.presentation_frames,
        source.sample_rate,
        &artist,
        &album,
    );
    if segments.is_empty() {
        Ok(None)
    } else {
        Ok(Some(segments))
    }
}

fn apply_segments(source: &mut Source, segments: Vec<Segment>, album: &str) {
    if !album.is_empty() && source.album.is_empty() {
        source.album = album.to_string();
    }
    let group = if segments.len() > 1 {
        Some(if source.album.is_empty() {
            file_stem(&source.path).to_string()
        } else {
            source.album.clone()
        })
    } else {
        None
    };
    source.group_title = group;
    source.segments = segments;
}

fn whole_file_segment(source: &Source) -> Segment {
    Segment {
        number: source.track_number,
        title: source.title.clone(),
        artist: source.artist.clone(),
        album: source.album.clone(),
        start_frame: 0,
        end_frame: source.presentation_frames,
    }
}

struct StreamInfo {
    format: AudioFormat,
    sample_rate: u32,
    channels: u16,
    decoded_frames: Option<u64>,
    he_aac: bool,
}

fn stream_info(path: &Path) -> Result<StreamInfo, Error> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(ext);
    }
    let mut format_opts = FormatOptions::default();
    format_opts.enable_gapless = false;
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &format_opts, &MetadataOptions::default())
        .map_err(|err| Error::new(format!("{err}")))?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|track| {
            symphonia::default::get_codecs()
                .make(&track.codec_params, &DecoderOptions::default())
                .is_ok()
        })
        .ok_or_else(|| Error::new(unsupported_message(path)))?;
    let codec = track.codec_params.codec;
    if codec == CODEC_TYPE_OPUS {
        return Err(Error::new(
            "Opus is not supported yet. The pre-skip in this Symphonia version is incomplete.",
        ));
    }
    let he_aac = track
        .codec_params
        .extra_data
        .as_deref()
        .is_some_and(|extra| !extra.is_empty() && (extra[0] >> 3 == 5 || extra[0] >> 3 == 29));
    let audio_format = classify_codec(codec, path);
    Ok(StreamInfo {
        format: audio_format,
        sample_rate: track.codec_params.sample_rate.unwrap_or(0),
        channels: track
            .codec_params
            .channels
            .map(|channels| channels.count() as u16)
            .unwrap_or(0),
        decoded_frames: track.codec_params.n_frames,
        he_aac,
    })
}

fn classify_codec(codec: CodecType, path: &Path) -> AudioFormat {
    if matches!(codec, CODEC_TYPE_MP1 | CODEC_TYPE_MP2 | CODEC_TYPE_MP3) {
        AudioFormat::Mp3
    } else if codec == CODEC_TYPE_AAC {
        AudioFormat::Aac
    } else if codec == CODEC_TYPE_FLAC {
        AudioFormat::Flac
    } else if codec == CODEC_TYPE_ALAC {
        AudioFormat::Alac
    } else if codec == CODEC_TYPE_VORBIS {
        AudioFormat::Vorbis
    } else {
        match extension(path).as_str() {
            "mp3" => AudioFormat::Mp3,
            "m4a" | "m4b" | "mp4" | "aac" => AudioFormat::Aac,
            "flac" => AudioFormat::Flac,
            "wav" => AudioFormat::Wav,
            "aiff" | "aif" => AudioFormat::Aiff,
            "ogg" | "oga" => AudioFormat::Vorbis,
            _ => AudioFormat::Other,
        }
    }
}

fn unsupported_message(path: &Path) -> String {
    format!("no supported audio stream in {}", path.display())
}

struct Tags {
    title: String,
    artist: String,
    album: String,
    album_artist: String,
    track_number: Option<u32>,
    disc: Option<u32>,
    duration_secs: Option<f64>,
    cover: Option<Cover>,
}

fn read_tags(path: &Path) -> Tags {
    let mut tags = Tags {
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        album_artist: String::new(),
        track_number: None,
        disc: None,
        duration_secs: None,
        cover: None,
    };
    let Ok(probe) = Probe::open(path) else {
        return tags;
    };
    let Ok(tagged) = probe.read() else {
        return tags;
    };
    let properties = tagged.properties();
    let duration = properties.duration();
    if duration.as_secs() > 0 || duration.subsec_nanos() > 0 {
        tags.duration_secs = Some(duration.as_secs_f64());
    }
    if let Some(tag) = tagged.primary_tag() {
        if let Some(title) = tag.title() {
            tags.title = title.into_owned();
        }
        if let Some(artist) = tag.artist() {
            tags.artist = artist.into_owned();
        }
        if let Some(album) = tag.album() {
            tags.album = album.into_owned();
        }
        if let Some(album_artist) = tag.get_string(&ItemKey::AlbumArtist) {
            tags.album_artist = album_artist.to_string();
        }
        tags.track_number = tag.track();
        tags.disc = tag.disk();
        if let Some(picture) = tag.pictures().first() {
            let mime = picture
                .mime_type()
                .map(|mime| mime.to_string())
                .unwrap_or_else(|| "image/jpeg".into());
            tags.cover = Some(Cover {
                mime,
                bytes: picture.data().to_vec(),
            });
        }
    }
    if is_mp4(path) {
        if let Ok(mp4) = Mp4Tag::read_from_path(path) {
            if tags.title.is_empty() {
                if let Some(title) = mp4.title() {
                    tags.title = title.to_string();
                }
            }
            if tags.artist.is_empty() {
                if let Some(artist) = mp4.artist() {
                    tags.artist = artist.to_string();
                }
            }
            if tags.album.is_empty() {
                if let Some(album) = mp4.album() {
                    tags.album = album.to_string();
                }
            }
            if tags.album_artist.is_empty() {
                if let Some(artist) = mp4.album_artist() {
                    tags.album_artist = artist.to_string();
                }
            }
            if tags.cover.is_none() {
                if let Some(art) = mp4.artwork() {
                    let mime = match art.fmt {
                        mp4ameta::ImgFmt::Jpeg => "image/jpeg",
                        mp4ameta::ImgFmt::Png => "image/png",
                        mp4ameta::ImgFmt::Bmp => "image/bmp",
                    };
                    if !art.data.is_empty() {
                        tags.cover = Some(Cover {
                            mime: mime.to_string(),
                            bytes: art.data.to_vec(),
                        });
                    }
                }
            }
        }
    }
    tags
}

fn error_source(path: &Path, message: impl Into<String>) -> Source {
    let message = message.into();
    Source {
        id: SourceId(0),
        path: path.to_path_buf(),
        format: AudioFormat::Other,
        sample_rate: 0,
        channels: 0,
        delay: 0,
        padding: 0,
        presentation_frames: 0,
        artist: String::new(),
        album: String::new(),
        title: file_stem(path).to_string(),
        disc: 0,
        track_number: 0,
        segments: vec![Segment {
            number: 0,
            title: file_stem(path).to_string(),
            artist: String::new(),
            album: String::new(),
            start_frame: 0,
            end_frame: 0,
        }],
        group_title: None,
        error: Some(message),
        warning: None,
        cover: None,
    }
}

fn first_nonempty<'a>(values: impl IntoIterator<Item = &'a str>) -> &'a str {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or("")
}

fn file_stem(path: &Path) -> &str {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Untitled")
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_m3u(path: &Path) -> bool {
    matches!(extension(path).as_str(), "m3u" | "m3u8")
}

fn is_mp4(path: &Path) -> bool {
    matches!(extension(path).as_str(), "m4a" | "m4b" | "mp4" | "m4p")
}

fn normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn examples() -> Option<PathBuf> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        if path.is_dir() {
            Some(path)
        } else {
            None
        }
    }

    #[test]
    fn gapless_mp3_album_has_lame_delay_and_one_row_each() {
        let Some(examples) = examples() else {
            return;
        };
        let dir = examples.join("Gapless MP3 Album");
        if !dir.is_dir() {
            return;
        }
        let sources = load_paths(&[dir]);
        let playable: Vec<_> = sources.iter().filter(|source| source.playable()).collect();
        assert!(playable.len() >= 20, "playable files: {}", playable.len());
        for source in &playable {
            assert_eq!(source.format, AudioFormat::Mp3, "{}", source.path.display());
            assert_eq!(source.segments.len(), 1, "{}", source.path.display());
            assert_eq!(source.delay, 1105, "{}", source.path.display());
            assert!(source.padding > 0, "{}", source.path.display());
            assert!(source.presentation_frames > source.sample_rate as u64);
            assert!(!source.title.is_empty(), "{}", source.path.display());
        }
    }

    #[test]
    fn flac_embedded_cue_becomes_abutting_tracks() {
        let Some(examples) = examples() else {
            return;
        };
        let path = examples.join("FLAC with embedded CUE.flac");
        if !path.is_file() {
            return;
        }
        let source = probe_file(&path);
        assert!(source.error.is_none(), "{:?}", source.error);
        assert!(
            source.segments.len() > 1,
            "expected cue tracks, got {}",
            source.segments.len()
        );
        assert!(source.group_title.is_some());
        for pair in source.segments.windows(2) {
            assert_eq!(pair[0].end_frame, pair[1].start_frame);
            assert!(!pair[0].title.is_empty());
        }
        let last = source.segments.last().unwrap();
        assert_eq!(last.end_frame, source.presentation_frames);
        assert_eq!(source.delay, 0);
    }

    #[test]
    fn m4a_chapters_become_rows_and_priming_is_read() {
        let Some(examples) = examples() else {
            return;
        };
        let path = examples.join("ACC with chapters.m4a");
        if !path.is_file() {
            return;
        }
        let source = probe_file(&path);
        assert!(source.error.is_none(), "{:?}", source.error);
        assert!(
            source.segments.len() >= 15,
            "chapters: {} delay {} padding {} frames {} rate {}",
            source.segments.len(),
            source.delay,
            source.padding,
            source.presentation_frames,
            source.sample_rate
        );
        assert_eq!(source.segments[0].title.to_ascii_lowercase(), "last resort");
        assert!(source.segments[0].start_frame < source.sample_rate as u64);
        for pair in source.segments.windows(2) {
            assert_eq!(pair[0].end_frame, pair[1].start_frame);
        }
        assert_eq!(
            source.segments.last().unwrap().end_frame,
            source.presentation_frames
        );
        assert_eq!(source.delay, 2112, "iTunSMPB priming");
        assert_eq!(source.padding, 0x190);
    }
}
