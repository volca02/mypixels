//! Configuration with defaults ported from the Python version's config dict,
//! plus keybinding parsing ("MOD1+SHIFT+Left", "CONTROL+ALT+x", "MOD1+_1" ...).

use winit::keyboard::{Key, ModifiersState, NamedKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn to_f32(&self) -> [f32; 3] {
        [self.0 as f32 / 255.0, self.1 as f32 / 255.0, self.2 as f32 / 255.0]
    }
}

impl Rgb {
    /// Strict #rgb / #rrggbb parse (Gdk.RGBA.parse compatible).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        if s.len() == 3 {
            let v = u32::from_str_radix(s, 16).ok()?;
            let (r, g, b) = ((v >> 8 & 0xf) as u8, (v >> 4 & 0xf) as u8, (v & 0xf) as u8);
            return Some(Rgb(r * 17, g * 17, b * 17));
        }
        if s.len() == 6 {
            let v = u32::from_str_radix(s, 16).ok()?;
            return Some(Rgb((v >> 16 & 0xff) as u8, (v >> 8 & 0xff) as u8, (v & 0xff) as u8));
        }
        None
    }
}

pub fn parse_hex(s: &str) -> Rgb {
    Rgb::parse(s).unwrap_or(Rgb(0, 0, 0))
}

/// "Cascadia Code 9" -> ("Cascadia Code", 9.0). Pango-style: the size is
/// the last whitespace-separated token; everything before is the family.
pub fn parse_font(s: &str) -> (String, f32) {
    let s = s.trim();
    match s.rfind(char::is_whitespace) {
        Some(idx) if idx + 1 < s.len() => {
            let size: f32 = s[idx + 1..].trim().parse().unwrap_or(12.0);
            (s[..idx].trim().to_string(), size)
        },
        _ => (s.to_string(), 12.0),
    }
}

/// Colon-separated hex colors (terminator/wal format). Takes the first 16;
/// more than 16 (256-color palettes) is fine. Requires at least 16 so a
/// truncated palette doesn't silently pad the rest with black.
fn parse_palette(s: &str) -> Option<[Rgb; 16]> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 16 {
        return None;
    }
    let mut out = [Rgb(0, 0, 0); 16];
    for (slot, part) in out.iter_mut().zip(&parts).take(16) {
        *slot = Rgb::parse(part)?;
    }
    Some(out)
}

#[derive(Clone)]
pub struct Config {
    // tablist
    pub tablist_background: Rgb,
    pub tab_color_bg: Rgb,
    pub tab_color_bg_selected: Rgb,
    pub tab_color_fg: Rgb,
    pub tab_color_fg_selected: Rgb,
    pub tab_color_entangle: Rgb,
    pub tab_color_idx: Rgb,
    pub add_button_color: Rgb,
    pub tab_title_max_length: usize,
    pub tab_title_short_current: usize,
    pub tab_shorten_str: String,
    // terminal
    pub foreground_color: Rgb,
    pub background_color: Rgb,
    pub selection_color: Rgb,
    /// 16-color palette (None = alacritty defaults). Terminal runtime OSC 4
    /// changes override these per color.
    pub palette: Option<[Rgb; 16]>,
    pub term_font: String,
    pub tablist_font: String,
    pub scrollback_lines: usize,
    // tab color groups
    pub col: [Rgb; 8],
    // behavior
    pub tab_overview_on_alt: bool,
    pub tab_overview_opacity: f32,
    pub tab_overview_delay_ms: u32,
    pub tab_overview_thumb_width: u32,
    pub tab_overview_thumb_height: u32,
    pub focus_new_tabs: bool,
    pub hide_single_tab: bool,
    pub multiline_tabs: bool,
    pub tablist_top: bool,
    pub tablist_hide: bool,
    pub tab_title_shorten_start: bool,
    pub default_tab_title: String,
    pub default_width: u32,
    pub default_height: u32,
    pub default_maximize: bool,
    pub term_opacity: f32,
    pub bg_tint_color: Rgb,
    pub tab_format: String,
    pub tab_prepend: Option<String>,
    pub tab_append: Option<String>,
    pub autohide_delay: i32,
    // keybindings
    pub bindings: Vec<(String, Vec<Binding>)>,
}

impl Default for Config {
    fn default() -> Self {
        let mut bindings: Vec<(String, Vec<Binding>)> = Vec::new();
        let mut bind = |k: &str, v: &[&str]| {
            let list: Vec<Binding> = v.iter().filter_map(|s| Binding::parse(s)).collect();
            bindings.push((k.to_string(), list));
        };

        bind("move_prev_tab", &["MOD1+SHIFT+Left"]);
        bind("move_next_tab", &["MOD1+SHIFT+Right"]);
        bind("move_first_tab", &["MOD1+SHIFT+Home"]);
        bind("move_last_tab", &["MOD1+SHIFT+End"]);
        bind("prev_tab", &["MOD1+Left", "MOD1+SHIFT+f"]);
        bind("next_tab", &["MOD1+Right", "MOD1+SHIFT+b"]);
        bind("prev_tab_row", &["MOD1+Up"]);
        bind("next_tab_row", &["MOD1+Down"]);
        bind("first_tab", &["MOD1+Home"]);
        bind("last_tab", &["MOD1+End"]);
        bind("close_tab", &["CONTROL+ALT+x", "CONTROL+ALT+q"]);
        bind("new_tab", &["CONTROL+ALT+z", "CONTROL+SHIFT+t"]);
        // Copy/paste follow the VTE and GNOME Terminal bindings the original gets
        // from GTK, so the fingers that already know them keep working. Selecting
        // text feeds the *primary* buffer separately and needs no key.
        bind("copy", &["CONTROL+SHIFT+c"]);
        bind("paste", &["CONTROL+SHIFT+v"]);
        bind("paste_primary", &["SHIFT+Insert"]);
        bind("scroll_page_up", &["SHIFT+Page_Up"]);
        bind("scroll_page_down", &["SHIFT+Page_Down"]);
        bind("scroll_bottom", &["CONTROL+SHIFT+End"]);
        bind("shrink_font", &["MOD1+SHIFT+KP_Subtract"]);
        bind("enlarge_font", &["MOD1+SHIFT+KP_Add"]);
        bind("toggle_hide_tabs", &["CONTROL+ALT+v"]);
        bind("prev_color", &["MOD1+SHIFT+braceleft"]);
        bind("next_color", &["MOD1+SHIFT+braceright"]);
        bind("toggle_entangle", &["MOD1+SHIFT+KP_Multiply"]);
        bind("rename_tab", &["MOD1+SHIFT+n"]);
        bind("show_search", &["MOD1+SHIFT+s"]);
        bind("show_search_all", &["MOD1+SHIFT+g"]);
        bind("search_prev", &["MOD1+SHIFT+a"]);
        bind("search_next", &["MOD1+SHIFT+d"]);
        bind("config_reload", &["CONTROL+SHIFT+r"]);
        for (i, d) in (0..10).zip(["_1", "_2", "_3", "_4", "_5", "_6", "_7", "_8", "_9", "_0"]) {
            bind(&format!("goto_tab_{i}"), &[&format!("MOD1+{d}")]);
        }

        Self {
            tablist_background: parse_hex("#303030"),
            tab_color_bg: parse_hex("#303030"),
            tab_color_bg_selected: parse_hex("#505020"),
            tab_color_fg: parse_hex("#888888"),
            tab_color_fg_selected: parse_hex("#ffffff"),
            tab_color_entangle: parse_hex("#ff0000"),
            tab_color_idx: parse_hex("#808080"),
            add_button_color: parse_hex("#ffa500"),
            tab_title_max_length: 16,
            tab_title_short_current: 32,
            tab_shorten_str: "\u{2026}".to_string(),
            foreground_color: parse_hex("#bbbbbb"),
            background_color: parse_hex("#000000"),
            selection_color: parse_hex("#3584e4"),
            palette: None,
            term_font: "Terminus 8".to_string(),
            tablist_font: "Liberation Mono 7".to_string(),
            scrollback_lines: 1000,
            col: [
                parse_hex("#b58900"),
                parse_hex("#cb4b16"),
                parse_hex("#dc322f"),
                parse_hex("#d33682"),
                parse_hex("#6c71c4"),
                parse_hex("#268bd2"),
                parse_hex("#2aa198"),
                parse_hex("#859900"),
            ],
            tab_overview_on_alt: true,
            tab_overview_opacity: 0.3,
            tab_overview_delay_ms: 500,
            tab_overview_thumb_width: 300,
            tab_overview_thumb_height: 225,
            focus_new_tabs: true,
            hide_single_tab: false,
            multiline_tabs: true,
            tablist_top: true,
            tablist_hide: false,
            tab_title_shorten_start: false,
            default_tab_title: String::new(),
            default_width: 640,
            default_height: 480,
            default_maximize: false,
            term_opacity: 0.1,
            bg_tint_color: parse_hex("#140008"),
            tab_format: "<span %(color)s>%(tcorner)s%(entangled)s%(tname)s]</span>".to_string(),
            tab_prepend: Some("clock".to_string()),
            tab_append: None,
            autohide_delay: -1,
            bindings,
        }
    }
}

impl Config {
    /// Load the config from `~/.mypixels` (YAML) merged over the defaults.
    /// Same semantics as the Python version: `bindings` merge per action,
    /// every other key replaces the default when present in the file.
    pub fn load() -> Config {
        let mut cfg = Config::default();
        // $SPIKE_CONFIG overrides the path (used for testing reloads without
        // touching the real ~/.mypixels).
        let path = match std::env::var("SPIKE_CONFIG") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => {
                let Some(home) = dirs::home_dir() else {
                    return cfg;
                };
                home.join(".mypixels")
            }
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("no config file at {}, using defaults", path.display());
            return cfg;
        };
        // The Python dumper tags multiple bindings `!!python/tuple`; the
        // block list underneath is plain YAML, so strip the tag.
        let text = text.replace("!!python/tuple", "");
        // Real-world configs can repeat a key (Python keeps the last
        // occurrence; serde_yaml rejects duplicates), so dedupe.
        let text = dedupe_top_level(&text);
        match serde_yaml::from_str::<serde_yaml::Value>(&text) {
            Ok(v) => merge(&mut cfg, &v),
            Err(e) => eprintln!("config parse error in {}: {e}", path.display()),
        }
        cfg
    }
}

fn top_level_key(line: &str) -> Option<String> {
    let mut chars = line.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    for c in chars {
        if c == ':' {
            return line.split(':').next().map(String::from);
        }
        if !c.is_ascii_alphanumeric() && c != '_' {
            return None;
        }
    }
    None
}

/// Keep only the last occurrence of each top-level key (Python YAML
/// semantics); drop the earlier blocks.
fn dedupe_top_level(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks: Vec<(String, usize)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some(k) = top_level_key(l) {
            blocks.push((k, i));
        }
    }
    let mut remove = vec![false; blocks.len()];
    for (bi, (key, _)) in blocks.iter().enumerate() {
        if let Some(last) = blocks.iter().rposition(|(k, _)| k == key) {
            if last != bi {
                remove[bi] = true;
            }
        }
    }
    if !remove.iter().any(|&r| r) {
        return text.to_string();
    }
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        let bi = blocks.iter().rposition(|(_, s)| *s <= i);
        let keep = match bi {
            Some(b) => !remove[b],
            None => true, // header comments before the first key
        };
        if keep {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

    fn merge(cfg: &mut Config, v: &serde_yaml::Value) {
        let Some(map) = v.as_mapping() else {
            return;
        };
        for (k, val) in map {
            let Some(key) = k.as_str() else {
                continue;
            };
            match key {
                "bindings" => merge_bindings(&mut cfg.bindings, val),
                "tablist_background" => set_rgb(&mut cfg.tablist_background, val),
                "tab_color_bg" => set_rgb(&mut cfg.tab_color_bg, val),
                "tab_color_bg_selected" => set_rgb(&mut cfg.tab_color_bg_selected, val),
                "tab_color_fg" => set_rgb(&mut cfg.tab_color_fg, val),
                "tab_color_fg_selected" => set_rgb(&mut cfg.tab_color_fg_selected, val),
                "tab_color_entangle" => set_rgb(&mut cfg.tab_color_entangle, val),
                "tab_color_idx" => set_rgb(&mut cfg.tab_color_idx, val),
                "add_button_color" => set_rgb(&mut cfg.add_button_color, val),
                "foreground_color" => set_rgb(&mut cfg.foreground_color, val),
                "background_color" => set_rgb(&mut cfg.background_color, val),
                "selection_color" => set_rgb(&mut cfg.selection_color, val),
                "bg_tint_color" => set_rgb(&mut cfg.bg_tint_color, val),
                key if key.starts_with("col_") && key.len() == 5 => {
                    let i = (key.as_bytes()[4] as u8 - b'0') as usize;
                    if i < 8 {
                        set_rgb(&mut cfg.col[i], val);
                    }
                },
                "palette" => {
                    if let Some(s) = val.as_str() {
                        cfg.palette = parse_palette(s);
                    }
                },
                "term_font" => {
                    if let Some(s) = val.as_str() {
                        cfg.term_font = s.to_string();
                    }
                },
                "tablist_font" => {
                    if let Some(s) = val.as_str() {
                        cfg.tablist_font = s.to_string();
                    }
                },
                "tab_format" => {
                    if let Some(s) = val.as_str() {
                        cfg.tab_format = s.to_string();
                    }
                },
                "tab_shorten_str" => {
                    if let Some(s) = val.as_str() {
                        cfg.tab_shorten_str = s.to_string();
                    }
                },
                "default_tab_title" => {
                    if let Some(s) = val.as_str() {
                        cfg.default_tab_title = s.to_string();
                    }
                },
                "tab_prepend" => {
                    cfg.tab_prepend = val.as_str().map(String::from);
                },
                "tab_append" => {
                    cfg.tab_append = val.as_str().map(String::from);
                },
                "scrollback_lines" => {
                    if let Some(n) = val.as_u64() {
                        cfg.scrollback_lines = n as usize;
                    }
                },
                "default_width" => {
                    if let Some(n) = val.as_u64() {
                        cfg.default_width = n as u32;
                    }
                },
                "default_height" => {
                    if let Some(n) = val.as_u64() {
                        cfg.default_height = n as u32;
                    }
                },
                "tab_title_max_length" => {
                    if let Some(n) = val.as_i64().filter(|n| *n >= 0) {
                        cfg.tab_title_max_length = n as usize;
                    }
                },
                "tab_title_short_current" => {
                    if let Some(n) = val.as_i64().filter(|n| *n >= 0) {
                        cfg.tab_title_short_current = n as usize;
                    }
                },
                "tab_overview_delay_ms" => {
                    if let Some(n) = val.as_u64() {
                        cfg.tab_overview_delay_ms = n as u32;
                    }
                },
                "term_opacity" => {
                    if let Some(f) = val.as_f64() {
                        cfg.term_opacity = f.clamp(0.0, 1.0) as f32;
                    }
                },
                "tab_overview_opacity" => {
                    if let Some(f) = val.as_f64() {
                        cfg.tab_overview_opacity = f.clamp(0.0, 1.0) as f32;
                    }
                },
                "tab_overview_thumb_width" => {
                    if let Some(n) = val.as_u64() {
                        cfg.tab_overview_thumb_width = (n as u32).max(40);
                    }
                },
                "tab_overview_thumb_height" => {
                    if let Some(n) = val.as_u64() {
                        cfg.tab_overview_thumb_height = (n as u32).max(30);
                    }
                },
                "autohide_delay" => {
                    if let Some(n) = val.as_i64() {
                        cfg.autohide_delay = n as i32;
                    }
                },
                "default_maximize" => {
                    if let Some(b) = val.as_bool() {
                        cfg.default_maximize = b;
                    }
                },
                "tab_overview_on_alt" => {
                    if let Some(b) = val.as_bool() {
                        cfg.tab_overview_on_alt = b;
                    }
                },
                "focus_new_tabs" => {
                    if let Some(b) = val.as_bool() {
                        cfg.focus_new_tabs = b;
                    }
                },
                "hide_single_tab" => {
                    if let Some(b) = val.as_bool() {
                        cfg.hide_single_tab = b;
                    }
                },
                "multiline_tabs" => {
                    if let Some(b) = val.as_bool() {
                        cfg.multiline_tabs = b;
                    }
                },
                "tablist_top" => {
                    if let Some(b) = val.as_bool() {
                        cfg.tablist_top = b;
                    }
                },
                "tablist_hide" => {
                    if let Some(b) = val.as_bool() {
                        cfg.tablist_hide = b;
                    }
                },
                "tab_title_shorten_start" => {
                    if let Some(b) = val.as_bool() {
                        cfg.tab_title_shorten_start = b;
                    }
                },
                _ => {}, // unknown keys ignored, like in Python
            }
        }
    }

fn set_rgb(dst: &mut Rgb, v: &serde_yaml::Value) {
    if let Some(s) = v.as_str() {
        if let Some(c) = Rgb::parse(s) {
            *dst = c;
        } else {
            eprintln!("bad color value: {s:?}");
        }
    }
}

fn merge_bindings(bindings: &mut Vec<(String, Vec<Binding>)>, val: &serde_yaml::Value) {
    let Some(map) = val.as_mapping() else {
        return;
    };
    for (k, v) in map {
        let Some(action) = k.as_str() else {
            continue;
        };
        // Standalone overview was retired; old bindings must not swallow keys.
        if action == "show_tab_overview" || action.starts_with("overview_") { continue; }
        let specs: Vec<String> = match v {
            serde_yaml::Value::String(s) => vec![s.clone()],
            serde_yaml::Value::Sequence(seq) => {
                seq.iter().filter_map(|s| s.as_str().map(String::from)).collect()
            },
            _ => continue,
        };
        let parsed: Vec<Binding> = specs.iter().filter_map(|s| Binding::parse(s)).collect();
        if parsed.is_empty() {
            eprintln!("bad binding for {action:?}: {specs:?}");
            continue;
        }
        match bindings.iter_mut().find(|(a, _)| a == action) {
            Some(entry) => entry.1 = parsed,
            None => bindings.push((action.to_string(), parsed)),
        }
    }
}

// ---------------------------------------------------------------------------
// Keybindings
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum BoundKey {
    Named(NamedKey),
    Char(String),
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub mods: ModifiersState,
    pub key: BoundKey,
}

impl Binding {
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('+').collect();
        let (mod_strs, key_str) = parts.split_at(parts.len().saturating_sub(1));
        let key_str = key_str[0].trim();
        if key_str.is_empty() {
            return None;
        }

        let mut mods = ModifiersState::empty();
        for m in mod_strs.iter().map(|s| s.trim()) {
            match m {
                "MOD1" | "ALT" | "Alt_L" | "Alt_R" => mods |= ModifiersState::ALT,
                "CONTROL" | "CTRL" => mods |= ModifiersState::CONTROL,
                "SHIFT" => mods |= ModifiersState::SHIFT,
                _ => return None,
            }
        }

        // "_1" style: underscore prefix means the literal character key
        if let Some(rest) = key_str.strip_prefix('_') {
            if rest.len() == 1 {
                return Some(Self { mods, key: BoundKey::Char(rest.to_string()) });
            }
        }

        let key = match key_str {
            "Left" => BoundKey::Named(NamedKey::ArrowLeft),
            "Right" => BoundKey::Named(NamedKey::ArrowRight),
            "Up" => BoundKey::Named(NamedKey::ArrowUp),
            "Down" => BoundKey::Named(NamedKey::ArrowDown),
            "Home" => BoundKey::Named(NamedKey::Home),
            "End" => BoundKey::Named(NamedKey::End),
            "Return" => BoundKey::Named(NamedKey::Enter),
            "Escape" => BoundKey::Named(NamedKey::Escape),
            // Numpad keys arrive as characters (NumLock on).
            "KP_Add" => BoundKey::Char("+".to_string()),
            "KP_Subtract" => BoundKey::Char("-".to_string()),
            "KP_Multiply" => BoundKey::Char("*".to_string()),
            "KP_Divide" => BoundKey::Char("/".to_string()),
            "KP_Enter" => BoundKey::Char("\r".to_string()),
            "Space" => BoundKey::Named(NamedKey::Space),
            "Page_Up" => BoundKey::Named(NamedKey::PageUp),
            "Page_Down" => BoundKey::Named(NamedKey::PageDown),
            "Insert" => BoundKey::Named(NamedKey::Insert),
            "Delete" => BoundKey::Named(NamedKey::Delete),
            "Tab" => BoundKey::Named(NamedKey::Tab),
            "Backspace" => BoundKey::Named(NamedKey::Backspace),
            "F1" => BoundKey::Named(NamedKey::F1),
            "F2" => BoundKey::Named(NamedKey::F2),
            "F3" => BoundKey::Named(NamedKey::F3),
            "F4" => BoundKey::Named(NamedKey::F4),
            "F5" => BoundKey::Named(NamedKey::F5),
            "F6" => BoundKey::Named(NamedKey::F6),
            "F7" => BoundKey::Named(NamedKey::F7),
            "F8" => BoundKey::Named(NamedKey::F8),
            "F9" => BoundKey::Named(NamedKey::F9),
            "F10" => BoundKey::Named(NamedKey::F10),
            "F11" => BoundKey::Named(NamedKey::F11),
            "F12" => BoundKey::Named(NamedKey::F12),
            "braceleft" => BoundKey::Char("{".to_string()),
            "braceright" => BoundKey::Char("}".to_string()),
            _ => {
                if key_str.len() == 1 {
                    BoundKey::Char(key_str.to_string())
                } else {
                    return None;
                }
            },
        };
        Some(Self { mods, key })
    }

    /// Match a winit key + modifier state against this binding.
    pub fn matches(&self, key: &Key, mods: ModifiersState) -> bool {
        // Compare the three relevant modifier bits exactly.
        if mods.control_key() != self.mods.control_key()
            || mods.shift_key() != self.mods.shift_key()
            || mods.alt_key() != self.mods.alt_key()
        {
            return false;
        }
        match &self.key {
            BoundKey::Named(n) => matches!(key, Key::Named(k) if k == n),
            // winit reports Shift+C as "C", while legacy GTK binding files
            // spell CONTROL+SHIFT+c. Caps Lock must not break shortcuts either.
            BoundKey::Char(c) => matches!(key, Key::Character(k) if k == c ||
                (self.mods.intersects(ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SHIFT)
                    && k.eq_ignore_ascii_case(c))),
        }
    }

    /// Match against raw character input (for keys that arrive as text).
    pub fn matches_char(&self, c: &str, _mods: ModifiersState) -> bool {
        if self.mods != ModifiersState::empty() {
            return false;
        }
        matches!(&self.key, BoundKey::Char(k) if k == c)
    }
}

/// Find the first action whose any binding matches.
/// Overview bindings only apply while the overview is open; otherwise
/// they would swallow plain Return/Escape/arrows meant for the terminal.
pub fn find_action(bindings: &[(String, Vec<Binding>)], key: &Key, mods: ModifiersState, overview_active: bool) -> Option<String> {
    for (action, list) in bindings {
        if action.starts_with("overview_") && !overview_active {
            continue;
        }
        if list.iter().any(|b| b.matches(key, mods)) {
            return Some(action.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::ModifiersState as M;

    #[test]
    fn clipboard_shortcuts_accept_the_shifted_keys_winit_reports() {
        let cfg = Config::default();
        assert_eq!(find_action(&cfg.bindings, &Key::Character("C".into()), M::CONTROL | M::SHIFT, false).as_deref(), Some("copy"));
        assert_eq!(find_action(&cfg.bindings, &Key::Character("V".into()), M::CONTROL | M::SHIFT, false).as_deref(), Some("paste"));
        assert_eq!(find_action(&cfg.bindings, &Key::Named(NamedKey::Insert), M::SHIFT, false).as_deref(), Some("paste_primary"));
    }

    #[test]
    fn retired_standalone_overview_bindings_do_not_swallow_keys() {
        let mut cfg = Config::default();
        let legacy: serde_yaml::Value = serde_yaml::from_str("show_tab_overview: MOD1+o\noverview_exit: Escape\noverview_nav_left: Left\n").unwrap();
        merge_bindings(&mut cfg.bindings, &legacy);
        assert_eq!(find_action(&cfg.bindings, &Key::Character("o".into()), M::ALT, false), None);
        assert_eq!(find_action(&cfg.bindings, &Key::Named(NamedKey::Escape), M::empty(), false), None);
        assert!(cfg.tab_overview_on_alt, "the Alt-held visual switcher remains enabled");
    }

    #[test]
    fn rgb_parse_forms() {
        assert_eq!(Rgb::parse("#ff2c4b"), Some(Rgb(255, 44, 75)));
        assert_eq!(Rgb::parse("#141413"), Some(Rgb(20, 20, 19)));
        // #rgb shorthand expands each nibble to x17.
        assert_eq!(Rgb::parse("#f00"), Some(Rgb(255, 0, 0)));
        assert_eq!(Rgb::parse("#fff"), Some(Rgb(255, 255, 255)));
        // invalid: wrong length / non-hex.
        assert_eq!(Rgb::parse("#12345"), None);
        assert_eq!(Rgb::parse("#zzzzzz"), None);
    }

    #[test]
    fn font_parse_family_and_size() {
        assert_eq!(parse_font("Cascadia Code 9"), ("Cascadia Code".to_string(), 9.0));
        // family may contain parentheses and spaces.
        assert_eq!(
            parse_font("Terminus (TTF) 12"),
            ("Terminus (TTF)".to_string(), 12.0)
        );
        // no size -> default 12.
        assert_eq!(parse_font("Monospace"), ("Monospace".to_string(), 12.0));
    }

    #[test]
    fn palette_badwolf_first_sixteen() {
        let full = "#141413:#ff2c4b:#aeee00:#fade3e:#0a9dff:#ff9eb8:#8cffba:#d9cec3:#000000:#ff2c4b:#aeee00:#ffa724:#0a9dff:#ff9eb8:#8cffba:#ffffff";
        let p = parse_palette(&full).expect("16 colors");
        assert_eq!(p[0], Rgb(20, 20, 19));
        assert_eq!(p[1], Rgb(255, 44, 75));
        assert_eq!(p[2], Rgb(174, 238, 0));
        assert_eq!(p[15], Rgb(255, 255, 255));
        assert_eq!(parse_palette(""), None);
        assert_eq!(parse_palette("#000:#111"), None); // too few
    }

    #[test]
    fn dedupe_keeps_last_top_level_key() {
        let text = "a: 1\nb: 2\na: 3\n  - x\nc: 4\n";
        let out = dedupe_top_level(text);
        // first `a: 1` block dropped; second `a: 3` (with its list item) kept.
        assert!(!out.contains("a: 1"));
        assert!(out.contains("a: 3"));
        assert!(out.contains("- x"));
        assert!(out.contains("b: 2"));
        assert!(out.contains("c: 4"));
    }

    #[test]
    fn dedupe_preserves_header_and_no_duplicates() {
        let text = "# comment\na: 1\nb: 2\n";
        assert_eq!(dedupe_top_level(text), text);
    }

    #[test]
    fn binding_parse_mods_and_key() {
        let b = Binding::parse("MOD1+SHIFT+Left").unwrap();
        assert_eq!(b.mods, M::ALT | M::SHIFT);
        assert!(matches!(b.key, BoundKey::Named(NamedKey::ArrowLeft)));

        let b = Binding::parse("CONTROL+ALT+x").unwrap();
        assert_eq!(b.mods, M::CONTROL | M::ALT);
        assert!(matches!(b.key, BoundKey::Char(ref c) if c == "x"));

        // underscore prefix -> literal char even for a digit.
        let b = Binding::parse("MOD1+_1").unwrap();
        assert_eq!(b.mods, M::ALT);
        assert!(matches!(b.key, BoundKey::Char(ref c) if c == "1"));

        // numpad add arrives as the "+" character.
        let b = Binding::parse("SHIFT+KP_Add").unwrap();
        assert_eq!(b.mods, M::SHIFT);
        assert!(matches!(b.key, BoundKey::Char(ref c) if c == "+"));

        assert!(Binding::parse("").is_none());
        assert!(Binding::parse("FOO+x").is_none()); // unknown modifier
    }
}
