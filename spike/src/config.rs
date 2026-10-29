//! Configuration with defaults ported from the Python version's config dict,
//! plus keybinding parsing ("MOD1+SHIFT+Left", "CONTROL+ALT+x", "MOD1+_1" ...).

use winit::keyboard::{Key, ModifiersState, NamedKey};

#[derive(Clone, Copy, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn to_f32(&self) -> [f32; 3] {
        [self.0 as f32 / 255.0, self.1 as f32 / 255.0, self.2 as f32 / 255.0]
    }
}

pub fn parse_hex(s: &str) -> Rgb {
    let s = s.trim().trim_start_matches('#');
    let v = u32::from_str_radix(s, 16).unwrap_or(0);
    Rgb((v >> 16 & 0xff) as u8, (v >> 8 & 0xff) as u8, (v & 0xff) as u8)
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
    // tab color groups
    pub col: [Rgb; 8],
    // behavior
    pub tab_overview_on_alt: bool,
    pub tab_overview_opacity: f32,
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
        bind("shrink_font", &["MOD1+SHIFT+KP_Subtract"]);
        bind("enlarge_font", &["MOD1+SHIFT+KP_Add"]);
        bind("toggle_hide_tabs", &["CONTROL+SHIFT+v"]);
        bind("prev_color", &["MOD1+SHIFT+braceleft"]);
        bind("next_color", &["MOD1+SHIFT+braceright"]);
        bind("toggle_entangle", &["MOD1+SHIFT+KP_Multiply"]);
        bind("rename_tab", &["MOD1+SHIFT+n"]);
        bind("show_search", &["MOD1+SHIFT+s"]);
        bind("show_search_all", &["MOD1+SHIFT+g"]);
        bind("search_prev", &["MOD1+SHIFT+a"]);
        bind("search_next", &["MOD1+SHIFT+d"]);
        bind("config_reload", &["CONTROL+SHIFT+r"]);
        bind("show_tab_overview", &["MOD1+o"]);
        bind("overview_exit", &["Escape"]);
        bind("overview_select", &["Return"]);
        bind("overview_new_tab", &["MOD1+Return"]);
        bind("overview_nav_left", &["Left"]);
        bind("overview_nav_right", &["Right"]);
        bind("overview_nav_up", &["Up"]);
        bind("overview_nav_down", &["Down"]);
        bind("overview_move_left", &["SHIFT+Left"]);
        bind("overview_move_right", &["SHIFT+Right"]);
        bind("overview_move_up", &["SHIFT+Up"]);
        bind("overview_move_down", &["SHIFT+Down"]);
        bind("overview_first", &["Home"]);
        bind("overview_last", &["End"]);
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
            bindings,
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
            BoundKey::Char(c) => matches!(key, Key::Character(k) if k == c),
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
