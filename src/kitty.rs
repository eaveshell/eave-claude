//! Which kitty pane a session runs in, and whether it's the one in front. kitty tells every
//! pane's programs which pane they're in (`KITTY_WINDOW_ID`), and where it takes remote control
//! (`KITTY_LISTEN_ON`), so a session's environment says both. Asking is kitty's remote control
//! protocol, spoken here: an `ls` of the focused pane. It needs, in `kitty.conf`:
//!
//! ```text
//! allow_remote_control socket-only
//! listen_on unix:@kitty-{kitty_pid}
//! ```

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const START: &[u8] = b"\x1bP@kitty-cmd";
const END: &[u8] = b"\x1b\\";
/// What kitty's replies can't grow past: one pane's `ls`, environment and all.
const MOST: usize = 1 << 20;

/// A pane of kitty's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// The kitty process: the window Hyprland knows is its.
    pub kitty: u32,
    /// The pane, as that kitty numbers them.
    pub window: u64,
    /// Where it takes remote control, if it does.
    pub socket: Option<PathBuf>,
}

/// The pane process `pid` runs in, if it's kitty's.
pub fn pane(pid: u32) -> Option<Pane> {
    parse(&std::fs::read(format!("/proc/{pid}/environ")).ok()?)
}

/// From an environment, as `/proc/<pid>/environ` has it.
fn parse(environ: &[u8]) -> Option<Pane> {
    let (mut kitty, mut window, mut socket) = (None, None, None);
    for variable in environ.split(|&byte| byte == 0) {
        let Ok(variable) = std::str::from_utf8(variable) else {
            continue;
        };
        match variable.split_once('=') {
            Some(("KITTY_PID", value)) => kitty = value.parse().ok(),
            Some(("KITTY_WINDOW_ID", value)) => window = value.parse().ok(),
            // `fd:` is a pane's own, launched with remote control: not ours to use.
            Some(("KITTY_LISTEN_ON", value)) => {
                socket = value
                    .strip_prefix("unix:")
                    .map(|path| match path.strip_prefix('@') {
                        // Abstract, as tokio takes it.
                        Some(name) => PathBuf::from(format!("\0{name}")),
                        None => PathBuf::from(path),
                    })
            }
            _ => {}
        }
    }
    Some(Pane {
        kitty: kitty?,
        window: window?,
        socket,
    })
}

/// The pane in front of the kitty on `socket`: the active one of the active tab, while kitty
/// has the keyboard. `None` when kitty doesn't.
pub async fn focused(socket: &Path) -> io::Result<Option<u64>> {
    let data = command(socket, "ls", json!({"match": "state:focused"})).await?;
    Ok(focused_in(&data))
}

/// Brings pane `window` of the kitty on `socket` in front: its tab, its split, its OS window.
pub async fn focus(socket: &Path, window: u64) -> io::Result<()> {
    command(
        socket,
        "focus-window",
        json!({"match": format!("id:{window}")}),
    )
    .await
    .map(drop)
}

/// Sends `cmd` and gives back what kitty answered with, or why it refused.
async fn command(socket: &Path, cmd: &str, payload: Value) -> io::Result<String> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(&request(cmd, payload)).await?;
    let mut reply = Vec::new();
    let mut chunk = [0; 8192];
    while !reply.ends_with(END) {
        let read = stream.read(&mut chunk).await?;
        if read == 0 || reply.len() > MOST {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "kitty didn't finish",
            ));
        }
        reply.extend_from_slice(&chunk[..read]);
    }
    unframe(&reply)
}

fn request(cmd: &str, payload: Value) -> Vec<u8> {
    // The oldest version that has what we use: kitty refuses commands from newer ones than itself.
    let command = json!({"cmd": cmd, "version": [0, 14, 2], "payload": payload});
    [START, command.to_string().as_bytes(), END].concat()
}

/// A reply's data, or kitty's error: a password it wants, a pane that's gone.
fn unframe(reply: &[u8]) -> io::Result<String> {
    #[derive(Deserialize)]
    struct Reply {
        ok: bool,
        #[serde(default)]
        data: Option<String>,
        #[serde(default)]
        error: String,
    }
    let invalid = |error| io::Error::new(io::ErrorKind::InvalidData, error);
    let json = reply
        .strip_prefix(START)
        .and_then(|reply| reply.strip_suffix(END))
        .ok_or_else(|| invalid("not a kitty reply".to_string()))?;
    let reply: Reply = serde_json::from_slice(json).map_err(|error| invalid(error.to_string()))?;
    match reply.ok {
        true => Ok(reply.data.unwrap_or_default()),
        false => Err(io::Error::other(format!("kitty: {}", reply.error))),
    }
}

/// The focused pane in what `ls` answers.
fn focused_in(data: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct OsWindow {
        tabs: Vec<Tab>,
    }
    #[derive(Deserialize)]
    struct Tab {
        windows: Vec<Window>,
    }
    #[derive(Deserialize)]
    struct Window {
        id: u64,
        is_focused: bool,
    }
    let windows: Vec<OsWindow> = serde_json::from_str(data).ok()?;
    windows
        .iter()
        .flat_map(|os| &os.tabs)
        .flat_map(|tab| &tab.windows)
        .find(|window| window.is_focused)
        .map(|window| window.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_is_in_the_environment() {
        let environ = b"HOME=/home/f\0KITTY_PID=2255\0KITTY_WINDOW_ID=3\0\
            KITTY_LISTEN_ON=unix:@kitty-2255\0";
        let pane = parse(environ).unwrap();
        assert_eq!((pane.kitty, pane.window), (2255, 3));
        assert_eq!(pane.socket, Some(PathBuf::from("\0kitty-2255")));
        let path = parse(b"KITTY_PID=1\0KITTY_WINDOW_ID=2\0KITTY_LISTEN_ON=unix:/tmp/kitty\0");
        assert_eq!(path.unwrap().socket, Some(PathBuf::from("/tmp/kitty")));
        let fd = parse(b"KITTY_PID=1\0KITTY_WINDOW_ID=2\0KITTY_LISTEN_ON=fd:5\0");
        assert_eq!(fd.unwrap().socket, None);
        assert_eq!(parse(b"KITTY_PID=1\0HOME=/home/f\0"), None, "not in a pane");
        assert_eq!(parse(b"TERM=foot\0"), None);
    }

    fn reply(body: Value) -> Vec<u8> {
        [START, body.to_string().as_bytes(), END].concat()
    }

    #[test]
    fn the_focused_pane_is_in_the_reply() {
        let windows = |focused| {
            json!([{"id": 1, "tabs": [{"windows": [
                {"id": 7, "is_focused": focused, "title": "claude"},
            ]}]}])
            .to_string()
        };
        let data = unframe(&reply(json!({"ok": true, "data": windows(true)}))).unwrap();
        assert_eq!(focused_in(&data), Some(7));
        assert_eq!(focused_in(&windows(false)), None, "kitty isn't focused");
        // A password, say: kitty says no, and it isn't taken for "none in front".
        let refused = reply(json!({"ok": false, "error": "Password required"}));
        assert!(unframe(&refused).is_err());
        assert!(unframe(b"garbage").is_err());
        // Focusing answers with no data.
        assert_eq!(unframe(&reply(json!({"ok": true}))).unwrap(), "");
    }

    #[test]
    fn requests_are_framed() {
        let request = request("focus-window", json!({"match": "id:7"}));
        let json = request
            .strip_prefix(START)
            .and_then(|r| r.strip_suffix(END))
            .unwrap();
        let command: Value = serde_json::from_slice(json).unwrap();
        assert_eq!(command["cmd"], "focus-window");
        assert_eq!(command["payload"]["match"], "id:7");
    }
}
