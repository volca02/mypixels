//! Color-only themes. Presets are embedded; custom files and pywal use the same
//! validation and derived UI colors. File monitoring stays off the draw thread.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use crate::config::{Config, Rgb};
use serde_yaml::Value;

pub const NAMES: &[&str] = &[
    "default",
    "badwolf",
    "dracula",
    "catppuccin-mocha",
    "catppuccin-latte",
    "wal",
];

pub fn mix(a: Rgb, b: Rgb, amount: f32) -> Rgb {
    let channel =
        |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount.clamp(0.0, 1.0)).round() as u8;
    Rgb(channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
}

/// Text color for small accent-filled index chips, on light and dark themes.
pub fn on_color(color: Rgb) -> Rgb {
    let luminance = (299 * color.0 as u32 + 587 * color.1 as u32 + 114 * color.2 as u32) / 1000;
    if luminance >= 145 {
        Rgb(20, 20, 24)
    } else {
        Rgb(250, 250, 250)
    }
}

fn color(value: &Value, name: &str) -> Result<Rgb, String> {
    value
        .as_str()
        .and_then(Rgb::parse)
        .ok_or_else(|| format!("Invalid theme color {name}; expected #rgb or #rrggbb"))
}

pub fn parse_palette(value: &Value) -> Result<[Rgb; 16], String> {
    let colors = if let Some(s) = value.as_str() {
        s.split(':')
            .map(|s| Value::String(s.to_string()))
            .collect::<Vec<_>>()
    } else if let Some(colors) = value.as_sequence() {
        colors.clone()
    } else {
        return Err("Theme palette must be a list or colon-separated colors".into());
    };
    if colors.len() < 16 {
        return Err("Theme palette needs 16 ANSI colors".into());
    }
    let mut palette = [Rgb(0, 0, 0); 16];
    for (i, slot) in palette.iter_mut().enumerate() {
        *slot = color(&colors[i], &format!("palette[{i}]"))?;
    }
    Ok(palette)
}

pub struct Theme {
    foreground: Rgb,
    background: Rgb,
    cursor: Rgb,
    selection: Rgb,
    accent: Rgb,
    palette: [Rgb; 16],
    overrides: Value,
}

impl Theme {
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value =
            serde_yaml::from_str(text).map_err(|e| format!("Invalid theme YAML: {e}"))?;
        let map = value.as_mapping().ok_or("Theme must be a YAML mapping")?;
        let wal = map.contains_key("special");
        let foreground = color(
            if wal {
                &value["special"]["foreground"]
            } else {
                &value["foreground_color"]
            },
            "foreground",
        )?;
        let background = color(
            if wal {
                &value["special"]["background"]
            } else {
                &value["background_color"]
            },
            "background",
        )?;
        let palette = if wal {
            let mut palette = [Rgb(0, 0, 0); 16];
            for (i, slot) in palette.iter_mut().enumerate() {
                *slot = color(&value["colors"][format!("color{i}")], &format!("color{i}"))?;
            }
            palette
        } else {
            parse_palette(&value["palette"])?
        };
        let optional = |value: &Value, name: &str, fallback| {
            if value.is_null() {
                Ok(fallback)
            } else {
                color(value, name)
            }
        };
        let cursor = optional(
            if wal {
                &value["special"]["cursor"]
            } else {
                &value["cursor_color"]
            },
            "cursor",
            foreground,
        )?;
        let accent = optional(&value["accent_color"], "accent", palette[4])?;
        let selection = optional(&value["selection_color"], "selection", accent)?;
        let mut overrides = serde_yaml::Mapping::new();
        for (key, val) in map {
            if let Some(name) = key.as_str().filter(|name| is_ui_color(name)) {
                color(val, name)?;
                overrides.insert(key.clone(), val.clone());
            }
        }
        Ok(Self {
            foreground,
            background,
            cursor,
            selection,
            accent,
            palette,
            overrides: Value::Mapping(overrides),
        })
    }

    pub fn apply(&self, cfg: &mut Config) {
        cfg.foreground_color = self.foreground;
        cfg.background_color = self.background;
        cfg.bg_tint_color = self.background;
        cfg.cursor_color = self.cursor;
        cfg.selection_color = self.selection;
        cfg.accent_color = self.accent;
        cfg.palette = Some(self.palette);
        // Blend toward foreground/accent rather than multiplying brightness:
        // multiplication collapses light themes' tab backgrounds to white.
        let bar = mix(self.background, self.foreground, 0.06);
        cfg.tablist_background = bar;
        cfg.tab_color_bg = bar;
        cfg.tab_color_bg_selected = mix(self.background, self.accent, 0.20);
        cfg.tab_color_fg = mix(self.foreground, self.background, 0.24);
        cfg.tab_color_fg_selected = self.foreground;
        cfg.tab_color_entangle = self.palette[1];
        cfg.tab_color_idx = self.palette[2];
        cfg.add_button_color = self.accent;
        cfg.col = self.palette[0..8].try_into().unwrap();
        crate::config::merge(cfg, &self.overrides);
    }
}

fn is_ui_color(name: &str) -> bool {
    matches!(
        name,
        "tablist_background"
            | "tab_color_bg"
            | "tab_color_bg_selected"
            | "tab_color_fg"
            | "tab_color_fg_selected"
            | "tab_color_entangle"
            | "tab_color_idx"
            | "add_button_color"
            | "bg_tint_color"
    ) || (name.len() == 5 && name.starts_with("col_") && matches!(name.as_bytes()[4], b'0'..=b'7'))
}

fn builtin(name: &str) -> Option<&'static str> {
    match name {
        "badwolf" => Some(include_str!("../themes/badwolf.yml")),
        "dracula" => Some(include_str!("../themes/dracula.yml")),
        "catppuccin-mocha" | "mocha" => Some(include_str!("../themes/catppuccin-mocha.yml")),
        "catppuccin-latte" | "latte" => Some(include_str!("../themes/catppuccin-latte.yml")),
        _ => None,
    }
}

pub fn expand_path(path: &str, base: &Path) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| base.to_path_buf());
    }
    if let Some(relative) = path.strip_prefix("~/") {
        return dirs::home_dir()
            .unwrap_or_else(|| base.to_path_buf())
            .join(relative);
    }
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

pub fn load_file(path: &Path) -> Result<Theme, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read theme {}: {e}", path.display()))?;
    Theme::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Preset -> explicit config overrides -> pywal. The implicit pywal file keeps
/// Python compatibility, but never unexpectedly replaces an explicit preset.
pub fn configure(cfg: &mut Config, value: &Value, base: &Path) -> Result<(), String> {
    let selected = value.get("theme").filter(|v| !v.is_null());
    let name = selected
        .map(|v| v.as_str().ok_or("theme must be a preset name or YAML path"))
        .transpose()?;
    let explicit_wal = value.get("wal_palette").filter(|v| !v.is_null());
    let wal_name = explicit_wal
        .map(|v| {
            v.as_str()
                .ok_or("wal_palette must be a path or an empty string")
        })
        .transpose()?
        .unwrap_or("~/.cache/wal/colors.yml");
    let wal = (!wal_name.is_empty()).then(|| expand_path(wal_name, base));
    let normalized = name
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .replace('_', "-");
    if let Some(name) = name {
        if !matches!(normalized.as_str(), "" | "default" | "wal") {
            if let Some(text) = builtin(&normalized) {
                Theme::parse(text)?.apply(cfg);
            } else {
                let direct = expand_path(name, base);
                let paths = if name.contains('/') || Path::new(name).extension().is_some() {
                    vec![direct]
                } else {
                    let dir = dirs::config_dir()
                        .unwrap_or_else(|| base.join(".config"))
                        .join("mypixels/themes");
                    vec![
                        dir.join(format!("{name}.yml")),
                        dir.join(format!("{name}.yaml")),
                    ]
                };
                let path = paths.iter().find(|p| p.is_file()).ok_or_else(|| {
                    format!(
                        "Unknown theme {name:?}; choose {} or a YAML file",
                        NAMES.join(", ")
                    )
                })?;
                load_file(path)?.apply(cfg);
                cfg.theme_files.push(path.clone());
            }
        }
    }
    crate::config::merge(cfg, value);
    if normalized == "wal" && wal.is_none() {
        return Err("theme: wal needs a nonempty wal_palette path".into());
    }
    let use_wal = normalized == "wal" || explicit_wal.is_some() || name.is_none();
    if let Some(path) = wal.filter(|_| use_wal) {
        cfg.theme_files.push(path.clone());
        // A missing implicit cache is normal; explicit sources are errors so
        // reload can preserve the current valid theme during a partial edit.
        if path.is_file() || normalized == "wal" || explicit_wal.is_some() {
            load_file(&path)?.apply(cfg);
        }
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct Stamp(Option<(std::time::SystemTime, u64, u64)>);
fn stamp(path: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    Stamp(
        std::fs::metadata(path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len(), m.ino()))),
    )
}

pub struct Watcher {
    paths: mpsc::Sender<Vec<PathBuf>>,
    changed: Arc<AtomicBool>,
}
impl Watcher {
    pub fn new(paths: Vec<PathBuf>, wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        let changed = Arc::new(AtomicBool::new(false));
        let flag = changed.clone();
        std::thread::Builder::new()
            .name("theme-files".into())
            .spawn(move || {
                let mut watched: Vec<_> = paths
                    .into_iter()
                    .map(|p| {
                        let s = stamp(&p);
                        (p, s)
                    })
                    .collect();
                loop {
                    let result = if watched.is_empty() {
                        rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    } else {
                        rx.recv_timeout(Duration::from_millis(300))
                    };
                    match result {
                        Ok(paths) => {
                        // Preserve stamps for unchanged paths: an edit arriving
                        // while reload reads a file must still be observed.
                        watched.retain(|(path, _)| paths.contains(path));
                        for path in paths {
                            if !watched.iter().any(|(p, _)| *p == path) {
                                let s = stamp(&path);
                                watched.push((path, s));
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let mut any = false;
                            for (path, old) in &mut watched {
                                let new = stamp(path);
                                if *old != new {
                                    *old = new;
                                    any = true;
                                }
                            }
                            if any && !flag.swap(true, Ordering::SeqCst) {
                                wake();
                            }
                        }
                    }
                }
            })?;
        Ok(Self { paths: tx, changed })
    }
    pub fn set_paths(&self, paths: Vec<PathBuf>) {
        let _ = self.paths.send(paths);
    }
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_cover_the_terminal_and_ui() {
        for name in NAMES.iter().filter(|n| !matches!(**n, "default" | "wal")) {
            let theme = Theme::parse(builtin(name).unwrap()).unwrap();
            let mut cfg = Config::default();
            theme.apply(&mut cfg);
            assert_eq!(cfg.palette, Some(theme.palette));
            assert_eq!(cfg.cursor_color, theme.cursor);
            assert_ne!(cfg.tab_color_bg, cfg.tab_color_bg_selected);
            assert_ne!(cfg.foreground_color, cfg.background_color);
        }
    }

    #[test]
    fn light_theme_tabs_keep_their_contrast() {
        let mut cfg = Config::default();
        Theme::parse(builtin("catppuccin-latte").unwrap())
            .unwrap()
            .apply(&mut cfg);
        assert!(cfg.background_color.0 > 200);
        assert!(cfg.tab_color_fg.0 < 150);
        assert_ne!(cfg.tab_color_bg, Rgb(255, 255, 255));
        assert_eq!(on_color(Rgb(240, 240, 240)), Rgb(20, 20, 24));
    }

    #[test]
    fn invalid_theme_never_partially_mutates_config() {
        assert!(
            Theme::parse("foreground_color: '#fff'\nbackground_color: nope\npalette: []").is_err()
        );
        assert!(parse_palette(&serde_yaml::from_str("['#fff', '#000']").unwrap()).is_err());
    }

    #[test]
    fn theme_cannot_change_fonts_or_keybindings() {
        let text = format!(
            "{}\nterm_font: 'Wrong Font 50'\nbindings: {{close_tab: Return}}\ntab_color_bg: '#123456'\n",
            builtin("dracula").unwrap()
        );
        let mut cfg = Config::default();
        let font = cfg.term_font.clone();
        Theme::parse(&text).unwrap().apply(&mut cfg);
        assert_eq!(cfg.term_font, font);
        assert_eq!(cfg.tab_color_bg, Rgb(0x12, 0x34, 0x56));
    }
    #[test]
    fn config_overrides_presets_without_losing_the_rest_of_the_theme() {
        let cfg = Config::from_text(
            "theme: dracula\nforeground_color: '#123456'\n",
            Path::new("."),
        )
        .unwrap();
        assert_eq!(cfg.foreground_color, Rgb(0x12, 0x34, 0x56));
        assert_eq!(cfg.background_color, Rgb(0x28, 0x2a, 0x36));
        assert!(
            cfg.theme_files.is_empty(),
            "explicit presets do not inherit the implicit wal file"
        );
        assert!(Config::from_text("theme: nonexistent-theme-name", Path::new(".")).is_err());
        assert!(Config::from_text("theme: wal\nwal_palette: ''", Path::new(".")).is_err());
    }

    #[test]
    fn custom_files_and_pywal_keep_python_precedence() {
        let dir = std::env::temp_dir().join(format!("mypixels-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("custom.yml"), builtin("catppuccin-latte").unwrap()).unwrap();
        let cfg = Config::from_text("theme: custom.yml\nwal_palette: ''", &dir).unwrap();
        assert_eq!(cfg.background_color, Rgb(0xef, 0xf1, 0xf5));
        assert_eq!(cfg.theme_files, vec![dir.join("custom.yml")]);
        let mut wal = String::from(
            "special:\n  foreground: '#123456'\n  background: '#080808'\n  cursor: '#654321'\ncolors:\n",
        );
        for i in 0..16 {
            wal.push_str(&format!("  color{i}: '#{i:02x}2040'\n"));
        }
        std::fs::write(dir.join("wal.yml"), &wal).unwrap();
        let cfg = Config::from_text(
            "theme: dracula\nforeground_color: '#ffffff'\nwal_palette: wal.yml",
            &dir,
        )
        .unwrap();
        assert_eq!(cfg.foreground_color, Rgb(0x12, 0x34, 0x56));
        assert_eq!(cfg.cursor_color, Rgb(0x65, 0x43, 0x21));
        assert_eq!(cfg.palette.unwrap()[12], Rgb(12, 32, 64));
        // Replacing the source with invalid YAML must fail as a whole, so the
        // caller can keep its last good config, then recover on the next edit.
        std::fs::write(dir.join("wal.yml"), "special: [").unwrap();
        assert!(Config::from_text("theme: wal\nwal_palette: wal.yml", &dir).is_err());
        std::fs::write(dir.join("wal.yml"), wal).unwrap();
        assert!(Config::from_text("theme: wal\nwal_palette: wal.yml", &dir).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
