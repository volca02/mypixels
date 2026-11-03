//! Surface-free data-control connection. ext-data-control is preferred; the
//! older wlr protocol uses the same lifecycle. These protocols do not require
//! input serials from winit's private connection. Normal wl_data_device does.
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop, event_created_child,
};

use super::{Command, Event, Kind, Reply, transfer};

const MIMES: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
];
type OfferData = Mutex<Vec<String>>;

fn text_mime(offered: &[String]) -> Option<&str> {
    MIMES
        .iter()
        .find(|mime| offered.iter().any(|s| s == **mime))
        .copied()
}

struct ReadJob {
    file: File,
    target: usize,
    deadline: Instant,
}
fn reader(reply: Reply) -> io::Result<mpsc::SyncSender<ReadJob>> {
    let (tx, rx) = mpsc::sync_channel::<ReadJob>(16);
    // One reader preserves paste ordering and bounds threads even if an owner
    // never closes its pipe. The protocol worker remains free to serve copies.
    std::thread::Builder::new()
        .name("clipboard-read".into())
        .spawn(move || {
            for job in rx {
                let result = transfer::read(job.file, job.deadline).map_err(|e| e.to_string());
                reply.send(Event::Paste {
                    target: job.target,
                    result,
                });
            }
        })?;
    Ok(tx)
}

struct WriterSlot(Arc<AtomicUsize>);
impl Drop for WriterSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
fn write_source(file: File, payload: Arc<crate::clipboard::Payload>, writers: &Arc<AtomicUsize>) {
    if writers
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n < 8).then_some(n + 1)
        })
        .is_err()
    {
        log::warn!("too many simultaneous clipboard readers");
        return;
    }
    let slot = WriterSlot(writers.clone());
    let deadline = Instant::now() + transfer::TIMEOUT;
    if let Err(e) = std::thread::Builder::new()
        .name("clipboard-write".into())
        .spawn(move || {
            let _slot = slot;
            if let Err(e) = transfer::write(file, payload.bytes(), deadline) {
                log::debug!("clipboard reader disconnected: {e}");
            }
        })
    {
        log::warn!("clipboard writer: {e}");
    }
}

fn wait<T>(conn: &Connection, queue: &EventQueue<T>, wake: &UnixStream) -> io::Result<()> {
    let guard = match queue.prepare_read() {
        Some(guard) => guard,
        None => return Ok(()),
    };
    let needs_flush = match conn.flush() {
        Ok(()) => false,
        Err(wayland_client::backend::WaylandError::Io(e))
            if e.kind() == io::ErrorKind::WouldBlock =>
        {
            true
        }
        Err(e) => return Err(io::Error::other(e)),
    };
    let mut fds = [
        libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN | if needs_flush { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: both descriptors and the pollfd array remain live during poll.
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(e)
        };
    }
    if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(io::Error::other(e)),
        }
    }
    Ok(())
}

// The protocol interfaces differ only in their names and wlr's version gate
// for primary selection. Keep offer/source destruction and transfer handling
// identical; a source is never reused in another set_selection request.
macro_rules! client {
    ($interface:literal, $version:literal, $primary_version:literal) => {
        pub(in crate::clipboard) struct Client { conn: Connection, queue: EventQueue<State>, state: State }
        struct State {
            manager: Option<Manager>, seat: Option<wl_seat::WlSeat>, device: Option<Device>,
            globals: Vec<u32>, offers: [Option<Offer>; 2], sources: [Option<Source>; 2],
            primary: bool, finished: bool, reply: Reply,
            reads: mpsc::SyncSender<ReadJob>, writers: Arc<AtomicUsize>,
        }
        impl Client {
            pub(super) fn new(reply: Reply) -> io::Result<Self> {
                let conn = Connection::connect_to_env().map_err(io::Error::other)?;
                let mut queue = conn.new_event_queue();
                let qh = queue.handle();
                let mut state = State {
                    manager: None, seat: None, device: None, globals: Vec::new(),
                    offers: [None, None], sources: [None, None], primary: false, finished: false,
                    reads: reader(reply.clone())?, reply, writers: Arc::new(AtomicUsize::new(0)),
                };
                conn.display().get_registry(&qh, ());
                queue.roundtrip(&mut state).map_err(io::Error::other)?;
                let manager = state.manager.as_ref().ok_or_else(|| io::Error::other(concat!($interface, " not advertised")))?;
                let seat = state.seat.as_ref().ok_or_else(|| io::Error::other("no Wayland seat"))?;
                state.device = Some(manager.get_data_device(seat, &qh, ()));
                queue.roundtrip(&mut state).map_err(io::Error::other)?;
                log::info!("clipboard: {} (primary: {})", $interface, state.primary);
                Ok(Self { conn, queue, state })
            }
            pub(super) fn dispatch(&mut self) -> io::Result<()> {
                self.queue.dispatch_pending(&mut self.state).map_err(io::Error::other)?;
                if self.state.finished { return Err(io::Error::other("data-control device finished")) }
                Ok(())
            }
            pub(super) fn wait(&self, wake: &UnixStream) -> io::Result<()> { wait(&self.conn, &self.queue, wake) }
            pub(super) fn command(&mut self, cmd: Command) -> Result<(), Command> {
                let kind = match &cmd { Command::Set(kind, _) | Command::Get(kind, _) => *kind };
                if kind == Kind::Primary && !self.state.primary { return Err(cmd) }
                let index = kind.index();
                match cmd {
                    Command::Set(_, payload) => {
                        let source = self.state.manager.as_ref().unwrap().create_data_source(&self.queue.handle(), Arc::clone(&payload));
                        match &*payload {
                            crate::clipboard::Payload::Text(_) => { for mime in MIMES { source.offer(mime.into()); } }
                            crate::clipboard::Payload::Png(_) => source.offer("image/png".into()),
                        }
                        let device = self.state.device.as_ref().unwrap();
                        if index == 0 { device.set_selection(Some(&source)); } else { device.set_primary_selection(Some(&source)); }
                        if let Some(old) = self.state.sources[index].replace(source) { old.destroy(); }
                    }
                    Command::Get(_, target) => {
                        // Synchronize ownership changes, including a copy followed
                        // immediately by paste before its selection event arrived.
                        let result = self.queue.roundtrip(&mut self.state).map_err(io::Error::other).and_then(|_| {
                            let Some(offer) = self.state.offers[index].as_ref() else { return Ok(None) };
                            let mimes = offer.data::<OfferData>().unwrap().lock().unwrap();
                            let mime = text_mime(&mimes).ok_or_else(|| io::Error::other("Clipboard does not contain text"))?;
                            let (file, writer) = transfer::pipe()?;
                            offer.receive(mime.into(), writer.as_fd());
                            drop(writer);
                            Ok(Some(ReadJob { file, target, deadline: Instant::now() + transfer::TIMEOUT }))
                        });
                        match result {
                            Ok(Some(job)) => if self.state.reads.try_send(job).is_err() {
                                self.state.reply.send(Event::Paste { target, result: Err("Clipboard busy; try again".into()) });
                            },
                            Ok(None) => self.state.reply.send(Event::Paste { target, result: Ok(String::new()) }),
                            Err(e) => self.state.reply.send(Event::Paste { target, result: Err(e.to_string()) }),
                        }
                    }
                }
                Ok(())
            }
        }
        impl Dispatch<wl_registry::WlRegistry, ()> for State {
            fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
                match event {
                    wl_registry::Event::Global { name, interface, version } if interface == $interface => {
                        state.manager = Some(registry.bind(name, version.min($version), qh, ()));
                        state.globals.push(name);
                    }
                    wl_registry::Event::Global { name, interface, version } if interface == "wl_seat" && state.seat.is_none() => {
                        state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                        state.globals.push(name);
                    }
                    wl_registry::Event::GlobalRemove { name } if state.globals.contains(&name) => state.finished = true,
                    _ => {},
                }
            }
        }
        delegate_noop!(State: ignore wl_seat::WlSeat);
        delegate_noop!(State: ignore Manager);
        impl Dispatch<Device, ()> for State {
            fn event(state: &mut Self, device: &Device, event: device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                let (index, offer) = match event {
                    device::Event::Selection { id } => (0, id),
                    device::Event::PrimarySelection { id } => { state.primary = device.version() >= $primary_version; (1, id) },
                    device::Event::Finished => { state.finished = true; device.destroy(); return },
                    _ => return,
                };
                if let Some(old) = state.offers[index].take() { old.destroy(); }
                state.offers[index] = offer;
            }
            event_created_child!(State, Device, [0 => (Offer, OfferData::default())]);
        }
        impl Dispatch<Offer, OfferData> for State {
            fn event(_: &mut Self, _: &Offer, event: offer::Event, mimes: &OfferData, _: &Connection, _: &QueueHandle<Self>) {
                if let offer::Event::Offer { mime_type } = event { mimes.lock().unwrap().push(mime_type); }
            }
        }
        impl Dispatch<Source, std::sync::Arc<crate::clipboard::Payload>> for State {
            fn event(state: &mut Self, source: &Source, event: source::Event, text: &Arc<crate::clipboard::Payload>, _: &Connection, _: &QueueHandle<Self>) {
                match event {
                    source::Event::Send { mime_type, fd }
                        if matches!(&**text, crate::clipboard::Payload::Text(_)) && MIMES.contains(&mime_type.as_str())
                            || matches!(&**text, crate::clipboard::Payload::Png(_)) && mime_type == "image/png" =>
                    { write_source(File::from(fd), text.clone(), &state.writers) }
                    source::Event::Cancelled => {
                        if let Some(slot) = state.sources.iter_mut().find(|s| s.as_ref() == Some(source)) { *slot = None; }
                        if source.is_alive() { source.destroy(); }
                    }
                    _ => {},
                }
            }
        }
    };
}

mod ext {
    use super::*;
    use device::ExtDataControlDeviceV1 as Device;
    use manager::ExtDataControlManagerV1 as Manager;
    use offer::ExtDataControlOfferV1 as Offer;
    use source::ExtDataControlSourceV1 as Source;
    use wayland_protocols::ext::data_control::v1::client::{
        ext_data_control_device_v1 as device, ext_data_control_manager_v1 as manager,
        ext_data_control_offer_v1 as offer, ext_data_control_source_v1 as source,
    };
    client!("ext_data_control_manager_v1", 1, 1);
}
mod wlr {
    use super::*;
    use device::ZwlrDataControlDeviceV1 as Device;
    use manager::ZwlrDataControlManagerV1 as Manager;
    use offer::ZwlrDataControlOfferV1 as Offer;
    use source::ZwlrDataControlSourceV1 as Source;
    use wayland_protocols_wlr::data_control::v1::client::{
        zwlr_data_control_device_v1 as device, zwlr_data_control_manager_v1 as manager,
        zwlr_data_control_offer_v1 as offer, zwlr_data_control_source_v1 as source,
    };
    client!("zwlr_data_control_manager_v1", 2, 2);
}

pub(super) enum Native {
    Ext(ext::Client),
    Wlr(wlr::Client),
}
impl Native {
    pub(super) fn new(reply: Reply) -> io::Result<Self> {
        ext::Client::new(reply.clone())
            .map(Self::Ext)
            .or_else(|_| wlr::Client::new(reply).map(Self::Wlr))
    }
    pub(super) fn dispatch(&mut self) -> io::Result<()> {
        match self {
            Self::Ext(c) => c.dispatch(),
            Self::Wlr(c) => c.dispatch(),
        }
    }
    pub(super) fn command(&mut self, cmd: Command) -> Result<(), Command> {
        match self {
            Self::Ext(c) => c.command(cmd),
            Self::Wlr(c) => c.command(cmd),
        }
    }
    pub(super) fn wait(&self, wake: &UnixStream) -> io::Result<()> {
        match self {
            Self::Ext(c) => c.wait(wake),
            Self::Wlr(c) => c.wait(wake),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mime_negotiation_prefers_utf8_and_rejects_binary() {
        assert_eq!(
            text_mime(&["text/plain".into(), "text/plain;charset=utf-8".into()]),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(text_mime(&["image/png".into()]), None);
    }
}
