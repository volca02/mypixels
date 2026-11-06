//! Kitty keyboard protocol, terminal side.
//!
//! The vendored terminal already tracks the flag stack (`CSI > u` / `CSI < u`
//! / `CSI = u`, query answered by `Term::report_keyboard_mode`); what it
//! cannot know is how MY key encoding works, because this application owns
//! the keymap. So the encoder that must agree with the flags lives here,
//! next to the legacy table in `key_bytes`, and it is a pure function:
//! (key event, modifiers, flags, DECCKM) -> bytes, unit-tested against the
//! spec's own examples.
//!
//! https://sw.kovidgoyal.net/kitty/keyboard-protocol/
//!
//! Return contract, shared with the caller:
//! - `None`  - this event has no enhanced encoding; the caller uses the
//!   legacy table (a press), and sends nothing (a release).
//! - `Some(v)` - the bytes to write, possibly empty: an event the protocol
//!   says must not be sent at all (e.g. a text key's release without
//!   report-all-keys). An explicit empty is different from `None` on
//!   purpose: "legacy fallback" and "say nothing" are not the same decision.

use alacritty_terminal::term::TermMode;
use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey, PhysicalKey};

pub const DISAMBIGUATE: u8 = 1;
pub const EVENT_TYPES: u8 = 2;
pub const ALT_KEYS: u8 = 4;
pub const ALL_KEYS: u8 = 8;
pub const ASSOC_TEXT: u8 = 16;

/// Translate the terminal's mode bits into the protocol's flag byte.
pub fn active_flags(mode: &TermMode) -> u8 {
    let mut f = 0u8;
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        f |= DISAMBIGUATE;
    }
    if mode.contains(TermMode::REPORT_EVENT_TYPES) {
        f |= EVENT_TYPES;
    }
    if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
        f |= ALT_KEYS;
    }
    if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
        f |= ALL_KEYS;
    }
    if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
        f |= ASSOC_TEXT;
    }
    f
}

pub struct Input<'a> {
    pub key: &'a Key,
    /// winit's logical text for this event, when it has one.
    pub text: Option<&'a str>,
    pub physical: PhysicalKey,
    pub location: KeyLocation,
    pub release: bool,
    pub repeat: bool,
    pub mods: ModifiersState,
    pub flags: u8,
    pub app_cursor: bool,
}

/// The US-layout identity of a physical key: (unshifted, shifted). The
/// base-layout parameter of the protocol is defined against the standard
/// PC-101 layout, and the shifted codepoint the same way; other layouts get
/// their text through the main codepoint and the text parameter instead.
fn us_identity(physical: PhysicalKey) -> Option<(u32, u32)> {
    use winit::keyboard::KeyCode as C;
    let code = match physical {
        PhysicalKey::Code(c) => c,
        PhysicalKey::Unidentified(_) => return None,
    };
    let pair = match code {
        C::KeyA => ('a', 'A'),
        C::KeyB => ('b', 'B'),
        C::KeyC => ('c', 'C'),
        C::KeyD => ('d', 'D'),
        C::KeyE => ('e', 'E'),
        C::KeyF => ('f', 'F'),
        C::KeyG => ('g', 'G'),
        C::KeyH => ('h', 'H'),
        C::KeyI => ('i', 'I'),
        C::KeyJ => ('j', 'J'),
        C::KeyK => ('k', 'K'),
        C::KeyL => ('l', 'L'),
        C::KeyM => ('m', 'M'),
        C::KeyN => ('n', 'N'),
        C::KeyO => ('o', 'O'),
        C::KeyP => ('p', 'P'),
        C::KeyQ => ('q', 'Q'),
        C::KeyR => ('r', 'R'),
        C::KeyS => ('s', 'S'),
        C::KeyT => ('t', 'T'),
        C::KeyU => ('u', 'U'),
        C::KeyV => ('v', 'V'),
        C::KeyW => ('w', 'W'),
        C::KeyX => ('x', 'X'),
        C::KeyY => ('y', 'Y'),
        C::KeyZ => ('z', 'Z'),
        C::Digit1 => ('1', '!'),
        C::Digit2 => ('2', '@'),
        C::Digit3 => ('3', '#'),
        C::Digit4 => ('4', '$'),
        C::Digit5 => ('5', '%'),
        C::Digit6 => ('6', '^'),
        C::Digit7 => ('7', '&'),
        C::Digit8 => ('8', '*'),
        C::Digit9 => ('9', '('),
        C::Digit0 => ('0', ')'),
        C::Minus => ('-', '_'),
        C::Equal => ('=', '+'),
        C::BracketLeft => ('[', '{'),
        C::BracketRight => (']', '}'),
        C::Backslash => ('\\', '|'),
        C::Semicolon => (';', ':'),
        C::Quote => ('\'', '"'),
        C::Comma => (',', '<'),
        C::Period => ('.', '>'),
        C::Slash => ('/', '?'),
        C::Backquote => ('`', '~'),
        C::IntlBackslash => ('\\', '|'),
        C::Space => (' ', ' '),
        _ => return None,
    };
    Some((pair.0 as u32, pair.1 as u32))
}

/// The functional-key codepoints with no legacy form (private use area).
fn named_u(n: NamedKey) -> Option<u32> {
    Some(match n {
        NamedKey::F13 => 57376,
        NamedKey::F14 => 57377,
        NamedKey::F15 => 57378,
        NamedKey::F16 => 57379,
        NamedKey::F17 => 57380,
        NamedKey::F18 => 57381,
        NamedKey::F19 => 57382,
        NamedKey::F20 => 57383,
        NamedKey::F21 => 57384,
        NamedKey::F22 => 57385,
        NamedKey::F23 => 57386,
        NamedKey::F24 => 57387,
        NamedKey::CapsLock => 57358,
        NamedKey::ScrollLock => 57359,
        NamedKey::NumLock => 57360,
        NamedKey::PrintScreen => 57361,
        NamedKey::Pause => 57362,
        NamedKey::ContextMenu => 57363,
        NamedKey::Shift => 57441,
        NamedKey::Control => 57442,
        NamedKey::Alt => 57443,
        NamedKey::Super => 57444,
        NamedKey::Hyper => 57445,
        NamedKey::Meta => 57446,
        NamedKey::AudioVolumeDown => 57438,
        NamedKey::AudioVolumeUp => 57439,
        NamedKey::AudioVolumeMute => 57440,
        NamedKey::MediaPlay => 57428,
        NamedKey::MediaPause => 57429,
        NamedKey::MediaPlayPause => 57430,
        NamedKey::MediaStop => 57432,
        NamedKey::MediaTrackNext => 57435,
        NamedKey::MediaTrackPrevious => 57436,
        _ => return None,
    })
}

/// Numpad codepoints for named navigation keys, when the event actually
/// came from the keypad (disambiguate's whole reason for existing).
fn named_kp(n: NamedKey, location: KeyLocation) -> Option<u32> {
    if location != KeyLocation::Numpad && location != KeyLocation::Left && location != KeyLocation::Right {
        return None;
    }
    Some(match n {
        NamedKey::PageUp => 57421,
        NamedKey::PageDown => 57422,
        NamedKey::Home => 57423,
        NamedKey::End => 57424,
        NamedKey::Insert => 57425,
        NamedKey::Delete => 57426,
        NamedKey::ArrowLeft => 57417,
        NamedKey::ArrowRight => 57418,
        NamedKey::ArrowUp => 57419,
        NamedKey::ArrowDown => 57420,
        NamedKey::Enter => 57414,
        _ => return None,
    })
}

/// Keypad codepoints for Character events produced by the keypad.
fn char_kp(text: Option<&str>, physical: PhysicalKey) -> Option<u32> {
    use winit::keyboard::KeyCode as C;
    let code = match physical {
        PhysicalKey::Code(c) => c,
        _ => return None,
    };
    let _ = text?;
    Some(match code {
        C::Numpad0 => 57399,
        C::Numpad1 => 57400,
        C::Numpad2 => 57401,
        C::Numpad3 => 57402,
        C::Numpad4 => 57403,
        C::Numpad5 => 57404,
        C::Numpad6 => 57405,
        C::Numpad7 => 57406,
        C::Numpad8 => 57407,
        C::Numpad9 => 57408,
        C::NumpadDecimal => 57409,
        C::NumpadDivide => 57410,
        C::NumpadMultiply => 57411,
        C::NumpadSubtract => 57412,
        C::NumpadAdd => 57413,
        C::NumpadEqual => 57415,
        C::NumpadComma => 57416,
        _ => return None,
    })
}

/// One CSI-u sequence with its optional sub-fields.
fn csi_u(cp: u32, mcode: u8, ev: &str, alts: &str, text: Option<&str>) -> Vec<u8> {
    // The modifier field is optional and defaults to 1 ("no modifiers"),
    // so a bare event omits it entirely - the way kitty itself prints
    // `ESC[27u` for a plain Escape.
    let mut s = if mcode == 1 && ev.is_empty() && alts.is_empty() && text.is_none() {
        format!("\x1b[{cp}")
    } else {
        format!("\x1b[{cp}{alts};{mcode}{ev}")
    };
    if let Some(t) = text {
        let cps: Vec<String> = t.chars().map(|c| (c as u32).to_string()).collect();
        if !cps.is_empty() {
            s.push(';');
            s.push_str(&cps.join(":"));
        }
    }
    s.push('u');
    s.into_bytes()
}

/// The `:event-type` sub-parameter, when event reporting is on.
fn ev_sub(i: &Input) -> String {
    if i.flags & EVENT_TYPES == 0 {
        return String::new();
    }
    if i.release {
        ":3".to_string()
    } else if i.repeat {
        ":2".to_string()
    } else {
        String::new()
    }
}

/// Modifiers as the protocol encodes them, from what the platform layer
/// reports. Hyper, meta and the lock keys have no representation in winit's
/// state (and no route to reach it on the keyboards in question), so those
/// bits are honestly zero rather than invented.
fn mcode(i: &Input) -> u8 {
    1 + i.mods.shift_key() as u8
        + (i.mods.alt_key() as u8) * 2
        + (i.mods.control_key() as u8) * 4
        + (i.mods.super_key() as u8) * 8
}

/// The `:shifted[:base]` sub-fields for the key code, when requested.
fn alt_key_fields(i: &Input, shifted: Option<u32>) -> String {
    if i.flags & ALT_KEYS == 0 {
        return String::new();
    }
    let base = us_identity(i.physical).map(|(b, _)| b);
    // The shifted codepoint is only sent while shift is actually in the
    // modifiers (spec, "Key codes"); a base-only event uses an empty
    // sub-field for the missing shifted key.
    match (shifted.filter(|_| i.mods.shift_key()), base) {
        (Some(s), Some(b)) => format!(":{s}:{b}"),
        (Some(s), None) => format!(":{s}"),
        (None, Some(b)) => format!("::{b}"),
        (None, None) => String::new(),
    }
}

/// Encode one key event; see the module comment for the return contract.
pub fn encode(i: &Input) -> Option<Vec<u8>> {
    let m = mcode(i);
    let ev = ev_sub(i);
    let text = i.text;

    match i.key {
        Key::Character(c) => {
            let ctrl = i.mods.control_key();
            let alt = i.mods.alt_key();
            let shift = i.mods.shift_key();
            let exotic = i.mods.super_key();
            // Keypad characters are separate keys under disambiguate.
            if i.flags & DISAMBIGUATE != 0 {
                if let Some(cp) = char_kp(text, i.physical) {
                    let alts = alt_key_fields(i, text.and_then(|t| t.chars().next()).map(|c| c as u32));
                    let t = if i.flags & ASSOC_TEXT != 0 && i.flags & ALL_KEYS != 0 { text } else { None };
                    return Some(csi_u(cp, m, &ev, &alts, t));
                }
            }
            // Decide the key's CSI-u codepoint once; `None` means this key
            // stays a text/legacy event. A release can only ever be sent
            // inside an escape-coded event with event reporting on, so the
            // same decision answers both directions.
            let cp: Option<u32> = if exotic {
                // Super/hyper/meta have no legacy encoding at all: the
                // CSI-u form is the only honest one, flags or not.
                us_identity(i.physical).map(|(u, _)| u).or_else(|| c.chars().next().map(|c| c as u32))
            } else if ctrl || alt {
                if i.flags & DISAMBIGUATE != 0 {
                    us_identity(i.physical).map(|(u, _)| u).or_else(|| {
                        let ch = c.chars().next()?;
                        Some(if shift { ch.to_lowercase().next().unwrap_or(ch) } else { ch } as u32)
                    })
                } else {
                    // ctrl/alt combos stay legacy bytes unless
                    // disambiguation was requested.
                    None
                }
            } else if i.flags & ALL_KEYS != 0 {
                // Plain text becomes an escape event only under
                // report-all-keys; the codepoint is the UNSHIFTED key.
                us_identity(i.physical).map(|(u, _)| u).or_else(|| {
                    c.chars().next().and_then(|ch| ch.to_lowercase().next()).map(|c| c as u32)
                })
            } else {
                None
            };
            let Some(cp) = cp else {
                return if i.release { Some(Vec::new()) } else { None };
            };
            if i.release && i.flags & EVENT_TYPES == 0 {
                return Some(Vec::new());
            }
            let shifted = if shift { text.and_then(|t| t.chars().next()).map(|c| c as u32) } else { None };
            let alts = alt_key_fields(i, shifted);
            let t = if i.flags & ASSOC_TEXT != 0 && i.flags & ALL_KEYS != 0 { text } else { None };
            Some(csi_u(cp, m, &ev, &alts, t))
        },
        Key::Unidentified(_) | Key::Dead(_) => {
            // Nothing to say without a key identity; the legacy path (which
            // at least has the composed character) decides.
            if i.release {
                Some(Vec::new())
            } else {
                None
            }
        },
        Key::Named(n) => {
            // Keys with no legacy form: CSI u whenever they can be sent.
            if let Some(cp) = named_u(n.clone()) {
                if i.release && i.flags & EVENT_TYPES == 0 {
                    return Some(Vec::new());
                }
                // Modifier keys report only under report-all-keys.
                let is_modifier = matches!(
                    n,
                    NamedKey::Shift
                        | NamedKey::Control
                        | NamedKey::Alt
                        | NamedKey::Super
                        | NamedKey::Hyper
                        | NamedKey::Meta
                );
                if is_modifier && i.flags & ALL_KEYS == 0 {
                    return Some(Vec::new());
                }
                return Some(csi_u(cp, m, &ev, "", None));
            }
            // Keypad navigation under disambiguate: dedicated keys.
            if i.flags & DISAMBIGUATE != 0 {
                if let Some(cp) = named_kp(n.clone(), i.location) {
                    if i.release && i.flags & EVENT_TYPES == 0 {
                        return Some(Vec::new());
                    }
                    return Some(csi_u(cp, m, &ev, "", None));
                }
            }
            match n {
                NamedKey::Escape => {
                    if i.flags & (ALL_KEYS | DISAMBIGUATE) == 0 {
                        return if i.release { Some(Vec::new()) } else { None };
                    }
                    if i.release && i.flags & EVENT_TYPES == 0 {
                        return Some(Vec::new());
                    }
                    let alts = alt_key_fields(i, None);
                    Some(csi_u(27, m, &ev, &alts, None))
                },
                NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace => {
                    // The documented exceptions: legacy bytes unless
                    // report-all-keys, so a crashed app cannot eat the
                    // shell. Shift+Tab is NOT a legacy survivor once
                    // disambiguating - backtab has no codepoint of its own.
                    let shift_tab = matches!(n, NamedKey::Tab) && i.mods.shift_key();
                    let enhanced = i.flags & ALL_KEYS != 0 || (shift_tab && i.flags & DISAMBIGUATE != 0);
                    if !enhanced {
                        return if i.release { Some(Vec::new()) } else { None };
                    }
                    if i.release && i.flags & EVENT_TYPES == 0 {
                        return Some(Vec::new());
                    }
                    let cp = match n {
                        NamedKey::Enter => 13,
                        NamedKey::Tab => 9,
                        _ => 127,
                    };
                    let shifted = if shift_tab { Some(9) } else { None };
                    let alts = alt_key_fields(i, shifted);
                    let t = if i.flags & ASSOC_TEXT != 0 && matches!(n, NamedKey::Tab) {
                        Some("\t")
                    } else {
                        None
                    };
                    Some(csi_u(cp, m, &ev, &alts, t))
                },
                NamedKey::Space => {
                    // Space is a text key with a legacy control mapping;
                    // it follows the Character rules through here when a
                    // modifier turns winit's output into a named key.
                    let legacy_able =
                        !i.mods.control_key() && !i.mods.alt_key() && !i.mods.super_key();
                    if !legacy_able && i.flags & DISAMBIGUATE != 0 {
                        if i.release && i.flags & EVENT_TYPES == 0 {
                            return Some(Vec::new());
                        }
                        return Some(csi_u(32, m, &ev, "", None));
                    }
                    if i.flags & ALL_KEYS != 0 {
                        if i.release && i.flags & EVENT_TYPES == 0 {
                            return Some(Vec::new());
                        }
                        return Some(csi_u(32, m, &ev, "", if i.flags & ASSOC_TEXT != 0 { Some(" ") } else { None }));
                    }
                    if i.release {
                        Some(Vec::new())
                    } else {
                        None
                    }
                },
                _ => encode_legacy_shaped(n.clone(), i, m, &ev),
            }
        },
    }
}

/// Functional keys whose canonical CSI forms carry the enhanced fields
/// themselves (`CSI 15;mods~`, `CSI 1;mods;event A`): they keep their legacy
/// shape and gain parameters, per the functional key table.
fn encode_legacy_shaped(n: NamedKey, i: &Input, m: u8, ev: &str) -> Option<Vec<u8>> {
    // (keyed-tilde number, or final letter of the `CSI 1;...` family).
    let form: (char, u8) = match n {
        NamedKey::Insert => ('~', 2),
        NamedKey::Delete => ('~', 3),
        NamedKey::PageUp => ('~', 5),
        NamedKey::PageDown => ('~', 6),
        NamedKey::Home => ('H', 1),
        NamedKey::End => ('F', 1),
        NamedKey::ArrowUp => ('A', 1),
        NamedKey::ArrowRight => ('C', 1),
        NamedKey::ArrowDown => ('B', 1),
        NamedKey::ArrowLeft => ('D', 1),
        NamedKey::F1 => ('P', 1),
        NamedKey::F2 => ('Q', 1),
        NamedKey::F3 => ('R', 1),
        NamedKey::F4 => ('S', 1),
        NamedKey::F5 => ('~', 15),
        NamedKey::F6 => ('~', 17),
        NamedKey::F7 => ('~', 18),
        NamedKey::F8 => ('~', 19),
        NamedKey::F9 => ('~', 20),
        NamedKey::F10 => ('~', 21),
        NamedKey::F11 => ('~', 23),
        NamedKey::F12 => ('~', 24),
        _ => return if i.release { Some(Vec::new()) } else { None },
    };
    // The legacy table already emits these forms correctly, modifiers
    // included - `CSI 15;5~` is `CSI 15;5~` whoever prints it. Only event
    // types are new information worth re-encoding here.
    if ev.is_empty() {
        return None;
    }
    if i.release && i.flags & EVENT_TYPES == 0 {
        return Some(Vec::new());
    }
    let body = if form.1 == 1 && form.0 != '~' {
        // arrows/home/end/F1-4
        format!("\x1b[1;{m}{ev}{}", form.0)
    } else {
        format!("\x1b[{};{m}{ev}~", form.1)
    };
    Some(body.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{Key, KeyLocation, ModifiersState, NamedKey, PhysicalKey};

    fn mk<'a>(key: &'a Key, text: Option<&'a str>, flags: u8) -> Input<'a> {
        Input {
            key,
            text,
            physical: PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
            location: KeyLocation::Standard,
            release: false,
            repeat: false,
            mods: ModifiersState::empty(),
            flags,
            app_cursor: false,
        }
    }

    fn s(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).replace('\x1b', "ESC")
    }

    #[test]
    fn disambiguate_examples_from_the_spec() {
        let a = Key::Character("a".into());
        let mut ctrl_a = mk(&a, Some("a"), 1);
        ctrl_a.mods = ModifiersState::CONTROL;
        assert_eq!(s(&encode(&ctrl_a).unwrap()), "ESC[97;5u");
        let mut alt_a = mk(&a, Some("a"), 1);
        alt_a.mods = ModifiersState::ALT;
        assert_eq!(s(&encode(&alt_a).unwrap()), "ESC[97;3u");
        let esc = Key::Named(NamedKey::Escape);
        assert_eq!(s(&encode(&mk(&esc, None, 1)).unwrap()), "ESC[27u");
        let tab = Key::Named(NamedKey::Tab);
        let mut shift_tab = mk(&tab, None, 1);
        shift_tab.mods = ModifiersState::SHIFT;
        assert_eq!(s(&encode(&shift_tab).unwrap()), "ESC[9;2u");
        // The legacy exceptions: plain Enter/Tab/Backspace stay legacy.
        let enter = Key::Named(NamedKey::Enter);
        assert!(encode(&mk(&enter, None, 1)).is_none());
        // Plain text is still plain text.
        assert!(encode(&mk(&a, Some("a"), 1)).is_none());
    }

    #[test]
    fn event_types_ride_the_modifier_field() {
        let a = Key::Character("a".into());
        let mut rel = mk(&a, Some("a"), 3);
        rel.mods = ModifiersState::CONTROL;
        rel.release = true;
        assert_eq!(s(&encode(&rel).unwrap()), "ESC[97;5:3u");
        let mut rep = mk(&a, Some("a"), 3);
        rep.mods = ModifiersState::CONTROL;
        rep.repeat = true;
        assert_eq!(s(&encode(&rep).unwrap()), "ESC[97;5:2u");
        // Text key releases are silent unless all-keys (explicit nothing).
        let mut plain_rel = mk(&a, Some("a"), 3);
        plain_rel.release = true;
        assert_eq!(encode(&plain_rel).unwrap(), Vec::<u8>::new());
        // Functional keys gain the sub-parameter in their legacy shape.
        let up = Key::Named(NamedKey::ArrowUp);
        let mut up_rel = mk(&up, None, 3);
        up_rel.release = true;
        assert_eq!(s(&encode(&up_rel).unwrap()), "ESC[1;1:3A");
        let f5 = Key::Named(NamedKey::F5);
        let mut f5_rep = mk(&f5, None, 2);
        f5_rep.repeat = true;
        assert_eq!(s(&encode(&f5_rep).unwrap()), "ESC[15;1:2~");
    }

    #[test]
    fn all_keys_reports_text_keys_as_escape_codes_with_text() {
        let a = Key::Character("a".into());
        assert_eq!(s(&encode(&mk(&a, Some("a"), 24)).unwrap()), "ESC[97;1;97u");
        let big = Key::Character("A".into());
        let mut shift_a = mk(&big, Some("A"), 24);
        shift_a.mods = ModifiersState::SHIFT;
        assert_eq!(s(&encode(&shift_a).unwrap()), "ESC[97;2;65u");
        let enter = Key::Named(NamedKey::Enter);
        assert_eq!(s(&encode(&mk(&enter, None, 8)).unwrap()), "ESC[13u");
    }

    #[test]
    fn alternate_keys_and_base_layout() {
        use winit::keyboard::{KeyCode, PhysicalKey};
        let big = Key::Character("A".into());
        let mut i = mk(&big, Some("A"), 8 | 4);
        i.mods = ModifiersState::SHIFT;
        i.physical = PhysicalKey::Code(KeyCode::KeyA);
        assert_eq!(s(&encode(&i).unwrap()), "ESC[97:65:97;2u");
    }

    #[test]
    fn unmodified_plain_function_keys_fall_back_to_legacy() {
        let f5 = Key::Named(NamedKey::F5);
        assert!(encode(&mk(&f5, None, 1)).is_none());
        let ins = Key::Named(NamedKey::Insert);
        assert!(encode(&mk(&ins, None, 0)).is_none());
        let left = Key::Named(NamedKey::ArrowLeft);
        let mut ctrl_left = mk(&left, None, 1);
        ctrl_left.mods = ModifiersState::CONTROL;
        // ctrl+arrow has a legacy form the caller already produces; the
        // enhanced encoder only speaks when there is something to add.
        assert!(encode(&ctrl_left).is_none());
        // A ctrl+arrow PRESS adds nothing the legacy table does not already
        // produce byte-identically; releases are where events matter:
        let mut ctrl_left_ev = mk(&left, None, 3);
        ctrl_left_ev.mods = ModifiersState::CONTROL;
        assert!(encode(&ctrl_left_ev).is_none());
        ctrl_left_ev.release = true;
        assert_eq!(s(&encode(&ctrl_left_ev).unwrap()), "ESC[1;5:3D");
    }

    #[test]
    fn keypad_gets_its_own_keys_under_disambiguate() {
        use winit::keyboard::{KeyCode, KeyLocation, PhysicalKey};
        let four = Key::Character("4".into());
        let mut i = mk(&four, Some("4"), 1);
        i.physical = PhysicalKey::Code(KeyCode::Numpad4);
        i.location = KeyLocation::Numpad;
        assert_eq!(s(&encode(&i).unwrap()), "ESC[57403u");
    }
}
