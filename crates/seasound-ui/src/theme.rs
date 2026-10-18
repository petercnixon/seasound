//! Colour themes. Built-in themes are JSON files compiled into the binary. Users add
//! their own as `*.json` in the `themes` folder of the config directory.
//!
//! A theme file names a `base` (`dark` or `light`) and overrides any of its colours.
//! Colours are `#rrggbb` or `#rrggbbaa`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, Color32};
use serde::{Deserialize, Deserializer};

pub const DEFAULT_THEME: &str = "Dark";

const BUILT_IN: &[&str] = &[
    include_str!("../themes/dark.json"),
    include_str!("../themes/light.json"),
    include_str!("../themes/strawberry-milkshake.json"),
];

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Theme {
    pub name: String,
    /// `dark` or `light`. Picks egui's defaults for anything the file does not set.
    #[serde(deserialize_with = "base_is_dark", rename = "base")]
    pub dark: bool,
    /// Window and modal fill. Also the notch on the playing row.
    pub background: Hex,
    /// Menu bar, transport, and playlist panels.
    pub panel: Hex,
    /// Text fields and the empty cover box.
    pub inset: Hex,
    pub group_header: Hex,
    pub group_header_text: Hex,
    /// Separators and column dividers.
    pub border: Hex,
    pub text: Hex,
    /// Column headings, the status line, and other secondary text.
    pub weak_text: Hex,
    /// Button and menu labels.
    pub control_text: Hex,
    pub control_text_hover: Hex,
    pub accent: Hex,
    /// Text drawn on an accent fill: the playing row and selected items.
    pub accent_text: Hex,
    pub error: Hex,
    pub row_selected: Hex,
    pub row_selected_text: Hex,
    pub row_hover: Hex,
    pub row_hover_text: Hex,
    /// Checkbox boxes and slider rails.
    pub control: Hex,
    pub control_hover: Hex,
    pub control_active: Hex,
    pub button: Hex,
    pub button_hover: Hex,
    pub button_active: Hex,
}

/// A colour read from `#rrggbb` or `#rrggbbaa`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hex(pub Color32);

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_hex(&text)
            .map(Hex)
            .ok_or_else(|| serde::de::Error::custom(format!("`{text}` is not #rrggbb or #rrggbbaa")))
    }
}

fn parse_hex(text: &str) -> Option<Color32> {
    let digits = text.trim().strip_prefix('#')?;
    if !digits.is_ascii() || !(digits.len() == 6 || digits.len() == 8) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
    let alpha = if digits.len() == 8 { byte(6)? } else { 255 };
    Some(Color32::from_rgba_unmultiplied(byte(0)?, byte(2)?, byte(4)?, alpha))
}

fn base_is_dark<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    match String::deserialize(deserializer)?.to_ascii_lowercase().as_str() {
        "dark" => Ok(true),
        "light" => Ok(false),
        other => Err(serde::de::Error::custom(format!(
            "base `{other}` is not `dark` or `light`"
        ))),
    }
}

/// The installed themes and the files that failed to load.
#[derive(Clone, Debug)]
pub struct Themes {
    pub list: Vec<Theme>,
    pub errors: Vec<String>,
}

impl Themes {
    /// Built-in themes, then each `*.json` in `dir` by file name. A file whose name
    /// matches a built-in theme replaces it.
    pub fn load(dir: Option<&Path>) -> Self {
        let mut list: Vec<Theme> = BUILT_IN
            .iter()
            .map(|text| parse_theme(text).expect("built-in theme parses"))
            .collect();
        let mut errors = Vec::new();
        for path in theme_files(dir) {
            let parsed = std::fs::read_to_string(&path)
                .map_err(|err| err.to_string())
                .and_then(|text| parse_theme(&text));
            match parsed {
                Ok(theme) => {
                    match list.iter_mut().find(|item| item.name.eq_ignore_ascii_case(&theme.name)) {
                        Some(slot) => *slot = theme,
                        None => list.push(theme),
                    }
                }
                Err(err) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    errors.push(format!("{} — {err}", name.unwrap_or_default()));
                }
            }
        }
        Self { list, errors }
    }

    /// The theme with this name, or the default.
    pub fn get(&self, name: &str) -> &Theme {
        self.list
            .iter()
            .find(|theme| theme.name.eq_ignore_ascii_case(name))
            .or_else(|| self.list.iter().find(|theme| theme.name.eq_ignore_ascii_case(DEFAULT_THEME)))
            .unwrap_or(&self.list[0])
    }
}

fn theme_files(dir: Option<&Path>) -> Vec<PathBuf> {
    let Some(entries) = dir.and_then(|dir| std::fs::read_dir(dir).ok()) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect();
    files.sort();
    files
}

/// Read a theme, filling unset colours from the built-in theme its `base` names.
fn parse_theme(text: &str) -> Result<Theme, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    let serde_json::Value::Object(fields) = value else {
        return Err("a theme is a JSON object".into());
    };
    let base_name = match fields.get("base") {
        None => "dark".to_string(),
        Some(serde_json::Value::String(base)) => base.to_ascii_lowercase(),
        Some(_) => return Err("`base` is not a string".into()),
    };
    let base_text = match base_name.as_str() {
        "dark" => BUILT_IN[0],
        "light" => BUILT_IN[1],
        other => return Err(format!("base `{other}` is not `dark` or `light`")),
    };
    let serde_json::Value::Object(mut merged) =
        serde_json::from_str(base_text).map_err(|err| err.to_string())?
    else {
        return Err("built-in base is not an object".into());
    };
    merged.extend(fields);
    merged.insert("base".into(), serde_json::Value::String(base_name));
    let theme: Theme =
        serde_json::from_value(serde_json::Value::Object(merged)).map_err(|err| err.to_string())?;
    if theme.name.trim().is_empty() {
        return Err("`name` is empty".into());
    }
    Ok(theme)
}

/// Set egui's visuals from a theme.
pub fn apply(ctx: &egui::Context, theme: &Theme) {
    let mut visuals = if theme.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.window_fill = theme.background.0;
    visuals.panel_fill = theme.panel.0;
    visuals.faint_bg_color = theme.background.0;
    visuals.extreme_bg_color = theme.inset.0;
    visuals.code_bg_color = theme.panel.0;
    visuals.window_stroke.color = theme.border.0;
    visuals.selection.bg_fill = theme.accent.0;
    visuals.selection.stroke.color = theme.accent_text.0;
    visuals.hyperlink_color = theme.accent.0;
    visuals.error_fg_color = theme.error.0;
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = theme.panel.0;
    widgets.noninteractive.weak_bg_fill = theme.panel.0;
    widgets.noninteractive.bg_stroke.color = theme.border.0;
    widgets.noninteractive.fg_stroke.color = theme.text.0;
    for (state, fill, button, text) in [
        (&mut widgets.inactive, theme.control, theme.button, theme.control_text),
        (&mut widgets.hovered, theme.control_hover, theme.button_hover, theme.control_text_hover),
        (&mut widgets.active, theme.control_active, theme.button_active, theme.control_text_hover),
        (&mut widgets.open, theme.control_hover, theme.button_hover, theme.control_text_hover),
    ] {
        state.bg_fill = fill.0;
        state.weak_bg_fill = button.0;
        state.fg_stroke.color = text.0;
    }
    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;
    style.spacing.slider_rail_height = 6.0;
    ctx.set_style(style);
    ctx.data_mut(|data| data.insert_temp(theme_id(), Arc::new(theme.clone())));
}

/// The theme last passed to [`apply`].
pub fn current(ctx: &egui::Context) -> Arc<Theme> {
    ctx.data(|data| data.get_temp::<Arc<Theme>>(theme_id()))
        .unwrap_or_else(|| Arc::new(parse_theme(BUILT_IN[0]).expect("built-in theme parses")))
}

fn theme_id() -> egui::Id {
    egui::Id::new("seasound-theme")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color32) -> f64 {
        let channel = |c: u8| {
            let c = c as f64 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    fn contrast(a: Hex, b: Hex) -> f64 {
        let (a, b) = (luminance(a.0), luminance(b.0));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn built_in_themes_load_and_dark_is_the_default() {
        let themes = Themes::load(None);
        let names: Vec<&str> = themes.list.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Dark", "Light", "Strawberry Milkshake"]);
        assert!(themes.errors.is_empty());
        assert_eq!(themes.get("no such theme").name, DEFAULT_THEME);
        assert_eq!(themes.get("light").name, "Light");
        let dark = themes.get(DEFAULT_THEME);
        assert!(dark.dark);
        assert_eq!(dark.background.0, Color32::from_rgb(0x0e, 0x14, 0x18));
        assert_eq!(dark.accent.0, Color32::from_rgb(0x20, 0xa8, 0x9c));
    }

    #[test]
    fn built_in_text_is_readable() {
        for theme in Themes::load(None).list {
            for (what, fg, bg) in [
                ("text", theme.text, theme.background),
                ("text on panel", theme.text, theme.panel),
                ("control text", theme.control_text, theme.panel),
                ("accent text", theme.accent_text, theme.accent),
                ("error", theme.error, theme.background),
                ("selected row", theme.row_selected_text, theme.row_selected),
                ("hovered row", theme.row_hover_text, theme.row_hover),
                ("group header", theme.group_header_text, theme.group_header),
            ] {
                let ratio = contrast(fg, bg);
                assert!(ratio >= 4.5, "{}: {what} contrast {ratio:.2}", theme.name);
            }
        }
    }

    #[test]
    fn a_partial_file_inherits_its_base_and_can_replace_a_built_in() {
        let dir = std::env::temp_dir().join(format!("seasound-themes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mint.json"),
            r##"{"name": "Mint", "base": "light", "accent": "#2e8b57"}"##,
        )
        .unwrap();
        std::fs::write(dir.join("dark.json"), r##"{"name": "dark", "error": "#ff000080"}"##).unwrap();
        std::fs::write(dir.join("broken.json"), r##"{"name": "Bad", "accent": "teal"}"##).unwrap();
        let themes = Themes::load(Some(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        let light = themes.get("Light").clone();
        let mint = themes.get("Mint");
        assert!(!mint.dark);
        assert_eq!(mint.accent.0, Color32::from_rgb(0x2e, 0x8b, 0x57));
        assert_eq!(mint.background, light.background);
        assert_eq!(themes.list.len(), 4);
        assert_eq!(themes.get("Dark").error.0, Color32::from_rgba_unmultiplied(255, 0, 0, 0x80));
        assert_eq!(themes.errors.len(), 1);
        assert!(themes.errors[0].starts_with("broken.json"), "{:?}", themes.errors);
    }

    #[test]
    fn hex_colours_need_a_hash_and_six_or_eight_digits() {
        assert_eq!(parse_hex("#0e1418"), Some(Color32::from_rgb(0x0e, 0x14, 0x18)));
        assert_eq!(parse_hex("#FFFFFF"), Some(Color32::WHITE));
        assert_eq!(parse_hex("0e1418"), None);
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#gg1418"), None);
    }
}
