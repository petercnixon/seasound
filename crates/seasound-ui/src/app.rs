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
    format_time, load_saved_playlist, Cover, DecodeParams, PlayState, Playlist, Repeat, RowView,
    SavedPlaylist, SavedRow, Source, SourceId,
};
use seasound_playback::{Command, Engine, Opener, Snapshot};
use serde::{Deserialize, Serialize};

const ROW_H: f32 = 28.0;
const DRAG_SLOP: f32 = 8.0;
const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "mp4", "wav", "aiff", "aif", "ogg", "cue", "m3u", "m3u8",
];

const BG: egui::Color32 = egui::Color32::from_rgb(0x0e, 0x14, 0x18);
const PANEL: egui::Color32 = egui::Color32::from_rgb(0x14, 0x1c, 0x22);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x20, 0xa8, 0x9c);
const ERROR: egui::Color32 = egui::Color32::from_rgb(0xd0, 0x78, 0x70);
// Light fills so the row text can use the window background and stay readable.
const ROW_SELECTED: egui::Color32 = egui::Color32::from_rgb(0x5d, 0xce, 0xc3);
const ROW_HOVER: egui::Color32 = egui::Color32::from_rgb(0xa8, 0xdd, 0xd6);

pub struct SeaSoundApp {
    engine: Engine,
    snap: Snapshot,
    volume: f32,
    muted: bool,
    seek: f32,
    seek_drag: bool,
    selected: HashSet<usize>,
    row_drag: Option<RowDrag>,
    covers: HashMap<SourceId, Cover>,
    warnings: HashMap<SourceId, String>,
    textures: HashMap<SourceId, egui::TextureHandle>,
    cover_failed: HashSet<SourceId>,
    load_rx: Option<Receiver<LoadDone>>,
    loading: bool,
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
    SavedFile(PathBuf),
}

enum LoadDone {
    Sources { sources: Vec<Source>, import: Import },
    Saved { playlist: Playlist, path: PathBuf },
    Failed(String),
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
}

fn default_volume() -> f32 {
    0.8
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: default_volume(),
            muted: false,
            device: None,
            playlist: None,
            disabled_plugins: Vec::new(),
        }
    }
}

impl SeaSoundApp {
    pub fn new(creation: &eframe::CreationContext<'_>) -> Self {
        theme(&creation.egui_ctx);
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
            selected: HashSet::new(),
            row_drag: None,
            covers: HashMap::new(),
            warnings: HashMap::new(),
            textures: HashMap::new(),
            cover_failed: HashSet::new(),
            load_rx: None,
            loading: false,
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
        };
        if let Some(path) = settings.playlist {
            if path.is_file() {
                app.spawn(LoadWork::SavedFile(path));
            }
        }
        app
    }

    fn spawn(&mut self, work: LoadWork) {
        let (tx, rx) = mpsc::channel();
        self.load_rx = Some(rx);
        self.loading = true;
        self.settle = 30;
        thread::spawn(move || {
            let _ = tx.send(work.run());
        });
    }

    fn poll_load(&mut self) {
        let done = match self.load_rx.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(done) => Some(done),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(LoadDone::Failed("reading stopped".into())),
            },
            None => None,
        };
        let Some(done) = done else {
            return;
        };
        self.load_rx = None;
        self.loading = false;
        self.settle = 30;
        match done {
            LoadDone::Sources { sources, import } => match import {
                Import::Replace => self.install_sources(sources, true),
                Import::Append => self.append_sources(sources),
            },
            LoadDone::Saved { playlist, path } => self.install_playlist(playlist, false, Some(path)),
            LoadDone::Failed(message) => self.notice = Some(message),
        }
    }

    fn install_sources(&mut self, sources: Vec<Source>, autoplay: bool) {
        self.install_playlist(Playlist::new(sources), autoplay, None);
    }

    fn append_sources(&mut self, sources: Vec<Source>) {
        let mut extra = Playlist::new(sources);
        let (covers, warnings) = take_art(&mut extra);
        self.covers.extend(covers);
        self.warnings.extend(warnings);
        self.engine.command(Command::Append(extra));
        self.settle = 30;
    }

    fn clear_playlist(&mut self) {
        self.load_rx = None;
        self.loading = false;
        self.engine.command(Command::Clear);
        self.covers.clear();
        self.warnings.clear();
        self.textures.clear();
        self.cover_failed.clear();
        self.forget_selection();
        self.seek = 0.0;
        self.seek_drag = false;
        self.last_followed = None;
        self.notice = None;
        self.settle = 15;
    }

    fn forget_selection(&mut self) {
        self.selected.clear();
        self.row_drag = None;
    }

    fn install_playlist(&mut self, mut playlist: Playlist, autoplay: bool, path: Option<PathBuf>) {
        let (covers, warnings) = take_art(&mut playlist);
        let repeat = self.snap.repeat;
        let shuffle = self.snap.shuffle;
        self.engine.command(Command::SetPlaylist { playlist, autoplay });
        self.engine.command(Command::SetRepeat(repeat));
        self.engine.command(Command::SetShuffle(shuffle));
        self.covers = covers;
        self.warnings = warnings;
        self.textures.clear();
        self.cover_failed.clear();
        self.forget_selection();
        self.seek = 0.0;
        self.seek_drag = false;
        self.last_followed = None;
        if let Some(path) = path {
            self.playlist_path = Some(path);
        }
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
                        .add_enabled(!self.snap.rows.is_empty(), egui::Button::new("Clear playlist"))
                        .clicked()
                    {
                        self.clear_playlist();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Open playlist…").clicked() {
                        self.dialog = Some(Dialog::OpenPlaylist);
                        ui.close_menu();
                    }
                    if ui.button("Save playlist…").clicked() {
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
                            ui.colored_label(ERROR, format!("{name} — {error}"));
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
            });
        });
    }

    fn transport(&mut self, ctx: &egui::Context) {
        let snap = self.snap.clone();
        if !self.seek_drag {
            self.seek = if snap.segment_frames > 0 {
                (snap.position_frames as f32 / snap.segment_frames as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
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
                        let repeat = colored(snap.repeat.label(), snap.repeat != Repeat::Off);
                        if ui.button(repeat).clicked() {
                            self.engine.command(Command::SetRepeat(snap.repeat.next()));
                            self.settle = 10;
                        }
                        let shuffle = colored(
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
                            self.engine.command(Command::SeekFraction(self.seek));
                        }
                        ui.label(end);
                    });
                    ui.horizontal(|ui| {
                        let mute = if self.muted { "Muted" } else { "Volume" };
                        if ui.button(colored(mute, self.muted)).clicked() {
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
        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(0x0a, 0x10, 0x14));
        ui.painter().rect_stroke(
            rect.shrink(10.0),
            2.0,
            egui::Stroke::new(2.0, ACCENT),
            egui::StrokeKind::Inside,
        );
    }

    fn status_line(&self, ui: &mut egui::Ui, snap: &Snapshot, source_id: Option<SourceId>) {
        let mut parts: Vec<String> = Vec::new();
        if self.loading {
            parts.push("Reading…".into());
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
        let color = if snap.error.is_some() || self.notice.is_some() {
            ERROR
        } else {
            ui.visuals().weak_text_color()
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
                            "Reading…"
                        } else if hovering {
                            "Drop audio to add it"
                        } else {
                            "No tracks yet"
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
                ui.label(egui::RichText::new("Drop audio to add it").color(ACCENT));
            }
            column_header(ui);
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
        match &slot.line {
            Line::Header(title) => {
                ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(0x10, 0x18, 0x1e));
                paint_text(
                    ui,
                    rect.shrink2(egui::vec2(10.0, 0.0)),
                    title,
                    ACCENT,
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
                let fill = row_highlight(playing, selected, pointer_on_row(slot, bar_inset));
                if let Some(fill) = fill {
                    ui.painter().rect_filled(rect, 0.0, fill);
                }
                if playing {
                    let bar = egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + 3.0, rect.bottom()));
                    ui.painter().rect_filled(bar, 0.0, BG);
                }
                if moving && selected {
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(1.0, BG),
                        egui::StrokeKind::Inside,
                    );
                }
                let columns = columns_for(rect);
                let color = if fill.is_some() {
                    BG
                } else if row.error.is_some() {
                    ERROR
                } else {
                    ui.visuals().text_color()
                };
                let number = if row.number == 0 {
                    String::new()
                } else {
                    row.number.to_string()
                };
                paint_text(ui, columns.number, &number, color, false, egui::Align2::LEFT_CENTER);
                paint_text(ui, columns.title, &row_title(row), color, playing, egui::Align2::LEFT_CENTER);
                paint_text(ui, columns.artist, &row.artist, color, false, egui::Align2::LEFT_CENTER);
                paint_text(ui, columns.album, &row.album, color, false, egui::Align2::LEFT_CENTER);
                let time = if let Some(error) = &row.error {
                    error.clone()
                } else {
                    format_time(row.frames, row.sample_rate.max(1))
                };
                paint_text(ui, columns.time, &time, color, false, egui::Align2::RIGHT_CENTER);
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
        paint_insertion_line(ui.painter(), slot.rect.left(), slot.rect.right(), y);
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
                    self.spawn(LoadWork::Paths { paths, import });
                }
            }
            Dialog::Folder(import) => {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
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
                    self.spawn(LoadWork::SavedFile(path));
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
        match saved_from_rows(&self.snap.rows).to_json() {
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
        let path = match self.playlist_path.clone() {
            Some(path) => path,
            None => {
                let Some(dir) = &self.config_dir else {
                    return;
                };
                dir.join("playlist.json")
            }
        };
        let before = self.playlist_path.clone();
        self.write_playlist_file(&path);
        if before.is_none() && path.is_file() {
            self.playlist_path = Some(path);
        }
    }
}

impl eframe::App for SeaSoundApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.snap = self.engine.snapshot();
        if !self.snap.rows.is_empty() {
            self.seen_rows = true;
        }
        self.poll_load();
        self.take_drops(ctx);
        self.handle_keys(ctx);
        self.retitle(ctx);
        self.menu(ctx);
        self.transport(ctx);
        self.playlist_panel(ctx);
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
        self.snap = self.engine.snapshot();
        if !self.snap.rows.is_empty() {
            self.seen_rows = true;
        }
        self.autosave_playlist();
        self.write_settings();
    }
}

impl LoadWork {
    fn run(self) -> LoadDone {
        match self {
            LoadWork::Paths { paths, import } => LoadDone::Sources {
                sources: seasound_media::load_paths(&paths),
                import,
            },
            LoadWork::SavedFile(path) => match std::fs::read_to_string(&path) {
                Ok(text) => match playlist_from_text(&text) {
                    Ok(playlist) => LoadDone::Saved { playlist, path },
                    Err(message) => LoadDone::Failed(message),
                },
                Err(err) => LoadDone::Failed(err.to_string()),
            },
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

fn playlist_from_text(text: &str) -> Result<Playlist, String> {
    let saved = SavedPlaylist::from_json(text).map_err(|err| err.to_string())?;
    let mut paths = Vec::new();
    for row in &saved.rows {
        if !paths.iter().any(|path| path == &row.path) {
            paths.push(row.path.clone());
        }
    }
    let probed = seasound_media::load_paths(&paths);
    Ok(load_saved_playlist(&saved, probed))
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
    SavedPlaylist {
        version: 1,
        rows: rows
            .iter()
            .map(|row| SavedRow {
                path: row.path.clone(),
                start_frame: row.start_frame,
                title: row.title.clone(),
            })
            .collect(),
    }
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

fn paint_insertion_line(painter: &egui::Painter, left: f32, right: f32, y: f32) {
    let line = egui::Rect::from_min_max(egui::pos2(left, y - 1.0), egui::pos2(right, y + 1.0));
    painter.rect_filled(line, 0.0, ACCENT);
    let nub = egui::Rect::from_min_max(egui::pos2(left, y - 5.0), egui::pos2(left + 8.0, y + 5.0));
    painter.rect_filled(nub, 2.0, ACCENT);
}

fn paint_ghost(ctx: &egui::Context, pointer: egui::Pos2, label: &str) {
    let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("playlist-drag-ghost"));
    let painter = ctx.layer_painter(layer);
    let font = egui::TextStyle::Body.resolve(ctx.style().as_ref());
    let galley = painter.layout_no_wrap(label.to_owned(), font, egui::Color32::WHITE);
    let pad = egui::vec2(10.0, 6.0);
    let origin = pointer + egui::vec2(14.0, 16.0);
    let rect = egui::Rect::from_min_size(origin, galley.size() + pad * 2.0);
    painter.rect_filled(
        rect,
        4.0,
        egui::Color32::from_rgba_unmultiplied(0x14, 0x1c, 0x22, 235),
    );
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, ACCENT), egui::StrokeKind::Inside);
    painter.galley(rect.min + pad, galley, egui::Color32::WHITE);
}

fn row_highlight(playing: bool, selected: bool, hovered: bool) -> Option<egui::Color32> {
    if playing {
        Some(ACCENT)
    } else if selected {
        Some(ROW_SELECTED)
    } else if hovered {
        Some(ROW_HOVER)
    } else {
        None
    }
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
    let weak = ui.visuals().weak_text_color();
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

fn colored(text: &str, active: bool) -> egui::RichText {
    let text = egui::RichText::new(text);
    if active {
        text.color(ACCENT)
    } else {
        text
    }
}

fn consume(ctx: &egui::Context, key: egui::Key) -> bool {
    ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, key))
}

fn theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = BG;
    visuals.panel_fill = PANEL;
    visuals.faint_bg_color = BG;
    visuals.extreme_bg_color = egui::Color32::from_rgb(0x0a, 0x10, 0x14);
    visuals.code_bg_color = PANEL;
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke.color = ACCENT;
    visuals.hyperlink_color = ACCENT;
    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x2a, 0x38, 0x42);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x34, 0x46, 0x50);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0x1a, 0x6e, 0x66);
    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;
    style.spacing.slider_rail_height = 6.0;
    ctx.set_style(style);
}

struct Columns {
    number: egui::Rect,
    title: egui::Rect,
    artist: egui::Rect,
    album: egui::Rect,
    time: egui::Rect,
}

fn columns_for(rect: egui::Rect) -> Columns {
    let number = 44.0;
    let time = 72.0;
    let rest = (rect.width() - number - time - 8.0).max(90.0);
    let artist = rest * 0.26;
    let album = rest * 0.26;
    let title = rest - artist - album;
    let y0 = rect.top();
    let y1 = rect.bottom();
    let mut x = rect.left() + 8.0;
    let number_rect = egui::Rect::from_min_max(egui::pos2(x, y0), egui::pos2(x + number, y1));
    x += number;
    let title_rect = egui::Rect::from_min_max(egui::pos2(x, y0), egui::pos2(x + title, y1));
    x += title;
    let artist_rect = egui::Rect::from_min_max(egui::pos2(x, y0), egui::pos2(x + artist, y1));
    x += artist;
    let album_rect = egui::Rect::from_min_max(egui::pos2(x, y0), egui::pos2(x + album, y1));
    x += album;
    let time_rect = egui::Rect::from_min_max(egui::pos2(x, y0), egui::pos2(x + time, y1));
    Columns {
        number: number_rect,
        title: title_rect,
        artist: artist_rect,
        album: album_rect,
        time: time_rect,
    }
}

fn column_header(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
    let columns = columns_for(rect);
    let color = ui.visuals().weak_text_color();
    for (label, column, align) in [
        ("#", columns.number, egui::Align2::LEFT_CENTER),
        ("Title", columns.title, egui::Align2::LEFT_CENTER),
        ("Artist", columns.artist, egui::Align2::LEFT_CENTER),
        ("Album", columns.album, egui::Align2::LEFT_CENTER),
        ("Time", columns.time, egui::Align2::RIGHT_CENTER),
    ] {
        paint_text(ui, column, label, color, false, align);
    }
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
        assert_eq!(row_highlight(true, true, true), Some(ACCENT));
        assert_eq!(row_highlight(false, true, true), Some(ROW_SELECTED));
        assert_eq!(row_highlight(false, false, true), Some(ROW_HOVER));
        assert_eq!(row_highlight(false, false, false), None);
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
}
