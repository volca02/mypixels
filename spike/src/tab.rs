//! A terminal tab: alacritty_terminal Term + pty + event loop thread,
//! plus tab metadata (title, color group, entanglement).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
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
        }
    }

    pub fn write(&self, bytes: &[u8]) {
        let _ = self.sender.send(Msg::Input(std::borrow::Cow::Owned(bytes.to_vec())));
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
    fn current_dir(&self) -> Option<String> {
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
