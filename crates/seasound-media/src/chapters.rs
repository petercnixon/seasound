//! Chapter ranges on the presentation timeline.
//!
//! Chapter times from mp4ameta are durations, so a boundary can be a frame or two
//! away from the sample the author intended. Playback of a chaptered file is still
//! one continuous decode. Only the moment the highlight moves is affected.

use std::time::Duration;

use seasound_model::Segment;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChapterMark {
    pub start: Duration,
    pub title: String,
}

pub fn segments_from_chapters(
    chapters: &[ChapterMark],
    presentation_frames: u64,
    sample_rate: u32,
    artist: &str,
    album: &str,
) -> Vec<Segment> {
    if chapters.is_empty() || presentation_frames == 0 || sample_rate == 0 {
        return Vec::new();
    }
    let mut marks: Vec<(u64, String)> = chapters
        .iter()
        .map(|chapter| (duration_to_frames(chapter.start, sample_rate), chapter.title.clone()))
        .filter(|(start, _)| *start < presentation_frames)
        .collect();
    marks.sort_by_key(|(start, _)| *start);
    marks.dedup_by(|a, b| a.0 == b.0);

    let mut segments = Vec::with_capacity(marks.len());
    for (index, (start, title)) in marks.iter().enumerate() {
        let end = marks
            .get(index + 1)
            .map(|(next, _)| *next)
            .unwrap_or(presentation_frames)
            .min(presentation_frames);
        if end <= *start {
            continue;
        }
        let title = if title.trim().is_empty() {
            format!("Chapter {}", index + 1)
        } else {
            title.clone()
        };
        segments.push(Segment {
            number: (index + 1) as u32,
            title,
            artist: artist.to_string(),
            album: album.to_string(),
            start_frame: *start,
            end_frame: end,
        });
    }
    // A single chapter that covers the whole file is still one row. Zero usable
    // chapters means the caller keeps the file as one row.
    segments
}

fn duration_to_frames(duration: Duration, sample_rate: u32) -> u64 {
    let ticks = duration.as_secs_f64() * f64::from(sample_rate);
    if !ticks.is_finite() || ticks <= 0.0 {
        0
    } else {
        ticks.round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapters_abut_and_the_last_ends_at_the_file() {
        let chapters = vec![
            ChapterMark {
                start: Duration::from_secs(10),
                title: "Second".into(),
            },
            ChapterMark {
                start: Duration::from_millis(20),
                title: "First".into(),
            },
            ChapterMark {
                start: Duration::from_secs(10),
                title: "Duplicate".into(),
            },
            ChapterMark {
                start: Duration::from_secs(40),
                title: "Past the end".into(),
            },
        ];
        let segments = segments_from_chapters(&chapters, 30 * 44100, 44100, "Author", "Book");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].title, "First");
        assert_eq!(segments[0].end_frame, segments[1].start_frame);
        assert_eq!(segments[1].end_frame, 30 * 44100);
        assert_eq!(segments[0].artist, "Author");
    }

    #[test]
    fn empty_and_backwards_marks_produce_no_rows() {
        assert!(segments_from_chapters(&[], 1000, 44100, "", "").is_empty());
        let chapters = vec![ChapterMark {
            start: Duration::from_secs(5),
            title: "Only".into(),
        }];
        assert!(segments_from_chapters(&chapters, 44100, 44100, "", "").is_empty());
    }
}
