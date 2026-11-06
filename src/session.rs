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

use std::sync::mpsc;

/// Serves the bus name on a dedicated thread: zbus is async-only, so the
/// thread owns a `block_on` future, and requests cross over on a channel the
/// event loop drains.
pub struct Server;

pub enum Request {
    /// Open a tab running `argv` (empty = the default shell), starting in
    /// `cwd` when it is a usable directory. `window` is the requesting
    /// shell's window number (0 = the caller knows no better, which means
    /// "the window the user is looking at").
    OpenTab { window: u32, cwd: Option<String>, argv: Vec<String> },
    /// Open another window of this session, same parameters otherwise --
    /// what `mypixels -n` asks for when a session is already up.
    NewWindow { cwd: Option<String>, argv: Vec<String> },
    /// Present the window without opening anything.
    Raise,
    /// Hand the last `n` commands' captured output of `window`'s active
    /// tab to the requester (`mypixels last`): reply on `respond` for
    /// stdout, and/or place it on the clipboard when `copy`. The pool
    /// lives in the app, so the answer comes back on this channel
    /// rather than in the D-Bus reply body directly; the bus thread
    /// waits on it (bounded by one event-loop pass, the app is alive by
    /// construction or the send above failed and nobody waits).
    LastOutput { window: u32, n: u64, copy: bool, respond: futures::channel::oneshot::Sender<String> },
}

const IFACE: &str = "com.github.volca.mypixels.Session";
const PATH: &str = "/com/github/volca/mypixels/Session";

/// The display this process lives on, sanitized to a file-safe token: the
/// per-display scoping key shared by the bus name and the MCP socket path,
/// so an X11 client can never reach a Wayland session (or vice versa).
pub fn display_key() -> Option<String> {
    let disp = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("DISPLAY").ok().filter(|s| !s.is_empty()))?;
    let safe: String = disp
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let safe = if safe
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        safe
    } else {
        format!("d_{safe}")
    };
    Some(safe)
}

/// The well-known name for this display's session, or None when there is no
/// display to key on (no way to scope the session sanely). The name is
/// per-display exactly as the original's GApplication id is: an X11 instance
/// must never adopt a Wayland tab request, or vice versa.
pub fn bus_name() -> Option<String> {
    // MYPIXELS_BUS names a private session: a test (or a second parallel
    // session on one display) hosts and queries a name the display's real
    // session never sees - and cannot disturb.
    if let Ok(v) = std::env::var("MYPIXELS_BUS") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    Some(format!("com.github.volca.mypixels.{}", display_key()?))
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
) -> Option<Server> {
    let base = base.to_string();
    std::thread::Builder::new()
        .name("session-dbus".into())
        .spawn(move || {
            if let Err(e) = futures::executor::block_on(serve(&base, tx, wake)) {
                log::warn!("session bus: {e}; running without single-instance");
            }
        })
        .ok()?;
    Some(Server)
}

async fn serve(
    base: &str,
    tx: mpsc::Sender<Request>,
    wake: impl Fn(),
) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    // One process, one session name, as many windows behind it as wanted.
    // A plain request queues nothing: if another session holds the name the
    // answer is an immediate error, and this process is simply not the
    // session -- the normal state of a --standalone or a racing second
    // launcher, not a failure.
    match conn.request_name(base).await {
        Ok(()) => log::info!("session registered as {base}"),
        Err(e) => {
            log::debug!("session {base} not acquired ({e}); another instance is up");
            return Ok(());
        }
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
            Some("OpenTab") => match msg.body().deserialize::<(u32, String, Vec<String>)>() {
                Ok((window, cwd, argv)) => {
                    let cwd = (!cwd.is_empty()).then_some(cwd);
                    let _ = tx.send(Request::OpenTab { window, cwd, argv });
                    wake();
                }
                Err(e) => log::warn!("session bus: bad OpenTab body: {e}"),
            },
            Some("NewWindow") => match msg.body().deserialize::<(String, Vec<String>)>() {
                Ok((cwd, argv)) => {
                    let cwd = (!cwd.is_empty()).then_some(cwd);
                    let _ = tx.send(Request::NewWindow { cwd, argv });
                    wake();
                }
                Err(e) => log::warn!("session bus: bad NewWindow body: {e}"),
            },
            Some("Raise") => {
                let _ = tx.send(Request::Raise);
                wake();
            }
            Some("LastOutput") => {
                match msg.body().deserialize::<(u32, u64, u8)>() {
                    Ok((window, n, copy)) => {
                        let (send, reply_rx) = futures::channel::oneshot::channel();
                        let _ = tx.send(Request::LastOutput { window, n, copy: copy != 0, respond: send });
                        wake();
                        // Both modes hold the reply until the app has
                        // acted: answering a --copy early would let the
                        // querying command finish, close its own command
                        // block, and then BE the newest output copied.
                        // A departed app answers empty, never hangs.
                        let text = reply_rx.await.unwrap_or_default();
                        let reply = zbus::Message::method_return(&header)?.build(&(text,))?;
                        conn.send(&reply).await?;
                        continue; // acknowledged on this path
                    }
                    Err(e) => log::warn!("session bus: bad LastOutput body: {e}"),
                }
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
/// Ask the session to open a tab (or a window). True = delivered, and the
/// client exits a winner; false = nobody answered, so become a session.
/// `new_window` picks the method; everything else -- routing, fallbacks --
/// is shared.
pub fn remote_open(cwd: Option<String>, argv: Vec<String>, new_window: bool) -> bool {
    let Some(base) = bus_name() else { return false };
    // A shell carries its window's number (0 marks --standalone: that window
    // joined no session, so neither do its tabs). Outside any terminal the
    // caller has no preference and says so with 0-window... which would also
    // be standalone; the difference is the environment being SET or ABSENT,
    // so None means "session default", Some(0) means "stay out".
    if launch_window() == Some(0) {
        return false;
    }
    let window = launch_window().unwrap_or(0);
    match futures::executor::block_on(remote_open_async(&base, window, new_window, cwd, argv)) {
        Ok(sent) => sent,
        Err(e) => {
            log::warn!("session lookup failed ({e}); starting a new instance");
            false
        }
    }
}

/// Ask the running session for its last `n` command outputs. None means
/// no session answered - the same "nothing to ask" every other command
/// treats as "start our own", except a query has no fallback: the
/// caller gets an error, not a window.
pub fn remote_last(n: u64, copy: bool) -> Option<String> {
    let base = bus_name()?;
    match futures::executor::block_on(remote_last_async(&base, n, copy)) {
        Ok(text) => text,
        Err(e) => {
            log::warn!("session query failed: {e}");
            None
        }
    }
}

async fn remote_last_async(name: &str, n: u64, copy: bool) -> zbus::Result<Option<String>> {
    let Ok(conn) = zbus::Connection::session().await else {
        return Ok(None);
    };
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
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    let window = launch_window().unwrap_or(0);
    let reply = conn
        .call_method(Some(name), PATH, Some(IFACE), "LastOutput", &(window, n, copy as u8))
        .await?;
    // Both modes get the text back (the copy already happened inside the
    // terminal, where the clipboard lives); the display mode prints it.
    let (text,) = reply.body().deserialize::<(String,)>()?;
    Ok(Some(text))
}

async fn remote_open_async(
    name: &str,
    window: u32,
    new_window: bool,
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
    let reply = if new_window {
        conn.call_method(Some(name), PATH, Some(IFACE), "NewWindow", &(cwd, argv)).await?
    } else {
        conn.call_method(Some(name), PATH, Some(IFACE), "OpenTab", &(window, cwd, argv)).await?
    };
    reply.body().deserialize::<()>()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

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
