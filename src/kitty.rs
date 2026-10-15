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
/// has the keyboard. `None` when kitty doesn't, or won't say.
pub async fn focused(socket: &Path) -> io::Result<Option<u64>> {
    let mut stream = UnixStream::connect(socket).await?;
    stream.write_all(&request()).await?;
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
    Ok(answer(&reply))
}

fn request() -> Vec<u8> {
    // The oldest version that has it: kitty refuses commands from newer ones than itself.
    let command = serde_json::json!({
        "cmd": "ls",
        "version": [0, 14, 2],
        "payload": {"match": "state:focused"},
    });
    [START, command.to_string().as_bytes(), END].concat()
}

/// The focused pane in a reply to [`request`].
fn answer(reply: &[u8]) -> Option<u64> {
    #[derive(Deserialize)]
    struct Reply {
        ok: bool,
        #[serde(default)]
        data: String,
    }
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
    let json = reply.strip_prefix(START)?.strip_suffix(END)?;
    let reply: Reply = serde_json::from_slice(json).ok()?;
    if !reply.ok {
        return None;
    }
    let windows: Vec<OsWindow> = serde_json::from_str(&reply.data).ok()?;
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

    fn reply(ok: bool, data: serde_json::Value) -> Vec<u8> {
        let body = serde_json::json!({"ok": ok, "data": data.to_string()});
        [START, body.to_string().as_bytes(), END].concat()
    }

    #[test]
    fn the_focused_pane_is_in_the_reply() {
        let windows = |focused| {
            serde_json::json!([{"id": 1, "tabs": [{"windows": [
                {"id": 7, "is_focused": focused, "title": "claude"},
            ]}]}])
        };
        assert_eq!(answer(&reply(true, windows(true))), Some(7));
        assert_eq!(
            answer(&reply(true, windows(false))),
            None,
            "kitty isn't focused"
        );
        let refused = [START, br#"{"ok":false,"error":"No matching windows"}"#, END].concat();
        assert_eq!(answer(&refused), None);
        assert_eq!(answer(b"garbage"), None);
    }

    #[test]
    fn the_request_is_framed() {
        let request = request();
        assert!(request.starts_with(START) && request.ends_with(END));
    }
}
