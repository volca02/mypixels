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
    "gruvbox-dark",
    "nord",
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
    let given = colors
        .iter()
        .enumerate()
        .map(|(i, c)| color(c, &format!("palette[{i}]")))
        .collect::<Result<Vec<_>, _>>()?;
    if given.is_empty() {
        return Err("Theme palette is empty".into());
    }
    Ok(derive_palette(&given))
}

/// The xterm defaults, by slot: the "question" each ANSI palette slot asks
/// (dark red, bright cyan, ...) that a foreign color list gets matched against.
const ANCHORS: [Rgb; 16] = [
    Rgb(0, 0, 0),
    Rgb(128, 0, 0),
    Rgb(0, 128, 0),
    Rgb(128, 128, 0),
    Rgb(0, 0, 128),
    Rgb(128, 0, 128),
    Rgb(0, 128, 128),
    Rgb(192, 192, 192),
    Rgb(128, 128, 128),
    Rgb(255, 0, 0),
    Rgb(0, 255, 0),
    Rgb(255, 255, 0),
    Rgb(0, 0, 255),
    Rgb(255, 0, 255),
    Rgb(0, 255, 255),
    Rgb(255, 255, 255),
];

/// Complete a theme's color list into the 16 ANSI slots, so a theme file can
/// ship the colors it actually cares about instead of enumerating fourteen it
/// barely uses.
///
///  * 16 or more: taken verbatim, in order -- the canonical case.
///  * exactly 8 (the classic scheme): the darks stand, brights are derived by
///    lifting lightness in CIE Lab and keeping hue and chroma -- which is what
///    "the bright variant" perceptually means, and what hand-authored themes
///    do anyway.
///  * any other length: every slot takes the nearest given color, distance
///    measured in Lab so it is gamut *and* brightness, not raw channel delta.
pub fn derive_palette(given: &[Rgb]) -> [Rgb; 16] {
    if given.len() >= 16 {
        return given[..16].try_into().unwrap();
    }
    if given.len() == 8 {
        let mut out = [Rgb(0, 0, 0); 16];
        out[..8].copy_from_slice(given);
        for i in 0..8 {
            out[8 + i] = brighten(given[i]);
        }
        return out;
    }
    let mut out = [Rgb(0, 0, 0); 16];
    for (slot, anchor) in out.iter_mut().zip(ANCHORS) {
        *slot = given
            .iter()
            .min_by(|a, b| lab_dist(**a, anchor).total_cmp(&lab_dist(**b, anchor)))
            .copied()
            .unwrap();
    }
    out
}

/// A bright variant of a normal ANSI color: +26 lightness, capped below pure
/// white so `bright white` stays distinguishable from a white background.
/// Colors that are already bright stand.
fn brighten(c: Rgb) -> Rgb {
    let mut l = lab(c);
    if l[0] >= 85.0 {
        return c;
    }
    l[0] = (l[0] + 26.0).min(90.0);
    from_lab(l)
}

// sRGB <-> CIE Lab (D65). Cheap, and far closer to "looks like" than any
// RGB-space distance: hue and lightness are separate coordinates here, so a
// dark red and its bright sibling end up adjacent, as they should.
fn lab(c: Rgb) -> [f32; 3] {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    let (r, g, b) = (lin(c.0), lin(c.1), lin(c.2));
    let enc = |t: f32| {
        if t > 0.008_856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let fx = enc((0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.950_470);
    let fy = enc(0.2126 * r + 0.7152 * g + 0.0722 * b);
    let fz = enc((0.019_300 * r + 0.119_2 * g + 0.950_5 * b) / 1.088_830);
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

fn from_lab(l: [f32; 3]) -> Rgb {
    let fy = (l[0] + 16.0) / 116.0;
    let fx = fy + l[1] / 500.0;
    let fz = fy - l[2] / 200.0;
    let dec = |t: f32| {
        let t3 = t * t * t;
        if t3 > 0.008_856 { t3 } else { (t - 16.0 / 116.0) / 7.787 }
    };
    let x = dec(fx) * 0.950_470;
    let y = if l[0] > 8.0 { fy * fy * fy } else { l[0] / 903.3 };
    let z = dec(fz) * 1.088_830;
    let gam = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        let v = if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Rgb(
        gam(3.2406 * x - 1.5372 * y - 0.4986 * z),
        gam(-0.9689 * x + 1.8758 * y + 0.0415 * z),
        gam(0.055_70 * x - 0.2040 * y + 1.0570 * z),
    )
}

fn lab_dist(a: Rgb, b: Rgb) -> f32 {
    let (p, q) = (lab(a), lab(b));
    (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)
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
        "gruvbox-dark" | "gruvbox" => Some(include_str!("../themes/gruvbox-dark.yml")),
        "nord" => Some(include_str!("../themes/nord.yml")),
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

/// Every directory a named theme may live in, in precedence order: the user's
/// XDG config dir, their data dir, then the system-installed ones
/// ($XDG_DATA_DIRS, default /usr/local/share:/usr/share). Packagers drop theme
/// files into <datadir>/mypixels/themes/ and they appear in the picker.
pub fn theme_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = dirs::config_dir() {
        dirs.push(d.join("mypixels/themes"));
    }
    if let Some(d) = dirs::data_dir() {
        dirs.push(d.join("mypixels/themes"));
    }
    let xdg = std::env::var_os("XDG_DATA_DIRS");
    match xdg.as_deref() {
        Some(list) => {
            for d in std::env::split_paths(list) {
                if !d.as_os_str().is_empty() {
                    dirs.push(d.join("mypixels/themes"));
                }
            }
        }
        None => {
            dirs.push(PathBuf::from("/usr/local/share/mypixels/themes"));
            dirs.push(PathBuf::from("/usr/share/mypixels/themes"));
        }
    }
    dirs
}

fn theme_file(name: &str) -> Option<PathBuf> {
    theme_dirs()
        .into_iter()
        .flat_map(|d| [d.join(format!("{name}.yml")), d.join(format!("{name}.yaml"))])
        .find(|p| p.is_file())
}

/// The theme names to offer a picker: the built-ins first, then anything found
/// in the theme directories that is not a built-in under another spelling.
pub fn available() -> Vec<String> {
    let mut names: Vec<String> = NAMES.iter().map(|s| s.to_string()).collect();
    let mut found: Vec<String> = theme_dirs()
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .filter_map(|e| {
            let p = e.path();
            let stem = p.file_stem()?.to_str()?.to_string();
            matches!(p.extension().and_then(|x| x.to_str()), Some("yml") | Some("yaml")).then_some(stem)
        })
        .collect();
    found.sort();
    found.dedup();
    let builtins = names.clone();
    names.extend(found.into_iter().filter(|n| !builtins.iter().any(|b| b == n)));
    names
}

/// Apply a named theme to a live config -- the settings UI's picker route,
/// which must not round-trip through a file reload. Records what it watched in
/// theme_files, exactly as `configure` does at load.
pub fn apply_name(cfg: &mut Config, name: &str) -> Result<(), String> {
    let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "" | "default" => {
            cfg.theme_files.clear();
            Ok(())
        }
        "wal" => {
            let path = expand_path("~/.cache/wal/colors.yml", Path::new("."));
            let theme = load_file(&path)?;
            cfg.theme_files = vec![path];
            theme.apply(cfg);
            Ok(())
        }
        _ => {
            if let Some(text) = builtin(&normalized) {
                cfg.theme_files.clear();
                Theme::parse(text)?.apply(cfg);
                return Ok(());
            }
            let path =
                theme_file(&normalized).ok_or_else(|| format!("Unknown theme {name:?}"))?;
            let theme = load_file(&path)?;
            cfg.theme_files = vec![path];
            theme.apply(cfg);
            Ok(())
        }
    }
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
                let paths: Vec<PathBuf> = if name.contains('/') || Path::new(name).extension().is_some() {
                    vec![direct]
                } else {
                    theme_dirs()
                        .into_iter()
                        .flat_map(|d| [d.join(format!("{normalized}.yml")), d.join(format!("{normalized}.yaml"))])
                        .collect()
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
        assert!(parse_palette(&serde_yaml::from_str("[]").unwrap()).is_err());
    }

    #[test]
    fn short_color_lists_complete_themselves() {
        // A full list is taken verbatim -- no derivation, no surprises.
        let full: Vec<Rgb> = (0..20).map(|i| Rgb(i as u8, 0, 0)).collect();
        assert_eq!(&derive_palette(&full)[..3], &[Rgb(0, 0, 0), Rgb(1, 0, 0), Rgb(2, 0, 0)]);
        // Eight classic colors: darks stand, brights are the same colors
        // lighter -- same family, more lightness.
        let base: Vec<Rgb> = [
            "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a",
            "#a89984",
        ]
        .iter()
        .map(|s| Rgb::parse(s).unwrap())
        .collect();
        let p = derive_palette(&base);
        assert_eq!(&p[..8], &base[..]);
        for i in 0..8 {
            assert!(
                lab(p[8 + i])[0] > lab(base[i])[0] + 15.0,
                "slot {i}: bright must read clearly lighter"
            );
        }
        // An arbitrary short list matches by nearest gamut/brightness: the
        // reds land on the red slots, black on black, white on the whites.
        let trio = [Rgb(0, 0, 0), Rgb(220, 30, 30), Rgb(250, 250, 250)];
        let q = derive_palette(&trio);
        assert_eq!(q[0], Rgb(0, 0, 0));
        assert_eq!(q[1], Rgb(220, 30, 30));
        assert_eq!(q[9], Rgb(220, 30, 30));
        assert_eq!(q[15], Rgb(250, 250, 250));
        assert_eq!(q[7], Rgb(250, 250, 250));
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
