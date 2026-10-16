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
const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "mp4", "wav", "aiff", "aif", "ogg", "cue", "m3u", "m3u8",
];

const BG: egui::Color32 = egui::Color32::from_rgb(0x0e, 0x14, 0x18);
const PANEL: egui::Color32 = egui::Color32::from_rgb(0x14, 0x1c, 0x22);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x20, 0xa8, 0x9c);
const ERROR: egui::Color32 = egui::Color32::from_rgb(0xd0, 0x78, 0x70);

pub struct SeaSoundApp {
    engine: Engine,
    snap: Snapshot,
    volume: f32,
    muted: bool,
    seek: f32,
    seek_drag: bool,
    selected: Option<usize>,
    drag_from: Option<usize>,
    drag_travel: f32,
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

enum Line {
    Header(String),
    Track(usize),
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
            selected: None,
            drag_from: None,
            drag_travel: 0.0,
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
        self.selected = None;
        self.drag_from = None;
        self.drag_travel = 0.0;
        self.seek = 0.0;
        self.seek_drag = false;
        self.last_followed = None;
        self.notice = None;
        self.settle = 15;
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
        self.selected = None;
        self.drag_from = None;
        self.drag_travel = 0.0;
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
        if consume(ctx, egui::Key::Delete) {
            if let Some(index) = self.selected {
                self.engine.command(Command::Remove(index));
                self.selected = None;
                self.settle = 15;
            }
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
            // The seek rail must use the panel width. Nested `horizontal` /
            // `vertical` calls shrink-wrap, and `Slider` paints `spacing.slider_width`
            // (default 100) rather than the rectangle passed to `add_sized`.
            let panel_width = ui.available_width();
            let cover = 84.0;
            let gap = 8.0;
            let column_width =
                (panel_width - cover - gap - ui.spacing().item_spacing.x).max(160.0);
            ui.horizontal_top(|ui| {
                self.cover_box(ui, source_id);
                ui.add_space(gap);
                ui.allocate_ui_with_layout(
                    egui::vec2(column_width, 132.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                    ui.set_min_width(column_width);
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
            let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
            if let Some(offset) = self.pending_scroll.take() {
                scroll = scroll.vertical_scroll_offset(offset);
            }
            let mut rects = Vec::new();
            scroll.show_rows(ui, ROW_H, lines.len(), |ui, range| {
                for line_i in range {
                    self.paint_line(ui, &rows, &lines[line_i], playing, &mut rects);
                }
            });
            self.finish_drag(ctx, &rects);
        });
    }

    fn paint_line(
        &mut self,
        ui: &mut egui::Ui,
        rows: &[RowView],
        line: &Line,
        playing_index: Option<usize>,
        rects: &mut Vec<(usize, egui::Rect)>,
    ) {
        let width = ui.available_width();
        let (rect, response) = match line {
            Line::Header(_) => ui.allocate_exact_size(egui::vec2(width, ROW_H), egui::Sense::hover()),
            Line::Track(_) => {
                ui.allocate_exact_size(egui::vec2(width, ROW_H), egui::Sense::click_and_drag())
            }
        };
        match line {
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
                let selected = self.selected == Some(row.index);
                if playing {
                    ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgba_unmultiplied(0x20, 0xa8, 0x9c, 48));
                    let bar = egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + 3.0, rect.bottom()));
                    ui.painter().rect_filled(bar, 0.0, ACCENT);
                } else if selected || response.hovered() {
                    ui.painter().rect_filled(rect, 0.0, egui::Color32::from_white_alpha(14));
                }
                let columns = columns_for(rect);
                let color = if row.error.is_some() {
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
                if let Some(warning) = self.warnings.get(&row.source_id) {
                    response.clone().on_hover_text(warning.as_str());
                }
                if response.double_clicked() {
                    self.selected = Some(row.index);
                    if row.error.is_none() {
                        self.engine.command(Command::PlayIndex(row.index));
                        self.settle = 15;
                    }
                } else if response.clicked() {
                    self.selected = Some(row.index);
                }
                if response.drag_started() {
                    self.drag_from = Some(row.index);
                    self.drag_travel = 0.0;
                }
                if response.dragged() {
                    self.drag_travel += response.drag_delta().length();
                }
                rects.push((row.index, rect));
            }
        }
    }

    fn finish_drag(&mut self, ctx: &egui::Context, rects: &[(usize, egui::Rect)]) {
        let down = ctx.input(|input| input.pointer.any_down());
        if self.drag_from.is_none() || down {
            return;
        }
        let from = self.drag_from.take().unwrap();
        let travel = self.drag_travel;
        self.drag_travel = 0.0;
        if travel <= 8.0 {
            return;
        }
        let Some(pos) = ctx.input(|input| input.pointer.interact_pos()) else {
            return;
        };
        let Some(&(to, _)) = rects.iter().rev().find(|(_, rect)| rect.contains(pos)) else {
            return;
        };
        if to != from {
            self.engine.command(Command::Reorder { from, to });
            self.selected = Some(to);
            self.settle = 15;
        }
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
                if let Some(paths) = rfd::FileDialog::new()
                    .add_filter("Audio", AUDIO_EXT)
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
        if self.loading || self.snap.state == PlayState::Playing || self.settle > 0 || self.dialog.is_some()
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
            group: group.map(str::to_string),
            error: None,
            path: PathBuf::from("/music/album.flac"),
        }
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
}
