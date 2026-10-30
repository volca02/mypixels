//! Clipboard and primary selection.
//!
//! winit 0.30 exposes neither, so text moves through the same helpers the rest
//! of the desktop uses: `wl-copy`/`wl-paste` on Wayland, `xsel` on X11. Those
//! tools speak the primary-selection protocol too, which is the part that is
//! genuinely awkward to do in process -- it needs a second protocol device and a
//! seat handshake inside the compositor event loop that winit owns. `wl-copy`
//! also keeps owning the selection after the writer exits, which is what makes a
//! drag-and-let-go primary selection work at all.
//!
//! A future in-process client can replace this module without touching callers.

use std::io::Write;
use std::process::{Command, Stdio};

/// Which X11-style selection buffer. Two buffers is the point: VTE (and every
/// terminal built on GTK) fills the primary one just by selecting text, and
/// middle click pastes from it, while Ctrl+Shift+C/V move the clipboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Clipboard,
    Primary,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Wayland,
    X11,
    None,
}

/// Wayland wins when both exist: an X11 client under XWayland has a primary
/// selection that is invisible to Wayland-native clients, and this app runs
/// natively on Wayland whenever it can.
pub fn backend() -> Backend {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        Backend::Wayland
    } else if std::env::var_os("DISPLAY").is_some() {
        Backend::X11
    } else {
        Backend::None
    }
}

/// The command for a backend and buffer, and whether it writes or reads. Pure,
/// so the mapping is testable without a display server.
pub fn argv(be: Backend, kind: Kind, write: bool) -> Option<Vec<&'static str>> {
    let primary = matches!(kind, Kind::Primary);
    match (be, write) {
        (Backend::None, _) => None,
        (Backend::Wayland, true) => {
            let mut v = vec!["wl-copy"];
            if primary {
                v.push("--primary");
            }
            Some(v)
        }
        (Backend::Wayland, false) => {
            let mut v = vec!["wl-paste", "--no-newline"];
            if primary {
                v.push("--primary");
            }
            Some(v)
        }
        (Backend::X11, true) => Some(vec!["xsel", if primary { "-p" } else { "-b" }, "-i"]),
        (Backend::X11, false) => Some(vec!["xsel", if primary { "-p" } else { "-b" }, "-o"]),
    }
}

/// Publish text. Never blocks the caller: the selection owner may not read the
/// pipe until someone asks for it, and the UI thread cannot wait on that.
pub fn set(kind: Kind, text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(argv) = argv(backend(), kind, true) else {
        log::debug!("no clipboard backend; ignoring copy");
        return;
    };
    let text = text.to_string();
    let spawned = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                    let _ = stdin.flush();
                }
                // Reap it. `wl-copy` forks a watcher and exits, so this returns
                // quickly; xsel holds the selection until it loses ownership.
                let _ = child.wait();
            });
        }
        Err(e) => log::warn!("{} failed to start: {e}", argv[0]),
    }
}

/// Paste-safe text. Some tools end lines with `\r\n`, and feeding that to a
/// shell runs the line and then rings the bell.
fn clean(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).replace('\r', "")
}

/// Read text back, blocking the caller for a few milliseconds. Only used on a
/// paste keypress or a middle click, where that is exactly what a terminal does.
pub fn get(kind: Kind) -> Option<String> {
    let argv = argv(backend(), kind, false)?;
    match Command::new(argv[0]).args(&argv[1..]).stderr(Stdio::null()).output() {
        Ok(out) if out.status.success() => Some(clean(&out.stdout)),
        Ok(out) => {
            log::debug!("{} exited with {}", argv[0], out.status);
            None
        }
        Err(e) => {
            log::warn!("{} failed to start: {e}", argv[0]);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_and_clipboard_are_different_targets() {
        assert_ne!(
            argv(Backend::Wayland, Kind::Clipboard, true),
            argv(Backend::Wayland, Kind::Primary, true)
        );
        assert_ne!(
            argv(Backend::X11, Kind::Clipboard, true),
            argv(Backend::X11, Kind::Primary, true)
        );
    }

    #[test]
    fn wayland_uses_the_primary_protocol_flag() {
        assert_eq!(
            argv(Backend::Wayland, Kind::Primary, true),
            Some(vec!["wl-copy", "--primary"])
        );
        assert_eq!(
            argv(Backend::Wayland, Kind::Primary, false),
            Some(vec!["wl-paste", "--no-newline", "--primary"])
        );
        // No stray empty argument on the clipboard path.
        assert_eq!(argv(Backend::Wayland, Kind::Clipboard, true), Some(vec!["wl-copy"]));
    }

    #[test]
    fn x11_uses_the_selection_flags() {
        assert_eq!(argv(Backend::X11, Kind::Primary, true), Some(vec!["xsel", "-p", "-i"]));
        assert_eq!(argv(Backend::X11, Kind::Clipboard, false), Some(vec!["xsel", "-b", "-o"]));
    }

    #[test]
    fn no_display_server_means_no_command() {
        for kind in [Kind::Clipboard, Kind::Primary] {
            for write in [true, false] {
                assert_eq!(argv(Backend::None, kind, write), None);
            }
        }
    }

    #[test]
    fn reading_strips_the_carriage_returns_paste_machinery_adds() {
        assert_eq!(clean(b"a\r\nb\r"), "a\nb");
        assert_eq!(clean(b"plain"), "plain");
    }
}
