//! A read-only MCP (Model Context Protocol) endpoint on a unix socket:
//! LLM tooling lists this session's windows and tabs and reads captured
//! command output without a shell.
//!
//! The framing is MCP's stdio framing - newline-delimited JSON-RPC 2.0 -
//! on a per-display socket, so an X11 client cannot read Wayland state
//! (the same scoping the session bus name gets). The server is read-only
//! by design: it answers, it never acts on the session. State crosses to
//! the event loop on a channel, because the app owns the data; this
//! thread only speaks the wire format.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// How long a query may wait for the event loop before the client gets an
/// answer that says so. The app is alive by construction (it hosts this
/// socket), so a timeout means a stuck loop, not a departed one.
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Server;

/// A state query the event loop must answer (it owns the data).
pub enum Query {
    ListWindows { respond: mpsc::Sender<serde_json::Value> },
    LastOutput { window: u32, n: u64, respond: mpsc::Sender<String> },
}

/// The per-display socket path, or None when there is no display to key
/// on. `MYPIXELS_MCP` names a private socket (a test, or a second parallel
/// session on one display) the display's real session never sees.
pub fn socket_path() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("MYPIXELS_MCP") {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").ok().filter(|s| !s.is_empty())?;
    let key = crate::session::display_key()?;
    Some(PathBuf::from(dir).join(format!("mypixels-{key}.mcp.sock")))
}

/// Host the socket on a dedicated thread. Returns None (and logs why) when
/// this process cannot or should not serve: no display, no runtime dir, or
/// another session already listens. Those are normal, not errors - an LLM
/// without a socket is the same as an LLM without a terminal.
pub fn start(
    tx: mpsc::Sender<Query>,
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> Option<Server> {
    let path = socket_path()?;
    std::thread::Builder::new()
        .name("mcp-server".into())
        .spawn(move || {
            let listener = match bind(&path) {
                Some(l) => l,
                None => return,
            };
            log::info!("mcp: listening on {}", path.display());
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let tx = tx.clone();
                        let wake = std::sync::Arc::clone(&wake);
                        std::thread::Builder::new()
                            .name("mcp-client".into())
                            .spawn(move || {
                                let _ = serve_conn(s, tx, wake);
                            })
                            .ok();
                    }
                    Err(e) => log::warn!("mcp: accept: {e}"),
                }
            }
        })
        .ok()?;
    Some(Server)
}

/// Bind, giving one chance to a stale file. A live listener answers a
/// connect; a leftover from a crashed session does not.
fn bind(path: &Path) -> Option<UnixListener> {
    if UnixStream::connect(path).is_ok() {
        log::debug!("mcp: {} already serves; no socket", path.display());
        return None;
    }
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).ok()?;
    // Command output is other people's business: owner read/write only.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Some(l)
}

fn serve_conn(
    mut stream: UnixStream,
    tx: mpsc::Sender<Query>,
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        let msg: serde_json::Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                // No id to answer with; the line is simply not ours.
                log::debug!("mcp: bad line: {e}");
                line.clear();
                continue;
            }
        };
        line.clear();
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or_default();
        if let Some(resp) = handle(method, msg.get("params"), id, &tx, wake.as_ref()) {
            let mut out = serde_json::to_string(&resp).unwrap_or_default();
            out.push('\n');
            stream.write_all(out.as_bytes())?;
        }
    }
    Ok(())
}

fn handle(
    method: &str,
    params: Option<&serde_json::Value>,
    id: Option<serde_json::Value>,
    tx: &mpsc::Sender<Query>,
    wake: &(dyn Fn() + Send + Sync),
) -> Option<serde_json::Value> {
    match method {
        "initialize" => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": params
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("2024-11-05")
                    .to_string(),
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "mypixels",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        })),
        // Handshake end; notifications carry no id and get no reply.
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => Some(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
        "tools/list" => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": [
                {
                    "name": "list_windows",
                    "description": "List this mypixels session's windows and their tabs: 1-based window and tab numbers, titles, working directories, and which tab is active.",
                    "inputSchema": { "type": "object", "properties": {} }
                },
                {
                    "name": "last_output",
                    "description": "Captured output of the last n commands of a window's active tab, from the session's OSC 133 pool. One block is bare pasteable text; several get '# N of M' headers. Empty means no session output was captured.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "window": { "type": "number", "description": "1-based window number; the default is the window the user is looking at." },
                            "n": { "type": "number", "description": "How many commands to fetch (default 1)." }
                        }
                    }
                }
            ] }
        })),
        "tools/call" => {
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let args = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match name {
                "list_windows" => tool_result(id, ask_list_windows(tx, wake)),
                "last_output" => {
                    let window = args.get("window").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let n = args.get("n").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
                    tool_result(id, ask_last_output(tx, wake, window, n))
                }
                other => err(id, -32602, format!("unknown tool {other}")),
            }
        }
        other => err(id, -32601, format!("unknown method {other}")),
    }
}

fn err(id: Option<serde_json::Value>, code: i64, message: String) -> Option<serde_json::Value> {
    id.map(|id| {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
    })
}

fn tool_result(id: Option<serde_json::Value>, text: String) -> Option<serde_json::Value> {
    id.map(|id| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [ { "type": "text", "text": text } ],
                "isError": text.is_empty()
            }
        })
    })
}

fn ask_list_windows(tx: &mpsc::Sender<Query>, wake: &(dyn Fn() + Send + Sync)) -> String {
    let (s, r) = mpsc::channel();
    if tx.send(Query::ListWindows { respond: s }).is_err() {
        return "session is gone".into();
    }
    wake();
    r.recv_timeout(QUERY_TIMEOUT)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "app did not answer in time".into())
}

fn ask_last_output(tx: &mpsc::Sender<Query>, wake: &(dyn Fn() + Send + Sync), window: u32, n: u64) -> String {
    let (s, r) = mpsc::channel();
    if tx.send(Query::LastOutput { window, n, respond: s }).is_err() {
        return "session is gone".into();
    }
    wake();
    r.recv_timeout(QUERY_TIMEOUT)
        .unwrap_or_else(|_| "app did not answer in time".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_method_and_tool_answer_errors() {
        let (tx, _rx) = mpsc::channel::<Query>();
        let no_wake = || {};
        let r = handle("bogus", None, Some(serde_json::json!(7)), &tx, &no_wake);
        assert_eq!(r.unwrap()["error"]["code"], -32601);
        // A notification to an unknown method is ignored, not failed.
        assert!(handle("bogus", None, None, &tx, &no_wake).is_none());
        let r = handle(
            "tools/call",
            Some(&serde_json::json!({ "name": "nope" })),
            Some(serde_json::json!(8)),
            &tx,
            &no_wake,
        );
        assert_eq!(r.unwrap()["error"]["code"], -32602);
    }

    #[test]
    fn the_handshake_answers_the_version_it_is_given() {
        let (tx, _rx) = mpsc::channel::<Query>();
        let no_wake = || {};
        let r = handle(
            "initialize",
            Some(&serde_json::json!({ "protocolVersion": "2025-06-18" })),
            Some(serde_json::json!(1)),
            &tx,
            &no_wake,
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "mypixels");
    }
}
