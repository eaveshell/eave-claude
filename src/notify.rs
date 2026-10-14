//! Notifications, sent like any app's: `Notify` on `org.freedesktop.Notifications`, on the
//! session bus (eave's D-52). Whoever owns that name shows them: eave's own engine
//! (eave-notifications) or mako, dunst, swaync. Answers come back as its signals: an action
//! picked, a reply typed (KDE's inline reply), a notification closed.
//!
//! The mascot goes with each one, in the note's mood, as a file in `$XDG_RUNTIME_DIR`: servers
//! take pictures by path, and it shows without an icon theme. Notifications are still pictures,
//! so it's the sprite's resting frame, made square. The bodies are Claude's Markdown, which eave sets as such (`x-eave-markdown`); other
//! daemons show it as it is.
//!
//! eave's server says more (eave's D-85), when it lists the capabilities: the app is Claude and
//! the session its thread, with the mascot as the app's icon (`x-eave-thread`, `x-eave-icon`),
//! and a question comes with its options, descriptions and all, one at a time
//! (`x-eave-choices`). Other servers get the session as the app, the mascot as the picture, and
//! a question's options as actions when they're few and short ([`Note`] as it is).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use eave_dbus::zbus::zvariant::Value;
use eave_dbus::zbus::{self, MatchRule, MessageStream};
use futures_util::{Stream, StreamExt, stream};

use crate::mood::Mood;

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// What a notification says and offers, as any server shows it. [`rich`] makes it eave's.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// Who it's from: the session.
    pub app: String,
    /// The session's id, for eave to stack its notifications by.
    pub thread: String,
    /// The spec's: 0 low, 1 normal, 2 critical.
    pub urgency: u8,
    /// The question it asks, for a server that takes options ([`rich`]).
    pub question: Option<Asking>,
    pub summary: String,
    /// Markdown.
    pub body: String,
    /// Each action's key and label. `default` is a click on the notification.
    pub actions: Vec<(String, String)>,
    /// A field to type an answer into, with this placeholder.
    pub reply: Option<String>,
    /// Shown until dismissed, or for this many seconds.
    pub seconds: Option<u64>,
    /// The mascot's, as its picture.
    pub mood: Mood,
}

/// A question, one of several maybe, as eave asks it.
#[derive(Debug, Clone, PartialEq)]
pub struct Asking {
    pub text: String,
    /// Each option's label and what it means.
    pub options: Vec<(String, String)>,
    pub multiple: bool,
    /// Which question, from 0, of how many.
    pub step: (u32, u32),
}

/// The action key of a question's option `index`, picked.
pub fn pick_key(index: usize) -> String {
    format!("pick:{index}")
}

/// The action that goes back to the question before (eave's).
pub const BACK: &str = "x-eave-back";

/// What a label says it is: Claude marks the option it suggests "(Recommended)".
const RECOMMENDED: &str = "(Recommended)";

/// The hints eave's server takes, if the one there lists them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    /// `x-eave-thread` and `x-eave-icon`.
    pub threads: bool,
    /// `x-eave-choices`.
    pub choices: bool,
}

/// `note` as eave shows it: from Claude, the session its thread; and its question, if it asks
/// one, with every option, the field for an answer of your own, and a way back to the question
/// before. What it says otherwise stays.
pub fn rich(mut note: Note, caps: Caps) -> Note {
    if caps.choices
        && let Some(question) = &note.question
    {
        note.summary = question.text.clone();
        note.body = String::new();
        note.actions
            .retain(|(key, _)| !key.starts_with("pick:") && key != "answer");
        if question.step.0 > 0 {
            note.actions.push((BACK.into(), "Back".into()));
        }
        note.reply = Some("Or answer in your own words".into());
    } else {
        note.question = None;
    }
    note
}

/// What the notification server said about one of the notifications.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    /// An action was picked, by key.
    Action(u32, String),
    /// Several options were picked, by key, and sent (eave's).
    Picked(u32, Vec<String>),
    /// An answer was typed into it.
    Replied(u32, String),
    /// It closed, and why: 1 expired, 2 dismissed, 3 closed by us, 4 otherwise.
    Closed(u32, u32),
}

/// Shows `note`, in place of the notification `replaces` if it's not 0, and returns its id. It
/// asks the server what it takes first: whoever owns the name may have changed.
pub async fn send(note: Note, replaces: u32) -> zbus::Result<u32> {
    let connection = eave_dbus::session().await?;
    let caps = capabilities(&connection).await;
    // Built whole before the call, which borrows from it.
    let label = note.app.clone();
    let note = rich(note, caps);
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
    hints.insert("urgency", Value::from(note.urgency));
    let picture = picture(note.mood);
    let app = if caps.threads {
        "Claude"
    } else {
        label.as_str()
    };
    if caps.threads {
        hints.insert("x-eave-thread", Value::from(note.thread.as_str()));
        hints.insert("x-eave-thread-name", Value::from(label.as_str()));
        if let Some(picture) = &picture {
            hints.insert("x-eave-icon", Value::from(picture.as_str()));
        }
    } else if let Some(picture) = &picture {
        hints.insert("image-path", Value::from(picture.as_str()));
    }
    if let Some(question) = &note.question {
        let options: Vec<(String, String, String, bool)> = question
            .options
            .iter()
            .enumerate()
            .map(|(index, (label, description))| {
                let recommended = label.ends_with(RECOMMENDED);
                let label = label.trim_end_matches(RECOMMENDED).trim_end().to_string();
                (pick_key(index), label, description.clone(), recommended)
            })
            .collect();
        hints.insert("x-eave-choices", Value::from(options));
        hints.insert("x-eave-multiple", Value::from(question.multiple));
        if question.step.1 > 1 {
            hints.insert("x-eave-step", Value::from(question.step));
        }
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
    // The app's icon is the mascot too, for servers that take it there.
    let app_icon = if caps.threads {
        ""
    } else {
        picture.as_deref().unwrap_or("")
    };
    let reply = connection
        .call_method(
            Some(NAME),
            PATH,
            Some(NAME),
            "Notify",
            &(
                app,
                replaces,
                app_icon,
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

/// What the server owning the name takes. Nothing of eave's if it can't say.
async fn capabilities(connection: &zbus::Connection) -> Caps {
    let reply = connection
        .call_method(Some(NAME), PATH, Some(NAME), "GetCapabilities", &())
        .await;
    let caps: Vec<String> = reply
        .ok()
        .and_then(|reply| reply.body().deserialize().ok())
        .unwrap_or_default();
    let has = |cap: &str| caps.iter().any(|given| given == cap);
    Caps {
        threads: has("x-eave-thread") && has("x-eave-icon"),
        choices: has("x-eave-choices"),
    }
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
        "EaveChoicesPicked" => {
            let (id, keys): (u32, Vec<String>) = body.deserialize().ok()?;
            Some(Signal::Picked(id, keys))
        }
        "NotificationClosed" => {
            let (id, reason): (u32, u32) = body.deserialize().ok()?;
            Some(Signal::Closed(id, reason))
        }
        _ => None,
    }
}

/// The mascot in `mood`, at rest, as a file written once: notification servers take pictures by
/// path.
fn picture(mood: Mood) -> Option<String> {
    static PICTURES: [OnceLock<Option<String>>; Mood::ALL.len()] =
        [const { OnceLock::new() }; Mood::ALL.len()];
    let at = Mood::ALL.iter().position(|m| *m == mood)?;
    PICTURES[at]
        .get_or_init(|| {
            let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("eave-claude");
            std::fs::create_dir_all(&dir).ok()?;
            let path = dir.join(format!("{}.png", mood.name()));
            std::fs::write(&path, mood.sprite().note).ok()?;
            Some(path.to_str()?.to_string())
        })
        .clone()
}
