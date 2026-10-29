//! A terminal tab: alacritty_terminal Term + pty + event loop thread,
//! plus tab metadata (title, color group, entanglement).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options, Shell};

use winit::window::Window;

/// Runs on the terminal event loop thread. Notifies the UI to redraw,
/// records title changes, and reports child exit.
#[derive(Clone)]
pub struct SpikeListener {
    pub wake: Arc<AtomicBool>,
    pub window: Arc<Mutex<Option<Arc<Window>>>>,
    pub tab_id: usize,
    pub title: Arc<Mutex<String>>,
    /// tab ids whose child process exited (drained by the app)
    pub exited: Arc<Mutex<Vec<usize>>>,
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
                self.wake.store(true, Ordering::SeqCst);
                self.notify_window();
            },
            Event::Title(t) => {
                *self.title.lock().unwrap() = t;
                self.wake.store(true, Ordering::SeqCst);
                self.notify_window();
            },
            Event::Exit | Event::ChildExit(_) => {
                self.exited.lock().unwrap().push(self.tab_id);
                self.wake.store(true, Ordering::SeqCst);
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
    ) -> Tab {
        let listener = SpikeListener {
            wake: Arc::clone(&shared.wake),
            window: Arc::clone(&shared.window),
            tab_id,
            title: Arc::new(Mutex::new(String::new())),
            exited: Arc::clone(&shared.exited),
        };

        let term = Arc::new(FairMutex::new(Term::new(
            TermConfig::default(),
            dims,
            listener.clone(),
        )));

        tty::setup_env();
        let pty = tty::new(&pty_options(program, args), window_size, tab_id as u64)
            .expect("failed to open pty");

        let title = Arc::clone(&listener.title);

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
            group: 8,
            entangled: false,
            name: None,
        }
    }

    pub fn write(&self, bytes: &[u8]) {
        let _ = self.sender.send(Msg::Input(std::borrow::Cow::Owned(bytes.to_vec())));
    }

    pub fn display_title(&self) -> String {
        if let Some(n) = &self.name {
            n.clone()
        } else {
            let t = self.title.lock().unwrap();
            if t.is_empty() {
                "shell".to_string()
            } else {
                t.clone()
            }
        }
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}
