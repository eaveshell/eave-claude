//! The bar's end of the hooks that the eave plugin for Claude Code installs (`claude-plugin/`).
//!
//! Claude Code runs each hook as `curl`, posting the hook's JSON over HTTP to a Unix socket in
//! `$XDG_RUNTIME_DIR`. Most hooks are news: the server answers `{}` at once and reports them. A
//! `PermissionRequest` waits for an answer: its connection stays open until the widget calls
//! [`answer`] or [`withdraw`], or until Claude hangs up. The terminal shows the same prompt
//! meanwhile, and whichever answers first wins.
//!
//! When the bar isn't running, curl fails and the hook prints nothing, which Claude reads as "no
//! opinion": the terminal works as if the plugin weren't there.
//!
//! The first watcher starts the server, and every watcher gets every event.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use futures_util::future::{Either, select};
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OnceCell, broadcast, oneshot};

/// The socket's name in `$XDG_RUNTIME_DIR`. `claude-plugin/hooks/hooks.json` uses it too.
const SOCKET: &str = "eave-claude.sock";
/// Larger than any hook payload: a Write's whole file comes in its `tool_input`.
const MAX_BODY: usize = 64 << 20;
const MAX_HEAD: usize = 64 << 10;

#[derive(Debug, Clone)]
pub enum Event {
    Listening,
    /// The server couldn't start. Sessions still show, but nothing can be answered.
    Failed(String),
    Hook(Box<Hook>),
    /// A permission prompt, waiting for [`answer`], by an id of its own.
    Asked(u64, Box<Hook>),
    /// That prompt no longer waits: it was answered or withdrawn, or Claude stopped waiting.
    Settled(u64),
}

/// What a hook says. Every event has the first few fields; the rest belong to some events only,
/// and stay empty for the others.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Hook {
    pub hook_event_name: String,
    pub session_id: String,
    pub cwd: String,
    /// Set inside a subagent.
    pub agent_id: Option<String>,
    /// The session's permission mode: `default`, `plan`, `acceptEdits`, `auto`…
    pub permission_mode: String,
    /// UserPromptSubmit and SessionStart, once Claude has named the session.
    pub session_title: Option<String>,
    /// Stop and StopFailure.
    pub last_assistant_message: Option<String>,
    /// Stop: this turn continues because a Stop hook asked it to.
    pub stop_hook_active: bool,
    /// StopFailure.
    pub error: Value,
    /// PermissionRequest.
    pub tool_name: String,
    pub tool_input: Value,
    /// PermissionRequest: the rules the terminal offers as "don't ask again".
    pub permission_suggestions: Value,
}

/// The reply to a PermissionRequest that makes `decision` (`{"behavior": "allow", ...}`) the
/// user's answer.
pub fn decision(decision: Value) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": decision,
        }
    })
    .to_string()
}

/// Answers prompt `id` with `body`, a [`decision`]. False if it no longer waits.
pub fn answer(id: u64, body: String) -> bool {
    take(id).is_some_and(|reply| reply.send(body).is_ok())
}

/// Stops waiting on prompt `id` without an opinion, leaving it to the terminal.
pub fn withdraw(id: u64) {
    // Dropping the sender sends `{}`.
    take(id);
}

/// Every event from now on. The first one says whether the server runs.
pub fn watch() -> impl Stream<Item = Event> {
    // Subscribed before the server starts, so nothing it reports is missed.
    let events = bus().subscribe();
    let status = stream::once(async {
        static SERVER: OnceCell<Result<(), String>> = OnceCell::const_new();
        match SERVER.get_or_init(start).await {
            Ok(()) => Event::Listening,
            Err(error) => Event::Failed(error.clone()),
        }
    });
    let events = stream::unfold(events, |mut events| async move {
        loop {
            match events.recv().await {
                Ok(event) => return Some((event, events)),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    eprintln!("eave-claude: fell behind and missed {missed} hook events");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    status.chain(events)
}

fn bus() -> &'static broadcast::Sender<Event> {
    static BUS: OnceLock<broadcast::Sender<Event>> = OnceLock::new();
    BUS.get_or_init(|| broadcast::channel(256).0)
}

type Pending = Vec<(u64, oneshot::Sender<String>)>;

/// The prompts waiting for an answer, and where to send it.
static PENDING: Mutex<Pending> = Mutex::new(Vec::new());

/// Makes prompt `id` wait, as if a hook had asked, and returns where its answer goes.
#[cfg(test)]
pub fn expect(id: u64) -> oneshot::Receiver<String> {
    let (reply, answer) = oneshot::channel();
    PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((id, reply));
    answer
}

fn take(id: u64) -> Option<oneshot::Sender<String>> {
    let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
    let index = pending.iter().position(|(waiting, _)| *waiting == id)?;
    Some(pending.swap_remove(index).1)
}

async fn start() -> Result<(), String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR isn't set")?;
    let path = PathBuf::from(dir).join(SOCKET);
    let listener = bind(&path)
        .await
        .map_err(|error| format!("can't listen on {}: {error}", path.display()))?;
    tokio::spawn(serve(listener));
    Ok(())
}

/// Listens on `path`, taking it over from a bar that's gone but not from one still running.
async fn bind(path: &Path) -> io::Result<UnixListener> {
    let listener = match UnixListener::bind(path) {
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another eave is answering Claude's hooks",
                ));
            }
            std::fs::remove_file(path)?;
            UnixListener::bind(path)?
        }
        bound => bound?,
    };
    // Only you may answer your prompts. The runtime dir is yours alone already; this is in case
    // it isn't.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn serve(listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async move {
                    if let Err(error) = handle(stream).await {
                        eprintln!("eave-claude: a hook failed: {error}");
                    }
                });
            }
            Err(error) => {
                eprintln!("eave-claude: no longer listening for hooks: {error}");
                return;
            }
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

async fn handle<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S) -> io::Result<()> {
    let body = read_request(&mut stream).await?;
    let hook: Hook = match serde_json::from_slice(&body) {
        Ok(hook) => hook,
        Err(error) => {
            eprintln!("eave-claude: a hook sent something unreadable: {error}");
            return respond(&mut stream, "{}").await;
        }
    };
    if hook.hook_event_name != "PermissionRequest" {
        let _ = bus().send(Event::Hook(Box::new(hook)));
        return respond(&mut stream, "{}").await;
    }

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (reply, answer) = oneshot::channel();
    PENDING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((id, reply));
    let _ = bus().send(Event::Asked(id, Box::new(hook)));

    // Claude sends nothing more, so a read only returns when it hangs up: the prompt was
    // answered in the terminal and Claude gave up on the hook, or the session ended.
    let mut probe = [0; 1];
    let answer = match select(answer, pin!(stream.read(&mut probe))).await {
        Either::Left((answer, _)) => answer.ok(),
        Either::Right(_) => None,
    };
    take(id);
    let _ = bus().send(Event::Settled(id));
    respond(&mut stream, answer.as_deref().unwrap_or("{}")).await
}

/// Reads one HTTP request and returns its body. Only what curl sends is understood: a body with
/// a `Content-Length`.
async fn read_request<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        if buffer.len() > MAX_HEAD {
            return Err(invalid("headers too long"));
        }
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = std::str::from_utf8(&buffer[..head_end]).map_err(|_| invalid("headers"))?;
    let mut length = None;
    let mut expects_continue = false;
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("expect") {
            expects_continue = value.eq_ignore_ascii_case("100-continue");
        }
    }
    let length = length.ok_or_else(|| invalid("no Content-Length"))?;
    if length > MAX_BODY {
        return Err(invalid("body too large"));
    }
    if expects_continue {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
    }
    let mut body = buffer.split_off(head_end);
    if body.len() < length {
        let start = body.len();
        body.resize(length, 0);
        stream.read_exact(&mut body[start..]).await?;
    }
    body.truncate(length);
    Ok(body)
}

async fn respond<S: AsyncWrite + Unpin>(stream: &mut S, body: &str) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap()
    }

    fn post(body: &str) -> String {
        format!(
            "POST /hook HTTP/1.1\r\nHost: eave\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    async fn response(client: &mut UnixStream) -> String {
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        response.split_once("\r\n\r\n").unwrap().1.to_string()
    }

    /// Waits for the event about `session` that `pick` accepts. Tests run in parallel on one bus.
    async fn next<T>(
        events: &mut broadcast::Receiver<Event>,
        pick: impl Fn(&Event) -> Option<T>,
    ) -> T {
        loop {
            if let Some(found) = pick(&events.recv().await.unwrap()) {
                return found;
            }
        }
    }

    #[test]
    fn news_is_reported_and_answered_with_nothing() {
        runtime().block_on(async {
            let mut events = bus().subscribe();
            let (mut client, server) = UnixStream::pair().unwrap();
            tokio::spawn(handle(server));
            let hook = r#"{"hook_event_name":"Stop","session_id":"news","cwd":"/x","last_assistant_message":"done","unknown":1}"#;
            client.write_all(post(hook).as_bytes()).await.unwrap();
            assert_eq!(response(&mut client).await, "{}");
            let stop = next(&mut events, |event| match event {
                Event::Hook(hook) if hook.session_id == "news" => Some(hook.clone()),
                _ => None,
            })
            .await;
            assert_eq!(stop.hook_event_name, "Stop");
            assert_eq!(stop.last_assistant_message.as_deref(), Some("done"));
        });
    }

    #[test]
    fn a_prompt_waits_for_its_answer() {
        runtime().block_on(async {
            let mut events = bus().subscribe();
            let (mut client, server) = UnixStream::pair().unwrap();
            tokio::spawn(handle(server));
            let hook = r#"{"hook_event_name":"PermissionRequest","session_id":"ask","cwd":"/x","tool_name":"Write","tool_input":{"file_path":"/x/a"}}"#;
            client.write_all(post(hook).as_bytes()).await.unwrap();
            let id = next(&mut events, |event| match event {
                Event::Asked(id, hook) if hook.session_id == "ask" => Some(*id),
                _ => None,
            })
            .await;
            let body = decision(json!({"behavior": "allow"}));
            assert!(answer(id, body.clone()));
            assert_eq!(response(&mut client).await, body);
            next(&mut events, |event| {
                matches!(event, Event::Settled(settled) if *settled == id).then_some(())
            })
            .await;
            assert!(!answer(id, body), "only once");
        });
    }

    #[test]
    fn a_withdrawn_prompt_gets_no_opinion() {
        runtime().block_on(async {
            let mut events = bus().subscribe();
            let (mut client, server) = UnixStream::pair().unwrap();
            tokio::spawn(handle(server));
            let hook = r#"{"hook_event_name":"PermissionRequest","session_id":"withdrawn","tool_name":"Bash"}"#;
            client.write_all(post(hook).as_bytes()).await.unwrap();
            let id = next(&mut events, |event| match event {
                Event::Asked(id, hook) if hook.session_id == "withdrawn" => Some(*id),
                _ => None,
            })
            .await;
            withdraw(id);
            assert_eq!(response(&mut client).await, "{}");
        });
    }

    #[test]
    fn claude_hanging_up_settles_the_prompt() {
        runtime().block_on(async {
            let mut events = bus().subscribe();
            let (mut client, server) = UnixStream::pair().unwrap();
            let served = tokio::spawn(handle(server));
            let hook =
                r#"{"hook_event_name":"PermissionRequest","session_id":"gone","tool_name":"Bash"}"#;
            client.write_all(post(hook).as_bytes()).await.unwrap();
            let id = next(&mut events, |event| match event {
                Event::Asked(id, hook) if hook.session_id == "gone" => Some(*id),
                _ => None,
            })
            .await;
            drop(client);
            next(&mut events, |event| {
                matches!(event, Event::Settled(settled) if *settled == id).then_some(())
            })
            .await;
            let _ = served.await;
            assert!(!answer(id, "{}".into()));
        });
    }

    #[test]
    fn bodies_arrive_in_pieces_and_after_continue() {
        runtime().block_on(async {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let body = "x".repeat(10_000);
            let head = format!(
                "POST /hook HTTP/1.1\r\nexpect: 100-continue\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            let reader = tokio::spawn(async move { read_request(&mut server).await.unwrap() });
            client.write_all(head.as_bytes()).await.unwrap();
            let mut continued = [0; 25];
            client.read_exact(&mut continued).await.unwrap();
            assert_eq!(&continued, b"HTTP/1.1 100 Continue\r\n\r\n");
            for piece in body.as_bytes().chunks(3000) {
                client.write_all(piece).await.unwrap();
            }
            assert_eq!(reader.await.unwrap(), body.as_bytes());
        });
    }

    #[test]
    fn requests_without_a_length_are_refused() {
        runtime().block_on(async {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            client
                .write_all(b"POST /hook HTTP/1.1\r\n\r\n{}")
                .await
                .unwrap();
            assert!(read_request(&mut server).await.is_err());
        });
    }

    #[test]
    fn a_running_server_keeps_its_socket() {
        runtime().block_on(async {
            let dir = std::env::temp_dir().join(format!("eave-claude-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("hooks.sock");
            let _ = std::fs::remove_file(&path);
            let first = bind(&path).await.unwrap();
            assert!(bind(&path).await.is_err(), "the first bar is still there");
            drop(first);
            assert!(bind(&path).await.is_ok(), "a stale socket is taken over");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
