//! Notifications, sent like any app's: `Notify` on `org.freedesktop.Notifications`, on the
//! session bus (eave's D-52). Whoever owns that name shows them: eave's own engine
//! (eave-notifications) or mako, dunst, swaync. Answers come back as its signals: an action
//! picked, a reply typed (KDE's inline reply), a notification closed.
//!
//! Claude's logo goes with each one as a file in `$XDG_RUNTIME_DIR`, so it shows without an icon
//! theme. The bodies are Claude's Markdown, which eave sets as such (`x-eave-markdown`); other
//! daemons show it as it is.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use eave_dbus::zbus::zvariant::Value;
use eave_dbus::zbus::{self, MatchRule, MessageStream};
use futures_util::{Stream, StreamExt, stream};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// What a notification says and offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// Who it's from: the session.
    pub app: String,
    pub summary: String,
    /// Markdown.
    pub body: String,
    /// Each action's key and label. `default` is a click on the notification.
    pub actions: Vec<(String, String)>,
    /// A field to type an answer into, with this placeholder.
    pub reply: Option<String>,
    /// Shown until dismissed, or for this many seconds.
    pub seconds: Option<u64>,
}

/// What the notification server said about one of the notifications.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// An action was picked, by key.
    Action(u32, String),
    /// An answer was typed into it.
    Replied(u32, String),
    /// It closed, and why: 1 expired, 2 dismissed, 3 closed by us, 4 otherwise.
    Closed(u32, u32),
}

/// Shows `note`, in place of the notification `replaces` if it's not 0, and returns its id.
pub async fn send(note: Note, replaces: u32) -> zbus::Result<u32> {
    let connection = eave_dbus::session().await?;
    let actions: Vec<&str> = note
        .actions
        .iter()
        .flat_map(|(key, label)| [key.as_str(), label.as_str()])
        .chain(
            note.reply
                .as_ref()
                .map(|_| ["inline-reply", "Reply"])
                .into_iter()
                .flatten(),
        )
        .collect();
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("x-eave-markdown", Value::from(true));
    hints.insert("urgency", Value::from(1u8));
    let logo = logo();
    if let Some(logo) = &logo {
        hints.insert("image-path", Value::from(logo.as_str()));
    }
    if let Some(placeholder) = &note.reply {
        hints.insert(
            "x-kde-reply-placeholder-text",
            Value::from(placeholder.as_str()),
        );
    }
    let timeout = note.seconds.map_or(0, |seconds| {
        i32::try_from(seconds * 1000).unwrap_or(i32::MAX)
    });
    let reply = connection
        .call_method(
            Some(NAME),
            PATH,
            Some(NAME),
            "Notify",
            &(
                note.app.as_str(),
                replaces,
                logo.as_deref().unwrap_or(""),
                note.summary.as_str(),
                note.body.as_str(),
                actions,
                hints,
                timeout,
            ),
        )
        .await?;
    reply.body().deserialize()
}

/// Takes the notification `id` away.
pub async fn close(id: u32) -> zbus::Result<()> {
    let connection = eave_dbus::session().await?;
    connection
        .call_method(Some(NAME), PATH, Some(NAME), "CloseNotification", &(id,))
        .await?;
    Ok(())
}

/// Every signal of the notification server, from now on. Not only about ours: the caller keeps
/// to the ids it was given.
pub fn signals() -> impl Stream<Item = Signal> {
    stream::once(async {
        let stream = async {
            let connection = eave_dbus::session().await?;
            let rule = MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(NAME)?
                .path(PATH)?
                .build();
            MessageStream::for_match_rule(rule, &connection, Some(64)).await
        };
        match stream.await {
            Ok(messages) => messages
                .filter_map(|message| async move { signal(&message.ok()?) })
                .boxed(),
            Err(error) => {
                eprintln!("eave-claude: not hearing back from notifications: {error}");
                stream::empty().boxed()
            }
        }
    })
    .flatten()
}

fn signal(message: &zbus::Message) -> Option<Signal> {
    let header = message.header();
    let body = message.body();
    match header.member()?.as_str() {
        "ActionInvoked" => {
            let (id, key): (u32, String) = body.deserialize().ok()?;
            Some(Signal::Action(id, key))
        }
        "NotificationReplied" => {
            let (id, text): (u32, String) = body.deserialize().ok()?;
            Some(Signal::Replied(id, text))
        }
        "NotificationClosed" => {
            let (id, reason): (u32, u32) = body.deserialize().ok()?;
            Some(Signal::Closed(id, reason))
        }
        _ => None,
    }
}

/// Claude's logo as a file, written once: notification servers take icons by path.
fn logo() -> Option<String> {
    static LOGO: OnceLock<Option<String>> = OnceLock::new();
    LOGO.get_or_init(|| {
        let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("eave-claude");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("claude.svg");
        std::fs::write(&path, include_bytes!("../assets/claude.svg")).ok()?;
        Some(path.to_str()?.to_string())
    })
    .clone()
}
