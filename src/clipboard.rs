//! Clipboard work never runs on the window thread. Native Wayland ownership
//! lives on one worker; X11 and desktops without data-control use bounded helper
//! transfers. Clipboard contents never appear in logs.
mod native;
mod transfer;

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command as Process, Stdio};
use std::sync::{Arc, mpsc};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Clipboard,
    Primary,
}
impl Kind {
    fn index(self) -> usize {
        if self == Self::Clipboard { 0 } else { 1 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Wayland,
    X11,
    None,
}
impl Backend {
    pub fn for_window(window: &winit::window::Window) -> Self {
        use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        match window.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(_)) => Self::Wayland,
            Ok(RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_)) => Self::X11,
            _ => Self::None,
        }
    }
}

pub enum Event {
    Paste {
        target: usize,
        result: Result<String, String>,
    },
    Error(String),
}
enum Command {
    Set(Kind, Arc<str>),
    Get(Kind, usize),
}

#[derive(Clone)]
struct Reply {
    tx: mpsc::Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
impl Reply {
    fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            (self.wake)();
        }
    }
}

pub struct Clipboard {
    tx: mpsc::SyncSender<Command>,
    wake: UnixStream,
    events: mpsc::Receiver<Event>,
}
impl Clipboard {
    pub fn new(backend: Backend, wake_ui: impl Fn() + Send + Sync + 'static) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(32);
        let (events_tx, events) = mpsc::channel();
        let (wake, mut reader) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let reply = Reply {
            tx: events_tx,
            wake: Arc::new(wake_ui),
        };
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let mut native = if backend == Backend::Wayland {
                    native::Native::new(reply.clone())
                        .map_err(|e| {
                            log::info!("native clipboard unavailable ({e}); using wl-clipboard");
                        })
                        .ok()
                } else {
                    None
                };
                loop {
                    if let Some(client) = &mut native {
                        if let Err(e) = client.dispatch() {
                            log::warn!("clipboard connection lost: {e}");
                            native = None;
                            continue;
                        }
                        // Drain wake bytes before commands: an enqueue racing with
                        // this drain is either consumed below or leaves a wake byte.
                        let mut buf = [0; 128];
                        while reader.read(&mut buf).is_ok_and(|n| n > 0) {}
                        loop {
                            match rx.try_recv() {
                                Ok(cmd) => {
                                    if let Err(cmd) = client.command(cmd) {
                                        helper(backend, cmd, &reply);
                                    }
                                }
                                Err(mpsc::TryRecvError::Empty) => break,
                                Err(mpsc::TryRecvError::Disconnected) => return,
                            }
                        }
                        if let Err(e) = client.wait(&reader) {
                            log::warn!("clipboard connection lost: {e}");
                            native = None;
                        }
                    } else {
                        match rx.recv() {
                            Ok(cmd) => helper(backend, cmd, &reply),
                            Err(_) => return,
                        }
                    }
                }
            })?;
        Ok(Self { tx, wake, events })
    }

    fn send(&self, command: Command) -> Result<(), String> {
        self.tx.try_send(command).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => "Clipboard busy; try again".to_string(),
            mpsc::TrySendError::Disconnected(_) => "Clipboard unavailable".to_string(),
        })?;
        let _ = (&self.wake).write(&[1]); // WouldBlock means it is already awake.
        Ok(())
    }

    pub fn set(&self, kind: Kind, text: String) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        if text.len() > transfer::MAX_BYTES {
            return Err("Selection exceeds the 16 MiB clipboard limit".into());
        }
        self.send(Command::Set(kind, text.into()))
    }
    pub fn paste(&self, kind: Kind, target: usize) -> Result<(), String> {
        self.send(Command::Get(kind, target))
    }
    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.try_iter()
    }
}

fn argv(backend: Backend, kind: Kind, write: bool) -> Option<Vec<&'static str>> {
    let primary = kind == Kind::Primary;
    match (backend, write) {
        (Backend::None, _) => None,
        (Backend::Wayland, true) => {
            let mut args = vec!["wl-copy", "--type", "text/plain;charset=utf-8"];
            if primary {
                args.push("--primary");
            }
            Some(args)
        }
        (Backend::Wayland, false) => {
            let mut args = vec!["wl-paste", "--no-newline", "--type", "text"];
            if primary {
                args.push("--primary");
            }
            Some(args)
        }
        (Backend::X11, write) => Some(vec![
            "xsel",
            if primary { "-p" } else { "-b" },
            if write { "-i" } else { "-o" },
        ]),
    }
}

struct Helper(Child);
impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn execute(backend: Backend, kind: Kind, text: Option<&str>) -> Result<String, String> {
    let args =
        argv(backend, kind, text.is_some()).ok_or("Clipboard unavailable on this display")?;
    let deadline = Instant::now() + transfer::TIMEOUT;
    let child = Process::new(args[0])
        .args(&args[1..])
        .stdin(if text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if text.is_none() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            log::warn!("{} failed: {e}", args[0]);
            if e.kind() == io::ErrorKind::NotFound {
                format!(
                    "Clipboard unavailable: install {}",
                    if backend == Backend::Wayland {
                        "wl-clipboard"
                    } else {
                        "xsel"
                    }
                )
            } else {
                "Could not start clipboard helper".into()
            }
        })?;
    let mut child = Helper(child);
    let result = if let Some(text) = text {
        transfer::write(child.0.stdin.take().unwrap(), text.as_bytes(), deadline)
            .map(|()| String::new())
    } else {
        transfer::read(child.0.stdout.take().unwrap(), deadline)
    }
    .map_err(|e| e.to_string())?;
    loop {
        match child.0.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(result),
            Some(status) => return Err(format!("Clipboard helper exited with {status}")),
            None if Instant::now() >= deadline => return Err("Clipboard transfer timed out".into()),
            None => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
}

fn helper(backend: Backend, command: Command, reply: &Reply) {
    match command {
        Command::Set(kind, text) => {
            if let Err(e) = execute(backend, kind, Some(&text)) {
                reply.send(Event::Error(e));
            }
        }
        Command::Get(kind, target) => reply.send(Event::Paste {
            target,
            result: execute(backend, kind, None),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compatibility_paths_keep_the_buffers_separate() {
        for backend in [Backend::Wayland, Backend::X11] {
            for write in [false, true] {
                assert_ne!(
                    argv(backend, Kind::Primary, write),
                    argv(backend, Kind::Clipboard, write)
                );
            }
        }
        assert_eq!(argv(Backend::None, Kind::Clipboard, false), None);
    }
    #[test]
    fn unavailable_clipboard_returns_an_asynchronous_error() {
        let clipboard = Clipboard::new(Backend::None, || {}).unwrap();
        clipboard.paste(Kind::Clipboard, 42).unwrap();
        let Event::Paste { target, result } = clipboard
            .events
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(target, 42);
        assert!(result.is_err());
    }

    /// Run only under a disposable compositor. This changes that display's
    /// clipboard and verifies against independent wl-copy/wl-paste clients.
    #[test]
    #[ignore = "requires an isolated Wayland compositor and wl-clipboard"]
    fn wayland_interoperability() {
        assert_eq!(
            std::env::var("MYPIXELS_ISOLATED_CLIPBOARD_TEST").as_deref(),
            Ok("1")
        );
        let (tx, _rx) = mpsc::channel();
        let native = native::Native::new(Reply {
            tx,
            wake: Arc::new(|| {}),
        })
        .expect("native data-control required");
        drop(native);
        let clipboard = Clipboard::new(Backend::Wayland, || {}).unwrap();
        let text = "žluťoučký 🦀\nsecond line\n";
        clipboard.set(Kind::Clipboard, text.into()).unwrap();
        clipboard
            .set(Kind::Primary, "different primary".into())
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if execute(Backend::Wayland, Kind::Primary, None).as_deref() == Ok("different primary")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "native copy never became available"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            execute(Backend::Wayland, Kind::Clipboard, None).unwrap(),
            text
        );
        clipboard.paste(Kind::Clipboard, 42).unwrap();
        match clipboard
            .events
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
        {
            Event::Paste { target, result } => {
                assert_eq!(target, 42);
                assert_eq!(result.unwrap(), text);
            }
            Event::Error(e) => panic!("{e}"),
        }
        let external = "external text\r\nincluding CRLF\n";
        execute(Backend::Wayland, Kind::Clipboard, Some(external)).unwrap();
        clipboard.paste(Kind::Clipboard, 43).unwrap();
        match clipboard
            .events
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
        {
            Event::Paste { target, result } => {
                assert_eq!(target, 43);
                assert_eq!(result.unwrap(), external);
            }
            Event::Error(e) => panic!("{e}"),
        }
        assert_eq!(
            execute(Backend::Wayland, Kind::Primary, None).unwrap(),
            "different primary"
        );
        for i in 0..16 {
            clipboard
                .set(Kind::Clipboard, format!("replacement {i}"))
                .unwrap();
        }
        clipboard.paste(Kind::Clipboard, 44).unwrap();
        match clipboard
            .events
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
        {
            Event::Paste { target, result } => {
                assert_eq!(target, 44);
                assert_eq!(result.unwrap(), "replacement 15");
            }
            Event::Error(e) => panic!("{e}"),
        }
    }
}
