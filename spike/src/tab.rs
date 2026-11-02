//! A terminal tab: alacritty_terminal Term + pty + event loop thread,
//! plus tab metadata (title, color group, entanglement).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::index::{Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options, Shell};
// `process_id()` on the pty child; needed to read the child's cwd from /proc.
use portable_pty::Child as _;

use winit::window::Window;

/// Runs on the terminal event loop thread. Notifies the UI to redraw,
/// records title changes, and reports child exit.
#[derive(Clone)]
pub struct SpikeListener {
    pub window: Arc<Mutex<Option<Arc<Window>>>>,
    pub tab_id: usize,
    pub title: Arc<Mutex<String>>,
    /// tab ids whose child process exited (drained by the app)
    pub exited: Arc<Mutex<Vec<usize>>>,
    /// last time the terminal produced output (drives cursor-blink cadence)
    pub last_activity: Arc<Mutex<std::time::Instant>>,
}

impl SpikeListener {
    fn notify_window(&self) {
        if let Some(w) = self.window.lock().unwrap().as_ref() {
            w.request_redraw();
        }
    }
}

impl EventListener for SpikeListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                *self.last_activity.lock().unwrap() = std::time::Instant::now();
                self.notify_window();
            },
            Event::Title(t) => {
                *self.title.lock().unwrap() = t;
                *self.last_activity.lock().unwrap() = std::time::Instant::now();
                self.notify_window();
            },
            Event::Exit | Event::ChildExit(_) => {
                self.exited.lock().unwrap().push(self.tab_id);
                *self.last_activity.lock().unwrap() = std::time::Instant::now();
                self.notify_window();
            },
            _ => {},
        }
    }
}

/// Static-ish per-tab pty options.
fn pty_options(program: String, args: Vec<String>) -> Options {
    Options {
        shell: Some(Shell::new(program, args)),
        working_directory: None,
        drain_on_exit: false,
        env: HashMap::new(),
    }
}

pub struct Tab {
    /// Stable id (assigned at creation, unaffected by tab removals).
    pub id: usize,
    pub term: Arc<FairMutex<Term<SpikeListener>>>,

    pub sender: EventLoopSender,
    /// Keep the event loop thread alive.
    pub handle: std::thread::JoinHandle<(
        alacritty_terminal::event_loop::EventLoop<alacritty_terminal::tty::Pty, SpikeListener>,
        alacritty_terminal::event_loop::State,
    )>,
    pub title: Arc<Mutex<String>>,
    /// Pid of the tab's child process, for reading its cwd from /proc.
    pub child_pid: Option<u32>,
    /// Cached cwd text, and when it was last read (`None` = never).
    cwd: Arc<Mutex<(String, Option<std::time::Instant>)>>,
    /// Color group 0..7, 8 = default (no group override)
    pub group: usize,
    pub entangled: bool,
    /// User rename (None = use terminal title)
    pub name: Option<String>,
    /// Damage-tracking frame cache; see `App::build_frame`. Rebuilding only
    /// the rows the terminal reports as changed is what keeps an idle tab
    /// under an open config panel at zero terminal-walk cost per redraw.
    pub frame_cache: std::cell::RefCell<std::rc::Rc<crate::FrameCache>>,
    /// `last_activity` value whose output this tab's frame cache already
    /// incorporates. Equal stamps mean no PTY output since: the runs cannot
    /// have gone stale, so the damage machinery is not even consulted.
    pub out_seen: std::cell::Cell<Option<std::time::Instant>>,
}

impl Tab {
    pub fn new(
        tab_id: usize,
        dims: &impl Dimensions,
        window_size: WindowSize,
        program: String,
        args: Vec<String>,
        shared: &SpikeListener,
        history: usize,
    ) -> Tab {
        let listener = SpikeListener {
            window: Arc::clone(&shared.window),
            tab_id,
            title: Arc::new(Mutex::new(String::new())),
            exited: Arc::clone(&shared.exited),
            last_activity: Arc::clone(&shared.last_activity),
        };

        let term = Arc::new(FairMutex::new(Term::new(
            TermConfig {
                scrolling_history: history,
                ..Default::default()
            },
            dims,
            listener.clone(),
        )));

        tty::setup_env();
        let pty = tty::new(&pty_options(program, args), window_size, tab_id as u64)
            .expect("failed to open pty");

        let title = Arc::clone(&listener.title);

        let child_pid = pty.child().process_id();

        let loop_ = alacritty_terminal::event_loop::EventLoop::new(
            Arc::clone(&term),
            listener,
            pty,
            false,
            false,
        )
        .expect("terminal event loop");
        let sender = loop_.channel();
        let handle = loop_.spawn();

        Tab {
            id: tab_id,
            term,
            sender,
            handle,
            title,
            child_pid,
            cwd: Arc::new(Mutex::new((String::new(), None))),
            group: 8,
            entangled: false,
            name: None,
            frame_cache: std::cell::RefCell::new(std::rc::Rc::new(crate::FrameCache::default())),
            out_seen: std::cell::Cell::new(None),
        }
    }

    pub fn write(&self, bytes: &[u8]) {
        let _ = self.sender.send(Msg::Input(std::borrow::Cow::Owned(bytes.to_vec())));
    }

    /// Start a selection at a cell. `lines`/`word` come from click count: three
    /// clicks take whole rows, two take the word under the pointer.
    pub fn begin_selection(&mut self, at: Point, side: Side, ty: SelectionType) {
        let mut sel = Selection::new(ty, at, side);
        if ty != SelectionType::Simple {
            // Semantic and line selections expand to fill their unit, which is
            // what `to_range` needs to see the whole word or row.
            sel.include_all();
        }
        self.term.lock().selection = Some(sel);
    }

    /// Grow the selection to a cell while dragging.
    pub fn update_selection(&mut self, at: Point, side: Side) {
        if let Some(sel) = self.term.lock().selection.as_mut() {
            sel.update(at, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.lock().selection = None;
    }

    /// The text under the selection, or None if nothing is really selected.
    pub fn selected_text(&self) -> Option<String> {
        let term = self.term.lock();
        let text = term.selection_to_string()?;
        (!text.is_empty()).then_some(text)
    }

    /// Rows covered by the selection, clamped to the viewport, as
    /// `(row, first_column, one_past_last_column)` for drawing. Selection lines
    /// are grid coordinates -- line 0 is the top of the unscrolled screen, above
    /// that is negative -- so the scroll offset comes off here.
    pub fn selection_rows(&self) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        let term = self.term.lock();
        let Some(sel) = term.selection.as_ref() else {
            return out;
        };
        // A click with no drag is an empty selection, not a one-cell one: it must
        // not paint a wash where the pointer happened to land.
        if sel.is_empty() {
            return out;
        }
        let Some(range) = sel.to_range(&term) else {
            return out;
        };
        let off = term.grid().display_offset() as i32;
        let cols = term.columns();
        let rows = term.screen_lines();
        for line in range.start.line.0..=range.end.line.0 {
            let vis = line + off;
            if vis < 0 || vis as usize >= rows {
                continue;
            }
            let (left, right) = if range.is_block {
                // A block selection keeps its columns on every row it spans.
                (range.start.column.0, range.end.column.0 + 1)
            } else {
                let left = if line == range.start.line.0 { range.start.column.0 } else { 0 };
                let right =
                    if line == range.end.line.0 { range.end.column.0 + 1 } else { cols };
                (left, right)
            };
            let (l, r) = (left.min(cols), right.min(cols));
            if r > l {
                out.push((vis as usize, l, r));
            }
        }
        out
    }

    /// Paste, honouring bracketed paste so a multi-line paste is not run as keys.
    pub fn paste(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut term = self.term.lock();
        let bracketed = term.mode().contains(TermMode::BRACKETED_PASTE);
        term.selection = None;
        term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        drop(term);
        let bytes = paste_bytes(text, bracketed);
        self.write(&bytes);
    }

    pub fn scroll(&self, scroll: alacritty_terminal::grid::Scroll) {
        self.term.lock().scroll_display(scroll);
    }

    /// Tab label, matching the original's precedence: an explicit rename wins,
    /// then the terminal title from OSC 0/2 (which is what a configured shell
    /// feeds it -- the user's own zsh `precmd` sends `\e]0;%~%#`), then, when the
    /// program never sets a title at all, the child's working directory. VTE gets
    /// that from its child-cwd tracking; the equivalent here is /proc, because
    /// alacritty_terminal parses no OSC 7 (there is no `CurrentDir` event), so a
    /// shell that reports its cwd is silently ignored.
    pub fn display_title(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        {
            let t = self.title.lock().unwrap();
            if !t.is_empty() {
                return t.clone();
            }
        }
        self.current_dir().unwrap_or_else(|| "shell".to_string())
    }

    /// The tab child's working directory, with `$HOME` collapsed to `~` the way a
    /// prompt would. Read at most every 400ms: the strip asks for every tab on
    /// every frame, and a frame that only blinks the cursor should not walk
    /// /proc.
    pub fn current_dir(&self) -> Option<String> {
        const EVERY: std::time::Duration = std::time::Duration::from_millis(400);
        let mut c = self.cwd.lock().unwrap();
        if c.1.is_none_or(|t| t.elapsed() >= EVERY) {
            c.1 = Some(std::time::Instant::now());
            c.0 = read_cwd(self.child_pid).unwrap_or_default();
        }
        if c.0.is_empty() {
            None
        } else {
            Some(c.0.clone())
        }
    }
}

/// Preserve newlines while normalizing foreign line endings. A plain PTY wants
/// CR for Enter; bracketed paste wants LF. Embedded ESC/ETX cannot terminate a
/// bracketed paste early and turn the remainder into shell keypresses.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.replace(['\x1b', '\x03'], "");
    if bracketed { format!("\x1b[200~{text}\x1b[201~").into_bytes() }
    else { text.replace('\n', "\r").into_bytes() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paste_line_endings_match_the_terminal_mode() {
        assert_eq!(paste_bytes("a\r\nb\rc\n", false), b"a\rb\rc\r");
        assert_eq!(paste_bytes("a\r\nb\rc\n", true), b"\x1b[200~a\nb\nc\n\x1b[201~");
    }
    #[test]
    fn clipboard_cannot_end_bracketed_paste_early() {
        assert_eq!(paste_bytes("x\x1b[201~\x03echo bad", true), b"\x1b[200~x[201~echo bad\x1b[201~");
        assert_eq!(paste_bytes("č 🦀\t", false), "č 🦀\t".as_bytes());
    }
}

/// Read a process's cwd, shortened for a tab label.
fn read_cwd(pid: Option<u32>) -> Option<String> {
    let link = std::fs::read_link(format!("/proc/{}/cwd", pid?));
    if let Err(e) = &link {
        // Expected once the child is gone; not worth a warning every 400ms.
        log::debug!("cwd read for pid {pid:?} failed: {e}");
    }
    let p = link.ok()?;
    let s = p.to_string_lossy().into_owned();
    if s.is_empty() {
        return None;
    }
    match dirs::home_dir().map(|h| h.to_string_lossy().into_owned()) {
        Some(home) if s == home => Some("~".to_string()),
        Some(home) if s.starts_with(&home) => Some(format!("~{}", &s[home.len()..])),
        _ => Some(s),
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}
