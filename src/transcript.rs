//! A session's conversation, from its transcript: `projects/<project>/<session>.jsonl`, which
//! Claude Code appends a line to for every message, tool call and tool result as they happen.
//!
//! Read only while the session's chat shows. Its last stretch is read once, then the project's
//! directory is watched with inotify and each change to the file is read from where the last
//! read stopped. Every change sends the whole conversation again: it's kept short.
//!
//! The format is Claude Code's own and undocumented, so everything in it is optional, and a line
//! that can't be read is skipped.

use std::io::{Read, Seek, SeekFrom};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use eave_api::ui::Change;
use futures_util::{Stream, stream};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::inotify::{self, CreateFlags, WatchFlags};
use serde_json::Value;
use tokio::sync::mpsc::{self, UnboundedSender};

/// How much of a long transcript is read when the chat opens: its last stretch.
const TAIL: u64 = 512 << 10;
/// The entries kept: the latest ones.
const KEEP: usize = 200;
/// The longest text kept of one message.
const LONGEST: usize = 6000;
/// The most lines of a change kept, and of a new file's start.
const DIFF_LINES: usize = 60;
const NEW_FILE_LINES: usize = 40;
/// The longest a tool's result is said in.
const RESULT: usize = 120;

/// One thing in a conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// What you said, and when, if the transcript says.
    You {
        text: String,
        at: Option<DateTime<Local>>,
    },
    /// What Claude said: Markdown.
    Claude(String),
    /// A tool Claude used: what for, and whether it's done yet, or failed. Once done, what came
    /// of it in a line (a count, the error), and the change it made to a file, if it did.
    Tool {
        id: String,
        name: String,
        what: String,
        done: bool,
        failed: bool,
        result: String,
        diff: Option<Vec<(Change, String)>>,
    },
    /// Something that happened, said in passing: an interruption, a command.
    Aside(String),
}

impl Entry {
    /// What you said, at a time not known.
    pub fn you(text: impl Into<String>) -> Self {
        Entry::You {
            text: text.into(),
            at: None,
        }
    }
}

/// The conversation so far, newest last.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chat {
    pub entries: Vec<Entry>,
    /// The permission mode it was last in: `default`, `plan`, `acceptEdits`, `auto`…
    pub mode: Option<String>,
}

impl Chat {
    /// Adds what one line of the transcript says.
    pub fn read_line(&mut self, line: &str, cwd: &str) {
        let Ok(line) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if line["isSidechain"].as_bool() == Some(true) {
            return;
        }
        match line["type"].as_str() {
            Some("user") => self.user(&line),
            Some("assistant") => self.assistant(&line, cwd),
            Some("permission-mode") => {
                if let Some(mode) = line["permissionMode"].as_str() {
                    self.mode = Some(mode.to_string());
                }
            }
            Some("system") if line["subtype"] == "compact_boundary" => {
                self.push(Entry::Aside("The conversation was compacted".into()));
            }
            _ => {}
        }
        if self.entries.len() > KEEP {
            self.entries.drain(..self.entries.len() - KEEP);
        }
    }

    fn user(&mut self, line: &Value) {
        let meta = line["isMeta"].as_bool() == Some(true);
        let at = line["timestamp"]
            .as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Local));
        let said = |text: &str| {
            said(text).map(|entry| match entry {
                Entry::You { text, .. } => Entry::You { text, at },
                entry => entry,
            })
        };
        match &line["message"]["content"] {
            Value::String(text) if meta => {
                // Messages typed in the bar arrive as from another session.
                if let Some(text) = from_bar(text) {
                    self.push(Entry::You { text, at });
                }
            }
            Value::String(text) => {
                if let Some(entry) = said(text) {
                    self.push(entry);
                }
            }
            Value::Array(items) => {
                // What the tool returned, in full, is beside the result when there's one.
                let results = items
                    .iter()
                    .filter(|item| item["type"] == "tool_result")
                    .count();
                let returned = (results == 1).then(|| &line["toolUseResult"]);
                for item in items {
                    match item["type"].as_str() {
                        Some("tool_result") => {
                            let id = item["tool_use_id"].as_str().unwrap_or_default();
                            let failed = item["is_error"].as_bool() == Some(true);
                            self.finish(id, failed, &content(&item["content"]), returned);
                        }
                        Some("text") if !meta => {
                            if let Some(entry) = item["text"].as_str().and_then(said) {
                                self.push(entry);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn assistant(&mut self, line: &Value, cwd: &str) {
        let Value::Array(items) = &line["message"]["content"] else {
            return;
        };
        for item in items {
            match item["type"].as_str() {
                Some("text") => {
                    let text = item["text"].as_str().unwrap_or_default().trim();
                    if !text.is_empty() {
                        self.push(Entry::Claude(crate::shorten(text, LONGEST)));
                    }
                }
                Some("tool_use") => {
                    let name = item["name"].as_str().unwrap_or_default();
                    self.push(Entry::Tool {
                        id: item["id"].as_str().unwrap_or_default().to_string(),
                        name: crate::tool_name(name),
                        what: crate::describe(name, &item["input"], cwd),
                        done: false,
                        failed: false,
                        result: String::new(),
                        diff: None,
                    });
                }
                _ => {}
            }
        }
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    /// The tool call `id` is done: it said `content`, and returned `returned`, if known.
    fn finish(&mut self, id: &str, failed: bool, content: &str, returned: Option<&Value>) {
        let call = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry, Entry::Tool { id: call, .. } if call == id));
        let Some(Entry::Tool {
            name,
            done,
            failed: flag,
            result,
            diff,
            ..
        }) = call
        else {
            return;
        };
        *done = true;
        *flag = failed;
        let returned = returned.unwrap_or(&Value::Null);
        *result = if failed {
            first_line(
                &content
                    .replace("<tool_use_error>", "")
                    .replace("</tool_use_error>", ""),
            )
        } else {
            summary(name, content, returned)
        };
        if !failed {
            *diff = change(returned);
        }
    }

    /// The tool it's running now, if any: the last one not done yet.
    pub fn running(&self) -> Option<(&str, &str)> {
        match self.entries.last()? {
            Entry::Tool {
                name,
                what,
                done: false,
                ..
            } => Some((name, what)),
            _ => None,
        }
    }
}

/// A tool result's text: a string, or a list of blocks of it.
fn content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty());
    crate::shorten(line.unwrap_or_default(), RESULT)
}

/// What came of a tool, in a few words: lines read, a command's last line, files found. Tools
/// whose result is long prose (an agent's report, a page) or a change (shown as one) say nothing.
fn summary(name: &str, content: &str, returned: &Value) -> String {
    match name {
        "Read" => match returned["file"]["numLines"].as_u64() {
            Some(1) => "1 line".into(),
            Some(lines) => format!("{lines} lines"),
            None => String::new(),
        },
        "Bash" => {
            let out = returned["stdout"].as_str().unwrap_or(content);
            let last = out.lines().map(str::trim).rfind(|line| !line.is_empty());
            crate::shorten(last.unwrap_or_default(), RESULT)
        }
        "Grep" | "Glob" => first_line(content),
        _ => String::new(),
    }
}

/// The change an Edit, MultiEdit or Write made, from the patch Claude Code keeps of it, or a new
/// file's first lines.
fn change(returned: &Value) -> Option<Vec<(Change, String)>> {
    let mut lines = Vec::new();
    if let Some(hunks) = returned["structuredPatch"]
        .as_array()
        .filter(|h| !h.is_empty())
    {
        for hunk in hunks {
            let number = |key: &str| hunk[key].as_u64().unwrap_or(0);
            lines.push((
                Change::Hunk,
                format!(
                    "@@ -{},{} +{},{} @@",
                    number("oldStart"),
                    number("oldLines"),
                    number("newStart"),
                    number("newLines")
                ),
            ));
            for line in hunk["lines"].as_array().into_iter().flatten() {
                let line = line.as_str().unwrap_or_default();
                let (kind, text) = match line.chars().next() {
                    Some('+') => (Change::Added, &line[1..]),
                    Some('-') => (Change::Removed, &line[1..]),
                    Some(' ') => (Change::Context, &line[1..]),
                    // "\ No newline at end of file"
                    _ => continue,
                };
                lines.push((kind, text.to_string()));
            }
        }
        return Some(cut(lines, DIFF_LINES));
    }
    if returned["type"] == "create" {
        let content = returned["content"].as_str()?;
        lines.extend(
            content
                .lines()
                .map(|line| (Change::Added, line.to_string())),
        );
        return Some(cut(lines, NEW_FILE_LINES));
    }
    None
}

/// The first `most` lines, then how many more there were.
fn cut(mut lines: Vec<(Change, String)>, most: usize) -> Vec<(Change, String)> {
    if lines.len() > most {
        let more = lines.len() - most;
        lines.truncate(most);
        let unit = if more == 1 { "line" } else { "lines" };
        lines.push((Change::Hunk, format!("… {more} more {unit}")));
    }
    lines
}

/// What a line you sent shows as, if anything: your words, a command you ran, or an
/// interruption. Claude Code's own notes to Claude, like a task finishing, aren't shown.
fn said(text: &str) -> Option<Entry> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with("[Request interrupted") {
        return Some(Entry::Aside("Interrupted".into()));
    }
    if let Some(command) = between(text, "<command-name>", "</command-name>") {
        let args = between(text, "<command-args>", "</command-args>").unwrap_or_default();
        let command = format!("{command} {args}").trim().to_string();
        return Some(Entry::you(command));
    }
    if let Some(input) = between(text, "<bash-input>", "</bash-input>") {
        return Some(Entry::you(format!("! {input}")));
    }
    let hidden = [
        "<task-notification>",
        "<local-command-",
        "<bash-stdout>",
        "<bash-stderr>",
        "<system-reminder>",
        "<command-message>",
    ];
    if hidden.iter().any(|tag| text.starts_with(tag)) {
        return None;
    }
    if let Some(text) = from_bar(text) {
        return Some(Entry::you(text));
    }
    Some(Entry::you(crate::shorten(text, LONGEST)))
}

/// What you typed in the bar, from the message [`peer`](crate::peer) wrapped it in.
fn from_bar(text: &str) -> Option<String> {
    if !text.contains(crate::peer::FROM) {
        return None;
    }
    let start = text.find(crate::peer::PREAMBLE)? + crate::peer::PREAMBLE.len();
    let rest = &text[start..];
    let end = rest.find("</cross-session-message>").unwrap_or(rest.len());
    let typed = rest[..end].trim();
    (!typed.is_empty()).then(|| typed.to_string())
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(text[start..end].trim())
}

/// The conversation in `path`, now and after every change, for a session working in `cwd`.
pub fn watch(path: PathBuf, cwd: String) -> impl Stream<Item = Chat> {
    let (sender, receiver) = mpsc::unbounded_channel();
    // inotify reads block, so they get their own thread instead of the UI's executor.
    let spawned = std::thread::Builder::new()
        .name("eave-claude-chat".into())
        .spawn(move || {
            if let Err(error) = run(&path, &cwd, sender) {
                eprintln!("eave-claude: not following the conversation: {error}");
            }
        });
    if let Err(error) = spawned {
        eprintln!("eave-claude: not following the conversation: {error}");
    }
    stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|chat| (chat, receiver))
    })
}

/// Where a read left off: the offset of the first byte not read yet.
struct Reader {
    path: PathBuf,
    offset: u64,
    /// The start of a line whose end hasn't been written yet.
    partial: Vec<u8>,
}

impl Reader {
    /// Reads what was added since the last read, the last stretch on the first, into `chat`.
    /// False if nothing was.
    fn read(&mut self, chat: &mut Chat, cwd: &str) -> bool {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        let Ok(length) = file.metadata().map(|meta| meta.len()) else {
            return false;
        };
        if length < self.offset {
            // Rewritten from scratch: start over.
            *chat = Chat::default();
            self.offset = 0;
            self.partial.clear();
        }
        if length == self.offset {
            return false;
        }
        let first = self.offset == 0 && self.partial.is_empty();
        let start = if first {
            length.saturating_sub(TAIL)
        } else {
            self.offset
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return false;
        }
        let mut bytes = std::mem::take(&mut self.partial);
        let before = bytes.len();
        if file.read_to_end(&mut bytes).is_err() {
            return false;
        }
        self.offset = start + (bytes.len() - before) as u64;
        let mut text = bytes.as_slice();
        if first && start > 0 {
            // The first line is cut.
            let cut = text
                .iter()
                .position(|b| *b == b'\n')
                .map_or(text.len(), |at| at + 1);
            text = &text[cut..];
        }
        let complete = text
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |at| at + 1);
        for line in text[..complete].split(|b| *b == b'\n') {
            if let Ok(line) = std::str::from_utf8(line)
                && !line.is_empty()
            {
                chat.read_line(line, cwd);
            }
        }
        self.partial = text[complete..].to_vec();
        true
    }
}

/// Blocks, sending the conversation whenever it changed. Returns once nobody listens.
fn run(path: &Path, cwd: &str, changed: UnboundedSender<Chat>) -> rustix::io::Result<()> {
    let Some((dir, name)) = path.parent().zip(path.file_name()) else {
        return Ok(());
    };
    let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
    // The directory, not the file: the file may not be there yet.
    let events = WatchFlags::MODIFY
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::CREATE
        | WatchFlags::MOVED_TO
        | WatchFlags::ONLYDIR;
    inotify::add_watch(&fd, dir, events)?;
    let mut reader = Reader {
        path: path.to_path_buf(),
        offset: 0,
        partial: Vec::new(),
    };
    let mut chat = Chat::default();
    reader.read(&mut chat, cwd);
    if changed.send(chat.clone()).is_err() {
        return Ok(());
    }
    let mut buffer = [MaybeUninit::uninit(); 4096];
    let mut reader_fd = inotify::Reader::new(&fd, &mut buffer);
    // Wakes now and then without events, to notice the chat was closed.
    let every = Timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    loop {
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        match poll(&mut fds, Some(&every)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error),
        }
        if changed.is_closed() {
            return Ok(());
        }
        let mut ours = false;
        loop {
            match reader_fd.next() {
                Ok(event) => {
                    ours |=
                        event.file_name().map(|n| n.to_bytes()) == Some(name.as_encoded_bytes());
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error),
            }
        }
        if ours && reader.read(&mut chat, cwd) && changed.send(chat.clone()).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(lines: &[&str]) -> Chat {
        let mut chat = Chat::default();
        for line in lines {
            chat.read_line(line, "/home/f/code/eave");
        }
        chat
    }

    #[test]
    fn a_turn_reads_as_you_claude_and_tools() {
        let chat = chat(&[
            r#"{"type":"user","message":{"role":"user","content":"fix the bar"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/home/f/code/eave/src/lib.rs"}}]}}"#,
            r#"{"type":"permission-mode","permissionMode":"plan"}"#,
        ]);
        assert_eq!(
            chat.entries,
            [
                Entry::you("fix the bar"),
                Entry::Claude("Looking.".into()),
                Entry::Tool {
                    id: "t1".into(),
                    name: "Bash".into(),
                    what: "$ cargo test".into(),
                    done: true,
                    failed: false,
                    result: "ok".into(),
                    diff: None,
                },
                Entry::Tool {
                    id: "t2".into(),
                    name: "Read".into(),
                    what: "src/lib.rs".into(),
                    done: false,
                    failed: false,
                    result: String::new(),
                    diff: None,
                },
            ]
        );
        assert_eq!(chat.running(), Some(("Read", "src/lib.rs")));
        assert_eq!(chat.mode.as_deref(), Some("plan"));
    }

    #[test]
    fn claude_codes_own_notes_stay_hidden() {
        let chat = chat(&[
            r#"{"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>x</local-command-caveat>"}}"#,
            r#"{"type":"user","message":{"content":"<task-notification>\n<task-id>1</task-id>"}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/context</command-name>\n<command-message>context</command-message>\n<command-args></command-args>"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"content":"a subagent's prompt"}}"#,
            r#"not json"#,
        ]);
        assert_eq!(
            chat.entries,
            [Entry::you("/context"), Entry::Aside("Interrupted".into())]
        );
    }

    #[test]
    fn messages_from_the_bar_read_as_yours() {
        let wrapped = crate::peer::content("ship it\nnow");
        let line = serde_json::json!({"type":"user","isMeta":true,"message":{"content":
            format!("Another Claude session sent a message:\n<agent-message from=\"x\">\n{wrapped}\n</agent-message>")}});
        let chat = chat(&[&line.to_string()]);
        assert_eq!(chat.entries, [Entry::you("ship it\nnow")]);
    }

    #[test]
    fn a_failed_tool_says_so_and_the_list_stays_short() {
        let mut lines = vec![
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x","name":"Bash","input":{"command":"false"}}]}}"#.to_string(),
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"x","is_error":true}]}}"#.to_string(),
        ];
        for n in 0..300 {
            lines.push(format!(
                r#"{{"type":"user","message":{{"content":"message {n}"}}}}"#
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let chat = chat(&refs);
        assert_eq!(chat.entries.len(), KEEP);
        assert_eq!(chat.entries.last(), Some(&Entry::you("message 299")));
        let mut short = Chat::default();
        for line in &lines[..2] {
            short.read_line(line, "/");
        }
        assert!(matches!(
            short.entries[0],
            Entry::Tool {
                done: true,
                failed: true,
                ..
            }
        ));
    }

    #[test]
    fn results_say_what_came_of_a_tool_and_edits_their_change() {
        let chat = chat(&[
            r#"{"type":"user","timestamp":"2026-10-13T12:02:00.000Z","message":{"content":"go"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"r","name":"Read","input":{"file_path":"/home/f/code/eave/a.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"r","content":"..."}]},"toolUseResult":{"type":"text","file":{"numLines":812}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b","content":"x"}]},"toolUseResult":{"stdout":"running\ntest result: ok. 184 passed\n\n","stderr":""}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"f","name":"Bash","input":{"command":"cc"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"f","is_error":true,"content":"<tool_use_error>linker not found</tool_use_error>"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"e","name":"Edit","input":{"file_path":"/home/f/code/eave/a.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"e","content":"ok"}]},"toolUseResult":{"structuredPatch":[{"oldStart":88,"oldLines":2,"newStart":88,"newLines":2,"lines":[" keep","-old","+new","\\ No newline at end of file"]}]}}"#,
        ]);
        let Entry::You { at: Some(at), .. } = &chat.entries[0] else {
            panic!("the time was lost");
        };
        assert_eq!(at.to_utc().to_rfc3339(), "2026-10-13T12:02:00+00:00");
        let results: Vec<_> = chat.entries[1..]
            .iter()
            .map(|entry| match entry {
                Entry::Tool { result, .. } => result.as_str(),
                _ => panic!("not a tool"),
            })
            .collect();
        assert_eq!(
            results,
            [
                "812 lines",
                "test result: ok. 184 passed",
                "linker not found",
                ""
            ]
        );
        let Entry::Tool {
            diff: Some(diff), ..
        } = &chat.entries[4]
        else {
            panic!("the edit lost its change");
        };
        assert_eq!(
            diff,
            &[
                (Change::Hunk, "@@ -88,2 +88,2 @@".to_string()),
                (Change::Context, "keep".into()),
                (Change::Removed, "old".into()),
                (Change::Added, "new".into()),
            ]
        );
    }

    #[test]
    fn a_long_change_is_cut() {
        let lines: Vec<_> = (0..100).map(|n| format!("+{n}")).collect();
        let returned = serde_json::json!({"structuredPatch":[{"oldStart":1,"oldLines":0,"newStart":1,"newLines":100,"lines":lines}]});
        let diff = change(&returned).unwrap();
        assert_eq!(diff.len(), DIFF_LINES + 1);
        assert_eq!(diff.last().unwrap().1, "… 41 more lines");
        let created = serde_json::json!({"type":"create","content":"a\nb"});
        assert_eq!(change(&created).unwrap().len(), 2);
    }

    #[test]
    fn reading_follows_the_file_a_line_at_a_time() {
        let dir = std::env::temp_dir().join(format!("eave-claude-chat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"one\"}}\n{\"type\":\"user\",\"mess",
        )
        .unwrap();
        let mut reader = Reader {
            path: path.clone(),
            offset: 0,
            partial: Vec::new(),
        };
        let mut chat = Chat::default();
        assert!(reader.read(&mut chat, "/"));
        assert_eq!(chat.entries, [Entry::you("one")]);
        assert!(!reader.read(&mut chat, "/"), "nothing new");
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"age\":{\"content\":\"two\"}}\n").unwrap();
        assert!(reader.read(&mut chat, "/"));
        assert_eq!(chat.entries, [Entry::you("one"), Entry::you("two")]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
