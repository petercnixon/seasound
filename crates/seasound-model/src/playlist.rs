//! Playlist order, the transport cursor, and the on-disk playlist file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Error, Source, SourceId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    Off,
    One,
    All,
}

impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Repeat::Off => "Repeat off",
            Repeat::One => "Repeat one",
            Repeat::All => "Repeat all",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

/// One row in playback order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub source_id: SourceId,
    pub segment: usize,
}

#[derive(Clone, Debug)]
pub struct Playlist {
    pub sources: Vec<Source>,
    pub entries: Vec<PlaylistEntry>,
}

impl Playlist {
    pub fn new(mut sources: Vec<Source>) -> Self {
        for source in &mut sources {
            source.id = SourceId::fresh();
        }
        let entries = entries_for(&sources);
        Self { sources, entries }
    }

    /// Add rows without changing the ids already in this playlist.
    pub fn append(&mut self, extra: Playlist) {
        self.sources.extend(extra.sources);
        self.entries.extend(extra.entries);
    }

    pub fn empty() -> Self {
        Self {
            sources: Vec::new(),
            entries: Vec::new(),
        }
    }

    pub fn source(&self, id: SourceId) -> Option<&Source> {
        self.sources.iter().find(|source| source.id == id)
    }

    pub fn rows(&self) -> Vec<RowView> {
        let mut rows = Vec::with_capacity(self.entries.len());
        let mut previous: Option<SourceId> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            let Some(source) = self.source(entry.source_id) else {
                continue;
            };
            let Some(segment) = source.segments.get(entry.segment) else {
                continue;
            };
            let group = if source.group_title.is_some() && previous != Some(source.id) {
                source.group_title.clone()
            } else {
                None
            };
            previous = Some(source.id);
            rows.push(RowView {
                index,
                source_id: source.id,
                segment: entry.segment,
                number: segment.number,
                title: segment.title.clone(),
                artist: segment.artist.clone(),
                album: segment.album.clone(),
                frames: segment.frames(),
                start_frame: segment.start_frame,
                sample_rate: source.sample_rate,
                group,
                error: source.error.clone(),
                path: source.path.clone(),
            });
        }
        rows
    }

    pub fn reorder(&mut self, from: usize, to: usize) {
        if from >= self.entries.len() || to >= self.entries.len() || from == to {
            return;
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
    }

    pub fn remove(&mut self, index: usize) -> Option<PlaylistEntry> {
        if index >= self.entries.len() {
            return None;
        }
        Some(self.entries.remove(index))
    }

    /// Move the chosen rows together, preserving their order, so the block sits in front of
    /// `insert_before`. `insert_before == len` appends. Returns the block's new indices, or
    /// `None` when the order would not change.
    pub fn move_rows(&mut self, indices: &[usize], insert_before: usize) -> Option<Vec<usize>> {
        let order = plan_row_move(self.entries.len(), indices, insert_before)?;
        let chosen = row_membership(self.entries.len(), indices);
        let places = places_of(&order, &chosen);
        let old = std::mem::take(&mut self.entries);
        self.entries = order.into_iter().map(|index| old[index].clone()).collect();
        Some(places)
    }

    /// Where `move_rows` would put the chosen rows. The playlist is left unchanged.
    pub fn moved_places(len: usize, indices: &[usize], insert_before: usize) -> Option<Vec<usize>> {
        let order = plan_row_move(len, indices, insert_before)?;
        Some(places_of(&order, &row_membership(len, indices)))
    }

    pub fn remove_indices(&mut self, indices: &[usize]) {
        let chosen = row_membership(self.entries.len(), indices);
        let mut index = 0;
        self.entries.retain(|_| {
            let drop = chosen.get(index).copied().unwrap_or(false);
            index += 1;
            !drop
        });
    }

    /// Remember rows by path and start frame so a later probe can find them again.
    pub fn to_saved(&self) -> SavedPlaylist {
        let mut rows = Vec::new();
        for entry in &self.entries {
            let Some(source) = self.source(entry.source_id) else {
                continue;
            };
            let Some(segment) = source.segments.get(entry.segment) else {
                continue;
            };
            rows.push(SavedRow {
                path: source.path.clone(),
                start_frame: segment.start_frame,
                title: segment.title.clone(),
            });
        }
        SavedPlaylist { version: 1, rows }
    }
}

fn row_membership(len: usize, indices: &[usize]) -> Vec<bool> {
    let mut chosen = vec![false; len];
    for index in indices {
        if *index < len {
            chosen[*index] = true;
        }
    }
    chosen
}

/// Old indices in the order they appear after the move.
fn plan_row_move(len: usize, indices: &[usize], insert_before: usize) -> Option<Vec<usize>> {
    if len == 0 {
        return None;
    }
    let chosen = row_membership(len, indices);
    if !chosen.iter().any(|flag| *flag) {
        return None;
    }
    let moving: Vec<usize> = (0..len).filter(|index| chosen[*index]).collect();
    let rest: Vec<usize> = (0..len).filter(|index| !chosen[*index]).collect();
    let insert_before = insert_before.min(len);
    let dest = rest.iter().take_while(|index| **index < insert_before).count();
    let mut order = Vec::with_capacity(len);
    order.extend_from_slice(&rest[..dest]);
    order.extend_from_slice(&moving);
    order.extend_from_slice(&rest[dest..]);
    let identity: Vec<usize> = (0..len).collect();
    if order == identity {
        None
    } else {
        Some(order)
    }
}

fn places_of(order: &[usize], chosen: &[bool]) -> Vec<usize> {
    order
        .iter()
        .enumerate()
        .filter_map(|(new_index, old_index)| chosen[*old_index].then_some(new_index))
        .collect()
}

fn entries_for(sources: &[Source]) -> Vec<PlaylistEntry> {
    let mut entries = Vec::new();
    for source in sources {
        if source.segments.is_empty() {
            continue;
        }
        for segment in 0..source.segments.len() {
            entries.push(PlaylistEntry {
                source_id: source.id,
                segment,
            });
        }
    }
    entries
}

/// A row the interface can draw without borrowing the source list.
#[derive(Clone, Debug)]
pub struct RowView {
    pub index: usize,
    pub source_id: SourceId,
    pub segment: usize,
    pub number: u32,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub frames: u64,
    pub start_frame: u64,
    pub sample_rate: u32,
    pub group: Option<String>,
    pub error: Option<String>,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedPlaylist {
    pub version: u32,
    pub rows: Vec<SavedRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedRow {
    pub path: PathBuf,
    pub start_frame: u64,
    pub title: String,
}

impl SavedPlaylist {
    pub fn to_json(&self) -> Result<String, Error> {
        serde_json::to_string_pretty(self).map_err(|err| Error::new(err.to_string()))
    }

    pub fn from_json(text: &str) -> Result<Self, Error> {
        serde_json::from_str(text).map_err(|err| Error::new(err.to_string()))
    }
}

/// Rebuild a playlist from a saved file.
///
/// `probed` must contain one source per distinct path, in any order, with fresh ids.
/// Each saved row picks the segment whose start frame matches. A file that no longer
/// has that range falls back to its first segment.
pub fn load_saved_playlist(saved: &SavedPlaylist, mut probed: Vec<Source>) -> Playlist {
    for source in &mut probed {
        source.id = SourceId::fresh();
    }
    let mut entries = Vec::new();
    for row in &saved.rows {
        let Some(source) = probed.iter().find(|source| source.path == row.path) else {
            continue;
        };
        let segment = source
            .segments
            .iter()
            .position(|segment| segment.start_frame == row.start_frame)
            .unwrap_or(0);
        if source.segments.is_empty() {
            continue;
        }
        entries.push(PlaylistEntry {
            source_id: source.id,
            segment,
        });
    }
    Playlist {
        sources: probed,
        entries,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Move to this row. The engine seeks when the new range is not contiguous.
    To(usize),
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Back {
    Restart,
    To(usize),
    Stop,
}

/// Playback order. Shuffle changes which row comes next and leaves the visible list alone.
#[derive(Clone, Debug)]
pub struct Cursor {
    len: usize,
    index: Option<usize>,
    repeat: Repeat,
    shuffle: bool,
    rng: u64,
    history: Vec<usize>,
    bag: Vec<usize>,
}

impl Cursor {
    pub fn new(len: usize) -> Self {
        Self {
            len,
            index: None,
            repeat: Repeat::Off,
            shuffle: false,
            rng: 0x5EED_5EED,
            history: Vec::new(),
            bag: Vec::new(),
        }
    }

    pub fn index(&self) -> Option<usize> {
        self.index
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn set_len(&mut self, len: usize) {
        let old = self.len;
        self.len = len;
        if let Some(index) = self.index {
            if index >= len {
                self.index = None;
            }
        }
        self.history.retain(|index| *index < len);
        self.bag.retain(|index| *index < len);
        if self.shuffle && len > old {
            self.bag.extend(old..len);
            self.shuffle_bag();
        }
    }

    pub fn set_index(&mut self, index: Option<usize>) {
        if index.is_some_and(|index| index >= self.len) {
            return;
        }
        self.index = index;
        if self.shuffle {
            self.refill_bag();
        }
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    pub fn set_shuffle(&mut self, shuffle: bool) {
        self.shuffle = shuffle;
        self.history.clear();
        if shuffle {
            self.refill_bag();
        } else {
            self.bag.clear();
        }
    }

    /// What to do when the current row finishes or the user presses Next.
    pub fn advance(&mut self) -> Step {
        if self.len == 0 {
            self.index = None;
            return Step::Stop;
        }
        let Some(current) = self.index else {
            let first = self.pick_first();
            self.index = Some(first);
            return Step::To(first);
        };
        if self.repeat == Repeat::One {
            return Step::To(current);
        }
        if self.shuffle {
            self.history.push(current);
            if let Some(next) = self.bag.pop() {
                self.index = Some(next);
                return Step::To(next);
            }
            if self.repeat == Repeat::All {
                self.refill_bag();
                if let Some(next) = self.bag.pop() {
                    self.index = Some(next);
                    return Step::To(next);
                }
            }
            return Step::Stop;
        }
        if current + 1 < self.len {
            let next = current + 1;
            self.history.push(current);
            self.index = Some(next);
            Step::To(next)
        } else if self.repeat == Repeat::All {
            self.history.push(current);
            self.index = Some(0);
            Step::To(0)
        } else {
            Step::Stop
        }
    }

    /// Previous. After three seconds of the current row, players restart it.
    pub fn back(&mut self, elapsed_secs: f64) -> Back {
        if self.len == 0 {
            return Back::Stop;
        }
        let Some(current) = self.index else {
            return Back::Stop;
        };
        if elapsed_secs > 3.0 {
            return Back::Restart;
        }
        if self.shuffle {
            if let Some(prev) = self.history.pop() {
                if prev < self.len {
                    self.bag.push(current);
                    self.index = Some(prev);
                    return Back::To(prev);
                }
            }
            return Back::Restart;
        }
        if current > 0 {
            let prev = current - 1;
            self.index = Some(prev);
            Back::To(prev)
        } else if self.repeat == Repeat::All {
            let prev = self.len - 1;
            self.index = Some(prev);
            Back::To(prev)
        } else {
            Back::Restart
        }
    }

    fn pick_first(&mut self) -> usize {
        if self.shuffle {
            self.refill_bag();
            self.bag.pop().unwrap_or(0)
        } else {
            0
        }
    }

    fn refill_bag(&mut self) {
        self.bag.clear();
        let Some(current) = self.index else {
            self.bag.extend(0..self.len);
            self.shuffle_bag();
            return;
        };
        self.bag.extend((0..self.len).filter(|index| *index != current));
        self.shuffle_bag();
    }

    fn shuffle_bag(&mut self) {
        if self.bag.len() < 2 {
            return;
        }
        for i in (1..self.bag.len()).rev() {
            let j = (self.next_random() as usize) % (i + 1);
            self.bag.swap(i, j);
        }
    }

    fn next_random(&mut self) -> u64 {
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1);
        self.rng
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(len: usize) -> Cursor {
        Cursor::new(len)
    }

    #[test]
    fn next_stops_at_the_end_and_repeat_all_wraps() {
        let mut cursor = cursor(3);
        assert_eq!(cursor.advance(), Step::To(0));
        assert_eq!(cursor.advance(), Step::To(1));
        assert_eq!(cursor.advance(), Step::To(2));
        assert_eq!(cursor.advance(), Step::Stop);

        cursor.set_repeat(Repeat::All);
        assert_eq!(cursor.advance(), Step::To(0));
    }

    #[test]
    fn repeat_one_stays_on_the_row() {
        let mut cursor = cursor(3);
        cursor.set_index(Some(1));
        cursor.set_repeat(Repeat::One);
        assert_eq!(cursor.advance(), Step::To(1));
        assert_eq!(cursor.advance(), Step::To(1));
    }

    #[test]
    fn previous_restarts_after_three_seconds() {
        let mut cursor = cursor(3);
        cursor.set_index(Some(1));
        assert_eq!(cursor.back(3.1), Back::Restart);
        assert_eq!(cursor.back(3.0), Back::To(0));
    }

    #[test]
    fn previous_on_the_first_row_restarts_unless_repeat_all() {
        let mut cursor = cursor(3);
        cursor.set_index(Some(0));
        assert_eq!(cursor.back(0.0), Back::Restart);
        cursor.set_repeat(Repeat::All);
        assert_eq!(cursor.back(0.0), Back::To(2));
    }

    #[test]
    fn shuffle_does_not_repeat_until_the_bag_is_empty() {
        let mut cursor = cursor(4);
        cursor.set_index(Some(0));
        cursor.set_shuffle(true);
        let mut seen = vec![0];
        for _ in 0..3 {
            match cursor.advance() {
                Step::To(index) => seen.push(index),
                Step::Stop => panic!("shuffle ended early"),
            }
        }
        seen.sort();
        assert_eq!(seen, vec![0, 1, 2, 3]);
        assert_eq!(cursor.advance(), Step::Stop);
    }

    #[test]
    fn shuffle_back_walks_history() {
        let mut cursor = cursor(4);
        cursor.set_index(Some(0));
        cursor.set_shuffle(true);
        let Step::To(first) = cursor.advance() else {
            panic!("expected a row");
        };
        assert_eq!(cursor.back(0.2), Back::To(0));
        assert_eq!(cursor.index(), Some(0));
        let _ = first;
    }

    #[test]
    fn reorder_moves_one_entry() {
        let mut playlist = Playlist::empty();
        playlist.entries = vec![
            PlaylistEntry {
                source_id: SourceId(1),
                segment: 0,
            },
            PlaylistEntry {
                source_id: SourceId(1),
                segment: 1,
            },
            PlaylistEntry {
                source_id: SourceId(1),
                segment: 2,
            },
        ];
        playlist.reorder(0, 2);
        let segments: Vec<_> = playlist.entries.iter().map(|entry| entry.segment).collect();
        assert_eq!(segments, vec![1, 2, 0]);
    }

    fn numbered(len: usize) -> Playlist {
        let mut playlist = Playlist::empty();
        playlist.entries = (0..len)
            .map(|segment| PlaylistEntry {
                source_id: SourceId(1),
                segment,
            })
            .collect();
        playlist
    }

    fn segments_of(playlist: &Playlist) -> Vec<usize> {
        playlist.entries.iter().map(|entry| entry.segment).collect()
    }

    #[test]
    fn move_rows_keeps_relative_order_and_skips_a_noop() {
        let mut playlist = numbered(5);
        assert!(playlist.move_rows(&[1, 2], 3).is_none());
        assert!(Playlist::moved_places(5, &[1, 2], 1).is_none());
        let places = playlist.move_rows(&[1, 2], 4).unwrap();
        assert_eq!(segments_of(&playlist), vec![0, 3, 1, 2, 4]);
        assert_eq!(places, vec![2, 3]);
        let places = playlist.move_rows(&[0, 2], 5).unwrap();
        assert_eq!(segments_of(&playlist), vec![3, 2, 4, 0, 1]);
        assert_eq!(places, vec![3, 4]);
        assert!(playlist.move_rows(&[], 0).is_none());
        assert!(playlist.move_rows(&[9], 0).is_none());
    }

    #[test]
    fn remove_indices_drops_every_listed_row() {
        let mut playlist = numbered(5);
        playlist.remove_indices(&[0, 2, 2, 9]);
        assert_eq!(segments_of(&playlist), vec![1, 3, 4]);
        playlist.remove_indices(&[]);
        assert_eq!(segments_of(&playlist), vec![1, 3, 4]);
    }

    #[test]
    fn saved_playlist_matches_start_frames() {
        let saved = SavedPlaylist {
            version: 1,
            rows: vec![
                SavedRow {
                    path: "album.flac".into(),
                    start_frame: 1000,
                    title: "Breathe".into(),
                },
                SavedRow {
                    path: "album.flac".into(),
                    start_frame: 0,
                    title: "Speak to Me".into(),
                },
            ],
        };
        let source = crate::Source {
            id: SourceId(0),
            path: "album.flac".into(),
            format: crate::AudioFormat::Flac,
            sample_rate: 44100,
            channels: 2,
            delay: 0,
            padding: 0,
            presentation_frames: 5000,
            artist: String::new(),
            album: String::new(),
            title: String::new(),
            disc: 1,
            track_number: 1,
            segments: vec![
                crate::Segment {
                    number: 1,
                    title: "Speak to Me".into(),
                    artist: String::new(),
                    album: String::new(),
                    start_frame: 0,
                    end_frame: 1000,
                },
                crate::Segment {
                    number: 2,
                    title: "Breathe".into(),
                    artist: String::new(),
                    album: String::new(),
                    start_frame: 1000,
                    end_frame: 5000,
                },
            ],
            group_title: Some("album".into()),
            error: None,
            warning: None,
            format_label: String::new(),
            cover: None,
        };
        let playlist = load_saved_playlist(&saved, vec![source]);
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[0].segment, 1);
        assert_eq!(playlist.entries[1].segment, 0);
        let rows = playlist.rows();
        assert_eq!(rows[0].group.as_deref(), Some("album"));
        assert!(rows[1].group.is_none());
    }

    #[test]
    fn each_playlist_gets_new_source_ids() {
        let first = Playlist::new(vec![bare("a.flac")]);
        let second = Playlist::new(vec![bare("b.mp3")]);
        assert_ne!(first.sources[0].id, second.sources[0].id);
        let mut combined = first;
        let extra_rows = second.rows().len();
        combined.append(second);
        assert_eq!(combined.rows().len(), 1 + extra_rows);
        assert_ne!(combined.sources[0].id, combined.sources[1].id);
    }

    fn bare(path: &str) -> crate::Source {
        crate::Source {
            id: SourceId(0),
            path: path.into(),
            format: crate::AudioFormat::Flac,
            sample_rate: 44100,
            channels: 2,
            delay: 0,
            padding: 0,
            presentation_frames: 1000,
            artist: String::new(),
            album: String::new(),
            title: path.into(),
            disc: 1,
            track_number: 1,
            segments: vec![crate::Segment {
                number: 1,
                title: path.into(),
                artist: String::new(),
                album: String::new(),
                start_frame: 0,
                end_frame: 1000,
            }],
            group_title: None,
            error: None,
            warning: None,
            format_label: String::new(),
            cover: None,
        }
    }
}
