//! Playlist window. The engine owns playback. This crate sends commands and draws
//! the latest snapshot.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use directories::ProjectDirs;
use eframe::egui;
use seasound_model::{
    format_time, playlist_from_descriptions, playlist_matching, Cover, DecodeParams, PlayState,
    Playlist, Repeat, RowView, SavedPlaylist, SavedPosition, SavedRow, Source, SourceId,
};
use seasound_playback::{Command, Engine, Opener, Snapshot, StartAt};
use serde::{Deserialize, Serialize};

use crate::theme::{self, Theme, Themes};

const ROW_H: f32 = 28.0;
const DRAG_SLOP: f32 = 8.0;
const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "mp4", "wav", "aiff", "aif", "ogg", "cue", "m3u", "m3u8",
];

pub struct SeaSoundApp {
    engine: Engine,
    snap: Snapshot,
    volume: f32,
    muted: bool,
    seek: f32,
    seek_drag: bool,
    /// The seek command is in flight. Keep the bar where the user left it until the
    /// snapshot catches up, including while playback stays paused.
    seek_sent: bool,
    selected: HashSet<usize>,
    row_drag: Option<RowDrag>,
    covers: HashMap<SourceId, Cover>,
    warnings: HashMap<SourceId, String>,
    textures: HashMap<SourceId, egui::TextureHandle>,
    cover_failed: HashSet<SourceId>,
    load_rx: Option<Receiver<LoadEvent>>,
    loading: bool,
    /// Files read so far, and how many the playlist names. `total` is 0 when it is not known yet.
    load_progress: Option<(usize, usize)>,
    /// The saved track is already playing while the other files are still being read.
    priority_started: bool,
    dialog: Option<Dialog>,
    dialog_arm: bool,
    seen_rows: bool,
    playlist_path: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    settings_path: Option<PathBuf>,
    notice: Option<String>,
    settle: u8,
    last_followed: Option<usize>,
    pending_scroll: Option<f32>,
    device_pref: Option<String>,
    plugins: Vec<seasound_media::PluginStatus>,
    disabled_plugins: Vec<String>,
    /// Remember the in-app playlist, track, and position across launches.
    /// Also starts an opened playlist file from its saved place.
    resume_playback: bool,
    /// Write the open playlist file when leaving it, and again on quit.
    /// A playlist that has never been saved is still written on quit.
    auto_save_playlists: bool,
    session: Option<SessionPlayback>,
    settings_open: bool,
    columns: ColumnLayout,
    themes: Themes,
    themes_dir: Option<PathBuf>,
    /// Name of the chosen theme. Kept when the file is missing so it comes back on reload.
    theme_name: String,
}

/// Replace clears the playlist. Append keeps what is already there.
#[derive(Clone, Copy)]
enum Import {
    Replace,
    Append,
}

#[derive(Clone, Copy)]
enum Dialog {
    Files(Import),
    Folder(Import),
    OpenPlaylist,
    SavePlaylist,
}

enum LoadWork {
    Paths { paths: Vec<PathBuf>, import: Import },
    /// `restore_position` plays the saved place. Startup leaves it off so a fallback
    /// file still loads without starting playback.
    SavedFile {
        path: PathBuf,
        restore_position: bool,
    },
    /// The playlist stored in settings, not a playlist file the user opened.
    Session(SessionPlayback),
}

enum LoadEvent {
    /// The file that was playing is ready. The other files are still being read.
    Priority {
        playlist: Playlist,
        path: Option<PathBuf>,
        position: SavedPosition,
    },
    /// One probed file. The first of these onto an empty playlist starts playback.
    Source { source: Source, import: Import },
    /// Rows read so far. The track already playing keeps its decoder.
    Extend {
        playlist: Playlist,
        path: Option<PathBuf>,
    },
    Progress { done: usize, total: usize },
    /// Every file has been reported. The playlist is already in the window.
    Settled,
    Finished(LoadDone),
    Failed(String),
}

enum LoadDone {
    Saved {
        playlist: Playlist,
        path: PathBuf,
        position: Option<SavedPosition>,
    },
    Session { playlist: Playlist, session: SessionPlayback },
}

#[derive(Clone)]
enum Line {
    Header(String),
    Track(usize),
}

#[derive(Clone, Copy)]
enum RowGesture {
    /// `anchor` is a position in the row slice. `pressed` is the playlist entry index.
    Select { anchor: usize, pressed: usize },
    /// `pressed` is the playlist entry index.
    Move { pressed: usize },
}

#[derive(Clone, Copy)]
struct RowDrag {
    gesture: RowGesture,
    travel: f32,
    insert_before: Option<usize>,
}

struct Slot {
    line: Line,
    rect: egui::Rect,
    response: egui::Response,
}

/// One visible track, in playlist order. Geometry helpers take this instead of egui widgets.
#[derive(Clone, Copy)]
struct TrackSpan {
    index: usize,
    position: usize,
    top: f32,
    bottom: f32,
}

impl TrackSpan {
    fn center(self) -> f32 {
        (self.top + self.bottom) * 0.5
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Settings {
    #[serde(default = "default_volume")]
    volume: f32,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    playlist: Option<PathBuf>,
    #[serde(default)]
    disabled_plugins: Vec<String>,
    /// Play the saved in-app playlist from the saved position on the next launch.
    /// Opening a playlist file uses the same switch.
    #[serde(default = "default_resume_playback")]
    resume_playback: bool,
    /// Write an open playlist file before leaving it, and again on quit.
    #[serde(default = "default_auto_save_playlists")]
    auto_save_playlists: bool,
    /// Playlist, track, and position saved from the app. Absent when resume is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<SessionPlayback>,
    /// Playlist column widths set by dragging the header.
    #[serde(default)]
    columns: ColumnLayout,
    /// Theme name. Built-in themes and `*.json` files in `themes/` of the config directory.
    #[serde(default = "default_theme")]
    theme: String,
}

/// The playlist that was in the app at exit, plus the track that was current.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SessionPlayback {
    rows: Vec<SavedRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track: Option<SavedPosition>,
}

fn default_volume() -> f32 {
    0.8
}

fn default_resume_playback() -> bool {
    true
}

fn default_auto_save_playlists() -> bool {
    true
}

fn default_theme() -> String {
    theme::DEFAULT_THEME.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: default_volume(),
            muted: false,
            device: None,
            playlist: None,
            disabled_plugins: Vec::new(),
            resume_playback: default_resume_playback(),
            auto_save_playlists: default_auto_save_playlists(),
            session: None,
            columns: ColumnLayout::default(),
            theme: default_theme(),
        }
    }
}

/// What to put in the window on launch.
#[derive(Debug)]
enum StartupLoad {
    /// Rows saved in settings. `track` is played when it still matches a row.
    Session(SessionPlayback),
    /// A playlist path, loaded without starting playback.
    File(PathBuf),
    Empty,
}

impl SeaSoundApp {
    pub fn new(creation: &eframe::CreationContext<'_>) -> Self {
        let config_dir = ProjectDirs::from("audio", "SeaSound", "SeaSound")
            .map(|dirs| dirs.config_dir().to_path_buf());
        if let Some(dir) = &config_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        let settings_path = config_dir.as_ref().map(|dir| dir.join("settings.json"));
        let settings = settings_path
            .as_ref()
            .map(|path| load_settings(path))
            .unwrap_or_default();
        let volume = settings.volume.clamp(0.0, 1.0);
        let themes_dir = config_dir.as_ref().map(|dir| dir.join("themes"));
        if let Some(dir) = &themes_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        let themes = Themes::load(themes_dir.as_deref());
        theme::apply(&creation.egui_ctx, themes.get(&settings.theme));
        let plugin_dir = config_dir.as_ref().map(|dir| dir.join("plugins"));
        if let Some(dir) = &plugin_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        let extra_dirs: Vec<PathBuf> = plugin_dir.into_iter().collect();
        seasound_media::install_plugins(&extra_dirs, &settings.disabled_plugins);
        let plugins = seasound_media::plugin_statuses();
        let engine = Engine::start(opener());
        engine.command(Command::SetVolume(volume));
        engine.command(Command::SetMuted(settings.muted));
        if let Some(device) = settings.device.clone().filter(|name| !name.is_empty()) {
            engine.command(Command::SetDevice(Some(device)));
        }
        let mut app = Self {
            engine,
            snap: Snapshot::default(),
            volume,
            muted: settings.muted,
            seek: 0.0,
            seek_drag: false,
            seek_sent: false,
            selected: HashSet::new(),
            row_drag: None,
            covers: HashMap::new(),
            warnings: HashMap::new(),
            textures: HashMap::new(),
            cover_failed: HashSet::new(),
            load_rx: None,
            loading: false,
            load_progress: None,
            priority_started: false,
            dialog: None,
            dialog_arm: false,
            seen_rows: false,
            playlist_path: settings.playlist.clone(),
            config_dir,
            settings_path,
            notice: None,
            settle: 30,
            last_followed: None,
            pending_scroll: None,
            device_pref: settings.device,
            plugins,
            disabled_plugins: settings.disabled_plugins,
            resume_playback: settings.resume_playback,
            auto_save_playlists: settings.auto_save_playlists,
            session: settings.session.clone(),
            settings_open: false,
            columns: settings.columns.sanitized(),
            themes,
            themes_dir,
            theme_name: settings.theme.clone(),
        };
        match startup_load(settings.resume_playback, settings.session, settings.playlist) {
            StartupLoad::Session(session) => {
                // Rows come from the session. The playlist path stays attached so a later
                // switch or exit still writes this place back to that file.
                app.spawn(LoadWork::Session(session));
            }
            StartupLoad::File(path) => app.spawn(LoadWork::SavedFile {
                path,
                restore_position: false,
            }),
            StartupLoad::Empty => {}
        }
        app
    }

    fn spawn(&mut self, work: LoadWork) {
        let replacing = match &work {
            LoadWork::Paths { import: Import::Replace, .. } | LoadWork::SavedFile { .. } => {
                self.playlist_path = None;
                true
            }
            LoadWork::Session(_) => true,
            LoadWork::Paths { import: Import::Append, .. } => false,
        };
        if replacing {
            // The previous list stays out of the way while the new one is read.
            self.engine.command(Command::Clear);
            self.covers.clear();
            self.warnings.clear();
            self.textures.clear();
            self.cover_failed.clear();
            self.forget_selection();
            self.seek = 0.0;
            self.seek_drag = false;
            self.seek_sent = false;
            self.last_followed = None;
        }
        let (tx, rx) = mpsc::channel();
        self.load_rx = Some(rx);
        self.loading = true;
        self.load_progress = None;
        self.priority_started = false;
        self.settle = 30;
        thread::spawn(move || work.run(tx));
    }

    fn poll_load(&mut self) {
        loop {
            let event = match self.load_rx.as_ref() {
                Some(rx) => match rx.try_recv() {
                    Ok(event) => event,
                    Err(TryRecvError::Empty) => return,
                    Err(TryRecvError::Disconnected) => {
                        self.load_rx = None;
                        self.loading = false;
                        self.load_progress = None;
                        return;
                    }
                },
                None => return,
            };
            self.apply_load(event);
        }
    }

    fn apply_load(&mut self, event: LoadEvent) {
        match event {
            LoadEvent::Progress { done, total } => {
                self.load_progress = Some((done, total));
            }
            LoadEvent::Priority {
                playlist,
                path,
                position,
            } => {
                let start = resume_start(&playlist, &position);
                self.install_playlist(playlist, false, path, start);
                self.priority_started = true;
                self.loading = true;
                self.settle = 30;
            }
            LoadEvent::Source { source, import } => {
                // Open replaces by clearing first, so this append starts playback only
                // when the window was empty. Later files join without restarting it.
                match import {
                    Import::Replace | Import::Append => self.append_sources(vec![source]),
                }
            }
            LoadEvent::Extend { playlist, path } => {
                if let Some(path) = path {
                    self.playlist_path = Some(path);
                }
                self.complete_playlist(playlist);
            }
            LoadEvent::Settled => {
                self.loading = false;
                self.load_progress = None;
                self.priority_started = false;
                self.settle = 30;
            }
            LoadEvent::Failed(message) => {
                self.loading = false;
                self.load_progress = None;
                self.priority_started = false;
                self.notice = Some(message);
            }
            LoadEvent::Finished(done) => {
                self.loading = false;
                self.load_progress = None;
                self.settle = 30;
                let priority = self.priority_started;
                self.priority_started = false;
                match done {
                    LoadDone::Saved {
                        playlist,
                        path,
                        position,
                    } => {
                        if priority {
                            self.playlist_path = Some(path);
                            self.complete_playlist(playlist);
                        } else {
                            let start = position
                                .as_ref()
                                .and_then(|position| resume_start(&playlist, position));
                            self.install_playlist(playlist, false, Some(path), start);
                        }
                    }
                    LoadDone::Session { playlist, session } => {
                        if priority {
                            self.complete_playlist(playlist);
                        } else {
                            let start = session
                                .track
                                .as_ref()
                                .and_then(|track| resume_start(&playlist, track));
                            self.install_playlist(playlist, false, None, start);
                        }
                    }
                }
            }
        }
    }

    fn append_sources(&mut self, sources: Vec<Source>) {
        let mut extra = Playlist::new(sources);
        let (covers, warnings) = take_art(&mut extra);
        self.covers.extend(covers);
        self.warnings.extend(warnings);
        self.engine.command(Command::Append(extra));
        self.settle = 30;
    }

    fn new_playlist(&mut self) {
        self.snap = self.engine.snapshot();
        if !self.snap.rows.is_empty() {
            // Keep the place in the file that is open, then leave that file alone.
            self.flush_open_playlist();
        }
        self.playlist_path = None;
        self.empty_window();
    }

    fn clear_playlist(&mut self) {
        // Drop the window without writing. The open file keeps its last saved place.
        self.playlist_path = None;
        self.empty_window();
    }

    fn empty_window(&mut self) {
        self.load_rx = None;
        self.loading = false;
        self.load_progress = None;
        self.priority_started = false;
        self.engine.command(Command::Clear);
        self.covers.clear();
        self.warnings.clear();
        self.textures.clear();
        self.cover_failed.clear();
        self.forget_selection();
        self.seek = 0.0;
        self.seek_drag = false;
        self.seek_sent = false;
        self.last_followed = None;
        self.notice = None;
        self.settle = 15;
    }

    fn forget_selection(&mut self) {
        self.selected.clear();
        self.row_drag = None;
    }

    fn install_playlist(
        &mut self,
        mut playlist: Playlist,
        autoplay: bool,
        path: Option<PathBuf>,
        start: Option<StartAt>,
    ) {
        let (covers, warnings) = take_art(&mut playlist);
        let repeat = self.snap.repeat;
        let shuffle = self.snap.shuffle;
        self.engine.command(Command::SetPlaylist {
            playlist,
            autoplay,
            start,
        });
        self.engine.command(Command::SetRepeat(repeat));
        self.engine.command(Command::SetShuffle(shuffle));
        self.covers = covers;
        self.warnings = warnings;
        self.textures.clear();
        self.cover_failed.clear();
        self.forget_selection();
        self.seek = 0.0;
        self.seek_drag = false;
        self.seek_sent = false;
        self.last_followed = None;
        if let Some(path) = path {
            self.playlist_path = Some(path);
        }
    }

    /// Add the files that finished reading. The track already playing keeps its decoder.
    fn complete_playlist(&mut self, mut playlist: Playlist) {
        let (covers, warnings) = take_art(&mut playlist);
        self.covers.extend(covers);
        self.warnings.extend(warnings);
        self.engine.command(Command::ReplacePlaylist(playlist));
        self.forget_selection();
        self.settle = 30;
    }

    fn take_drops(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if !paths.is_empty() {
            self.spawn(LoadWork::Paths {
                paths,
                import: Import::Append,
            });
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if consume(ctx, egui::Key::Space) {
            self.engine.command(Command::TogglePause);
            self.settle = 15;
        }
        if consume(ctx, egui::Key::ArrowLeft) {
            self.seek_by(-5.0);
        }
        if consume(ctx, egui::Key::ArrowRight) {
            self.seek_by(5.0);
        }
        if consume(ctx, egui::Key::ArrowUp) {
            self.nudge_volume(0.05);
        }
        if consume(ctx, egui::Key::ArrowDown) {
            self.nudge_volume(-0.05);
        }
        if consume(ctx, egui::Key::Delete) && !self.selected.is_empty() {
            let mut indices: Vec<usize> = self.selected.iter().copied().collect();
            indices.sort_unstable();
            self.engine.command(Command::RemoveMany(indices));
            self.selected.clear();
            self.row_drag = None;
            self.settle = 15;
        }
    }

    fn seek_by(&mut self, seconds: f64) {
        let total = self.snap.segment_frames;
        if total == 0 {
            return;
        }
        let row_rate = current_row(&self.snap).map(|row| row.sample_rate).unwrap_or(0);
        let rate = if row_rate > 0 {
            row_rate
        } else {
            self.snap.sample_rate
        };
        if rate == 0 {
            return;
        }
        let position = self.snap.position_frames as f64 / rate as f64;
        let length = total as f64 / rate as f64;
        if length <= 0.0 {
            return;
        }
        let fraction = ((position + seconds) / length).clamp(0.0, 0.999) as f32;
        self.seek = fraction;
        self.seek_drag = false;
        self.seek_sent = true;
        self.engine.command(Command::SeekFraction(fraction));
        self.settle = 15;
    }

    fn nudge_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.engine.command(Command::SetVolume(self.volume));
        self.settle = 10;
    }

    fn retitle(&self, ctx: &egui::Context) {
        let title = if self.snap.title.is_empty() {
            "SeaSound".to_string()
        } else {
            format!("{} — SeaSound", self.snap.title)
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
    }

    fn menu(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui
                        .button("Add files…")
                        .on_hover_text("Add to the playlist")
                        .clicked()
                    {
                        self.dialog = Some(Dialog::Files(Import::Append));
                        ui.close_menu();
                    }
                    if ui
                        .button("Add folder…")
                        .on_hover_text("Add to the playlist")
                        .clicked()
                    {
                        self.dialog = Some(Dialog::Folder(Import::Append));
                        ui.close_menu();
                    }
                    if ui
                        .button("Open files…")
                        .on_hover_text("Replace the playlist")
                        .clicked()
                    {
                        self.dialog = Some(Dialog::Files(Import::Replace));
                        ui.close_menu();
                    }
                    if ui
                        .button("Open folder…")
                        .on_hover_text("Replace the playlist")
                        .clicked()
                    {
                        self.dialog = Some(Dialog::Folder(Import::Replace));
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui
                        .button("New playlist")
                        .on_hover_text(
                            "Save your place in the open file, then start an empty playlist. The next Save uses a new name.",
                        )
                        .clicked()
                    {
                        self.new_playlist();
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(!self.snap.rows.is_empty(), egui::Button::new("Clear playlist"))
                        .on_hover_text(
                            "Remove every track. The open file is left unchanged, and the next Save uses a new name.",
                        )
                        .clicked()
                    {
                        self.clear_playlist();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui
                        .button("Open playlist…")
                        .on_hover_text("Replace the playlist and resume its saved position")
                        .clicked()
                    {
                        self.dialog = Some(Dialog::OpenPlaylist);
                        ui.close_menu();
                    }
                    if ui
                        .button("Save playlist…")
                        .on_hover_text("Save this playlist and the current position")
                        .clicked()
                    {
                        self.dialog = Some(Dialog::SavePlaylist);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Playback", |ui| {
                    if ui.button(play_word(self.snap.state)).clicked() {
                        self.engine.command(Command::TogglePause);
                        ui.close_menu();
                    }
                    if ui.button("Stop").clicked() {
                        self.engine.command(Command::Stop);
                        ui.close_menu();
                    }
                    if ui.button("Next").clicked() {
                        self.engine.command(Command::Next);
                        ui.close_menu();
                    }
                    if ui.button("Previous").clicked() {
                        self.engine.command(Command::Previous);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button(self.snap.repeat.label()).clicked() {
                        self.engine.command(Command::SetRepeat(self.snap.repeat.next()));
                        ui.close_menu();
                    }
                    let shuffle = if self.snap.shuffle {
                        "Shuffle on"
                    } else {
                        "Shuffle off"
                    };
                    if ui.button(shuffle).clicked() {
                        self.engine.command(Command::SetShuffle(!self.snap.shuffle));
                        ui.close_menu();
                    }
                });
                ui.menu_button("Audio", |ui| {
                    let devices = self.snap.devices.clone();
                    let current = self.snap.device.clone();
                    if ui.button("System default").clicked() {
                        self.device_pref = None;
                        self.engine.command(Command::SetDevice(None));
                        ui.close_menu();
                    }
                    for name in devices.iter() {
                        let selected = name == &current;
                        if ui.selectable_label(selected, name).clicked() {
                            self.device_pref = Some(name.clone());
                            self.engine.command(Command::SetDevice(Some(name.clone())));
                            ui.close_menu();
                        }
                    }
                    ui.separator();
                    let mute = if self.muted { "Unmute" } else { "Mute" };
                    if ui.button(mute).clicked() {
                        self.muted = !self.muted;
                        self.engine.command(Command::SetMuted(self.muted));
                        ui.close_menu();
                    }
                });
                ui.menu_button("Plugins", |ui| {
                    ui.set_min_width(280.0);
                    ui.label("Plugins run with the same permissions as SeaSound.");
                    ui.separator();
                    if self.plugins.is_empty() {
                        ui.label("No plugins loaded.");
                        return;
                    }
                    let snapshot = self.plugins.clone();
                    let mut toggle = None;
                    for plugin in &snapshot {
                        if let Some(error) = &plugin.error {
                            let name = if plugin.name.is_empty() {
                                plugin.file_name.as_str()
                            } else {
                                plugin.name.as_str()
                            };
                            ui.colored_label(ui.visuals().error_fg_color, format!("{name} — {error}"));
                            continue;
                        }
                        let mut on = plugin.enabled;
                        let exts = plugin.extensions.join(", ");
                        let label = if plugin.version.is_empty() {
                            format!("{} ({exts})", plugin.name)
                        } else {
                            format!("{} {} ({exts})", plugin.name, plugin.version)
                        };
                        if ui.checkbox(&mut on, label).changed() {
                            toggle = Some((plugin.id.clone(), on));
                        }
                    }
                    if let Some((id, on)) = toggle {
                        if let Some(plugin) = self.plugins.iter_mut().find(|plugin| plugin.id == id) {
                            plugin.enabled = on;
                        }
                        seasound_media::set_plugin_enabled(&id, on);
                        if on {
                            self.disabled_plugins.retain(|item| item != &id);
                        } else if !self.disabled_plugins.iter().any(|item| item == &id) {
                            self.disabled_plugins.push(id);
                        }
                        self.write_settings();
                    }
                });
                ui.menu_button("Settings", |ui| {
                    if ui.button("Settings…").clicked() {
                        self.settings_open = true;
                        ui.close_menu();
                    }
                    ui.menu_button("Theme", |ui| self.theme_menu(ui));
                });
            });
        });
    }

    fn theme_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(220.0);
        let current = self.themes.get(&self.theme_name).name.clone();
        let mut chosen = None;
        for theme in &self.themes.list {
            if ui.radio(theme.name == current, theme.name.as_str()).clicked() {
                chosen = Some(theme.name.clone());
            }
        }
        ui.separator();
        if ui
            .button("Reload themes")
            .on_hover_text("Read the theme files again")
            .clicked()
        {
            self.themes = Themes::load(self.themes_dir.as_deref());
            chosen = Some(self.theme_name.clone());
        }
        if let Some(dir) = &self.themes_dir {
            ui.label(
                egui::RichText::new(format!("Add *.json themes to {}", dir.display()))
                    .size(12.0)
                    .color(theme::current(ui.ctx()).weak_text.0),
            );
        }
        for error in &self.themes.errors {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(name) = chosen {
            theme::apply(ui.ctx(), self.themes.get(&name));
            if name != self.theme_name {
                self.theme_name = name;
                self.write_settings();
            }
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let frame = egui::Frame::window(&ctx.style()).inner_margin(16);
        let shown = egui::Modal::new(egui::Id::new("settings"))
            .frame(frame)
            .backdrop_color(egui::Color32::from_black_alpha(160))
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.set_max_width(440.0);
                ui.label(egui::RichText::new("Settings").strong().size(18.0));
                ui.add_space(12.0);
                let mut resume = self.resume_playback;
                if ui.checkbox(&mut resume, "Resume playback").changed() {
                    self.resume_playback = resume;
                    if resume {
                        self.capture_session();
                    } else {
                        self.session = None;
                    }
                    self.write_settings();
                }
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "On exit, SeaSound saves the playlist in the app, the current track, and the position. The next launch plays that track from the saved position. This is the playlist in the window, not a playlist file you opened. Save playlist stores that same place in the file. Open playlist plays the file from there when this is on.",
                    )
                    .size(13.0)
                    .color(theme::current(ui.ctx()).weak_text.0),
                );
                ui.add_space(12.0);
                let mut auto_save = self.auto_save_playlists;
                if ui.checkbox(&mut auto_save, "Auto save playlists").changed() {
                    self.auto_save_playlists = auto_save;
                    self.write_settings();
                }
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "When this is on, SeaSound writes the open playlist before you open another one, open files or a folder, start a new playlist, or quit. A new playlist that has never been saved is still written to the app playlist when you quit.",
                    )
                    .size(13.0)
                    .color(theme::current(ui.ctx()).weak_text.0),
                );
                ui.add_space(14.0);
                if ui.button("Close").clicked() {
                    self.settings_open = false;
                }
            });
        if shown.should_close() {
            self.settings_open = false;
        }
    }

    fn transport(&mut self, ctx: &egui::Context) {
        let snap = self.snap.clone();
        let shown = if snap.segment_frames > 0 {
            (snap.position_frames as f32 / snap.segment_frames as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if self.seek_sent && ((shown - self.seek).abs() < 0.015 || self.settle == 0) {
            self.seek_sent = false;
        }
        if !self.seek_drag && !self.seek_sent {
            self.seek = shown;
        }
        let row = current_row(&snap).cloned();
        let source_id = row.as_ref().map(|row| row.source_id);
        if let Some(id) = source_id {
            self.ensure_cover(ctx, id);
        }
        egui::TopBottomPanel::bottom("transport").show(ctx, |ui| {
            ui.add_space(6.0);
            // The seek rail must use its column. Nested `horizontal` / `vertical`
            // calls shrink-wrap, and `Slider` paints `spacing.slider_width`
            // (default 100) rather than the rectangle passed to `add_sized`.
            // The column is the left half. File facts use the right half.
            let gap = 8.0;
            ui.horizontal_top(|ui| {
                self.cover_box(ui, source_id);
                ui.add_space(gap);
                let gutter = 16.0;
                let rest = ui.available_width();
                let main_width = ((rest - gutter) * 0.5).max(140.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(main_width, 132.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                    ui.set_min_width(main_width);
                    let title = row
                        .as_ref()
                        .map(|row| row_title(row))
                        .filter(|title| !title.is_empty())
                        .unwrap_or_else(|| {
                            if snap.title.is_empty() {
                                "Not playing".to_string()
                            } else {
                                snap.title.clone()
                            }
                        });
                    ui.label(egui::RichText::new(title).strong().size(18.0));
                    ui.label(subtitle(row.as_ref(), &snap));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("Prev").clicked() {
                            self.engine.command(Command::Previous);
                        }
                        if ui.button(play_word(snap.state)).clicked() {
                            self.engine.command(Command::TogglePause);
                        }
                        if ui.button("Next").clicked() {
                            self.engine.command(Command::Next);
                        }
                        if ui.button("Stop").clicked() {
                            self.engine.command(Command::Stop);
                        }
                        ui.add_space(8.0);
                        let repeat = colored(ui, snap.repeat.label(), snap.repeat != Repeat::Off);
                        if ui.button(repeat).clicked() {
                            self.engine.command(Command::SetRepeat(snap.repeat.next()));
                            self.settle = 10;
                        }
                        let shuffle = colored(
                            ui,
                            if snap.shuffle { "Shuffle on" } else { "Shuffle" },
                            snap.shuffle,
                        );
                        if ui.button(shuffle).clicked() {
                            self.engine.command(Command::SetShuffle(!snap.shuffle));
                            self.settle = 10;
                        }
                    });
                    ui.horizontal(|ui| {
                        let (shown_pos, total_frames, rate) = clock(&snap, row.as_ref(), self.seek, self.seek_drag);
                        ui.label(format_time(shown_pos, rate));
                        let end = format_time(total_frames, rate);
                        let end_width = 56.0;
                        let slider_width = (ui.available_width() - end_width).max(48.0);
                        let previous = ui.spacing().slider_width;
                        ui.spacing_mut().slider_width = slider_width;
                        let slider = egui::Slider::new(&mut self.seek, 0.0..=1.0)
                            .show_value(false)
                            .trailing_fill(true);
                        let response = ui.add(slider);
                        ui.spacing_mut().slider_width = previous;
                        if response.drag_started() || response.dragged() {
                            self.seek_drag = true;
                        }
                        if response.drag_stopped() || response.clicked() {
                            self.seek_drag = false;
                            self.seek_sent = true;
                            self.engine.command(Command::SeekFraction(self.seek));
                            self.settle = 20;
                        }
                        ui.label(end);
                    });
                    ui.horizontal(|ui| {
                        let mute = if self.muted { "Muted" } else { "Volume" };
                        if ui.button(colored(ui, mute, self.muted)).clicked() {
                            self.muted = !self.muted;
                            self.engine.command(Command::SetMuted(self.muted));
                        }
                        let slider = egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false);
                        let previous = ui.spacing().slider_width;
                        ui.spacing_mut().slider_width = 140.0;
                        let changed = ui.add(slider).changed();
                        ui.spacing_mut().slider_width = previous;
                        if changed {
                            self.engine.command(Command::SetVolume(self.volume));
                        }
                    });
                });
                let (rule, _) = ui.allocate_exact_size(egui::vec2(gutter, 112.0), egui::Sense::hover());
                let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
                ui.painter().vline(
                    rule.center().x,
                    (rule.top() + 6.0)..=(rule.bottom() - 2.0),
                    stroke,
                );
                let details_width = ui.available_width();
                ui.allocate_ui_with_layout(
                    egui::vec2(details_width, 132.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_width(details_width);
                        file_details(ui, row.as_ref(), &snap.rows);
                    },
                );
            });
            ui.add_space(4.0);
            self.status_line(ui, &snap, source_id);
            ui.add_space(4.0);
        });
    }

    fn cover_box(&self, ui: &mut egui::Ui, source_id: Option<SourceId>) {
        let size = egui::vec2(84.0, 84.0);
        if let Some(texture) = source_id.and_then(|id| self.textures.get(&id)) {
            ui.add(
                egui::Image::new(texture)
                    .fit_to_exact_size(size)
                    .maintain_aspect_ratio(true),
            );
            return;
        }
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let theme = theme::current(ui.ctx());
        ui.painter().rect_filled(rect, 4.0, theme.inset.0);
        ui.painter().rect_stroke(
            rect.shrink(10.0),
            2.0,
            egui::Stroke::new(2.0, theme.accent.0),
            egui::StrokeKind::Inside,
        );
    }

    fn status_line(&self, ui: &mut egui::Ui, snap: &Snapshot, source_id: Option<SourceId>) {
        let mut parts: Vec<String> = Vec::new();
        if self.loading {
            parts.push(loading_label(self.load_progress));
        }
        if let Some(notice) = &self.notice {
            parts.push(notice.clone());
        }
        if let Some(error) = &snap.error {
            parts.push(error.clone());
        }
        if let Some(id) = source_id {
            if let Some(warning) = self.warnings.get(&id) {
                parts.push(warning.clone());
            }
        }
        if snap.underruns > 0 {
            parts.push(format!("underruns: {}", snap.underruns));
        }
        if let Some(path) = &self.playlist_path {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                parts.push(name.to_string());
            }
        }
        let text = if parts.is_empty() {
            if snap.rows.is_empty() {
                "Open files, a folder, or a cue sheet.".to_string()
            } else {
                format!("{} tracks", snap.rows.len())
            }
        } else {
            parts.join("  ·  ")
        };
        let theme = theme::current(ui.ctx());
        let color = if snap.error.is_some() || self.notice.is_some() {
            theme.error.0
        } else {
            theme.weak_text.0
        };
        ui.label(egui::RichText::new(text).color(color).size(12.0));
    }

    fn playlist_panel(&mut self, ctx: &egui::Context) {
        let rows = std::sync::Arc::clone(&self.snap.rows);
        let playing = self.snap.index;
        let hovering = ctx.input(|input| !input.raw.hovered_files.is_empty());
        if rows.is_empty() {
            self.forget_selection();
        }
        if playing != self.last_followed {
            self.last_followed = playing;
            let lines = playlist_lines(&rows);
            if let Some(index) = playing {
                if let Some(line) = line_of(&lines, &rows, index) {
                    self.pending_scroll = Some((line as f32 * ROW_H - ROW_H * 2.0).max(0.0));
                }
            }
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            if rows.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        let message = if self.loading {
                            loading_label(self.load_progress)
                        } else if hovering {
                            "Drop audio to add it".to_string()
                        } else {
                            "No tracks yet".to_string()
                        };
                        ui.label(egui::RichText::new(message).size(18.0));
                        ui.add_space(8.0);
                        if !self.loading && ui.button("Open files").clicked() {
                            self.dialog = Some(Dialog::Files(Import::Replace));
                        }
                    });
                });
                return;
            }
            if hovering {
                ui.label(egui::RichText::new("Drop audio to add it").color(ui.visuals().selection.bg_fill));
            }
            if self.loading {
                ui.label(
                    egui::RichText::new(loading_label(self.load_progress))
                        .color(ui.visuals().selection.bg_fill)
                        .size(14.0),
                );
            }
            if column_header(ui, &mut self.columns) {
                ui.ctx().request_repaint();
            }
            ui.separator();
            ui.spacing_mut().item_spacing.y = 0.0;
            let lines = playlist_lines(&rows);
            let scroll_style = ui.spacing().scroll;
            // Floating bars are painted over the rows. Their clicks must not select or drag.
            let bar_inset = vertical_bar_inset(
                scroll_style.floating,
                lines.len() as f32 * ROW_H,
                ui.available_height(),
                scroll_style.bar_width,
                scroll_style.bar_outer_margin,
            );
            let mut scroll = egui::ScrollArea::vertical()
                .id_salt("playlist")
                .auto_shrink([false, false])
                .drag_to_scroll(false);
            if let Some(offset) = self.pending_scroll.take() {
                scroll = scroll.vertical_scroll_offset(offset);
            }
            let output = scroll.show_rows(ui, ROW_H, lines.len(), |ui, range| {
                let mut slots = Vec::with_capacity(range.len());
                for line_i in range {
                    let width = ui.available_width();
                    let sense = match &lines[line_i] {
                        Line::Header(_) => egui::Sense::hover(),
                        Line::Track(_) => egui::Sense::click(),
                    };
                    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, ROW_H), sense);
                    slots.push(Slot {
                        line: lines[line_i].clone(),
                        rect,
                        response,
                    });
                }
                let cursor = self.handle_row_pointer(ctx, &rows, &slots, bar_inset);
                for slot in &slots {
                    self.paint_slot(ui, &rows, slot, playing, bar_inset);
                }
                self.paint_insertion(ui, &rows, &slots);
                cursor
            });
            if let Some(icon) = output.inner {
                ctx.set_cursor_icon(icon);
            }
            self.autoscroll(ctx, &output);
            self.paint_row_ghost(ctx, &rows);
        });
    }

    fn handle_row_pointer(
        &mut self,
        ctx: &egui::Context,
        rows: &[RowView],
        slots: &[Slot],
        bar_inset: f32,
    ) -> Option<egui::CursorIcon> {
        for slot in slots {
            if !slot.response.double_clicked() {
                continue;
            }
            let Line::Track(position) = &slot.line else {
                continue;
            };
            let Some(row) = rows.get(*position) else {
                continue;
            };
            self.selected.clear();
            self.selected.insert(row.index);
            self.row_drag = None;
            if row.error.is_none() {
                self.engine.command(Command::PlayIndex(row.index));
                self.settle = 15;
            }
            return None;
        }

        let (pointer, pressed, down, delta) = ctx.input(|input| {
            (
                input.pointer.interact_pos(),
                input.pointer.primary_pressed(),
                input.pointer.primary_down(),
                input.pointer.delta().length(),
            )
        });

        let mut started = false;
        if pressed {
            if let Some(pos) = pointer {
                if let Some((position, index)) = hit_track(pos, rows, slots, bar_inset) {
                    started = true;
                    if self.selected.contains(&index) {
                        self.row_drag = Some(RowDrag {
                            gesture: RowGesture::Move { pressed: index },
                            travel: 0.0,
                            insert_before: None,
                        });
                    } else {
                        self.selected.clear();
                        self.selected.insert(index);
                        self.row_drag = Some(RowDrag {
                            gesture: RowGesture::Select {
                                anchor: position,
                                pressed: index,
                            },
                            travel: 0.0,
                            insert_before: None,
                        });
                    }
                }
            }
        }

        let Some(mut drag) = self.row_drag else {
            return hover_cursor(&self.selected, rows, slots, bar_inset);
        };
        if !started {
            drag.travel += delta;
        }
        if drag.travel > DRAG_SLOP {
            let spans = track_spans(rows, slots);
            match drag.gesture {
                RowGesture::Select { anchor, .. } => {
                    if let Some(pos) = pointer {
                        if let Some(position) = row_position_at(pos.y, &spans) {
                            self.selected = indices_between(rows, anchor, position);
                        }
                    }
                }
                RowGesture::Move { .. } => {
                    if let Some(pos) = pointer {
                        drag.insert_before = drop_before(pos.y, &spans, playlist_len(rows));
                    }
                }
            }
        }

        if down {
            let icon = match drag.gesture {
                RowGesture::Move { .. } if drag.travel > DRAG_SLOP => Some(egui::CursorIcon::Grabbing),
                RowGesture::Move { .. } => hover_cursor(&self.selected, rows, slots, bar_inset),
                RowGesture::Select { .. } => None,
            };
            self.row_drag = Some(drag);
            ctx.request_repaint();
            return icon;
        }

        self.row_drag = None;
        if drag.travel <= DRAG_SLOP {
            let pressed_index = match drag.gesture {
                RowGesture::Select { pressed, .. } | RowGesture::Move { pressed } => pressed,
            };
            self.selected.clear();
            self.selected.insert(pressed_index);
            return hover_cursor(&self.selected, rows, slots, bar_inset);
        }
        if let RowGesture::Move { .. } = drag.gesture {
            self.commit_move(rows, drag.insert_before);
        }
        None
    }

    fn commit_move(&mut self, rows: &[RowView], insert_before: Option<usize>) {
        let Some(insert_before) = insert_before else {
            return;
        };
        let mut indices: Vec<usize> = self.selected.iter().copied().collect();
        indices.sort_unstable();
        let Some(places) = Playlist::moved_places(playlist_len(rows), &indices, insert_before) else {
            return;
        };
        self.engine.command(Command::MoveRows {
            indices,
            insert_before,
        });
        self.selected = places.into_iter().collect();
        self.settle = 15;
    }

    fn paint_slot(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[RowView],
        slot: &Slot,
        playing_index: Option<usize>,
        bar_inset: f32,
    ) {
        let rect = slot.rect;
        let theme = theme::current(ui.ctx());
        match &slot.line {
            Line::Header(title) => {
                ui.painter().rect_filled(rect, 0.0, theme.group_header.0);
                paint_text(
                    ui,
                    rect.shrink2(egui::vec2(10.0, 0.0)),
                    title,
                    theme.group_header_text.0,
                    true,
                    egui::Align2::LEFT_CENTER,
                );
            }
            Line::Track(position) => {
                let Some(row) = rows.get(*position) else {
                    return;
                };
                let playing = playing_index == Some(row.index);
                let selected = self.selected.contains(&row.index);
                let moving = self.row_drag.is_some_and(|drag| {
                    drag.travel > DRAG_SLOP && matches!(drag.gesture, RowGesture::Move { .. })
                });
                let highlight = row_highlight(&theme, playing, selected, pointer_on_row(slot, bar_inset));
                if let Some((fill, _)) = highlight {
                    ui.painter().rect_filled(rect, 0.0, fill);
                }
                if playing {
                    let bar = egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + 3.0, rect.bottom()));
                    ui.painter().rect_filled(bar, 0.0, theme.background.0);
                }
                if moving && selected {
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(1.0, theme.background.0),
                        egui::StrokeKind::Inside,
                    );
                }
                let columns = columns_for(rect, &self.columns);
                let color = if let Some((_, text)) = highlight {
                    text
                } else if row.error.is_some() {
                    theme.error.0
                } else {
                    ui.visuals().text_color()
                };
                let number = if row.number == 0 {
                    String::new()
                } else {
                    row.number.to_string()
                };
                let time = if let Some(error) = &row.error {
                    error.clone()
                } else {
                    format_time(row.frames, row.sample_rate.max(1))
                };
                let title = row_title(row);
                for (kind, column) in columns {
                    let (text, strong) = match kind {
                        ColumnKind::Number => (number.as_str(), false),
                        ColumnKind::Title => (title.as_str(), playing),
                        ColumnKind::Artist => (row.artist.as_str(), false),
                        ColumnKind::Album => (row.album.as_str(), false),
                        ColumnKind::Time => (time.as_str(), false),
                    };
                    paint_text(ui, column, text, color, strong, egui::Align2::LEFT_CENTER);
                }
                if let Some(error) = &row.error {
                    slot.response.clone().on_hover_text(error.as_str());
                } else if let Some(warning) = self.warnings.get(&row.source_id) {
                    slot.response.clone().on_hover_text(warning.as_str());
                }
            }
        }
    }

    fn paint_insertion(&self, ui: &egui::Ui, rows: &[RowView], slots: &[Slot]) {
        let Some(drag) = self.row_drag else {
            return;
        };
        if drag.travel <= DRAG_SLOP {
            return;
        }
        let RowGesture::Move { .. } = drag.gesture else {
            return;
        };
        let Some(insert_before) = drag.insert_before else {
            return;
        };
        let spans = track_spans(rows, slots);
        let Some(y) = insertion_y(insert_before, &spans, playlist_len(rows)) else {
            return;
        };
        let Some(slot) = slots.first() else {
            return;
        };
        let accent = ui.visuals().selection.bg_fill;
        paint_insertion_line(ui.painter(), accent, slot.rect.left(), slot.rect.right(), y);
    }

    fn autoscroll<R>(&self, ctx: &egui::Context, output: &egui::containers::scroll_area::ScrollAreaOutput<R>) {
        let Some(drag) = self.row_drag else {
            return;
        };
        if drag.travel <= DRAG_SLOP {
            return;
        }
        let Some(pos) = ctx.input(|input| input.pointer.interact_pos()) else {
            return;
        };
        let mut state = output.state;
        let max_offset = (output.content_size.y - output.inner_rect.height()).max(0.0);
        let mut next = state.offset.y;
        if pos.y < output.inner_rect.top() + ROW_H {
            let step = (output.inner_rect.top() + ROW_H - pos.y).clamp(6.0, 24.0);
            next = (state.offset.y - step).max(0.0);
        } else if pos.y > output.inner_rect.bottom() - ROW_H {
            let step = (pos.y - (output.inner_rect.bottom() - ROW_H)).clamp(6.0, 24.0);
            next = (state.offset.y + step).min(max_offset);
        }
        if next != state.offset.y {
            state.offset.y = next;
            state.store(ctx, output.id);
            ctx.request_repaint();
        }
    }

    fn paint_row_ghost(&self, ctx: &egui::Context, rows: &[RowView]) {
        let Some(drag) = self.row_drag else {
            return;
        };
        if drag.travel <= DRAG_SLOP || !matches!(drag.gesture, RowGesture::Move { .. }) {
            return;
        }
        let Some(pointer) = ctx.input(|input| input.pointer.interact_pos()) else {
            return;
        };
        let label = ghost_text(rows, &self.selected);
        if label.is_empty() {
            return;
        }
        paint_ghost(ctx, pointer, &label);
    }

    fn ensure_cover(&mut self, ctx: &egui::Context, id: SourceId) {
        if self.textures.contains_key(&id) || self.cover_failed.contains(&id) {
            return;
        }
        let Some(cover) = self.covers.get(&id) else {
            return;
        };
        match image::load_from_memory(&cover.bytes) {
            Ok(image) => {
                let rgba = image.to_rgba8();
                let color = egui::ColorImage::from_rgba_unmultiplied(
                    [rgba.width() as usize, rgba.height() as usize],
                    rgba.as_raw(),
                );
                let handle = ctx.load_texture(
                    format!("cover-{}", id.0),
                    color,
                    egui::TextureOptions::LINEAR,
                );
                self.textures.insert(id, handle);
            }
            Err(_) => {
                self.cover_failed.insert(id);
            }
        }
    }

    fn arm_dialog(&mut self, ctx: &egui::Context) {
        if self.dialog.is_none() {
            return;
        }
        if !self.dialog_arm {
            self.dialog_arm = true;
            ctx.request_repaint();
            return;
        }
        self.dialog_arm = false;
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        self.run_dialog(dialog);
    }

    fn run_dialog(&mut self, dialog: Dialog) {
        match dialog {
            Dialog::Files(import) => {
                let filters = audio_filter_exts();
                if let Some(paths) = rfd::FileDialog::new()
                    .add_filter("Audio", &filters)
                    .add_filter("All files", &["*"])
                    .pick_files()
                {
                    if matches!(import, Import::Replace) {
                        self.flush_open_playlist();
                    }
                    self.spawn(LoadWork::Paths { paths, import });
                }
            }
            Dialog::Folder(import) => {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    if matches!(import, Import::Replace) {
                        self.flush_open_playlist();
                    }
                    self.spawn(LoadWork::Paths {
                        paths: vec![path],
                        import,
                    });
                }
            }
            Dialog::OpenPlaylist => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Playlist", &["json"])
                    .pick_file()
                {
                    self.flush_open_playlist();
                    self.spawn(LoadWork::SavedFile {
                        path,
                        restore_position: self.resume_playback,
                    });
                }
            }
            Dialog::SavePlaylist => {
                let mut dialog = rfd::FileDialog::new()
                    .add_filter("Playlist", &["json"])
                    .set_file_name("playlist.json");
                if let Some(path) = &self.playlist_path {
                    if let Some(parent) = path.parent() {
                        dialog = dialog.set_directory(parent);
                    }
                    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                        dialog = dialog.set_file_name(name);
                    }
                }
                if let Some(path) = dialog.save_file() {
                    let path = ensure_json(path);
                    if self.playlist_path.as_ref() != Some(&path) {
                        self.flush_open_playlist();
                    }
                    self.playlist_path = Some(path.clone());
                    self.write_playlist_file(&path);
                }
            }
        }
    }

    fn write_playlist_file(&mut self, path: &Path) {
        self.snap = self.engine.snapshot();
        if !self.snap.rows.is_empty() {
            self.seen_rows = true;
        }
        match saved_playlist(&self.snap.rows, playback_position(&self.snap)).to_json() {
            Ok(json) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(err) = std::fs::write(path, json) {
                    self.notice = Some(format!("Could not save the playlist: {err}"));
                } else {
                    self.notice = None;
                }
            }
            Err(err) => self.notice = Some(err.to_string()),
        }
    }

    fn write_settings(&self) {
        let Some(path) = &self.settings_path else {
            return;
        };
        let device = if !self.snap.device.is_empty() && self.snap.device != "No device" {
            Some(self.snap.device.clone())
        } else {
            self.device_pref.clone()
        };
        let settings = Settings {
            volume: self.volume,
            muted: self.muted,
            device,
            playlist: self.playlist_path.clone(),
            disabled_plugins: self.disabled_plugins.clone(),
            resume_playback: self.resume_playback,
            auto_save_playlists: self.auto_save_playlists,
            session: if self.resume_playback {
                self.session.clone()
            } else {
                None
            },
            columns: self.columns.clone(),
            theme: self.theme_name.clone(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&settings) {
            let _ = std::fs::write(path, json);
        }
    }

    fn autosave_playlist(&mut self) {
        if !self.seen_rows || self.loading {
            return;
        }
        let Some(path) = quit_playlist_path(
            self.auto_save_playlists,
            self.playlist_path.as_deref(),
            self.config_dir.as_deref(),
        ) else {
            return;
        };
        let before = self.playlist_path.clone();
        self.write_playlist_file(&path);
        if before.is_none() && path.is_file() {
            self.playlist_path = Some(path);
        }
    }

    /// Remember the playlist currently in the window. Skipped while a load is in flight
    /// so a late save cannot replace a good session with an empty list.
    fn capture_session(&mut self) {
        if !self.resume_playback {
            self.session = None;
            return;
        }
        if self.loading || !self.seen_rows {
            return;
        }
        let rows = saved_from_rows(&self.snap.rows).rows;
        let track = playback_position(&self.snap);
        self.session = Some(SessionPlayback { rows, track });
    }

    /// Write the attached playlist file before leaving it, so its playhead stays put.
    /// Skipped when auto-save is off. A playlist with no file is left for quit.
    fn flush_open_playlist(&mut self) {
        if !self.auto_save_playlists {
            return;
        }
        let Some(path) = self.playlist_path.clone() else {
            return;
        };
        self.write_playlist_file(&path);
    }
}

/// Where quit writes the playlist.
/// An open file is written only when auto-save is on.
/// A playlist that was never saved still goes to `playlist.json`.
fn quit_playlist_path(
    auto_save: bool,
    open: Option<&Path>,
    config_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path) = open {
        if auto_save {
            return Some(path.to_path_buf());
        }
        return None;
    }
    config_dir.map(|dir| dir.join("playlist.json"))
}

impl eframe::App for SeaSoundApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snap = self.engine.snapshot();
        if !self.snap.rows.is_empty() {
            self.seen_rows = true;
        }
        // eframe only calls `App::save` when its persistence feature is on, and that
        // feature is not enabled. The close request is delivered on this frame.
        if ctx.input(|input| input.viewport().close_requested()) {
            self.persist();
        }
        self.poll_load();
        self.take_drops(ctx);
        self.handle_keys(ctx);
        self.retitle(ctx);
        self.menu(ctx);
        self.transport(ctx);
        self.playlist_panel(ctx);
        self.settings_window(ctx);
        self.arm_dialog(ctx);
        if self.loading
            || self.snap.state == PlayState::Playing
            || self.settle > 0
            || self.dialog.is_some()
            || self.row_drag.is_some()
        {
            if self.settle > 0 {
                self.settle -= 1;
            }
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist();
    }
}

impl SeaSoundApp {
    fn persist(&mut self) {
        self.snap = self.engine.snapshot();
        if !self.snap.rows.is_empty() {
            self.seen_rows = true;
        }
        self.capture_session();
        self.autosave_playlist();
        self.write_settings();
    }
}

impl LoadWork {
    fn run(self, tx: mpsc::Sender<LoadEvent>) {
        match self {
            LoadWork::Paths { paths, import } => {
                let mut done = 0usize;
                seasound_media::load_paths_each(&paths, |source| {
                    done += 1;
                    if tx.send(LoadEvent::Source { source, import }).is_err() {
                        return false;
                    }
                    tx.send(LoadEvent::Progress { done, total: 0 }).is_ok()
                });
                let _ = tx.send(LoadEvent::Settled);
            }
            LoadWork::SavedFile {
                path,
                restore_position,
            } => {
                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) => {
                        let _ = tx.send(LoadEvent::Failed(err.to_string()));
                        return;
                    }
                };
                let saved = match SavedPlaylist::from_json(&text) {
                    Ok(saved) => saved,
                    Err(err) => {
                        let _ = tx.send(LoadEvent::Failed(err.to_string()));
                        return;
                    }
                };
                let position = if restore_position {
                    saved.position.clone()
                } else {
                    None
                };
                load_saved_rows(&saved, position, Some(path), false, &tx);
            }
            LoadWork::Session(session) => {
                let saved = SavedPlaylist {
                    version: 1,
                    rows: session.rows.clone(),
                    position: session.track.clone(),
                };
                load_saved_rows(&saved, session.track, None, true, &tx);
            }
        }
    }
}

fn audio_filter_exts() -> Vec<String> {
    let mut exts: Vec<String> = AUDIO_EXT.iter().map(|ext| (*ext).to_string()).collect();
    for ext in seasound_media::plugin_extensions() {
        if !exts.iter().any(|have| have == &ext) {
            exts.push(ext);
        }
    }
    exts
}

fn opener() -> Opener {
    std::sync::Arc::new(|source: &Source| {
        seasound_media::open_decoder(&source.path, DecodeParams::from(source))
    })
}

/// The file to start before the others. One file, or a list with no saved place, waits.
fn early_file(paths: &[PathBuf], position: Option<&SavedPosition>) -> Option<PathBuf> {
    if paths.len() < 2 {
        return None;
    }
    let position = position?;
    paths
        .iter()
        .find(|path| *path == &position.path)
        .cloned()
}

fn distinct_paths(rows: &[SavedRow]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for row in rows {
        if !paths.iter().any(|path| path == &row.path) {
            paths.push(row.path.clone());
        }
    }
    paths
}

/// Install a playlist that already describes every row. Otherwise read the saved
/// track first, then the files after it, then the files before it, adding each as it finishes.
fn load_saved_rows(
    saved: &SavedPlaylist,
    position: Option<SavedPosition>,
    file: Option<PathBuf>,
    session: bool,
    tx: &mpsc::Sender<LoadEvent>,
) {
    if let Some(playlist) = playlist_from_descriptions(&saved.rows) {
        let finished = if session {
            LoadDone::Session {
                playlist,
                session: SessionPlayback {
                    rows: saved.rows.clone(),
                    track: position,
                },
            }
        } else {
            let Some(path) = file else {
                return;
            };
            LoadDone::Saved {
                playlist,
                path,
                position,
            }
        };
        let _ = tx.send(LoadEvent::Finished(finished));
        return;
    }
    let paths = distinct_paths(&saved.rows);
    let total = paths.len();
    let early = early_file(&paths, position.as_ref());
    let order = probe_order(&paths, early.as_deref());
    let mut probed: Vec<Source> = Vec::new();
    let mut started = false;
    for (index, path) in order.iter().enumerate() {
        let mut loaded = seasound_media::load_paths(std::slice::from_ref(path));
        for source in &mut loaded {
            source.id = SourceId::fresh();
        }
        probed.extend(loaded);
        let playlist = playlist_matching(saved, probed.clone());
        for source in &mut probed {
            source.cover = None;
        }
        let playable = playlist
            .sources
            .iter()
            .any(|source| source.path == *path && source.playable());
        let matches_place = position.as_ref().is_some_and(|place| place.path == *path);
        if !started && matches_place && playable {
            let Some(position) = position.clone() else {
                return;
            };
            if tx
                .send(LoadEvent::Priority {
                    playlist,
                    path: file.clone(),
                    position,
                })
                .is_err()
            {
                return;
            }
            started = true;
        } else if tx
            .send(LoadEvent::Extend {
                playlist,
                path: if session { None } else { file.clone() },
            })
            .is_err()
        {
            return;
        }
        let done = index + 1;
        if total > 1 && tx.send(LoadEvent::Progress { done, total }).is_err() {
            return;
        }
    }
    let _ = tx.send(LoadEvent::Settled);
}

/// Resume file, then everything after it, then everything before it.
/// The row that plays next is therefore read before the rows already passed.
fn probe_order(paths: &[PathBuf], early: Option<&Path>) -> Vec<PathBuf> {
    let Some(early) = early else {
        return paths.to_vec();
    };
    let Some(index) = paths.iter().position(|path| path == early) else {
        return paths.to_vec();
    };
    let mut order = Vec::with_capacity(paths.len());
    order.push(paths[index].clone());
    order.extend(paths[index + 1..].iter().cloned());
    order.extend(paths[..index].iter().cloned());
    order
}

fn loading_label(progress: Option<(usize, usize)>) -> String {
    match progress {
        Some((done, total)) if total > 1 => format!("Loading… {done} / {total}"),
        Some((done, 0)) if done > 0 => format!("Loading… {done}"),
        _ => "Loading…".to_string(),
    }
}

/// Resume uses the playlist saved in settings. A playlist file path is only a fallback
/// for a launch that has no saved session, and it does not start playback.
fn startup_load(
    resume_playback: bool,
    session: Option<SessionPlayback>,
    opened: Option<PathBuf>,
) -> StartupLoad {
    if resume_playback {
        if let Some(session) = session {
            if session.rows.is_empty() {
                return StartupLoad::Empty;
            }
            return StartupLoad::Session(session);
        }
    }
    if let Some(path) = opened {
        if path.is_file() {
            return StartupLoad::File(path);
        }
    }
    StartupLoad::Empty
}

fn resume_start(playlist: &Playlist, track: &SavedPosition) -> Option<StartAt> {
    let rows = playlist.rows();
    let position = rows
        .iter()
        .position(|row| row.path == track.path && row.start_frame == track.start_frame)?;
    let frames = rows.get(position).map(|row| row.frames).unwrap_or(0);
    let fraction = if frames == 0 {
        0.0
    } else {
        (track.position_frames as f32 / frames as f32).clamp(0.0, 0.999)
    };
    let index = rows.get(position).map(|row| row.index)?;
    Some(StartAt { index, fraction })
}

fn take_art(playlist: &mut Playlist) -> (HashMap<SourceId, Cover>, HashMap<SourceId, String>) {
    let mut covers = HashMap::new();
    let mut warnings = HashMap::new();
    for source in &mut playlist.sources {
        if let Some(cover) = source.cover.take() {
            covers.insert(source.id, cover);
        }
        if let Some(warning) = source.warning.clone() {
            warnings.insert(source.id, warning);
        }
    }
    (covers, warnings)
}

fn saved_from_rows(rows: &[RowView]) -> SavedPlaylist {
    saved_playlist(rows, None)
}

fn saved_playlist(rows: &[RowView], position: Option<SavedPosition>) -> SavedPlaylist {
    SavedPlaylist {
        version: 1,
        rows: rows
            .iter()
            .map(|row| SavedRow {
                path: row.path.clone(),
                start_frame: row.start_frame,
                title: row.title.clone(),
                artist: row.artist.clone(),
                album: row.album.clone(),
                end_frame: row.start_frame.saturating_add(row.frames),
                sample_rate: row.sample_rate,
                channels: row.channels,
                bitrate_bps: row.bitrate_bps,
                bits_per_sample: row.bits_per_sample,
                delay: row.delay,
                padding: row.padding,
                presentation_frames: row.presentation_frames,
                number: row.number,
                format_name: row.format_name.clone(),
                group: row.group.clone(),
            })
            .collect(),
        position,
    }
}

fn playback_position(snap: &Snapshot) -> Option<SavedPosition> {
    let index = snap.index?;
    let row = snap.rows.get(index)?;
    Some(SavedPosition {
        path: row.path.clone(),
        start_frame: row.start_frame,
        position_frames: snap.position_frames,
    })
}

fn hit_track(
    pos: egui::Pos2,
    rows: &[RowView],
    slots: &[Slot],
    bar_inset: f32,
) -> Option<(usize, usize)> {
    for slot in slots {
        if !row_hit_rect(slot.rect, bar_inset).contains(pos) {
            continue;
        }
        let Line::Track(position) = &slot.line else {
            continue;
        };
        let Some(row) = rows.get(*position) else {
            continue;
        };
        return Some((*position, row.index));
    }
    None
}

fn track_spans(rows: &[RowView], slots: &[Slot]) -> Vec<TrackSpan> {
    let mut spans = Vec::new();
    for slot in slots {
        let Line::Track(position) = &slot.line else {
            continue;
        };
        let Some(row) = rows.get(*position) else {
            continue;
        };
        spans.push(TrackSpan {
            index: row.index,
            position: *position,
            top: slot.rect.top(),
            bottom: slot.rect.bottom(),
        });
    }
    spans
}

/// Width of a floating vertical scrollbar painted on top of the rows.
/// Solid bars already sit outside the content, so they need no inset.
fn vertical_bar_inset(
    floating: bool,
    content_height: f32,
    viewport_height: f32,
    bar_width: f32,
    outer_margin: f32,
) -> f32 {
    if floating && content_height > viewport_height {
        outer_margin + bar_width
    } else {
        0.0
    }
}

fn row_hit_rect(rect: egui::Rect, bar_inset: f32) -> egui::Rect {
    let right = (rect.right() - bar_inset).max(rect.left());
    egui::Rect::from_min_max(rect.min, egui::pos2(right, rect.bottom()))
}

fn pointer_on_row(slot: &Slot, bar_inset: f32) -> bool {
    slot.response
        .hover_pos()
        .is_some_and(|pos| row_hit_rect(slot.rect, bar_inset).contains(pos))
}

fn hover_cursor(
    selected: &HashSet<usize>,
    rows: &[RowView],
    slots: &[Slot],
    bar_inset: f32,
) -> Option<egui::CursorIcon> {
    let hover = slots.iter().any(|slot| {
        if !pointer_on_row(slot, bar_inset) {
            return false;
        }
        let Line::Track(position) = &slot.line else {
            return false;
        };
        rows.get(*position).is_some_and(|row| selected.contains(&row.index))
    });
    if hover {
        Some(egui::CursorIcon::Grab)
    } else {
        None
    }
}

fn playlist_len(rows: &[RowView]) -> usize {
    rows.iter().map(|row| row.index + 1).max().unwrap_or(0)
}

fn indices_between(rows: &[RowView], anchor: usize, current: usize) -> HashSet<usize> {
    if rows.is_empty() {
        return HashSet::new();
    }
    let last = rows.len() - 1;
    let start = anchor.min(current).min(last);
    let end = anchor.max(current).min(last);
    rows[start..=end].iter().map(|row| row.index).collect()
}

/// The track whose rect contains `pointer_y`. A header gap belongs to the next track.
fn row_position_at(pointer_y: f32, tracks: &[TrackSpan]) -> Option<usize> {
    if tracks.is_empty() {
        return None;
    }
    for track in tracks {
        if pointer_y < track.bottom {
            return Some(track.position);
        }
    }
    Some(tracks[tracks.len() - 1].position)
}

fn after_track(index: usize, playlist_len: usize) -> usize {
    let next = index.saturating_add(1);
    if next < playlist_len {
        next
    } else {
        playlist_len
    }
}

/// Entry index to insert in front of. `playlist_len` means append.
fn drop_before(pointer_y: f32, tracks: &[TrackSpan], playlist_len: usize) -> Option<usize> {
    if tracks.is_empty() || playlist_len == 0 {
        return None;
    }
    if pointer_y < tracks[0].center() {
        return Some(tracks[0].index);
    }
    for pair in tracks.windows(2) {
        if pointer_y < pair[1].center() {
            return Some(pair[1].index);
        }
    }
    Some(after_track(tracks[tracks.len() - 1].index, playlist_len))
}

fn insertion_y(insert_before: usize, tracks: &[TrackSpan], playlist_len: usize) -> Option<f32> {
    for track in tracks {
        if track.index == insert_before {
            return Some(track.top);
        }
    }
    let last = tracks.last()?;
    if insert_before == playlist_len || insert_before == last.index + 1 {
        Some(last.bottom)
    } else {
        None
    }
}

fn ghost_label(count: usize, title: &str) -> String {
    if count != 1 {
        return format!("{count} tracks");
    }
    let len = title.chars().count();
    if len > 48 {
        let trimmed: String = title.chars().take(48).collect();
        format!("{trimmed}…")
    } else if title.is_empty() {
        "1 track".to_string()
    } else {
        title.to_string()
    }
}

fn ghost_text(rows: &[RowView], selected: &HashSet<usize>) -> String {
    let mut titles = Vec::new();
    for row in rows {
        if selected.contains(&row.index) {
            titles.push(row_title(row));
        }
    }
    match titles.as_slice() {
        [title] => ghost_label(1, title),
        _ => ghost_label(titles.len(), ""),
    }
}

fn paint_insertion_line(painter: &egui::Painter, accent: egui::Color32, left: f32, right: f32, y: f32) {
    let line = egui::Rect::from_min_max(egui::pos2(left, y - 1.0), egui::pos2(right, y + 1.0));
    painter.rect_filled(line, 0.0, accent);
    let nub = egui::Rect::from_min_max(egui::pos2(left, y - 5.0), egui::pos2(left + 8.0, y + 5.0));
    painter.rect_filled(nub, 2.0, accent);
}

fn paint_ghost(ctx: &egui::Context, pointer: egui::Pos2, label: &str) {
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("playlist-drag-ghost"));
    let painter = ctx.layer_painter(layer);
    let font = egui::TextStyle::Body.resolve(ctx.style().as_ref());
    let theme = theme::current(ctx);
    let text = theme.control_text_hover.0;
    let galley = painter.layout_no_wrap(label.to_owned(), font, text);
    let pad = egui::vec2(10.0, 6.0);
    let origin = pointer + egui::vec2(14.0, 16.0);
    let rect = egui::Rect::from_min_size(origin, galley.size() + pad * 2.0);
    let [r, g, b, _] = theme.panel.0.to_array();
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgba_unmultiplied(r, g, b, 235));
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, theme.accent.0), egui::StrokeKind::Inside);
    painter.galley(rect.min + pad, galley, text);
}

/// Row fill and the text colour drawn on it.
fn row_highlight(
    theme: &Theme,
    playing: bool,
    selected: bool,
    hovered: bool,
) -> Option<(egui::Color32, egui::Color32)> {
    let (fill, text) = if playing {
        (theme.accent, theme.accent_text)
    } else if selected {
        (theme.row_selected, theme.row_selected_text)
    } else if hovered {
        (theme.row_hover, theme.row_hover_text)
    } else {
        return None;
    };
    Some((fill.0, text.0))
}

fn playlist_lines(rows: &[RowView]) -> Vec<Line> {
    let mut lines = Vec::new();
    for (position, row) in rows.iter().enumerate() {
        if let Some(title) = &row.group {
            lines.push(Line::Header(title.clone()));
        }
        lines.push(Line::Track(position));
    }
    lines
}

fn line_of(lines: &[Line], rows: &[RowView], entry_index: usize) -> Option<usize> {
    lines.iter().position(|line| match line {
        Line::Track(position) => rows.get(*position).is_some_and(|row| row.index == entry_index),
        Line::Header(_) => false,
    })
}

fn current_row(snap: &Snapshot) -> Option<&RowView> {
    let index = snap.index?;
    snap.rows.iter().find(|row| row.index == index)
}

fn row_title(row: &RowView) -> String {
    if !row.title.is_empty() {
        return row.title.clone();
    }
    row.path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Untitled")
        .to_string()
}

fn file_details(ui: &mut egui::Ui, row: Option<&RowView>, rows: &[RowView]) {
    let (codec, bitrate, rate, channels, bits) = match row {
        Some(row) => (
            codec_label(&row.format_name, &row.path),
            format_bitrate(row.bitrate_bps),
            format_sample_rate(row.sample_rate),
            format_channels(row.channels),
            format_bit_depth(row.bits_per_sample),
        ),
        None => (
            em_dash(),
            em_dash(),
            em_dash(),
            em_dash(),
            em_dash(),
        ),
    };
    let playtime = playlist_playtime(rows);
    ui.add_space(2.0);
    egui::Grid::new("now-playing-details")
        .spacing(egui::vec2(18.0, 4.0))
        .show(ui, |ui| {
            detail_line(ui, "Format", &codec);
            detail_line(ui, "Bitrate", &bitrate);
            detail_line(ui, "Sample rate", &rate);
            detail_line(ui, "Channels", &channels);
            detail_line(ui, "Bit depth", &bits);
            detail_line(ui, "Playtime", &playtime);
        });
}

/// Sum of each playlist row, using the same truncated seconds as the Time column.
fn playlist_playtime(rows: &[RowView]) -> String {
    if rows.is_empty() {
        return em_dash();
    }
    let mut seconds = 0u64;
    let mut known = false;
    for row in rows {
        if row.sample_rate == 0 {
            continue;
        }
        known = true;
        seconds = seconds.saturating_add(row.frames / u64::from(row.sample_rate));
    }
    if known {
        format_time(seconds, 1)
    } else {
        em_dash()
    }
}

fn detail_line(ui: &mut egui::Ui, label: &str, value: &str) {
    let weak = theme::current(ui.ctx()).weak_text.0;
    ui.label(egui::RichText::new(label).color(weak).size(13.0));
    let value_color = if value == "—" { weak } else { ui.visuals().text_color() };
    ui.label(egui::RichText::new(value).color(value_color).size(13.0));
    ui.end_row();
}

fn em_dash() -> String {
    "—".to_string()
}

fn codec_label(format_name: &str, path: &Path) -> String {
    let codec = format_name.trim();
    if codec.is_empty() || codec == "Audio" {
        return extension_token(path).unwrap_or_else(em_dash);
    }
    match extension_token(path) {
        Some(ext) if !same_kind(codec, &ext) => format!("{codec} · {ext}"),
        _ => codec.to_string(),
    }
}

fn extension_token(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.trim();
    if ext.is_empty() {
        None
    } else {
        Some(ext.to_ascii_uppercase())
    }
}

fn same_kind(codec: &str, ext: &str) -> bool {
    let codec = codec.to_ascii_lowercase();
    let ext = ext.to_ascii_lowercase();
    codec == ext || (codec == "aiff" && ext == "aif")
}

fn format_bitrate(bps: u32) -> String {
    if bps == 0 {
        return em_dash();
    }
    let kbps = ((bps as u64) + 500) / 1000;
    format!("{kbps} kbps")
}

fn format_sample_rate(rate: u32) -> String {
    if rate == 0 {
        return em_dash();
    }
    if rate % 1000 == 0 {
        format!("{} kHz", rate / 1000)
    } else if rate % 100 == 0 {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    } else {
        format!("{rate} Hz")
    }
}

fn format_channels(channels: u16) -> String {
    match channels {
        0 => em_dash(),
        1 => "Mono".to_string(),
        2 => "Stereo".to_string(),
        6 => "5.1".to_string(),
        8 => "7.1".to_string(),
        n => format!("{n} channels"),
    }
}

fn format_bit_depth(bits: u16) -> String {
    if bits == 0 {
        em_dash()
    } else {
        format!("{bits}-bit")
    }
}

fn subtitle(row: Option<&RowView>, snap: &Snapshot) -> String {
    let artist = row.map(|row| row.artist.as_str()).unwrap_or(snap.artist.as_str());
    let album = row.map(|row| row.album.as_str()).unwrap_or(snap.album.as_str());
    match (artist.is_empty(), album.is_empty()) {
        (true, true) => String::new(),
        (false, true) => artist.to_string(),
        (true, false) => album.to_string(),
        (false, false) => format!("{artist} · {album}"),
    }
}

fn clock(snap: &Snapshot, row: Option<&RowView>, seek: f32, dragging: bool) -> (u64, u64, u32) {
    let total = row.map(|row| row.frames).unwrap_or(snap.segment_frames);
    let rate = row
        .map(|row| row.sample_rate)
        .filter(|rate| *rate > 0)
        .unwrap_or(snap.sample_rate.max(1));
    let position = if dragging {
        (seek.clamp(0.0, 1.0) * total as f32) as u64
    } else {
        snap.position_frames.min(total)
    };
    (position, total, rate.max(1))
}

fn play_word(state: PlayState) -> &'static str {
    if state == PlayState::Playing {
        "Pause"
    } else {
        "Play"
    }
}

fn colored(ui: &egui::Ui, text: &str, active: bool) -> egui::RichText {
    let text = egui::RichText::new(text);
    if active {
        text.color(ui.visuals().selection.bg_fill)
    } else {
        text
    }
}

fn consume(ctx: &egui::Context, key: egui::Key) -> bool {
    ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, key))
}


const COLUMN_PAD: f32 = 8.0;
const MIN_NUMBER: f32 = 24.0;
const MIN_TITLE: f32 = 60.0;
const MIN_TEXT: f32 = 40.0;
const MIN_TIME: f32 = 48.0;
/// Pointer distance from a header divider that grabs it.
const DIVIDER_GRAB: f32 = 4.0;

/// One playlist column. Title has no stored width. It takes what the others leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ColumnKind {
    Number,
    Title,
    Artist,
    Album,
    Time,
}

impl ColumnKind {
    const ALL: [ColumnKind; 5] = [
        ColumnKind::Number,
        ColumnKind::Title,
        ColumnKind::Artist,
        ColumnKind::Album,
        ColumnKind::Time,
    ];

    fn label(self) -> &'static str {
        match self {
            ColumnKind::Number => "#",
            ColumnKind::Title => "Title",
            ColumnKind::Artist => "Artist",
            ColumnKind::Album => "Album",
            ColumnKind::Time => "Time",
        }
    }

    fn min_width(self) -> f32 {
        match self {
            ColumnKind::Number => MIN_NUMBER,
            ColumnKind::Title => MIN_TITLE,
            ColumnKind::Artist | ColumnKind::Album => MIN_TEXT,
            ColumnKind::Time => MIN_TIME,
        }
    }
}

/// Order and widths of the playlist columns, in points. Title takes what is left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ColumnLayout {
    #[serde(default = "default_number_width")]
    number: f32,
    #[serde(default = "default_text_width")]
    artist: f32,
    #[serde(default = "default_text_width")]
    album: f32,
    #[serde(default = "default_time_width")]
    time: f32,
    /// Left to right. Set by dragging a heading.
    #[serde(default = "default_column_order")]
    order: Vec<ColumnKind>,
}

fn default_number_width() -> f32 {
    44.0
}

fn default_text_width() -> f32 {
    180.0
}

fn default_time_width() -> f32 {
    72.0
}

fn default_column_order() -> Vec<ColumnKind> {
    ColumnKind::ALL.to_vec()
}

impl Default for ColumnLayout {
    fn default() -> Self {
        Self {
            number: default_number_width(),
            artist: default_text_width(),
            album: default_text_width(),
            time: default_time_width(),
            order: default_column_order(),
        }
    }
}

impl ColumnLayout {
    /// Replace values a hand-edited or damaged settings file could hold.
    fn sanitized(self) -> Self {
        let fix = |value: f32, min: f32, default: f32| {
            if value.is_finite() {
                value.clamp(min, 2000.0)
            } else {
                default
            }
        };
        let complete = self.order.len() == ColumnKind::ALL.len()
            && ColumnKind::ALL.iter().all(|kind| self.order.contains(kind));
        Self {
            number: fix(self.number, MIN_NUMBER, default_number_width()),
            artist: fix(self.artist, MIN_TEXT, default_text_width()),
            album: fix(self.album, MIN_TEXT, default_text_width()),
            time: fix(self.time, MIN_TIME, default_time_width()),
            order: if complete { self.order } else { default_column_order() },
        }
    }

    /// Stored width. Title has none and reports 0.
    fn width(&self, kind: ColumnKind) -> f32 {
        match kind {
            ColumnKind::Number => self.number,
            ColumnKind::Title => 0.0,
            ColumnKind::Artist => self.artist,
            ColumnKind::Album => self.album,
            ColumnKind::Time => self.time,
        }
    }

    fn width_mut(&mut self, kind: ColumnKind) -> Option<&mut f32> {
        match kind {
            ColumnKind::Number => Some(&mut self.number),
            ColumnKind::Title => None,
            ColumnKind::Artist => Some(&mut self.artist),
            ColumnKind::Album => Some(&mut self.album),
            ColumnKind::Time => Some(&mut self.time),
        }
    }

    /// Width of each column in `order` for a row of `width`. Artist and album give way
    /// when title would fall under its minimum.
    fn fit(&self, width: f32) -> Vec<f32> {
        let avail = (width - COLUMN_PAD).max(0.0);
        let fixed: f32 = ColumnKind::ALL.iter().map(|&kind| self.width(kind)).sum();
        let deficit = MIN_TITLE - (avail - fixed);
        let flexible = self.artist + self.album;
        let keep = if deficit > 0.0 && flexible > 0.0 {
            ((flexible - deficit) / flexible).max(0.0)
        } else {
            1.0
        };
        let shrunk = |kind: ColumnKind| match kind {
            ColumnKind::Artist | ColumnKind::Album => self.width(kind) * keep,
            _ => self.width(kind),
        };
        let used: f32 = ColumnKind::ALL.iter().map(|&kind| shrunk(kind)).sum();
        let title = (avail - used).max(0.0);
        self.order
            .iter()
            .map(|&kind| if kind == ColumnKind::Title { title } else { shrunk(kind) })
            .collect()
    }

    /// Move the divider after column `index` by `dx`. The columns on both sides trade
    /// width, so the divider follows the pointer and the other edges stay put. Title
    /// absorbs the change when it is on one side.
    fn drag(&mut self, index: usize, dx: f32, width: f32) {
        let (Some(&left), Some(&right)) = (self.order.get(index), self.order.get(index + 1)) else {
            return;
        };
        let title_at = self.order.iter().position(|&kind| kind == ColumnKind::Title);
        let title = title_at.map_or(0.0, |at| self.fit(width)[at]);
        let title_room = (title - MIN_TITLE).max(0.0);
        let lo;
        let hi;
        if left == ColumnKind::Title {
            lo = -title_room;
            hi = self.width(right) - right.min_width();
        } else if right == ColumnKind::Title {
            lo = left.min_width() - self.width(left);
            hi = title_room;
        } else {
            lo = left.min_width() - self.width(left);
            hi = self.width(right) - right.min_width();
        }
        let dx = dx.clamp(lo.min(0.0), hi.max(0.0));
        if let Some(value) = self.width_mut(left) {
            *value += dx;
        }
        if let Some(value) = self.width_mut(right) {
            *value -= dx;
        }
    }

    /// Put both columns beside the divider after `index` back to their default widths.
    fn reset(&mut self, index: usize) {
        let defaults = Self::default();
        for at in [index, index + 1] {
            let Some(&kind) = self.order.get(at) else {
                continue;
            };
            if let Some(value) = self.width_mut(kind) {
                *value = defaults.width(kind);
            }
        }
    }

    /// Move column `from` to the gap `gap`, where gap 0 is before the first column and
    /// gap `len` is after the last. Returns false when nothing moved.
    fn move_column(&mut self, from: usize, gap: usize) -> bool {
        if from >= self.order.len() || gap > self.order.len() || gap == from || gap == from + 1 {
            return false;
        }
        let kind = self.order.remove(from);
        let to = if gap > from { gap - 1 } else { gap };
        self.order.insert(to, kind);
        true
    }
}

/// Each column in display order with its rectangle inside `rect`.
fn columns_for(rect: egui::Rect, layout: &ColumnLayout) -> Vec<(ColumnKind, egui::Rect)> {
    let mut x = rect.left() + COLUMN_PAD;
    layout
        .order
        .iter()
        .zip(layout.fit(rect.width()))
        .map(|(&kind, width)| {
            let column = egui::Rect::from_min_max(
                egui::pos2(x, rect.top()),
                egui::pos2(x + width, rect.bottom()),
            );
            x += width;
            (kind, column)
        })
        .collect()
}

/// The gap between columns nearest to `x`. Gap 0 is the left edge of the first column.
fn column_gap_at(columns: &[(ColumnKind, egui::Rect)], x: f32) -> usize {
    let mut edges = Vec::with_capacity(columns.len() + 1);
    if let Some((_, first)) = columns.first() {
        edges.push(first.left());
    }
    edges.extend(columns.iter().map(|(_, column)| column.right()));
    edges
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (*a - x).abs().total_cmp(&(*b - x).abs()))
        .map_or(0, |(gap, _)| gap)
}

/// Draw the header. Drag a divider to resize, double-click it to reset, and drag a
/// heading to move that column. Returns true when the layout changed.
fn column_header(ui: &mut egui::Ui, layout: &mut ColumnLayout) -> bool {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
    let columns = columns_for(rect, layout);
    let theme = theme::current(ui.ctx());
    let weak = theme.weak_text.0;
    let accent = theme.accent.0;
    let mut changed = false;

    // Headings first, so the dividers registered after them win where they overlap.
    let mut moving: Option<(usize, usize)> = None;
    let mut dropped: Option<(usize, usize)> = None;
    for (index, &(kind, column)) in columns.iter().enumerate() {
        let id = ui.id().with(("column_heading", kind as u8));
        let response = ui.interact(column, id, egui::Sense::drag());
        let pointer_x = response.interact_pointer_pos().map(|pos| pos.x);
        if response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            if let Some(x) = pointer_x {
                moving = Some((index, column_gap_at(&columns, x)));
            }
        } else if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if response.drag_stopped() {
            if let Some(x) = pointer_x {
                dropped = Some((index, column_gap_at(&columns, x)));
            }
        }
    }

    for (index, &(kind, column)) in columns.iter().enumerate() {
        let lifted = moving.is_some_and(|(from, _)| from == index);
        if lifted {
            ui.painter().rect_filled(column, 2.0, accent.gamma_multiply(0.18));
        }
        let color = if lifted { accent } else { weak };
        paint_text(ui, column, kind.label(), color, false, egui::Align2::LEFT_CENTER);
    }

    for index in 0..columns.len().saturating_sub(1) {
        let x = columns[index].1.right();
        let grab = egui::Rect::from_min_max(
            egui::pos2(x - DIVIDER_GRAB, rect.top()),
            egui::pos2(x + DIVIDER_GRAB, rect.bottom()),
        );
        // Keyed by the columns on both sides, so a reorder does not inherit a drag.
        let id = ui
            .id()
            .with(("column_divider", columns[index].0 as u8, columns[index + 1].0 as u8));
        let response = ui
            .interact(grab, id, egui::Sense::click_and_drag())
            .on_hover_cursor(egui::CursorIcon::ResizeColumn);
        if response.double_clicked() {
            layout.reset(index);
            changed = true;
        } else if response.dragged() {
            let dx = response.drag_delta().x;
            if dx != 0.0 {
                layout.drag(index, dx, width);
                changed = true;
            }
        }
        let active = moving.is_none() && (response.hovered() || response.dragged());
        let stroke = if active {
            egui::Stroke::new(2.0, accent)
        } else {
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
        };
        let inset = if active { 2.0 } else { 5.0 };
        ui.painter().vline(
            x,
            egui::Rangef::new(rect.top() + inset, rect.bottom() - inset),
            stroke,
        );
    }

    if let Some((from, gap)) = moving {
        if gap != from && gap != from + 1 {
            let x = if gap == 0 {
                columns[0].1.left()
            } else {
                columns[gap - 1].1.right()
            };
            ui.painter()
                .vline(x, egui::Rangef::new(rect.top(), rect.bottom()), egui::Stroke::new(3.0, accent));
        }
    }
    if let Some((from, gap)) = dropped {
        changed |= layout.move_column(from, gap);
    }
    changed
}

fn paint_text(
    ui: &egui::Ui,
    rect: egui::Rect,
    text: &str,
    color: egui::Color32,
    strong: bool,
    align: egui::Align2,
) {
    let mut font = egui::TextStyle::Body.resolve(ui.style());
    if strong {
        font.size += 0.5;
    }
    let anchor = match align {
        egui::Align2::RIGHT_CENTER => rect.right_center() - egui::vec2(6.0, 0.0),
        _ => rect.left_center() + egui::vec2(4.0, 0.0),
    };
    ui.painter().with_clip_rect(rect).text(anchor, align, text, font, color);
}

fn load_settings(path: &Path) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn ensure_json(mut path: PathBuf) -> PathBuf {
    if path.extension().is_none() {
        path.set_extension("json");
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(index: usize, group: Option<&str>, start: u64, title: &str) -> RowView {
        RowView {
            index,
            source_id: SourceId(1),
            segment: index,
            number: (index + 1) as u32,
            title: title.into(),
            artist: "Artist".into(),
            album: "Album".into(),
            frames: 1000,
            start_frame: start,
            sample_rate: 44100,
            format_name: "FLAC".into(),
            channels: 2,
            bitrate_bps: 900_000,
            bits_per_sample: 16,
            delay: 0,
            padding: 0,
            presentation_frames: start + 1000,
            group: group.map(str::to_string),
            error: None,
            path: PathBuf::from("/music/album.flac"),
        }
    }

    #[test]
    fn file_facts_read_as_player_labels() {
        assert_eq!(format_sample_rate(44_100), "44.1 kHz");
        assert_eq!(format_sample_rate(48_000), "48 kHz");
        assert_eq!(format_sample_rate(0), "—");
        assert_eq!(format_bitrate(320_000), "320 kbps");
        assert_eq!(format_bitrate(320_400), "320 kbps");
        assert_eq!(format_bitrate(0), "—");
        assert_eq!(format_channels(1), "Mono");
        assert_eq!(format_channels(2), "Stereo");
        assert_eq!(format_channels(6), "5.1");
        assert_eq!(format_bit_depth(24), "24-bit");
        assert_eq!(format_bit_depth(0), "—");
        assert_eq!(codec_label("FLAC", Path::new("album.flac")), "FLAC");
        assert_eq!(codec_label("AAC", Path::new("book.m4a")), "AAC · M4A");
        assert_eq!(codec_label("Vorbis", Path::new("track.ogg")), "Vorbis · OGG");
        assert_eq!(codec_label("WavPack", Path::new("tape.wv")), "WavPack · WV");
        assert_eq!(codec_label("AIFF", Path::new("take.aif")), "AIFF");
        assert_eq!(codec_label("Audio", Path::new("mystery.wv")), "WV");
    }

    #[test]
    fn playlist_playtime_sums_each_row() {
        let mut first = row(0, None, 0, "One");
        first.frames = 44_100 * 90;
        let mut second = row(1, None, 0, "Two");
        second.frames = 48_000 * 30;
        second.sample_rate = 48_000;
        assert_eq!(playlist_playtime(&[first, second]), "2:00");

        let mut unknown = row(0, None, 0, "Quiet");
        unknown.sample_rate = 0;
        unknown.frames = 44_100;
        assert_eq!(playlist_playtime(&[unknown]), "—");
        assert_eq!(playlist_playtime(&[]), "—");

        let mut long = row(0, None, 0, "Long");
        long.frames = 44_100 * (3600 + 125);
        assert_eq!(playlist_playtime(&[long]), "1:02:05");
    }

    #[test]
    fn highlighted_rows_keep_a_light_fill() {
        let themes = Themes::load(None);
        let theme = themes.get(theme::DEFAULT_THEME);
        let pair = |fill: theme::Hex, text: theme::Hex| Some((fill.0, text.0));
        assert_eq!(row_highlight(theme, true, true, true), pair(theme.accent, theme.accent_text));
        assert_eq!(
            row_highlight(theme, false, true, true),
            pair(theme.row_selected, theme.row_selected_text)
        );
        assert_eq!(row_highlight(theme, false, false, true), pair(theme.row_hover, theme.row_hover_text));
        assert_eq!(row_highlight(theme, false, false, false), None);
    }

    #[test]
    fn group_headers_are_not_tracks() {
        let rows = vec![
            row(0, Some("Dark Side"), 0, "Speak to Me"),
            row(1, None, 44100, "Breathe"),
        ];
        let lines = playlist_lines(&rows);
        assert!(matches!(lines[0], Line::Header(_)));
        assert!(matches!(lines[1], Line::Track(0)));
        assert!(matches!(lines[2], Line::Track(1)));
        assert_eq!(line_of(&lines, &rows, 1), Some(2));
    }

    #[test]
    fn saved_rows_keep_the_start_frame() {
        let saved = saved_from_rows(&[row(3, None, 88200, "On the Run")]);
        assert_eq!(saved.version, 1);
        assert_eq!(saved.rows.len(), 1);
        assert_eq!(saved.rows[0].start_frame, 88200);
        assert_eq!(saved.rows[0].title, "On the Run");
        assert_eq!(saved.rows[0].path, PathBuf::from("/music/album.flac"));
        assert!(saved.position.is_none());
        assert!(saved.rows[0].described());
        assert_eq!(saved.rows[0].artist, "Artist");
        assert_eq!(saved.rows[0].end_frame, 89200);
    }

    #[test]
    fn saved_playlist_keeps_the_play_position() {
        let rows = vec![row(3, None, 88200, "On the Run")];
        let position = SavedPosition {
            path: PathBuf::from("/music/album.flac"),
            start_frame: 88200,
            position_frames: 1000,
        };
        let saved = saved_playlist(&rows, Some(position.clone()));
        let json = saved.to_json().expect("json");
        let back = SavedPlaylist::from_json(&json).expect("parse");
        assert_eq!(back.position, Some(position));
        let snap = Snapshot {
            index: Some(0),
            position_frames: 1000,
            rows: std::sync::Arc::new(rows),
            ..Snapshot::default()
        };
        assert_eq!(playback_position(&snap), back.position);
        assert!(playback_position(&Snapshot::default()).is_none());
    }

    #[test]
    fn open_playlist_restores_the_position_and_startup_does_not() {
        let dir = std::env::temp_dir().join(format!(
            "seasound-playlist-place-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("chapter.flac");
        let file = dir.join("book.json");
        let saved = SavedPlaylist {
            version: 1,
            rows: vec![SavedRow {
                path: audio.clone(),
                start_frame: 0,
                end_frame: 1000,
                title: "Chapter".into(),
                sample_rate: 44100,
                channels: 2,
                presentation_frames: 1000,
                number: 1,
                format_name: "FLAC".into(),
                ..SavedRow::default()
            }],
            position: Some(SavedPosition {
                path: audio.clone(),
                start_frame: 0,
                position_frames: 50,
            }),
        };
        std::fs::write(&file, saved.to_json().unwrap()).unwrap();
        match finished_load(LoadWork::SavedFile {
            path: file.clone(),
            restore_position: true,
        }) {
            LoadDone::Saved {
                playlist,
                position: Some(position),
                ..
            } => {
                let start = resume_start(&playlist, &position).expect("place");
                assert_eq!(start.index, 0);
                assert!(playlist.sources[0].playable());
                assert!(playlist.sources[0].error.is_none());
                assert!(!audio.exists());
            }
            LoadDone::Saved { position: None, .. } => panic!("open dropped the position"),
            LoadDone::Session { .. } => panic!("wrong load"),
        }
        match finished_load(LoadWork::SavedFile {
            path: file,
            restore_position: false,
        }) {
            LoadDone::Saved { position: None, .. } => {}
            LoadDone::Saved { position: Some(_), .. } => panic!("startup used the position"),
            LoadDone::Session { .. } => panic!("wrong load"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_playlist_adds_each_file_before_the_load_finishes() {
        let dir = std::env::temp_dir().join(format!(
            "seasound-playlist-grow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let first = PathBuf::from("/no/such/seasound-001.mp3");
        let second = PathBuf::from("/no/such/seasound-002.mp3");
        let saved = SavedPlaylist {
            version: 1,
            rows: vec![
                SavedRow {
                    path: first.clone(),
                    start_frame: 0,
                    title: "One".into(),
                    ..SavedRow::default()
                },
                SavedRow {
                    path: second.clone(),
                    start_frame: 0,
                    title: "Two".into(),
                    ..SavedRow::default()
                },
            ],
            position: Some(SavedPosition {
                path: second.clone(),
                start_frame: 0,
                position_frames: 10,
            }),
        };
        let file = dir.join("book.json");
        std::fs::write(&file, saved.to_json().unwrap()).unwrap();
        let events = load_events(LoadWork::SavedFile {
            path: file,
            restore_position: true,
        });
        let playlists: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                LoadEvent::Extend { playlist, .. } => Some(playlist.sources.len()),
                LoadEvent::Priority { playlist, .. } => Some(playlist.sources.len()),
                _ => None,
            })
            .collect();
        assert!(
            playlists.len() >= 2,
            "rows should appear as each file is read, got {playlists:?}"
        );
        assert_eq!(playlists[0], 1);
        assert_eq!(*playlists.last().unwrap(), 2);
        assert!(matches!(events.last(), Some(LoadEvent::Settled)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropped_files_are_reported_one_at_a_time() {
        let first = PathBuf::from("/no/such/seasound-drop-a.mp3");
        let second = PathBuf::from("/no/such/seasound-drop-b.mp3");
        let events = load_events(LoadWork::Paths {
            paths: vec![first.clone(), second.clone()],
            import: Import::Append,
        });
        let paths: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                LoadEvent::Source { source, .. } => Some(source.path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, vec![first, second]);
        assert!(matches!(events.last(), Some(LoadEvent::Settled)));
        assert!(events.iter().any(|event| matches!(
            event,
            LoadEvent::Progress { done: 1, total: 0 }
        )));
    }

    #[test]
    fn a_saved_place_is_the_file_read_before_the_rest() {
        let first = PathBuf::from("/music/book/001.mp3");
        let second = PathBuf::from("/music/book/002.mp3");
        let paths = vec![first.clone(), second];
        assert!(early_file(&paths, None).is_none());
        assert!(early_file(
            &paths[..1],
            Some(&SavedPosition {
                path: first.clone(),
                start_frame: 0,
                position_frames: 10,
            })
        )
        .is_none());
        assert_eq!(
            early_file(
                &paths,
                Some(&SavedPosition {
                    path: first.clone(),
                    start_frame: 4,
                    position_frames: 10,
                })
            ),
            Some(first)
        );
        assert_eq!(loading_label(None), "Loading…");
        assert_eq!(loading_label(Some((1, 1))), "Loading…");
        assert_eq!(loading_label(Some((2, 40))), "Loading… 2 / 40");
        assert_eq!(loading_label(Some((3, 0))), "Loading… 3");
        let paths = vec![
            PathBuf::from("/music/book/001.mp3"),
            PathBuf::from("/music/book/002.mp3"),
            PathBuf::from("/music/book/003.mp3"),
        ];
        let early = paths[1].clone();
        assert_eq!(
            probe_order(&paths, Some(&early)),
            vec![paths[1].clone(), paths[2].clone(), paths[0].clone()]
        );
    }

    fn finished_load(work: LoadWork) -> LoadDone {
        let mut finished = None;
        for event in load_events(work) {
            match event {
                LoadEvent::Finished(done) => finished = Some(done),
                LoadEvent::Failed(message) => panic!("load failed: {message}"),
                LoadEvent::Priority { .. }
                | LoadEvent::Progress { .. }
                | LoadEvent::Source { .. }
                | LoadEvent::Extend { .. }
                | LoadEvent::Settled => {}
            }
        }
        finished.expect("the load did not finish")
    }

    fn load_events(work: LoadWork) -> Vec<LoadEvent> {
        let (tx, rx) = mpsc::channel();
        work.run(tx);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn span(index: usize, position: usize, top: f32) -> TrackSpan {
        TrackSpan {
            index,
            position,
            top,
            bottom: top + ROW_H,
        }
    }

    #[test]
    fn scrollbar_gutter_is_not_a_row_click() {
        let row = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(200.0, ROW_H));
        let gutter = vertical_bar_inset(true, 30.0 * ROW_H, 400.0, 10.0, 0.0);
        assert_eq!(gutter, 10.0);
        assert!(row_hit_rect(row, gutter).contains(egui::pos2(189.0, 10.0)));
        assert!(!row_hit_rect(row, gutter).contains(egui::pos2(191.0, 10.0)));
        assert_eq!(vertical_bar_inset(true, 200.0, 400.0, 10.0, 0.0), 0.0);
        assert!(row_hit_rect(row, 0.0).contains(egui::pos2(199.0, 10.0)));
        assert_eq!(vertical_bar_inset(false, 30.0 * ROW_H, 400.0, 10.0, 4.0), 0.0);
    }

    #[test]
    fn dragging_across_rows_selects_the_visual_range() {
        let rows = vec![
            row(0, Some("Dark Side"), 0, "Speak to Me"),
            row(1, None, 44100, "Breathe"),
            row(2, None, 88200, "On the Run"),
        ];
        let selected = indices_between(&rows, 2, 0);
        assert_eq!(selected.len(), 3);
        assert!(selected.contains(&0) && selected.contains(&1) && selected.contains(&2));
        assert_eq!(playlist_len(&rows), 3);
    }

    #[test]
    fn drop_line_follows_track_centers_and_headers() {
        // A header sits between track 0 (0..28) and track 1 (56..84).
        let tracks = vec![span(0, 0, 0.0), span(1, 1, 56.0)];
        assert_eq!(drop_before(10.0, &tracks, 3), Some(0));
        assert_eq!(row_position_at(40.0, &tracks), Some(1));
        assert_eq!(drop_before(40.0, &tracks, 3), Some(1));
        assert_eq!(drop_before(80.0, &tracks, 3), Some(2));
        assert_eq!(drop_before(90.0, &tracks, 2), Some(2));
        assert_eq!(insertion_y(1, &tracks, 3), Some(56.0));
        assert_eq!(insertion_y(2, &tracks, 3), Some(84.0));
        assert_eq!(insertion_y(3, &tracks, 3), Some(84.0));
        assert_eq!(insertion_y(5, &tracks, 6), None);
    }

    #[test]
    fn ghost_label_names_one_track_and_counts_the_rest() {
        assert_eq!(ghost_label(1, "Breathe"), "Breathe");
        assert_eq!(ghost_label(3, "Breathe"), "3 tracks");
        let long = "a".repeat(60);
        assert_eq!(ghost_label(1, &long).chars().count(), 49);
        let rows = vec![row(4, None, 0, "Speak to Me"), row(7, None, 1, "Breathe")];
        let mut selected = HashSet::new();
        selected.insert(7);
        assert_eq!(ghost_text(&rows, &selected), "Breathe");
        selected.insert(4);
        assert_eq!(ghost_text(&rows, &selected), "2 tracks");
    }

    fn chaptered(path: &str) -> Source {
        Source {
            id: SourceId(0),
            path: PathBuf::from(path),
            format: seasound_model::AudioFormat::Flac,
            sample_rate: 44100,
            channels: 2,
            bitrate_bps: 0,
            bits_per_sample: 16,
            delay: 0,
            padding: 0,
            presentation_frames: 2000,
            artist: String::new(),
            album: String::new(),
            title: String::new(),
            disc: 1,
            track_number: 1,
            segments: vec![
                seasound_model::Segment {
                    number: 1,
                    title: "One".into(),
                    artist: String::new(),
                    album: String::new(),
                    start_frame: 0,
                    end_frame: 1000,
                },
                seasound_model::Segment {
                    number: 2,
                    title: "Two".into(),
                    artist: String::new(),
                    album: String::new(),
                    start_frame: 1000,
                    end_frame: 2000,
                },
            ],
            group_title: Some("Album".into()),
            error: None,
            warning: None,
            format_label: String::new(),
            cover: None,
        }
    }

    #[test]
    fn resume_finds_the_chapter_and_clamps_the_fraction() {
        let playlist = Playlist::new(vec![chaptered("/music/album.flac")]);
        let start = resume_start(
            &playlist,
            &SavedPosition {
                path: "/music/album.flac".into(),
                start_frame: 1000,
                position_frames: 250,
            },
        )
        .expect("chapter");
        assert_eq!(start.index, 1);
        assert!((start.fraction - 0.25).abs() < 0.000_1);
        assert!(resume_start(
            &playlist,
            &SavedPosition {
                path: "/music/other.flac".into(),
                start_frame: 0,
                position_frames: 10,
            },
        )
        .is_none());
        let clamped = resume_start(
            &playlist,
            &SavedPosition {
                path: "/music/album.flac".into(),
                start_frame: 0,
                position_frames: 50_000,
            },
        )
        .expect("first chapter");
        assert_eq!(clamped.index, 0);
        assert_eq!(clamped.fraction, 0.999);
    }

    #[test]
    fn resume_uses_the_saved_app_playlist_not_an_opened_file() {
        let dir = std::env::temp_dir().join(format!(
            "seasound-resume-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let opened = dir.join("opened.json");
        std::fs::write(&opened, "{\"version\":1,\"rows\":[]}").unwrap();
        let session = SessionPlayback {
            rows: vec![SavedRow {
                path: PathBuf::from("/music/in-app.flac"),
                start_frame: 0,
                title: "In app".into(),
                ..SavedRow::default()
            }],
            track: Some(SavedPosition {
                path: PathBuf::from("/music/in-app.flac"),
                start_frame: 0,
                position_frames: 10,
            }),
        };
        match startup_load(true, Some(session.clone()), Some(opened.clone())) {
            StartupLoad::Session(got) => {
                assert_eq!(got.rows[0].path, PathBuf::from("/music/in-app.flac"));
            }
            other => panic!("expected the in-app playlist, got {other:?}"),
        }
        match startup_load(false, Some(session), Some(opened.clone())) {
            StartupLoad::File(path) => assert_eq!(path, opened),
            other => panic!("expected the opened file, got {other:?}"),
        }
        match startup_load(true, None, Some(opened.clone())) {
            StartupLoad::File(path) => assert_eq!(path, opened),
            other => panic!("expected the saved path before a session exists, got {other:?}"),
        }
        let cleared = SessionPlayback {
            rows: Vec::new(),
            track: None,
        };
        match startup_load(true, Some(cleared), Some(opened)) {
            StartupLoad::Empty => {}
            other => panic!("a cleared playlist should not reopen a file, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_roundtrip_keeps_resume_and_old_files_default_it_on() {
        let parsed: Settings = serde_json::from_str(r#"{"volume":0.4,"muted":true}"#).unwrap();
        assert!(parsed.resume_playback);
        assert!(parsed.auto_save_playlists);
        assert!(parsed.session.is_none());
        assert!((parsed.volume - 0.4).abs() < f32::EPSILON);
        let session = SessionPlayback {
            rows: vec![SavedRow {
                path: PathBuf::from("/music/in-app.flac"),
                start_frame: 5,
                title: "A".into(),
                ..SavedRow::default()
            }],
            track: Some(SavedPosition {
                path: PathBuf::from("/music/in-app.flac"),
                start_frame: 5,
                position_frames: 12,
            }),
        };
        let stored = Settings {
            resume_playback: true,
            session: Some(session.clone()),
            ..Settings::default()
        };
        let json = serde_json::to_string(&stored).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session, Some(session));
        assert!(back.resume_playback);
        let off = Settings {
            resume_playback: false,
            session: None,
            ..Settings::default()
        };
        let off_json = serde_json::to_string(&off).unwrap();
        assert!(!off_json.contains("session"));
        let off_back: Settings = serde_json::from_str(&off_json).unwrap();
        assert!(!off_back.resume_playback);
        assert!(off_back.auto_save_playlists);
        let manual = Settings {
            auto_save_playlists: false,
            ..Settings::default()
        };
        let manual_back: Settings =
            serde_json::from_str(&serde_json::to_string(&manual).unwrap()).unwrap();
        assert!(!manual_back.auto_save_playlists);
        let dir = PathBuf::from("/tmp/seasound-config");
        let open = PathBuf::from("/music/album.json");
        assert_eq!(
            quit_playlist_path(true, Some(&open), Some(&dir)),
            Some(open.clone())
        );
        assert_eq!(quit_playlist_path(false, Some(&open), Some(&dir)), None);
        assert_eq!(
            quit_playlist_path(false, None, Some(&dir)),
            Some(dir.join("playlist.json"))
        );
        assert_eq!(
            quit_playlist_path(true, None, Some(&dir)),
            Some(dir.join("playlist.json"))
        );
        assert_eq!(quit_playlist_path(false, None, None), None);
    }

    #[test]
    fn column_dividers_follow_the_pointer_and_respect_minimums() {
        let width = 800.0;
        let edges = |layout: &ColumnLayout| {
            let mut x = COLUMN_PAD;
            layout
                .fit(width)
                .into_iter()
                .map(|column| {
                    x += column;
                    x
                })
                .collect::<Vec<f32>>()
        };
        let mut layout = ColumnLayout::default();
        let before = edges(&layout);
        // Artist | Album
        layout.drag(2, 30.0, width);
        let after = edges(&layout);
        assert_eq!(after[2], before[2] + 30.0);
        assert_eq!(after[1], before[1]);
        assert_eq!(after[3], before[3]);
        // Title | Artist
        layout.drag(1, -50.0, width);
        assert_eq!(edges(&layout)[1], before[1] - 50.0);
        assert_eq!(edges(&layout)[2], after[2]);
        layout.drag(3, 1000.0, width);
        assert_eq!(layout.time, MIN_TIME);
        layout.drag(0, -1000.0, width);
        assert_eq!(layout.number, MIN_NUMBER);
        layout.drag(1, 5000.0, width);
        assert!(layout.fit(width)[1] >= MIN_TITLE - 0.01);
        let total: f32 = layout.fit(width).iter().sum();
        assert!((total - (width - COLUMN_PAD)).abs() < 0.01);
        layout.reset(2);
        assert_eq!(layout.artist, default_text_width());
        assert_eq!(layout.album, default_text_width());
    }

    #[test]
    fn dividers_still_follow_the_pointer_after_a_reorder() {
        let width = 900.0;
        let mut layout = ColumnLayout::default();
        // Time, #, Album, Title, Artist
        layout.order = vec![
            ColumnKind::Time,
            ColumnKind::Number,
            ColumnKind::Album,
            ColumnKind::Title,
            ColumnKind::Artist,
        ];
        let right_edges = |layout: &ColumnLayout| {
            let mut x = 0.0;
            layout
                .fit(width)
                .into_iter()
                .map(|column| {
                    x += column;
                    x
                })
                .collect::<Vec<f32>>()
        };
        let before = right_edges(&layout);
        for divider in 0..4 {
            let mut moved = layout.clone();
            moved.drag(divider, 12.0, width);
            let after = right_edges(&moved);
            for (edge, (a, b)) in after.iter().zip(&before).enumerate() {
                let expected = if edge == divider { b + 12.0 } else { *b };
                assert!((a - expected).abs() < 0.01, "divider {divider} edge {edge}: {a} vs {expected}");
            }
        }
    }

    #[test]
    fn dragging_a_heading_moves_that_column() {
        use ColumnKind::*;
        let mut layout = ColumnLayout::default();
        // Dropping beside itself changes nothing.
        assert!(!layout.move_column(2, 2));
        assert!(!layout.move_column(2, 3));
        assert_eq!(layout.order, vec![Number, Title, Artist, Album, Time]);
        // Time to the front.
        assert!(layout.move_column(4, 0));
        assert_eq!(layout.order, vec![Time, Number, Title, Artist, Album]);
        // Number to the end.
        assert!(layout.move_column(1, 5));
        assert_eq!(layout.order, vec![Time, Title, Artist, Album, Number]);
        // Album right, past Number.
        assert!(layout.move_column(3, 5));
        assert_eq!(layout.order, vec![Time, Title, Artist, Number, Album]);
        assert!(!layout.move_column(9, 0));
    }

    #[test]
    fn heading_drop_picks_the_nearest_gap() {
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(808.0, 22.0));
        let columns = columns_for(rect, &ColumnLayout::default());
        assert_eq!(column_gap_at(&columns, -50.0), 0);
        assert_eq!(column_gap_at(&columns, columns[2].1.right() + 3.0), 3);
        assert_eq!(column_gap_at(&columns, columns[2].1.left() + 2.0), 2);
        assert_eq!(column_gap_at(&columns, 5000.0), 5);
    }

    #[test]
    fn narrow_window_shrinks_artist_and_album_before_title() {
        let layout = ColumnLayout::default();
        let fitted = layout.fit(300.0);
        assert!((fitted[1] - MIN_TITLE).abs() < 0.01);
        assert!(fitted[2] < layout.artist);
        assert_eq!(fitted[0], layout.number);
        assert_eq!(fitted[4], layout.time);
    }

    #[test]
    fn column_widths_persist_and_old_settings_get_defaults() {
        let parsed: Settings = serde_json::from_str(r#"{"volume":0.4}"#).unwrap();
        assert_eq!(parsed.columns, ColumnLayout::default());
        let stored = Settings {
            columns: ColumnLayout {
                number: 30.0,
                artist: 120.0,
                album: 90.0,
                time: 60.0,
                order: vec![
                    ColumnKind::Album,
                    ColumnKind::Title,
                    ColumnKind::Time,
                    ColumnKind::Number,
                    ColumnKind::Artist,
                ],
            },
            ..Settings::default()
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&stored).unwrap()).unwrap();
        assert_eq!(back.columns, stored.columns);
        let bad = ColumnLayout {
            number: f32::NAN,
            artist: 1.0,
            order: vec![ColumnKind::Title, ColumnKind::Title],
            ..ColumnLayout::default()
        }
        .sanitized();
        assert_eq!(bad.number, default_number_width());
        assert_eq!(bad.artist, MIN_TEXT);
        assert_eq!(bad.order, default_column_order());
        let old: Settings =
            serde_json::from_str(r#"{"columns":{"number":30,"artist":100,"album":100,"time":60}}"#).unwrap();
        assert_eq!(old.columns.order, default_column_order());
        assert_eq!(old.columns.number, 30.0);
    }
}
