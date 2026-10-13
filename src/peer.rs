//! Messages typed in the bar, sent into a running session through its messaging socket: the one
//! Claude Code sessions use to talk to each other. No hook can add what you type to a session,
//! so this is how replies reach one.
//!
//! Each message is one line of JSON on a fresh connection, as Claude Code 2.1 (`peerProtocol` 1)
//! writes them. Claude shows it as a message from another session, so the text says it's you.

use std::hash::{BuildHasher, RandomState};
use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// The name Claude sees it from. Not `eave`, which could be taken for a session of that name.
const FROM: &str = "eave-bar";

/// Sends `text` to the session of `pid`, listening on `socket`.
pub async fn send(
    pid: u32,
    socket: PathBuf,
    sessions: Option<PathBuf>,
    text: String,
) -> io::Result<()> {
    let mut lines = String::new();
    // Linux doesn't ask for the token, but Claude's own sessions always send it.
    if let Some(token) = sessions.and_then(|dir| token(&dir, pid)) {
        lines += &json!({"type": "auth", "token": token}).to_string();
        lines.push('\n');
    }
    lines += &message(&text, &uuid());
    lines.push('\n');
    let mut stream = UnixStream::connect(&socket).await?;
    stream.write_all(lines.as_bytes()).await?;
    stream.shutdown().await
}

fn message(text: &str, id: &str) -> String {
    let content = format!(
        "<cross-session-message from-name=\"{FROM}\">\n\
         This is the user, typing in their desktop bar instead of the terminal. Answer them here as \
         usual; don't reply with SendMessage.\n\n{text}\n</cross-session-message>"
    );
    json!({
        "msgV": 1,
        "msg_id": id,
        "type": "user",
        "message": {"role": "user", "content": content},
        "priority": "next",
    })
    .to_string()
}

/// The session's peer token, from `<pid>.<hash>.key` beside its session file.
fn token(sessions: &Path, pid: u32) -> Option<String> {
    let prefix = format!("{pid}.");
    let key = std::fs::read_dir(sessions).ok()?.flatten().find(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with(&prefix) && name.ends_with(".key")
    })?;
    let key: serde_json::Value = serde_json::from_slice(&std::fs::read(key.path()).ok()?).ok()?;
    key.get("peerToken")?.as_str().map(str::to_string)
}

/// A random version 4 UUID, as Claude's own message ids are.
fn uuid() -> String {
    let random = RandomState::new();
    let high = random.hash_one(std::process::id());
    let low = random.hash_one(std::time::SystemTime::now());
    let bits = (u128::from(high) << 64 | u128::from(low)) & !(0xf << 76) & !(0x3 << 62)
        | 0x4 << 76
        | 0x2 << 62;
    let hex = format!("{bits:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_one_line_claude_understands() {
        let line = message("say \"hi\"\nplease", "id-1");
        assert!(!line.contains('\n'), "newlines are escaped");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "user");
        assert_eq!(value["msgV"], 1);
        assert_eq!(value["priority"], "next");
        let content = value["message"]["content"].as_str().unwrap();
        assert!(content.starts_with("<cross-session-message from-name=\"eave-bar\">\n"));
        assert!(content.ends_with("say \"hi\"\nplease\n</cross-session-message>"));
    }

    #[test]
    fn uuids_look_like_version_4() {
        let id = uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(uuid(), id);
    }

    #[test]
    fn the_token_comes_from_the_key_file() {
        let dir = std::env::temp_dir().join(format!("eave-claude-peer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("42.abc.key"), r#"{"peerToken":"secret"}"#).unwrap();
        std::fs::write(dir.join("420.def.key"), r#"{"peerToken":"other"}"#).unwrap();
        assert_eq!(token(&dir, 42).as_deref(), Some("secret"));
        assert_eq!(token(&dir, 7), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sending_writes_the_lines_and_hangs_up() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = std::env::temp_dir().join(format!("eave-claude-send-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("1.sock");
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let sent = tokio::spawn(send(1, path.clone(), None, "hello".into()));
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut received)
                .await
                .unwrap();
            sent.await.unwrap().unwrap();
            assert_eq!(received.lines().count(), 1, "no key file, so no auth line");
            assert!(received.contains("hello"));
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
