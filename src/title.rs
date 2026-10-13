//! What a session is called, from the end of its transcript. Claude Code appends its metadata
//! lines (the title it made up, the one you gave with `/rename`) again every so often, so the
//! latest ones are near the end and only the tail is read.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;

const TAIL: u64 = 256 << 10;

/// The session's name: an agent's name, else the title you gave it, else Claude's.
pub fn last(transcript: &Path) -> Option<String> {
    let mut file = std::fs::File::open(transcript).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut tail).ok()?;
    let mut tail = String::from_utf8_lossy(&tail).into_owned();
    if start > 0 {
        // The first line is cut.
        tail.drain(..tail.find('\n').map_or(tail.len(), |at| at + 1));
    }
    pick(&tail)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum Line {
    #[serde(rename_all = "camelCase")]
    AgentName { agent_name: String },
    #[serde(rename_all = "camelCase")]
    CustomTitle { custom_title: String },
    #[serde(rename_all = "camelCase")]
    AiTitle { ai_title: String },
}

fn pick(lines: &str) -> Option<String> {
    let (mut agent, mut custom, mut ai) = (None, None, None);
    for line in lines.lines().rev() {
        // Most lines are messages; only parse the ones that can be titles.
        if !line.contains("-title\"") && !line.contains("\"agent-name\"") {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(Line::AgentName { agent_name }) => {
                agent.get_or_insert(agent_name);
            }
            Ok(Line::CustomTitle { custom_title }) => {
                custom.get_or_insert(custom_title);
            }
            Ok(Line::AiTitle { ai_title }) => {
                ai.get_or_insert(ai_title);
            }
            Err(_) => {}
        }
    }
    [agent, custom, ai]
        .into_iter()
        .flatten()
        .find(|title| !title.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_and_most_personal_title_wins() {
        let lines = [
            r#"{"type":"ai-title","aiTitle":"Old title","sessionId":"s"}"#,
            r#"{"type":"user","message":{"content":"has \"ai-title\" in it"}}"#,
            r#"{"type":"ai-title","aiTitle":"New title","sessionId":"s"}"#,
        ]
        .join("\n");
        assert_eq!(pick(&lines).as_deref(), Some("New title"));
        let renamed = format!(
            "{lines}\n{}",
            r#"{"type":"custom-title","customTitle":"Mine"}"#
        );
        assert_eq!(pick(&renamed).as_deref(), Some("Mine"));
        assert_eq!(pick(r#"{"type":"user"}"#), None);
    }

    #[test]
    fn only_the_tail_of_a_long_transcript_is_read() {
        let dir = std::env::temp_dir().join(format!("eave-claude-title-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let filler = format!("{{\"type\":\"user\",\"x\":\"{}\"}}\n", "y".repeat(1000));
        let mut text = r#"{"type":"ai-title","aiTitle":"Too far back"}"#.to_string() + "\n";
        text += &filler.repeat(400);
        text += r#"{"type":"ai-title","aiTitle":"Recent"}"#;
        std::fs::write(&path, text).unwrap();
        assert_eq!(last(&path).as_deref(), Some("Recent"));
        assert_eq!(last(&dir.join("missing.jsonl")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
