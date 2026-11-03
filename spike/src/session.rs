//! Single-instance ("daemon") plumbing over the session D-Bus, with the
//! semantics the GTK original got from GApplication: the first mypixels on a
//! display owns a bus name; later invocations forward themselves to it and
//! exit. See `--standalone` for opting out.
//!
//! One deliberate divergence from the original: a forwarded invocation opens a
//! *tab* in the running window, in the caller's working directory -- a sibling
//! session beside the shell that ran `mypixels`, which is what "open in the
//! current session" is for. (The original opened a new window there; that
//! lands properly when the port has real multi-window support.)

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};

/// Serves the bus name on a dedicated thread: zbus is async-only, so the
/// thread owns a `block_on` future, and requests cross over on a channel the
/// event loop drains.
pub struct Server;

/// A window's place in the session: 1 is the session proper (the bare
/// display name), 2.. run under numbered suffixes. Tabs hand their shells
/// this number in $MYPIXELS_WINDOW, so a `mypixels` typed in a shell joins
/// *its* window, not whatever happens to be the primary -- and a second
/// window can exist long before the single-window engine learns to hold two.
pub type SessionIndex = Arc<AtomicU32>;

pub enum Request {
    /// Open a tab running `argv` (empty = the default shell), starting in
    /// `cwd` when it is a usable directory, and present the window.
    OpenTab { cwd: Option<String>, argv: Vec<String> },
    /// Present the window without opening anything.
    Raise,
}

const IFACE: &str = "com.github.volca.mypixels.Session";
const PATH: &str = "/com/github/volca/mypixels/Session";

/// The well-known name for this display's session, or None when there is no
/// display to key on (no way to scope the session sanely). The name is
/// per-display exactly as the original's GApplication id is: an X11 instance
/// must never adopt a Wayland tab request, or vice versa.
pub fn bus_name() -> Option<String> {
    let disp = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("DISPLAY").ok().filter(|s| !s.is_empty()))?;
    bus_name_for(&disp)
}

/// Name of a window's bus slot. Window 1 owns the bare name -- the session
/// everyone finds when they have no better idea.
pub fn slot_name(base: &str, index: u32) -> String {
    if index <= 1 {
        base.to_string()
    } else {
        format!("{base}_{index}")
    }
}

/// Which window this process was launched *from*: inherited by every shell
/// from its tab's window. 0 marks a standalone window's children -- a tab
/// in a --standalone instance joins nothing, not even a foreign session.
/// Unset means "outside any terminal": join the session's first window.
pub fn launch_window() -> Option<u32> {
    std::env::var("MYPIXELS_WINDOW").ok()?.trim().parse::<u32>().ok()
}

fn bus_name_for(disp: &str) -> Option<String> {
    let mut safe: String = disp
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if !safe.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        safe.insert_str(0, "d_");
    }
    Some(format!("com.github.volca.mypixels.{safe}"))
}

/// Register as the session for this display. Returns None (and logs why) when
/// this process should simply run without a bus presence: another session
/// already holds the name, or there is no bus at all. Those are normal, not
/// errors -- mypixels with a broken or absent session bus is still a
/// terminal.
pub fn start(
    base: &str,
    tx: mpsc::Sender<Request>,
    wake: impl Fn() + Send + 'static,
) -> Option<(Server, SessionIndex)> {
    let base = base.to_string();
    let index: SessionIndex = Arc::new(AtomicU32::new(0));
    let thread_index = Arc::clone(&index);
    std::thread::Builder::new()
        .name("session-dbus".into())
        .spawn(move || {
            if let Err(e) = futures::executor::block_on(serve(&base, thread_index, tx, wake)) {
                log::warn!("session bus: {e}; running without single-instance");
            }
        })
        .ok()?;
    Some((Server, index))
}

async fn serve(
    base: &str,
    index: SessionIndex,
    tx: mpsc::Sender<Request>,
    wake: impl Fn(),
) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    // Take the lowest free window slot. Plain requests queue nothing on the
    // bus, so each try answers immediately: a taken slot is the normal lot of
    // every window after the first, not a failure. 64 windows deep is a
    // terminal session problem no bus policy will outvote.
    let mut name = String::new();
    for want in 1..=64u32 {
        name = slot_name(base, want);
        match conn.request_name(&*name).await {
            Ok(()) => {
                index.store(want, Ordering::SeqCst);
                log::info!("session registered as {name} (window {want})");
                break;
            }
            Err(e) => log::debug!("slot {name} unavailable ({e})"),
        }
    }
    if index.load(Ordering::SeqCst) == 0 {
        log::warn!("no free session slot for {base}; this window hosts nothing");
        return Ok(());
    }
    let mut stream = zbus::MessageStream::from(&conn);
    while let Some(msg) = futures::StreamExt::next(&mut stream).await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                log::warn!("session bus: {e}");
                continue;
            }
        };
        let header = msg.header();
        if header.message_type() != zbus::message::Type::MethodCall
            || header.interface().map(|i| i.as_str()) != Some(IFACE)
        {
            continue;
        }
        // Always ack: the only client is another mypixels, and a malformed
        // call from it is a version mismatch to log, not to fail its process
        // over. (A mismatched client degrades to "start locally", never to a
        // dropped command.)
        // cwd crosses the wire as a plain string; empty means "no preference"
        // (Option is a variant type, awkward inside a struct body).
        match header.member().map(|m| m.as_str()) {
            Some("OpenTab") => match msg.body().deserialize::<(String, Vec<String>, bool)>() {
                Ok((cwd, argv, _focus)) => {
                    let cwd = (!cwd.is_empty()).then_some(cwd);
                    let _ = tx.send(Request::OpenTab { cwd, argv });
                    wake();
                }
                Err(e) => log::warn!("session bus: bad OpenTab body: {e}"),
            },
            Some("Raise") => {
                let _ = tx.send(Request::Raise);
                wake();
            }
            other => log::warn!("session bus: unknown method {other:?}"),
        }
        let ack = zbus::Message::method_return(&header)?.build(&())?;
        conn.send(&ack).await?;
    }
    Ok(())
}

/// If a session is up on this display, hand it the request and return true
/// (the caller then exits, like a client). False means "nobody home" -- start
/// locally. Errors are reported and also answered with false: a bus that
/// throws is a reason to run, not a reason to lose the user's command.
pub fn remote_open(cwd: Option<String>, argv: Vec<String>) -> bool {
    let Some(base) = bus_name() else { return false };
    // A shell carries its window's number; outside any terminal, the session
    // proper (window 1) is the addressee. 0 means "inside a --standalone
    // window": that window joined no session, so neither do its tabs.
    match launch_window() {
        Some(0) => return false,
        Some(index) => return try_slot(&slot_name(&base, index), cwd, argv),
        None => {
            // Outside a terminal, or a shell whose window died mid-life: the
            // session proper answers if anybody home is there.
            return try_slot(&slot_name(&base, 1), cwd, argv);
        }
    }
}

fn try_slot(name: &str, cwd: Option<String>, argv: Vec<String>) -> bool {
    match futures::executor::block_on(remote_open_async(name, cwd, argv)) {
        Ok(sent) => sent,
        Err(e) => {
            log::warn!("session lookup failed ({e}); starting a new instance");
            false
        }
    }
}

async fn remote_open_async(
    name: &str,
    cwd: Option<String>,
    argv: Vec<String>,
) -> zbus::Result<bool> {
    let Ok(conn) = zbus::Connection::session().await else {
        return Ok(false); // no session bus at all: plain standalone life
    };
    // Ask the bus first whether anybody owns the name: "name absent" is the
    // normal case and deserves a quiet answer, not an error report.
    let owner = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &[name],
        )
        .await;
    match owner {
        // Nobody owns the name: an ordinary fresh start, not an incident.
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            return Ok(false);
        }
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    let cwd = cwd.unwrap_or_default();
    let reply = conn
        .call_method(Some(name), PATH, Some(IFACE), "OpenTab", &(cwd, argv, true))
        .await?;
    reply.body().deserialize::<()>()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_slots_number_from_the_session_up() {
        assert_eq!(slot_name("com.x.d_wayland_0", 1), "com.x.d_wayland_0");
        assert_eq!(slot_name("com.x.d_wayland_0", 2), "com.x.d_wayland_0_2");
    }

    #[test]
    fn display_scopes_the_bus_name() {
        assert_eq!(
            bus_name_for("wayland-1").unwrap(),
            "com.github.volca.mypixels.wayland_1"
        );
        assert_eq!(
            bus_name_for(":0").unwrap(),
            "com.github.volca.mypixels.d__0", // ':' becomes '_', start forced alphabetic
            "X11 display names must still form valid bus names"
        );
        // A degenerate display name still yields a syntactically valid name.
        assert_eq!(bus_name_for("").unwrap(), "com.github.volca.mypixels.d_");
    }
}
