//! The sessions running now, from `~/.claude/sessions/`. Claude Code keeps one `<pid>.json`
//! there per process, interactive or headless, rewrites it whenever the session's status changes
//! and deletes it on exit. The directory is watched with inotify, and every change sends the
//! whole list again: there are only ever a few.
//!
//! The format is Claude Code's own and undocumented, so everything in it is optional, and a file
//! that can't be read is skipped.

use std::ffi::OsStr;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};

use eave_api::Subscription;
use futures_util::stream;
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::title;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Working on a turn.
    Busy,
    Idle,
    /// Idle, with a shell it started still running.
    Shell,
    /// Showing a dialog in the terminal: what it waits for, like "permission prompt".
    Waiting(String),
}

/// One running session.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub pid: u32,
    pub id: String,
    pub cwd: String,
    /// Claude's short name for it, like `eave-01`.
    pub name: Option<String>,
    /// What the conversation is about, from its transcript.
    pub title: Option<String>,
    pub status: Status,
    /// Where it takes messages from other sessions, if it speaks a protocol we know.
    pub socket: Option<PathBuf>,
    /// Milliseconds since the epoch.
    pub started: u64,
}

/// `~/.claude`, or wherever `CLAUDE_CONFIG_DIR` puts it.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(dir.into());
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
}

/// The list now, then again after every change.
pub fn watch() -> Subscription<Vec<Live>> {
    Subscription::run(|| {
        let (sender, receiver) = mpsc::unbounded_channel();
        if let Some(config) = config_dir() {
            // inotify reads block, so they get their own thread instead of the UI's executor.
            let spawned = std::thread::Builder::new()
                .name("eave-claude".into())
                .spawn(move || {
                    if let Err(error) = run(&config, sender) {
                        eprintln!("eave-claude: not watching Claude's sessions: {error}");
                    }
                });
            if let Err(error) = spawned {
                eprintln!("eave-claude: not watching Claude's sessions: {error}");
            }
        }
        stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|sessions| (sessions, receiver))
        })
    })
}

/// Writes and renames into the directory, deletions, and the directory going away.
const DIR_EVENTS: WatchFlags = WatchFlags::CLOSE_WRITE
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::DELETE)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::MOVE_SELF)
    .union(WatchFlags::ONLYDIR);
const PARENT_EVENTS: WatchFlags = WatchFlags::CREATE
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::ONLYDIR);

/// Blocks, sending the list whenever it may have changed. Returns once nobody listens.
fn run(config: &Path, changed: UnboundedSender<Vec<Live>>) -> rustix::io::Result<()> {
    let dir = config.join("sessions");
    let fd = inotify::init(CreateFlags::CLOEXEC)?;
    let mut watching = arm(&fd, &dir, config)?;
    let mut buffer = [MaybeUninit::uninit(); 4096];
    let mut reader = inotify::Reader::new(&fd, &mut buffer);
    let mut last = scan(config);
    if changed.send(last.clone()).is_err() {
        return Ok(());
    }
    loop {
        let (wd, flags, relevant) = {
            let event = reader.next()?;
            let name = event.file_name().map(|name| name.to_bytes());
            let relevant = name.is_some_and(|name| name.ends_with(b".json") || name == b"sessions");
            (event.wd(), event.events(), relevant)
        };
        let rescan = match watching {
            _ if flags.contains(ReadFlags::QUEUE_OVERFLOW) => true,
            Watching::Dir(current) if wd == current => {
                if flags.intersects(ReadFlags::MOVE_SELF | ReadFlags::IGNORED) {
                    if flags.contains(ReadFlags::MOVE_SELF) {
                        let _ = inotify::remove_watch(&fd, current);
                    }
                    watching = arm(&fd, &dir, config)?;
                    true
                } else {
                    relevant
                }
            }
            Watching::Parent(current) if wd == current && relevant => {
                watching = arm(&fd, &dir, config)?;
                if matches!(watching, Watching::Dir(_)) {
                    let _ = inotify::remove_watch(&fd, current);
                }
                true
            }
            _ => false,
        };
        if !rescan {
            continue;
        }
        let now = scan(config);
        if now == last {
            continue;
        }
        if changed.send(now.clone()).is_err() {
            return Ok(());
        }
        last = now;
    }
}

#[derive(Clone, Copy)]
enum Watching {
    Dir(i32),
    /// `~/.claude`, until `sessions` appears in it.
    Parent(i32),
}

fn arm(fd: &impl rustix::fd::AsFd, dir: &Path, parent: &Path) -> rustix::io::Result<Watching> {
    match inotify::add_watch(fd, dir, DIR_EVENTS) {
        Ok(wd) => Ok(Watching::Dir(wd)),
        Err(Errno::NOENT | Errno::NOTDIR) => {
            inotify::add_watch(fd, parent, PARENT_EVENTS).map(Watching::Parent)
        }
        Err(error) => Err(error),
    }
}

/// The sessions whose process is still running, newest first.
fn scan(config: &Path) -> Vec<Live> {
    let Ok(entries) = std::fs::read_dir(config.join("sessions")) else {
        return Vec::new();
    };
    let mut sessions: Vec<Live> = entries
        .flatten()
        .filter(|entry| entry.path().extension() == Some(OsStr::new("json")))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|json| parse(&json))
        .filter(|file| running(file.pid, file.proc_start.as_deref()))
        .map(|file| {
            let transcript = transcript(config, &file.cwd, &file.session_id);
            let title = title::last(&transcript);
            file.into_live(title)
        })
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.started));
    sessions
}

/// A session file, as Claude Code 2.1 writes it.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct File {
    pid: u32,
    session_id: String,
    cwd: String,
    name: Option<String>,
    status: Option<String>,
    waiting_for: Option<String>,
    messaging_socket_path: Option<PathBuf>,
    peer_protocol: Option<u32>,
    /// The process's start time, as `/proc/<pid>/stat` has it: tells a reused pid apart.
    proc_start: Option<String>,
    started_at: u64,
}

fn parse(json: &str) -> Option<File> {
    let file: File = serde_json::from_str(json).ok()?;
    (file.pid != 0 && !file.session_id.is_empty()).then_some(file)
}

impl File {
    fn into_live(self, title: Option<String>) -> Live {
        let status = match self.status.as_deref() {
            Some("busy") => Status::Busy,
            Some("shell") => Status::Shell,
            Some("waiting") => Status::Waiting(self.waiting_for.unwrap_or_default()),
            _ => Status::Idle,
        };
        // Version 1 is the one whose messages this plugin knows how to write.
        let socket = self
            .messaging_socket_path
            .filter(|_| self.peer_protocol.unwrap_or(0) >= 1);
        Live {
            pid: self.pid,
            id: self.session_id,
            cwd: self.cwd,
            name: self.name,
            title,
            status,
            socket,
            started: self.started_at,
        }
    }
}

/// Whether `pid` still runs, and is the same process that wrote the file. Claude deletes the
/// file when it exits, but not when it's killed.
fn running(pid: u32, started: Option<&str>) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => started.is_none_or(|started| stat_field(&stat, 22) == Some(started)),
        Err(_) => false,
    }
}

/// Field `n` (from 1, as `man proc_pid_stat` counts) of a `/proc/<pid>/stat` line. The second
/// field, the command, is in parentheses and may hold spaces, so counting starts after it.
fn stat_field(stat: &str, n: usize) -> Option<&str> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(n.checked_sub(3)?)
}

/// The processes `pid` runs under, starting with itself: its shell, its terminal, and so on up.
pub fn ancestors(pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    while let Some(parent) =
        std::fs::read_to_string(format!("/proc/{}/stat", chain[chain.len() - 1]))
            .ok()
            .and_then(|stat| stat_field(&stat, 4)?.parse::<u32>().ok())
            .filter(|parent| *parent > 1 && chain.len() < 64)
    {
        chain.push(parent);
    }
    chain
}

/// Where Claude Code keeps a session's transcript: `projects/`, then the working directory with
/// everything but letters and digits made a `-`.
pub fn transcript(config: &Path, cwd: &str, id: &str) -> PathBuf {
    let project: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    config
        .join("projects")
        .join(project)
        .join(format!("{id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_file_reads() {
        let json = r#"{"pid":673122,"sessionId":"7784","cwd":"/home/f/eave","startedAt":1791860180114,
            "procStart":"3391076","version":"2.1.293","peerProtocol":1,"kind":"interactive",
            "messagingSocketPath":"/run/user/1001/cc-socks/673122.sock","name":"eave-01",
            "status":"waiting","waitingFor":"permission prompt","somethingNew":true}"#;
        let live = parse(json).unwrap().into_live(Some("Title".into()));
        assert_eq!(live.pid, 673122);
        assert_eq!(live.name.as_deref(), Some("eave-01"));
        assert_eq!(live.status, Status::Waiting("permission prompt".into()));
        assert_eq!(
            live.socket.as_deref(),
            Some(Path::new("/run/user/1001/cc-socks/673122.sock"))
        );
    }

    #[test]
    fn unknown_protocols_and_statuses_are_safe() {
        let json = r#"{"pid":1,"sessionId":"a","status":"pondering","messagingSocketPath":"/s"}"#;
        let live = parse(json).unwrap().into_live(None);
        assert_eq!(live.status, Status::Idle);
        assert_eq!(
            live.socket, None,
            "no peerProtocol: we don't know how to talk to it"
        );
        assert!(parse("{}").is_none());
        assert!(parse("not json").is_none());
    }

    #[test]
    fn stat_fields_count_past_the_command() {
        let stat = "325852 (claude (a) b) S 317693 325852 1 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 12 0 3391076 99";
        assert_eq!(stat_field(stat, 3), Some("S"));
        assert_eq!(stat_field(stat, 4), Some("317693"));
        assert_eq!(stat_field(stat, 22), Some("3391076"));
    }

    #[test]
    fn this_process_is_running_and_has_ancestors() {
        let pid = std::process::id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        assert!(running(pid, stat_field(&stat, 22)));
        assert!(
            !running(pid, Some("1")),
            "a different process with the same pid"
        );
        assert_eq!(ancestors(pid)[0], pid);
    }

    #[test]
    fn transcripts_live_under_the_project() {
        let path = transcript(Path::new("/c"), "/home/f/code/my.repo", "id");
        assert_eq!(path, Path::new("/c/projects/-home-f-code-my-repo/id.jsonl"));
    }
}
