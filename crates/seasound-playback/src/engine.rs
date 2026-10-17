//! Commands from the window, and the thread that keeps the output ring full.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use seasound_model::{Error, PlayState, Playlist, Repeat, RowView, Source, SourceId, PcmSource};

use crate::core::{PlayerCore, SegmentJob};
use crate::device::{AudioOut, MemoryOutput};

#[cfg(feature = "cpal-output")]
use crate::device::CpalOutput;

pub type Opener = Arc<dyn Fn(&Source) -> Result<Box<dyn PcmSource>, Error> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub state: PlayState,
    pub rows: Arc<Vec<RowView>>,
    pub index: Option<usize>,
    pub position_frames: u64,
    pub segment_frames: u64,
    pub sample_rate: u32,
    pub volume: f32,
    pub muted: bool,
    pub repeat: Repeat,
    pub shuffle: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub underruns: u64,
    pub error: Option<String>,
    pub device: String,
    pub devices: Arc<Vec<String>>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            state: PlayState::Stopped,
            rows: Arc::new(Vec::new()),
            index: None,
            position_frames: 0,
            segment_frames: 0,
            sample_rate: 44100,
            volume: 0.8,
            muted: false,
            repeat: Repeat::Off,
            shuffle: false,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            underruns: 0,
            error: None,
            device: String::new(),
            devices: Arc::new(Vec::new()),
        }
    }
}

/// A row to start, and how far into that row, from 0 to just under 1.
#[derive(Clone, Copy, Debug)]
pub struct StartAt {
    pub index: usize,
    pub fraction: f32,
}

pub enum Command {
    /// Replace the playlist. `start` plays that row from a fraction of its length.
    /// With no `start`, `autoplay` starts the first playable row.
    SetPlaylist {
        playlist: Playlist,
        autoplay: bool,
        start: Option<StartAt>,
    },
    /// Add rows. Playback of the current file continues.
    Append(Playlist),
    Clear,
    PlayIndex(usize),
    TogglePause,
    Stop,
    Next,
    Previous,
    SeekFraction(f32),
    SetVolume(f32),
    SetMuted(bool),
    SetRepeat(Repeat),
    SetShuffle(bool),
    Reorder { from: usize, to: usize },
    /// Move these rows, as a block, to sit in front of `insert_before`.
    MoveRows { indices: Vec<usize>, insert_before: usize },
    Remove(usize),
    /// Remove every listed row. Deleting the current row starts the next one that remains.
    RemoveMany(Vec<usize>),
    SetDevice(Option<String>),
    Shutdown,
}

struct Span {
    source_id: SourceId,
    segment: usize,
    out_start: u64,
    out_frames: u64,
    source_start: u64,
    source_frames: u64,
}

struct Inflight {
    id: SourceId,
    rx: Receiver<Result<Box<dyn PcmSource>, Error>>,
}

pub struct Engine {
    commands: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    #[cfg(feature = "cpal-output")]
    pub fn start(opener: Opener) -> Self {
        let (tx, rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let published = Arc::clone(&snapshot);
        let thread = thread::Builder::new()
            .name("seasound-playback".into())
            .spawn(move || {
                let output = Box::new(CpalOutput::new());
                playback_thread(rx, published, opener, output);
            })
            .expect("playback thread");
        Self {
            commands: tx,
            snapshot,
            thread: Some(thread),
        }
    }

    pub fn start_with(opener: Opener, output: Box<dyn AudioOut + Send>) -> Self {
        let (tx, rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let published = Arc::clone(&snapshot);
        let thread = thread::Builder::new()
            .name("seasound-playback".into())
            .spawn(move || playback_thread(rx, published, opener, output))
            .expect("playback thread");
        Self {
            commands: tx,
            snapshot,
            thread: Some(thread),
        }
    }

    pub fn command(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().map(|guard| guard.clone()).unwrap_or_default()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn playback_thread(
    rx: Receiver<Command>,
    published: Arc<Mutex<Snapshot>>,
    opener: Opener,
    mut output: Box<dyn AudioOut>,
) {
    let mut deck = Deck {
        playlist: Playlist::empty(),
        cursor: seasound_model::Cursor::new(0),
        core: PlayerCore::new(output.sample_rate().max(1)),
        opener,
        volume: 0.8,
        muted: false,
        state: PlayState::Stopped,
        error: None,
        spans: Vec::new(),
        queued: 0,
        origin: output.played(),
        rows: Arc::new(Vec::new()),
        inflight: None,
        display_index: None,
        seek_frame: None,
        device_played: output.played(),
    };
    output.set_gain(deck.gain());
    let mut buffer = vec![0f32; 2048 * 2];
    loop {
        deck.device_played = output.played();
        let command = if deck.state == PlayState::Playing {
            match rx.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        if let Some(command) = command {
            if handle(&mut deck, &mut *output, command) {
                break;
            }
            while let Ok(command) = rx.try_recv() {
                if handle(&mut deck, &mut *output, command) {
                    publish(&deck, &*output, &published);
                    return;
                }
            }
        }
        if deck.state == PlayState::Playing {
            let free = output.free_frames();
            if free < 256 {
                thread::sleep(Duration::from_millis(2));
            } else {
                let frames = free.min(1024);
                let samples = frames * 2;
                if buffer.len() < samples {
                    buffer.resize(samples, 0.0);
                }
                match deck.fill(&mut buffer[..samples]) {
                    Ok(Fill { frames: 0, waiting: true }) => thread::sleep(Duration::from_millis(2)),
                    Ok(Fill { frames: 0, waiting: false }) => deck.state = PlayState::Stopped,
                    Ok(Fill { frames, .. }) => {
                        let accepted = output.push(&buffer[..frames * 2]);
                        if accepted > 0 {
                            deck.note_written(accepted as u64);
                        }
                    }
                    Err(err) => {
                        deck.error = Some(err.message().to_string());
                        deck.state = PlayState::Stopped;
                    }
                }
            }
        }
        publish(&deck, &*output, &published);
    }
}

fn handle(deck: &mut Deck, output: &mut dyn AudioOut, command: Command) -> bool {
    match command {
        Command::Shutdown => return true,
        Command::SetPlaylist {
            playlist,
            autoplay,
            start,
        } => {
            deck.install(playlist);
            deck.halt(output);
            if let Some(start) = start {
                if deck.playable(start.index) {
                    deck.seek_fraction(start.index, start.fraction, output);
                } else if autoplay {
                    if let Some(index) = deck.first_playable() {
                        deck.begin(index, output);
                    }
                }
            } else if autoplay {
                if let Some(index) = deck.first_playable() {
                    deck.begin(index, output);
                }
            }
        }
        Command::Append(extra) => {
            let was_empty = deck.playlist.entries.is_empty();
            deck.playlist.append(extra);
            deck.cursor.set_len(deck.playlist.entries.len());
            deck.refresh_rows();
            if was_empty {
                if let Some(index) = deck.first_playable() {
                    deck.begin(index, output);
                }
            }
        }
        Command::Clear => {
            deck.install(Playlist::empty());
            deck.halt(output);
        }
        Command::PlayIndex(index) => {
            if deck.playable(index) {
                deck.begin(index, output);
            }
        }
        Command::TogglePause => match deck.state {
            PlayState::Playing => {
                deck.state = PlayState::Paused;
                output.set_paused(true);
            }
            PlayState::Paused => {
                deck.state = PlayState::Playing;
                output.set_paused(false);
            }
            PlayState::Stopped => {
                let index = deck.cursor.index().or_else(|| deck.first_playable());
                if let Some(index) = index {
                    deck.begin(index, output);
                }
            }
        },
        Command::Stop => {
            deck.state = PlayState::Stopped;
            output.set_paused(false);
            output.clear();
            deck.core.discard_output();
            deck.spans.clear();
            deck.queued = 0;
            deck.origin = output.played();
        }
        Command::Next => {
            if let Some(index) = deck.after_current() {
                deck.begin(index, output);
            } else {
                deck.state = PlayState::Stopped;
            }
        }
        Command::Previous => {
            let elapsed = deck.elapsed_secs();
            let restart_index = deck.cursor.index();
            match deck.cursor.back(elapsed) {
                seasound_model::Back::Restart => {
                    if let Some(index) = restart_index {
                        deck.begin(index, output);
                    }
                }
                seasound_model::Back::To(index) => deck.begin(index, output),
                seasound_model::Back::Stop => deck.state = PlayState::Stopped,
            }
        }
        Command::SeekFraction(fraction) => {
            if let Some(index) = deck.audible_index().or(deck.cursor.index()) {
                deck.seek_fraction(index, fraction, output);
            }
        }
        Command::SetVolume(volume) => {
            deck.volume = volume.clamp(0.0, 1.0);
            output.set_gain(deck.gain());
        }
        Command::SetMuted(muted) => {
            deck.muted = muted;
            output.set_gain(deck.gain());
        }
        Command::SetRepeat(repeat) => deck.cursor.set_repeat(repeat),
        Command::SetShuffle(shuffle) => deck.cursor.set_shuffle(shuffle),
        Command::Reorder { from, to } => {
            let current = deck.current_entry();
            deck.playlist.reorder(from, to);
            deck.cursor.set_len(deck.playlist.entries.len());
            if let Some(entry) = current {
                if let Some(index) = deck.playlist.entries.iter().position(|item| item == &entry) {
                    deck.cursor.set_index(Some(index));
                }
            }
            deck.refresh_rows();
        }
        Command::MoveRows { indices, insert_before } => {
            let current = deck.current_entry();
            if deck.playlist.move_rows(&indices, insert_before).is_some() {
                deck.cursor.set_len(deck.playlist.entries.len());
                if let Some(entry) = current {
                    if let Some(index) = deck.playlist.entries.iter().position(|item| item == &entry) {
                        deck.cursor.set_index(Some(index));
                        deck.display_index = Some(index);
                    }
                }
                deck.refresh_rows();
            }
        }
        Command::Remove(index) => {
            let current = deck.current_entry();
            let removed_current = deck.cursor.index() == Some(index);
            deck.playlist.remove(index);
            deck.cursor.set_len(deck.playlist.entries.len());
            deck.refresh_rows();
            if removed_current {
                deck.core.discard_output();
                output.clear();
                if let Some(entry) = deck.playlist.entries.get(index).cloned() {
                    if let Some(new_index) = deck.playlist.entries.iter().position(|item| item == &entry)
                    {
                        deck.begin(new_index, output);
                    }
                } else if let Some(index) = deck.first_playable() {
                    deck.begin(index, output);
                } else {
                    deck.state = PlayState::Stopped;
                }
            } else if let Some(entry) = current {
                if let Some(index) = deck.playlist.entries.iter().position(|item| item == &entry) {
                    deck.cursor.set_index(Some(index));
                }
            }
        }
        Command::RemoveMany(indices) => {
            let current = deck.current_entry();
            let len = deck.playlist.entries.len();
            let mut chosen = vec![false; len];
            for index in &indices {
                if *index < len {
                    chosen[*index] = true;
                }
            }
            let cursor_index = deck.cursor.index();
            let removed_current = match cursor_index {
                Some(index) => chosen.get(index).copied().unwrap_or(false),
                None => false,
            };
            let next = if removed_current {
                cursor_index.and_then(|current_index| {
                    let mut candidate = current_index + 1;
                    while candidate < len {
                        if !chosen[candidate] {
                            let removed_before = chosen[..candidate].iter().filter(|flag| **flag).count();
                            return Some(candidate - removed_before);
                        }
                        candidate += 1;
                    }
                    None
                })
            } else {
                None
            };
            deck.playlist.remove_indices(&indices);
            deck.cursor.set_len(deck.playlist.entries.len());
            deck.refresh_rows();
            if removed_current {
                if let Some(index) = next.filter(|index| deck.playable(*index)) {
                    deck.begin(index, output);
                } else if let Some(index) = deck.first_playable() {
                    deck.begin(index, output);
                } else {
                    deck.cursor.set_index(None);
                    deck.halt(output);
                }
            } else if let Some(entry) = current {
                if let Some(index) = deck.playlist.entries.iter().position(|item| item == &entry) {
                    deck.cursor.set_index(Some(index));
                    deck.display_index = Some(index);
                }
            }
        }
        Command::SetDevice(name) => {
            if let Err(err) = output.set_device(name.as_deref()) {
                deck.error = Some(err.message().to_string());
            } else {
                let rate = output.sample_rate().max(1);
                if rate != deck.core.output_rate() {
                    deck.core.set_output_rate(rate);
                }
                output.set_gain(deck.gain());
                output.set_paused(deck.state != PlayState::Playing);
            }
        }
    }
    deck.refresh_prefetch();
    false
}

struct Fill {
    frames: usize,
    waiting: bool,
}

enum Opened {
    Ready(Box<dyn PcmSource>),
    Later,
}

struct Deck {
    playlist: Playlist,
    cursor: seasound_model::Cursor,
    core: PlayerCore,
    opener: Opener,
    volume: f32,
    muted: bool,
    state: PlayState,
    error: Option<String>,
    spans: Vec<Span>,
    queued: u64,
    origin: u64,
    rows: Arc<Vec<RowView>>,
    inflight: Option<Inflight>,
    display_index: Option<usize>,
    seek_frame: Option<u64>,
    device_played: u64,
}

impl Deck {
    fn gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume * self.volume
        }
    }

    fn install(&mut self, playlist: Playlist) {
        self.playlist = playlist;
        self.cursor = seasound_model::Cursor::new(self.playlist.entries.len());
        self.cursor.set_repeat(Repeat::Off);
        self.refresh_rows();
        self.error = None;
        self.inflight = None;
    }

    /// Stop the audible stream and forget the open decoder.
    fn halt(&mut self, output: &mut dyn AudioOut) {
        self.state = PlayState::Stopped;
        self.core.release();
        self.core.discard_output();
        output.clear();
        output.set_paused(false);
        self.spans.clear();
        self.queued = 0;
        self.origin = output.played();
        self.seek_frame = None;
        self.display_index = None;
        self.inflight = None;
    }

    fn refresh_rows(&mut self) {
        self.rows = Arc::new(self.playlist.rows());
    }

    fn playable(&self, index: usize) -> bool {
        let Some(entry) = self.playlist.entries.get(index) else {
            return false;
        };
        let Some(source) = self.playlist.source(entry.source_id) else {
            return false;
        };
        source.playable() && source.segments.get(entry.segment).is_some()
    }

    fn first_playable(&self) -> Option<usize> {
        (0..self.playlist.entries.len()).find(|index| self.playable(*index))
    }

    fn current_entry(&self) -> Option<seasound_model::PlaylistEntry> {
        self.cursor
            .index()
            .and_then(|index| self.playlist.entries.get(index).cloned())
    }

    fn begin(&mut self, index: usize, output: &mut dyn AudioOut) {
        self.cursor.set_index(Some(index));
        self.display_index = Some(index);
        self.state = PlayState::Playing;
        output.set_paused(false);
        output.clear();
        self.core.discard_output();
        self.core.force_seek();
        self.spans.clear();
        self.queued = 0;
        self.origin = output.played();
        self.seek_frame = None;
        self.error = None;
        self.consider_prefetch();
    }

    fn seek_fraction(&mut self, index: usize, fraction: f32, output: &mut dyn AudioOut) {
        self.cursor.set_index(Some(index));
        self.display_index = Some(index);
        self.state = PlayState::Playing;
        output.set_paused(false);
        output.clear();
        self.core.discard_output();
        self.core.force_seek();
        self.spans.clear();
        self.queued = 0;
        self.origin = output.played();
        // The fraction shifts the segment job. Leave it set until a fill has a decoder.
        // A same-file cue prefetch makes the first open come back later; taking the
        // frame on that attempt would start at the cue point.
        self.seek_frame = Some(self.frame_for(index, fraction));
    }

    fn frame_for(&self, index: usize, fraction: f32) -> u64 {
        let Some(segment) = self.segment(index) else {
            return 0;
        };
        let fraction = fraction.clamp(0.0, 0.999);
        let offset = (segment.frames() as f64 * f64::from(fraction)) as u64;
        segment.start_frame + offset.min(segment.frames().saturating_sub(1))
    }

    fn fill(&mut self, out: &mut [f32]) -> Result<Fill, Error> {
        let mut written = 0usize;
        let capacity = out.len() / 2;
        let mut spins = 0;
        while written < capacity && spins < 8 {
            spins += 1;
            let Some(index) = self.cursor.index() else {
                break;
            };
            if !self.playable(index) {
                if self.advance_from(index).is_none() {
                    break;
                }
                continue;
            }
            let job = self.job(index)?;
            if !self.ensure(job.source_id)? {
                return Ok(Fill {
                    frames: written,
                    waiting: true,
                });
            }
            // `job` already captured `seek_frame`. Drop it only now, so a fill that
            // had to wait for the decoder builds the same job again.
            if self.cursor.index() == Some(index) {
                self.seek_frame = None;
            }
            let flush = !self.continues_after(index);
            let pulled = self.core.pull(job, &mut out[written * 2..], flush)?;
            if pulled.frames > 0 {
                self.spans.push(Span {
                    source_id: job.source_id,
                    segment: self.playlist.entries.get(index).map(|entry| entry.segment).unwrap_or(0),
                    out_start: self.queued + written as u64,
                    out_frames: pulled.frames as u64,
                    source_start: pulled.source_start,
                    source_frames: pulled.source_frames.max(1),
                });
                written += pulled.frames;
            }
            if pulled.pending {
                return Ok(Fill {
                    frames: written,
                    waiting: true,
                });
            }
            if pulled.frames == 0 {
                if self.core.job_done(job) {
                    if self.advance_from(index).is_none() {
                        break;
                    }
                    continue;
                }
                break;
            }
            if self.core.job_done(job) && self.advance_from(index).is_none() {
                break;
            }
        }
        Ok(Fill {
            frames: written,
            waiting: false,
        })
    }

    fn job(&mut self, index: usize) -> Result<SegmentJob, Error> {
        let entry = self
            .playlist
            .entries
            .get(index)
            .cloned()
            .ok_or_else(|| Error::new("that row is gone"))?;
        let source = self
            .playlist
            .source(entry.source_id)
            .ok_or_else(|| Error::new("that file is gone"))?;
        let segment = source
            .segments
            .get(entry.segment)
            .ok_or_else(|| Error::new("that chapter is gone"))?;
        let mut start = segment.start_frame;
        let end = segment.end_frame;
        if self.cursor.index() == Some(index) {
            if let Some(frame) = self.seek_frame {
                if frame >= start && frame < end {
                    start = frame;
                }
            }
        }
        Ok(SegmentJob {
            source_id: entry.source_id,
            start,
            end,
        })
    }

    fn segment(&self, index: usize) -> Option<&seasound_model::Segment> {
        let entry = self.playlist.entries.get(index)?;
        let source = self.playlist.source(entry.source_id)?;
        source.segments.get(entry.segment)
    }

    /// `false` means the decoder is still opening. The caller should wait and try again.
    fn ensure(&mut self, id: SourceId) -> Result<bool, Error> {
        if self.core.has_source(id) {
            return Ok(true);
        }
        match self.open_for(id)? {
            Opened::Later => Ok(false),
            Opened::Ready(source) => {
                self.core.store_prefetch(id, source);
                self.refresh_prefetch();
                Ok(true)
            }
        }
    }

    fn open_for(&mut self, id: SourceId) -> Result<Opened, Error> {
        if self.inflight.as_ref().is_some_and(|inflight| inflight.id == id) {
            let inflight = self.inflight.take().expect("checked");
            match inflight.rx.try_recv() {
                Ok(opened) => return opened.map(Opened::Ready),
                Err(mpsc::TryRecvError::Empty) => {
                    self.inflight = Some(inflight);
                    return Ok(Opened::Later);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Error::new("the next file failed to open"));
                }
            }
        }
        let source = self
            .playlist
            .source(id)
            .cloned()
            .ok_or_else(|| Error::new("that file is no longer in the playlist"))?;
        let opened = (self.opener)(&source)?;
        Ok(Opened::Ready(crate::ahead::Ahead::boxed(opened)))
    }

    fn upcoming_source(&self) -> Option<SourceId> {
        let index = self.cursor.index()?;
        let mut peek = self.cursor.clone();
        let seasound_model::Step::To(next) = peek.advance() else {
            return None;
        };
        if next == index || !self.playable(next) {
            return None;
        }
        self.playlist.entries.get(next).map(|entry| entry.source_id)
    }

    /// The prefetched decoder is the file we just moved to, and it is not in the core yet.
    fn inflight_is_current(&self) -> bool {
        let Some(inflight) = self.inflight.as_ref() else {
            return false;
        };
        let Some(index) = self.cursor.index() else {
            return false;
        };
        let Some(entry) = self.playlist.entries.get(index) else {
            return false;
        };
        inflight.id == entry.source_id && !self.core.has_source(entry.source_id)
    }

    /// Open the file that will play next, and drop a prefetch that is no longer that file.
    ///
    /// The open runs on a worker and decodes several seconds before the splice.
    /// A prefetch for the file we are about to adopt is left alone.
    fn refresh_prefetch(&mut self) {
        if self.state != PlayState::Playing {
            return;
        }
        let next = self.upcoming_source();
        if let Some(id) = next {
            if self.core.has_source(id) {
                let stale = self
                    .inflight
                    .as_ref()
                    .is_some_and(|inflight| inflight.id != id);
                if stale && !self.inflight_is_current() {
                    self.inflight = None;
                }
                return;
            }
            if self.inflight.as_ref().is_some_and(|inflight| inflight.id == id) {
                return;
            }
        }
        if self.inflight_is_current() {
            return;
        }
        self.inflight = None;
        let Some(id) = next else {
            return;
        };
        let Some(source) = self.playlist.source(id).cloned() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let opener = Arc::clone(&self.opener);
        thread::spawn(move || {
            let opened = opener(&source).map(crate::ahead::Ahead::boxed);
            let _ = tx.send(opened);
        });
        self.inflight = Some(Inflight { id, rx });
    }

    fn consider_prefetch(&mut self) {
        self.refresh_prefetch();
    }

    fn continues_after(&self, index: usize) -> bool {
        let mut peek = self.cursor.clone();
        let seasound_model::Step::To(next) = peek.advance() else {
            return false;
        };
        if next == index {
            return false;
        }
        let (Some(current), Some(upcoming)) = (self.segment(index), self.segment(next)) else {
            return false;
        };
        let Some(current_entry) = self.playlist.entries.get(index) else {
            return false;
        };
        let Some(next_entry) = self.playlist.entries.get(next) else {
            return false;
        };
        let Some(current_source) = self.playlist.source(current_entry.source_id) else {
            return false;
        };
        let Some(next_source) = self.playlist.source(next_entry.source_id) else {
            return false;
        };
        if current_source.sample_rate != next_source.sample_rate {
            return false;
        }
        current_entry.source_id == next_entry.source_id && current.end_frame == upcoming.start_frame
            || current_entry.source_id != next_entry.source_id
    }

    fn advance_from(&mut self, current: usize) -> Option<usize> {
        let len = self.playlist.entries.len().max(1);
        for _ in 0..len + 1 {
            match self.cursor.advance() {
                seasound_model::Step::Stop => return None,
                seasound_model::Step::To(index) => {
                    if index != current && self.playable(index) {
                        self.display_index = Some(index);
                        self.consider_prefetch();
                        return Some(index);
                    }
                    if index == current && self.cursor.repeat() == Repeat::One && self.playable(index) {
                        self.core.force_seek();
                        self.display_index = Some(index);
                        return Some(index);
                    }
                }
            }
        }
        None
    }

    fn after_current(&mut self) -> Option<usize> {
        let current = self.cursor.index()?;
        self.advance_from(current)
    }

    fn note_written(&mut self, frames: u64) {
        self.queued += frames;
        // `queued` runs ahead of the speakers by the whole output ring. Spans
        // have to stay until that audio has actually been played, or the clock
        // loses the span under the playhead and the seek bar sticks at zero.
        let heard = self.device_played.saturating_sub(self.origin);
        self.spans
            .retain(|span| span.out_start + span.out_frames + 2048 > heard);
    }

    fn audible_index(&self) -> Option<usize> {
        self.locate().map(|(index, _, _, _)| index).or(self.display_index)
    }

    fn elapsed_secs(&self) -> f64 {
        let (_, position, _, rate) = self
            .locate()
            .or_else(|| self.heard_from_decoder())
            .unwrap_or((0, 0, 0, self.core.output_rate()));
        if rate == 0 {
            0.0
        } else {
            position as f64 / f64::from(rate)
        }
    }

    fn locate(&self) -> Option<(usize, u64, u64, u32)> {
        let relative = self.device_played.saturating_sub(self.origin);
        let span = self
            .spans
            .iter()
            .rev()
            .find(|span| {
                relative >= span.out_start && relative < span.out_start + span.out_frames.max(1)
            })
            .or_else(|| {
                self.spans
                    .iter()
                    .rev()
                    .find(|span| relative >= span.out_start)
            })?;
        let into = (relative - span.out_start).min(span.out_frames.saturating_sub(1));
        let source_into = if span.out_frames == 0 {
            0
        } else {
            span.source_frames.saturating_mul(into) / span.out_frames
        };
        let source_frame = span.source_start + source_into;
        let index = self.playlist.entries.iter().position(|entry| {
            entry.source_id == span.source_id && entry.segment == span.segment
        })?;
        let segment = self.segment(index)?;
        let rate = self
            .playlist
            .source(span.source_id)
            .map(|source| source.sample_rate)
            .unwrap_or(44100);
        Some((
            index,
            source_frame.saturating_sub(segment.start_frame),
            segment.frames(),
            rate,
        ))
    }

    /// Decoder position, pulled back by the audio still sitting in the output ring.
    fn heard_from_decoder(&self) -> Option<(usize, u64, u64, u32)> {
        let index = self.display_index.or_else(|| self.cursor.index())?;
        let segment = self.segment(index)?;
        let entry = self.playlist.entries.get(index)?;
        let source = self.playlist.source(entry.source_id)?;
        let rate = source.sample_rate.max(1);
        let absolute = self.core.position().unwrap_or(segment.start_frame);
        let heard_out = self.device_played.saturating_sub(self.origin);
        let waiting_out = self.queued.saturating_sub(heard_out);
        let waiting_src = waiting_out.saturating_mul(u64::from(rate))
            / u64::from(self.core.output_rate().max(1));
        let heard = absolute.saturating_sub(waiting_src);
        let frames = segment.frames();
        let position = heard
            .saturating_sub(segment.start_frame)
            .min(frames.saturating_sub(1));
        Some((index, position, frames, rate))
    }
}

fn publish(deck: &Deck, output: &dyn AudioOut, published: &Mutex<Snapshot>) {
    let located = deck.locate().or_else(|| deck.heard_from_decoder());
    let index = located
        .map(|(index, _, _, _)| index)
        .or(deck.display_index)
        .or(deck.cursor.index())
        .filter(|index| *index < deck.rows.len());
    let (position, segment_frames, rate) = located
        .map(|(_, position, frames, rate)| (position, frames, rate))
        .unwrap_or_else(|| {
            let frames = index
                .and_then(|index| deck.segment(index))
                .map(|segment| segment.frames())
                .unwrap_or(0);
            (0, frames, deck.core.output_rate())
        });
    let row = index.and_then(|index| deck.rows.get(index));
    let snapshot = Snapshot {
        state: deck.state,
        rows: Arc::clone(&deck.rows),
        index,
        position_frames: position,
        segment_frames: if segment_frames == 0 { row.map(|row| row.frames).unwrap_or(0) } else { segment_frames },
        sample_rate: rate,
        volume: deck.volume,
        muted: deck.muted,
        repeat: deck.cursor.repeat(),
        shuffle: deck.cursor.shuffle(),
        title: row.map(|row| row.title.clone()).unwrap_or_default(),
        artist: row.map(|row| row.artist.clone()).unwrap_or_default(),
        album: row.map(|row| row.album.clone()).unwrap_or_default(),
        underruns: output.underruns(),
        error: deck.error.clone().or_else(|| output.last_error()),
        device: output.device_name(),
        devices: Arc::new(output.devices()),
    };
    if let Ok(mut slot) = published.lock() {
        *slot = snapshot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seasound_model::{AudioFormat, ChannelMap, Playlist, Segment, Source, SourceId};

    struct Ramp {
        len: u64,
        pos: u64,
    }

    impl PcmSource for Ramp {
        fn sample_rate(&self) -> u32 {
            44100
        }
        fn channels(&self) -> u16 {
            2
        }
        fn channel_map(&self) -> ChannelMap {
            ChannelMap::Stereo
        }
        fn presentation_frames(&self) -> u64 {
            self.len
        }
        fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
            let frames = interleaved.len() / 2;
            let n = frames.min(self.len.saturating_sub(self.pos) as usize);
            for index in 0..n {
                let value = (self.pos + index as u64) as f32;
                interleaved[index * 2] = value;
                interleaved[index * 2 + 1] = value;
            }
            self.pos += n as u64;
            Ok(n)
        }
        fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
            self.pos = frame.min(self.len);
            Ok(())
        }
    }

    fn source(path: &str, frames: u64) -> Source {
        Source {
            id: SourceId(0),
            path: path.into(),
            format: AudioFormat::Wav,
            sample_rate: 44100,
            channels: 2,
            bitrate_bps: 0,
            bits_per_sample: 16,
            delay: 0,
            padding: 0,
            presentation_frames: frames,
            artist: "Artist".into(),
            album: "Album".into(),
            title: path.into(),
            disc: 1,
            track_number: 1,
            segments: vec![Segment {
                number: 1,
                title: path.into(),
                artist: "Artist".into(),
                album: "Album".into(),
                start_frame: 0,
                end_frame: frames,
            }],
            group_title: None,
            error: None,
            warning: None,
            format_label: String::new(),
            cover: None,
        }
    }

    fn cue_like(path: &str, ranges: &[(u64, u64)]) -> Source {
        let end = ranges.last().map(|(_, end)| *end).unwrap_or(0);
        let mut source = source(path, end);
        source.segments = ranges
            .iter()
            .enumerate()
            .map(|(index, (start, end))| Segment {
                number: (index + 1) as u32,
                title: format!("Track {}", index + 1),
                artist: "Artist".into(),
                album: "Album".into(),
                start_frame: *start,
                end_frame: *end,
            })
            .collect();
        source.presentation_frames = end;
        source
    }

    #[test]
    fn a_cue_track_can_start_in_the_middle_of_its_range() {
        let memory = MemoryOutput::new(44100);
        let recorded = memory.samples_handle();
        let opener: Opener = Arc::new(|source| {
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let engine = Engine::start_with(opener, Box::new(memory));
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![cue_like(
                "album.flac",
                &[(0, 100_000), (100_000, 200_000)],
            )]),
            autoplay: false,
            start: Some(StartAt {
                index: 1,
                fraction: 0.5,
            }),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let ready = recorded.lock().unwrap().len() >= 2;
            if ready {
                break;
            }
            if std::time::Instant::now() > deadline {
                let snapshot = engine.snapshot();
                panic!(
                    "cue track produced no audio: {:?} {}",
                    snapshot.state,
                    snapshot.error.unwrap_or_default()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        let first = recorded.lock().unwrap()[0];
        assert!(
            (first - 150_000.0).abs() < 2.0,
            "started at {first}, expected the middle of the second cue track"
        );
        assert_eq!(engine.snapshot().title, "Track 2");
    }

    /// The row after the resumed cue track is the same file, so opening it is the
    /// prefetch already in flight. The start position has to survive that wait.
    #[test]
    fn a_cue_seek_survives_the_same_file_prefetch() {
        struct Gate {
            entered: bool,
            release: bool,
        }
        let gate = Arc::new((Mutex::new(Gate { entered: false, release: false }), std::sync::Condvar::new()));
        let entered = Arc::clone(&gate);
        let opener: Opener = Arc::new(move |source| {
            let (lock, cv) = &*entered;
            let mut guard = lock.lock().unwrap();
            if !guard.entered {
                guard.entered = true;
                cv.notify_all();
                while !guard.release {
                    guard = cv.wait(guard).unwrap();
                }
            }
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let memory = MemoryOutput::new(44100);
        let recorded = memory.samples_handle();
        let engine = Engine::start_with(opener, Box::new(memory));
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![cue_like(
                "album.flac",
                &[(0, 100_000), (100_000, 200_000), (200_000, 300_000)],
            )]),
            autoplay: false,
            start: Some(StartAt {
                index: 1,
                fraction: 0.5,
            }),
        });
        {
            let (lock, cv) = &*gate;
            let mut guard = lock.lock().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !guard.entered {
                let now = std::time::Instant::now();
                if now > deadline {
                    panic!("prefetch never opened the cue file");
                }
                let (next, _) = cv.wait_timeout(guard, deadline - now).unwrap();
                guard = next;
            }
            // The playback thread needs a turn to notice the open is still in flight.
            drop(guard);
            thread::sleep(Duration::from_millis(50));
            let mut guard = lock.lock().unwrap();
            guard.release = true;
            cv.notify_all();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if recorded.lock().unwrap().len() >= 2 {
                break;
            }
            if std::time::Instant::now() > deadline {
                let snapshot = engine.snapshot();
                panic!(
                    "cue track produced no audio: {:?} {}",
                    snapshot.state,
                    snapshot.error.unwrap_or_default()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        let first = recorded.lock().unwrap()[0];
        assert!(
            (first - 150_000.0).abs() < 2.0,
            "started at {first}, expected the middle of the second cue track"
        );
    }

    #[test]
    fn a_real_cue_flac_does_not_resume_at_the_start_of_the_track() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/FLAC with embedded CUE.flac");
        if !path.is_file() {
            return;
        }
        let probed = seasound_media::probe_file(&path);
        assert!(
            probed.segments.len() > 2,
            "need a following cue row, got {}",
            probed.segments.len()
        );
        let opener: Opener = Arc::new(|source| {
            seasound_media::open_decoder(source.path.as_path(), seasound_model::DecodeParams::from(source))
        });
        let play = |fraction: f32| -> Vec<f32> {
            let memory = MemoryOutput::new(44100);
            let recorded = memory.samples_handle();
            let engine = Engine::start_with(Arc::clone(&opener), Box::new(memory));
            engine.command(Command::SetPlaylist {
                playlist: Playlist::new(vec![probed.clone()]),
                autoplay: false,
                start: Some(StartAt { index: 1, fraction }),
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(4);
            loop {
                if recorded.lock().unwrap().len() >= 64 {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    let snapshot = engine.snapshot();
                    panic!(
                        "no audio at fraction {fraction}: {:?} {}",
                        snapshot.state,
                        snapshot.error.unwrap_or_default()
                    );
                }
                thread::sleep(Duration::from_millis(10));
            }
            let samples = recorded.lock().unwrap()[..64].to_vec();
            samples
        };
        let at_start = play(0.0);
        let in_the_middle = play(0.5);
        assert_ne!(
            at_start, in_the_middle,
            "resuming halfway through the second cue track played its beginning"
        );
    }

    #[test]
    fn a_playlist_can_start_on_a_later_track_and_position() {
        let memory = MemoryOutput::new(44100);
        let recorded = memory.samples_handle();
        let opener: Opener = Arc::new(|source| {
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let engine = Engine::start_with(opener, Box::new(memory));
        let frames = 44_100 * 10;
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![source("a", frames), source("b", frames)]),
            autoplay: false,
            start: Some(StartAt {
                index: 1,
                fraction: 0.5,
            }),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = engine.snapshot();
            let ready = recorded.lock().unwrap().len() >= 2;
            // Memory output accepts audio as fast as it is decoded, so the track can
            // finish before this loop observes Playing.
            if snapshot.index == Some(1) && ready {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "did not resume: state {:?} index {:?} samples {} {}",
                    snapshot.state,
                    snapshot.index,
                    recorded.lock().unwrap().len(),
                    snapshot.error.unwrap_or_default()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        let samples = recorded.lock().unwrap();
        let first = samples[0];
        assert!(
            first > (frames / 4) as f32,
            "playback started at {first}, not halfway through the second track"
        );
        assert_eq!(engine.snapshot().title, "b");
    }

    #[test]
    fn the_engine_plays_two_files_back_to_back() {
        let memory = MemoryOutput::new(44100);
        let recorded = memory.samples_handle();
        let opener: Opener = Arc::new(|source| {
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let engine = Engine::start_with(opener, Box::new(memory));
        let playlist = Playlist::new(vec![source("a", 300), source("b", 200)]);
        engine.command(Command::SetPlaylist {
            playlist,
            autoplay: true,
            start: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = engine.snapshot();
            let samples = recorded.lock().unwrap().len() / 2;
            if snapshot.state == PlayState::Stopped && samples >= 500 {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("playback did not finish: state {:?} samples {samples}", snapshot.state);
            }
            thread::sleep(Duration::from_millis(10));
        }
        let guard = recorded.lock().unwrap();
        let left: Vec<f32> = guard.iter().step_by(2).copied().collect();
        let expected: Vec<f32> = (0..300).chain(0..200).map(|frame| frame as f32).collect();
        assert_eq!(left, expected);
    }

    #[test]
    fn replacing_a_playlist_opens_the_new_file_even_when_ids_match() {
        let opens = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = std::sync::Arc::clone(&opens);
        let opener: Opener = std::sync::Arc::new(move |source| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let memory = MemoryOutput::new(44100);
        let recorded = memory.samples_handle();
        let engine = Engine::start_with(opener, Box::new(memory));
        let first = Playlist::new(vec![source("album.flac", 5000)]);
        let mut second = Playlist::new(vec![source("track.mp3", 400)]);
        let id = first.sources[0].id;
        second.sources[0].id = id;
        second.entries[0].source_id = id;
        engine.command(Command::SetPlaylist {
            playlist: first,
            autoplay: true,
            start: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let samples = recorded.lock().unwrap().len() / 2;
            if samples >= 200 {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("first file did not start");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let opened = opens.load(std::sync::atomic::Ordering::SeqCst);
        engine.command(Command::SetPlaylist {
            playlist: second,
            autoplay: true,
            start: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = engine.snapshot();
            let opened_now = opens.load(std::sync::atomic::Ordering::SeqCst);
            if snapshot.title == "track.mp3"
                && snapshot.state == PlayState::Stopped
                && opened_now > opened
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                let samples = recorded.lock().unwrap().len() / 2;
                panic!(
                    "replacement did not finish: state {:?} title {} samples {samples} opens {opened_now}",
                    snapshot.state, snapshot.title
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            opens.load(std::sync::atomic::Ordering::SeqCst) > opened,
            "the new file was never opened"
        );
        let guard = recorded.lock().unwrap();
        let left: Vec<f32> = guard.iter().step_by(2).copied().collect();
        let tail = &left[left.len() - 400..];
        let expected: Vec<f32> = (0..400).map(|frame| frame as f32).collect();
        assert_eq!(tail, expected.as_slice());
        assert_eq!(engine.snapshot().title, "track.mp3");
    }

    #[test]
    fn append_adds_rows_without_reopening_the_current_file() {
        let opens = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = std::sync::Arc::clone(&opens);
        let opener: Opener = std::sync::Arc::new(move |source| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let memory = MemoryOutput::new(44100);
        let engine = Engine::start_with(opener, Box::new(memory));
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![source("first.flac", u64::MAX / 4)]),
            autoplay: true,
            start: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = engine.snapshot();
            if snapshot.state == PlayState::Playing && snapshot.rows.len() == 1 {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("playback did not start");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let opened = opens.load(std::sync::atomic::Ordering::SeqCst);
        engine.command(Command::Append(Playlist::new(vec![source("second.mp3", 1000)])));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = engine.snapshot();
            let now = opens.load(std::sync::atomic::Ordering::SeqCst);
            if snapshot.rows.len() == 2 && snapshot.state == PlayState::Playing && now >= opened + 1 {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("append did not prefetch the new row: opens {now} state {:?}", snapshot.state);
            }
            thread::sleep(Duration::from_millis(10));
        }
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.rows.len(), 2);
        assert_eq!(snapshot.rows[1].title, "second.mp3");
        assert_eq!(snapshot.state, PlayState::Playing);
        assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), opened + 1);
        engine.command(Command::Clear);
        thread::sleep(Duration::from_millis(40));
        let snapshot = engine.snapshot();
        assert!(snapshot.rows.is_empty());
        assert_eq!(snapshot.state, PlayState::Stopped);
    }

    #[test]
    fn moving_rows_keeps_the_current_file_and_removing_it_starts_the_next() {
        let opens = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = std::sync::Arc::clone(&opens);
        let opener: Opener = std::sync::Arc::new(move |source| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let memory = MemoryOutput::new(44100);
        let engine = Engine::start_with(opener, Box::new(memory));
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![
                source("a.flac", u64::MAX / 4),
                source("b.flac", u64::MAX / 4),
                source("c.flac", u64::MAX / 4),
            ]),
            autoplay: true,
            start: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = engine.snapshot();
            if snapshot.state == PlayState::Playing && snapshot.title == "a.flac" {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("playback did not start: {:?}", snapshot.state);
            }
            thread::sleep(Duration::from_millis(5));
        }
        // The next file may be prefetched. Wait until that open has landed.
        let mut opened = opens.load(std::sync::atomic::Ordering::SeqCst);
        let mut stable = 0u8;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            thread::sleep(Duration::from_millis(10));
            let now = opens.load(std::sync::atomic::Ordering::SeqCst);
            if now == opened {
                stable += 1;
                if stable >= 3 {
                    break;
                }
            } else {
                opened = now;
                stable = 0;
            }
            if std::time::Instant::now() > deadline {
                break;
            }
        }
        engine.command(Command::MoveRows {
            indices: vec![2],
            insert_before: 0,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = engine.snapshot();
            let titles: Vec<_> = snapshot.rows.iter().map(|row| row.title.as_str()).collect();
            if titles == ["c.flac", "a.flac", "b.flac"]
                && snapshot.title == "a.flac"
                && snapshot.index == Some(1)
                && snapshot.state == PlayState::Playing
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "move did not stick: {:?} index {:?} title {} rows {titles:?}",
                    snapshot.state, snapshot.index, snapshot.title
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(30));
        assert_eq!(opens.load(std::sync::atomic::Ordering::SeqCst), opened);
        engine.command(Command::RemoveMany(vec![1]));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = engine.snapshot();
            let titles: Vec<_> = snapshot.rows.iter().map(|row| row.title.as_str()).collect();
            if titles == ["c.flac", "b.flac"]
                && snapshot.title == "b.flac"
                && snapshot.index == Some(1)
                && snapshot.state == PlayState::Playing
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "remove did not start the next row: {:?} index {:?} title {} rows {titles:?}",
                    snapshot.state, snapshot.index, snapshot.title
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Output that reports playback a fixed number of frames behind what was pushed.
    /// That is the gap between the decode thread and a real device ring.
    struct LagOutput {
        pushed: u64,
        lag: u64,
    }

    impl AudioOut for LagOutput {
        fn sample_rate(&self) -> u32 {
            44100
        }
        fn free_frames(&self) -> usize {
            2048
        }
        fn push(&mut self, stereo: &[f32]) -> usize {
            let frames = stereo.len() / 2;
            self.pushed += frames as u64;
            frames
        }
        fn clear(&mut self) {}
        fn set_paused(&mut self, _paused: bool) {}
        fn set_gain(&mut self, _gain: f32) {}
        fn played(&self) -> u64 {
            self.pushed.saturating_sub(self.lag)
        }
        fn underruns(&self) -> u64 {
            0
        }
        fn device_name(&self) -> String {
            "lag".into()
        }
        fn devices(&self) -> Vec<String> {
            Vec::new()
        }
        fn set_device(&mut self, _name: Option<&str>) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn the_seek_clock_moves_while_the_device_is_behind_the_decoder() {
        let opener: Opener = Arc::new(|source| {
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let engine = Engine::start_with(
            opener,
            Box::new(LagOutput {
                pushed: 0,
                lag: 8000,
            }),
        );
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![source("long.flac", 44100 * 30)]),
            autoplay: true,
            start: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut started = false;
        loop {
            let snapshot = engine.snapshot();
            if snapshot.state == PlayState::Playing {
                started = true;
            }
            if snapshot.position_frames > 1000 {
                assert!(snapshot.segment_frames > snapshot.position_frames);
                return;
            }
            if started && snapshot.state == PlayState::Stopped {
                panic!(
                    "playback ended with the clock at {} of {}",
                    snapshot.position_frames, snapshot.segment_frames
                );
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "clock stayed at {} (segment {}) state {:?}",
                    snapshot.position_frames, snapshot.segment_frames, snapshot.state
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_stalled_decoder_does_not_end_playback() {
        let hold = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let gate = std::sync::Arc::clone(&hold);
        let opener: Opener = std::sync::Arc::new(move |source| {
            Ok(Box::new(Hold {
                inner: Ramp {
                    len: source.presentation_frames,
                    pos: 0,
                },
                hold: std::sync::Arc::clone(&gate),
            }))
        });
        let memory = MemoryOutput::new(44100);
        let recorded = memory.samples_handle();
        let engine = Engine::start_with(opener, Box::new(memory));
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![source("held.wav", 1000)]),
            autoplay: true,
            start: None,
        });
        thread::sleep(Duration::from_millis(150));
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.state, PlayState::Playing, "a stall looked like the end of the file");
        assert!(snapshot.error.is_none());
        hold.store(false, std::sync::atomic::Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let samples = recorded.lock().unwrap().len() / 2;
            if engine.snapshot().state == PlayState::Stopped && samples >= 1000 {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("playback did not finish after the stall, samples {samples}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_next_file_is_decoded_before_the_splice_so_a_slow_open_does_not_empty_the_ring() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shared = std::sync::Arc::new(std::sync::Mutex::new(ClockState {
            samples: Vec::new(),
            queued: 0,
            played: 0,
        }));
        let empty_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let total = 88_200 + 17_640;
        let ticker_stop = std::sync::Arc::clone(&stop);
        let ticker_shared = std::sync::Arc::clone(&shared);
        let ticker_empty = std::sync::Arc::clone(&empty_us);
        let ticker = thread::spawn(move || {
            let mut empty_since: Option<std::time::Instant> = None;
            loop {
                if ticker_stop.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let now = std::time::Instant::now();
                let mut state = ticker_shared.lock().unwrap();
                let produced = state.samples.len() / 2;
                let starved = produced > 0 && produced < total && state.queued == 0;
                if starved {
                    if empty_since.is_none() {
                        empty_since = Some(now);
                    }
                } else if let Some(start) = empty_since.take() {
                    let gap = now.saturating_duration_since(start).as_micros() as u64;
                    ticker_empty.fetch_add(gap, std::sync::atomic::Ordering::SeqCst);
                }
                let step = 882.min(state.queued);
                state.queued -= step;
                state.played += step as u64;
                drop(state);
                thread::sleep(Duration::from_millis(20));
            }
        });
        let opener: Opener = std::sync::Arc::new(|source| {
            let slow = source.title == "b";
            if slow {
                thread::sleep(Duration::from_millis(200));
            }
            Ok(Box::new(Tone {
                len: source.presentation_frames,
                pos: 0,
                value: if slow { 0.75 } else { 0.25 },
                nap: if slow {
                    Duration::from_millis(500)
                } else {
                    Duration::from_millis(0)
                },
                napped: false,
            }))
        });
        let engine = Engine::start_with(
            opener,
            Box::new(ClockOutput {
                shared: std::sync::Arc::clone(&shared),
                capacity: 44_100 / 5,
            }),
        );
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![source("a", 88_200), source("b", 17_640)]),
            autoplay: true,
            start: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        loop {
            let state = engine.snapshot().state;
            let produced = shared.lock().unwrap().samples.len() / 2;
            if state == PlayState::Stopped && produced >= total {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("slow splice did not finish: state {:?} produced {produced}", state);
            }
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        ticker.join().unwrap();
        let empty = empty_us.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            empty < 120_000,
            "the ring was empty for {} ms across the splice",
            empty / 1000
        );
        let guard = shared.lock().unwrap();
        let left: Vec<f32> = guard.samples.iter().step_by(2).copied().collect();
        assert_eq!(left.len(), total);
        assert!(left[..88_200].iter().all(|sample| *sample == 0.25));
        assert!(left[88_200..].iter().all(|sample| *sample == 0.75));
    }

    #[test]
    fn reordering_the_upcoming_row_prefetches_the_new_next_file() {
        let opens = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = std::sync::Arc::clone(&opens);
        let opener: Opener = std::sync::Arc::new(move |source| {
            recorded.lock().unwrap().push(source.title.clone());
            Ok(Box::new(Ramp {
                len: source.presentation_frames,
                pos: 0,
            }))
        });
        let engine = Engine::start_with(opener, Box::new(MemoryOutput::new(44100)));
        engine.command(Command::SetPlaylist {
            playlist: Playlist::new(vec![
                source("a", u64::MAX / 4),
                source("b", u64::MAX / 4),
                source("c", u64::MAX / 4),
            ]),
            autoplay: true,
            start: None,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let names = opens.lock().unwrap().clone();
            if names.iter().any(|name| name == "a") && names.iter().any(|name| name == "b") {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("prefetch of b did not start: {names:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        engine.command(Command::Reorder { from: 2, to: 1 });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let names = opens.lock().unwrap().clone();
            if names.iter().any(|name| name == "c") {
                assert_eq!(names.iter().filter(|name| *name == "a").count(), 1);
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!("reorder did not prefetch c: {names:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    struct Hold {
        inner: Ramp,
        hold: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl PcmSource for Hold {
        fn sample_rate(&self) -> u32 {
            44100
        }
        fn channels(&self) -> u16 {
            2
        }
        fn channel_map(&self) -> ChannelMap {
            ChannelMap::Stereo
        }
        fn presentation_frames(&self) -> u64 {
            self.inner.len
        }
        fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
            while self.hold.load(std::sync::atomic::Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            self.inner.pull(interleaved)
        }
        fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
            self.inner.seek_presentation(frame)
        }
    }

    struct Tone {
        len: u64,
        pos: u64,
        value: f32,
        nap: Duration,
        napped: bool,
    }

    impl PcmSource for Tone {
        fn sample_rate(&self) -> u32 {
            44100
        }
        fn channels(&self) -> u16 {
            2
        }
        fn channel_map(&self) -> ChannelMap {
            ChannelMap::Stereo
        }
        fn presentation_frames(&self) -> u64 {
            self.len
        }
        fn pull(&mut self, interleaved: &mut [f32]) -> Result<usize, Error> {
            if !self.napped && !self.nap.is_zero() {
                self.napped = true;
                thread::sleep(self.nap);
            }
            let frames = interleaved.len() / 2;
            let n = frames.min(self.len.saturating_sub(self.pos) as usize);
            for index in 0..n {
                interleaved[index * 2] = self.value;
                interleaved[index * 2 + 1] = self.value;
            }
            self.pos += n as u64;
            Ok(n)
        }
        fn seek_presentation(&mut self, frame: u64) -> Result<(), Error> {
            self.pos = frame.min(self.len);
            Ok(())
        }
    }

    struct ClockState {
        samples: Vec<f32>,
        queued: usize,
        played: u64,
    }

    struct ClockOutput {
        shared: std::sync::Arc<std::sync::Mutex<ClockState>>,
        capacity: usize,
    }

    impl AudioOut for ClockOutput {
        fn sample_rate(&self) -> u32 {
            44100
        }
        fn free_frames(&self) -> usize {
            let state = self.shared.lock().unwrap();
            self.capacity.saturating_sub(state.queued)
        }
        fn push(&mut self, stereo: &[f32]) -> usize {
            let mut state = self.shared.lock().unwrap();
            let frames = (stereo.len() / 2).min(self.capacity.saturating_sub(state.queued));
            state.samples.extend_from_slice(&stereo[..frames * 2]);
            state.queued += frames;
            frames
        }
        fn clear(&mut self) {
            self.shared.lock().unwrap().queued = 0;
        }
        fn set_paused(&mut self, _paused: bool) {}
        fn set_gain(&mut self, _gain: f32) {}
        fn played(&self) -> u64 {
            self.shared.lock().unwrap().played
        }
        fn underruns(&self) -> u64 {
            0
        }
        fn device_name(&self) -> String {
            "clock".into()
        }
        fn devices(&self) -> Vec<String> {
            Vec::new()
        }
        fn set_device(&mut self, _name: Option<&str>) -> Result<(), Error> {
            Ok(())
        }
    }
}
