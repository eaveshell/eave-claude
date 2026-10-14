//! Claude Code, in the bar. The item shows when sessions run, and counts the ones waiting for
//! you. A permission prompt, a question or a plan to approve becomes a notification you can
//! answer, and a session that finishes its turn says so and takes a reply. The popover lists the
//! sessions; each one opens its conversation, live, with what it's doing now, what it asks, and a
//! field to answer it.
//!
//! Four sources, each for what only it knows:
//! - `~/.claude/sessions/` ([`sessions`]): which sessions run and whether they wait for you.
//! - The eave plugin for Claude Code ([`hooks`]): what they ask, and the answers.
//! - Each session's transcript ([`transcript`]): the conversation, while it shows.
//! - Each session's messaging socket ([`peer`]): what you type to them.
//!
//! Notifications go over the session bus like any app's ([`notify`]).

mod hooks;
mod mascot;
mod mood;
mod notify;
mod peer;
mod sessions;
mod title;
mod transcript;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

use eave_api::time::{Duration, sleep};
use eave_api::ui::{self, Item, Panel, Tone};
use eave_api::{Effect, Nudge, Registry, Subscription, Task, Widget};
use serde::Deserialize;
use serde_json::{Value, json};

use mood::{Clock, Facts, Mood};
use sessions::Status;
use transcript::{Chat, Entry};

pub fn register(registry: &mut Registry) {
    registry.widget::<Claude>("claude");
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Says so when a session finishes its turn.
    done: bool,
    /// Seconds that shows.
    timeout: u64,
    /// The mode an approved plan goes on in: `auto`, `acceptEdits`, `default`… Unset, the one
    /// the session was in before planning.
    plan: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            done: true,
            timeout: 8,
            plan: None,
        }
    }
}

/// How long a session waits in the terminal before the bar says so, when no hook asked: a hook's
/// prompt usually arrives with the dialog, and would say it better.
const UNASKED: Duration = Duration::from_millis(1500);

pub struct Claude {
    config: Config,
    /// Why prompts can't be answered here, if they can't.
    failed: Option<String>,
    /// Newest first.
    sessions: Vec<Session>,
    /// Waiting for an answer, newest first.
    prompts: Vec<Prompt>,
    /// Notifications that aren't prompts, newest first.
    notes: Vec<Note>,
    next_note: u64,
    /// The notifications on the bus: what each is for, and its id once the server gave it.
    posted: Vec<Posted>,
    /// Each session's permission mode before it went into plan mode, from its hooks: approving a
    /// plan goes back to it, as the terminal does.
    modes: HashMap<String, String>,
    /// The sessions in plan mode, as their hooks last said.
    planning: HashSet<String>,
    /// The sessions whose last turn ended in an error.
    failed_turns: HashSet<String>,
    /// The time when the last message came: moods change with it.
    clock: Clock,
    page: Page,
    /// What's being typed in the popover, and where it goes.
    draft: Option<(Target, String)>,
    /// Whether the popover shows.
    open: bool,
    /// The conversation of the session whose chat shows.
    chat: Option<(String, Chat)>,
    /// The session whose chat the dialog shows, or showed last: it keeps it while it goes back.
    talking: Option<String>,
    /// Whether the dialog is open.
    dialog_open: bool,
}

struct Session {
    id: String,
    pid: u32,
    cwd: String,
    name: Option<String>,
    title: Option<String>,
    status: Status,
    socket: Option<PathBuf>,
    /// The end of its last turn.
    last_reply: Option<String>,
    /// Milliseconds since the epoch.
    started: u64,
    /// When its status last changed, in milliseconds since the epoch.
    since: u64,
}

struct Prompt {
    /// The hook server's id for it.
    id: u64,
    session: String,
    cwd: String,
    ask: Ask,
    /// The session file said it waits, so it no longer waiting means this was answered elsewhere.
    seen_waiting: bool,
    /// Its notification was dismissed. It still waits, in the popover.
    hidden: bool,
}

enum Ask {
    /// May it use a tool.
    Permission {
        tool: String,
        what: String,
        /// What it says it's for, if it says: Bash's `description`.
        why: Option<String>,
        /// The rules the terminal offers as "don't ask again".
        always: Option<Value>,
    },
    /// AskUserQuestion: up to four questions, each with options, and what's picked so far.
    Question {
        questions: Vec<Question>,
        input: Value,
        picks: Vec<Vec<usize>>,
        /// Answers typed instead of picked.
        own: Vec<Option<String>>,
        /// The one the popover shows: they come one at a time.
        at: usize,
    },
    /// ExitPlanMode: a plan to approve.
    Plan {
        plan: String,
        /// The tool's input, given back when approving: Claude Code wants it for this tool.
        input: Value,
        /// What the hook suggested, in case it offers a mode to go on in.
        suggestions: Value,
    },
}

struct Question {
    text: String,
    header: String,
    multiple: bool,
    options: Vec<(String, String)>,
}

struct Note {
    key: u64,
    session: String,
    kind: NoteKind,
    /// Not shown as a notification: dismissed, or said while its chat showed.
    hidden: bool,
}

enum NoteKind {
    /// Its turn ended, with this reply.
    Done(String),
    Failed(String),
    /// A dialog waits in the terminal that no hook asked about: what it waits for.
    Waiting(String),
}

/// What a notification on the bus is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Prompt(u64),
    Note(u64),
}

struct Posted {
    key: Key,
    /// The server's id for it, once it answered.
    id: Option<u32>,
    /// No longer wanted, before its id came: close it once it does.
    gone: bool,
    /// What it says, as last sent: one that changes is sent again in its place, as a question
    /// of several moves on.
    note: notify::Note,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Page {
    Sessions,
    /// A session's conversation.
    Chat(String),
    /// A prompt from a session not in the list, like one run with `-p`.
    Prompt(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// Instead of the buttons: what to do instead, feedback on a plan, or an answer of your own.
    Prompt(u64),
    /// A message to the session.
    Session(String),
}

#[derive(Debug, Clone)]
pub enum Message {
    Hooks(hooks::Event),
    Sessions(Vec<sessions::Live>),
    /// The notification server said something about a notification.
    Signal(notify::Signal),
    /// A notification went up, with this id, or couldn't.
    Posted(Key, Option<u32>),
    /// The conversation of a session, read again.
    Chat(String, Chat),
    Allow(u64),
    AlwaysAllow(u64),
    Deny(u64),
    /// Approves a plan.
    Approve(u64),
    /// An option of a question: prompt, question, option.
    Pick(u64, usize, usize),
    /// Sends the options picked.
    Submit(u64),
    /// Shows another question of a prompt.
    Turn(u64, usize),
    Draft(Target, String),
    SendReply,
    /// Goes to a page, opening the popover if it's closed. A chat opens in the dialog.
    Show(Page),
    Back,
    /// Focuses the terminal a session runs in.
    Focus(String),
    /// A session has waited a moment: say so if no hook asked.
    Unasked(String),
    Opened,
    Closed,
    /// The chat's dialog opened, or started going back.
    DialogOpened,
    DialogClosed,
    /// Time changed a session's mood: done a while ago, or night fell.
    Tick,
}

impl Widget for Claude {
    type Config = Config;
    type Message = Message;

    fn new(config: Config) -> (Self, Task<Message>) {
        let claude = Self {
            config,
            failed: None,
            sessions: Vec::new(),
            prompts: Vec::new(),
            notes: Vec::new(),
            next_note: 1,
            posted: Vec::new(),
            modes: HashMap::new(),
            planning: HashSet::new(),
            failed_turns: HashSet::new(),
            clock: Clock::now(),
            page: Page::Sessions,
            draft: None,
            open: false,
            chat: None,
            talking: None,
            dialog_open: false,
        };
        (claude, Task::none())
    }

    fn update(&mut self, message: Message) -> Effect<Message> {
        self.clock = Clock::now();
        let effect = self.handle(message);
        self.quiet_chat();
        let post = self.post();
        effect.and_task(post)
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            Subscription::run(hooks::watch).map(Message::Hooks),
            sessions::watch().map(Message::Sessions),
            Subscription::run(notify::signals).map(Message::Signal),
        ];
        // The conversation, only while it shows.
        if let (true, Some(id)) = (self.dialog_open, &self.talking)
            && let Some(session) = self.session(id)
            && let Some(config) = sessions::config_dir()
        {
            let path = sessions::transcript(&config, &session.cwd, id);
            let data = (id.clone(), path, session.cwd.clone());
            subscriptions.push(
                Subscription::run_with(data, |(id, path, cwd)| {
                    use futures_util::StreamExt;
                    let id = id.clone();
                    transcript::watch(path.clone(), cwd.clone()).map(move |chat| (id.clone(), chat))
                })
                .map(|(id, chat)| Message::Chat(id, chat)),
            );
        }
        // Wakes when time alone changes a mood, not before.
        if let Some(at) = self.next_change() {
            subscriptions.push(
                Subscription::run_with(at, |at| {
                    let at = *at;
                    futures_util::stream::once(async move {
                        let left = at.saturating_sub(Clock::now().now);
                        sleep(Duration::from_millis(left)).await;
                    })
                })
                .map(|()| Message::Tick),
            );
        }
        Subscription::batch(subscriptions)
    }

    fn item(&self) -> Item<Message> {
        if self.sessions.is_empty() && self.prompts.is_empty() {
            return Item::hidden();
        }
        let waiting = self.waiting();
        // The mascot, in the mood of the session that most needs saying. It acts out a change of
        // mood, then rests: sessions work most of the day, and a mascot that keeps moving keeps
        // the bar drawing. Only a question goes on, until it's answered. Dimmed while nothing
        // works, waits or just finished.
        let mood = self.mood();
        let sprite = mood.sprite();
        let logo = ui::sprite(sprite.frames, sprite.order);
        let logo = match mood {
            Mood::Question => logo.looping(),
            mood if mood.resting() => logo.tone(Tone::Dim),
            _ => logo,
        };
        let content = match waiting {
            0 => logo,
            waiting => ui::row([
                logo,
                ui::counter(waiting.min(99) as i64, 0..=99, "").tone(Tone::Warning),
            ]),
        };
        Item::new(content)
    }

    fn popover(&self) -> Option<Panel<Message>> {
        let panel = match &self.page {
            Page::Prompt(id) => self
                .prompt(*id)
                .map(|prompt| Panel::new(self.prompt_page(prompt)).wide()),
            Page::Chat(_) | Page::Sessions => None,
        };
        let panel = panel.unwrap_or_else(|| Panel::new(self.sessions_page()));
        Some(panel.on_open(Message::Opened).on_close(Message::Closed))
    }

    /// A session's chat, in the middle of the screen: it comes out of what opened it.
    fn dialog(&self) -> Option<Panel<Message>> {
        let session = self.session(self.talking.as_ref()?)?;
        Some(
            Panel::new(self.chat_page(session))
                .on_open(Message::DialogOpened)
                .on_close(Message::DialogClosed),
        )
    }

    /// What the sessions ask, answered from the dormer: a permission allowed or denied, a plan
    /// approved, a question opened to answer. Then each session's chat, and its terminal.
    fn actions(&self) -> Vec<ui::Action<Message>> {
        let words = ["claude", "session"];
        let mascot = |mood: Mood| ui::image(mood.sprite().note);
        let mut actions = Vec::new();
        for prompt in &self.prompts {
            let label = self.label(&prompt.session, &prompt.cwd);
            let asking = |title: String, message| {
                ui::action(title, message)
                    .subtitle(label.clone())
                    .keywords(words)
                    .leading(mascot(Mood::Question))
            };
            match &prompt.ask {
                Ask::Permission { tool, .. } => actions.extend([
                    asking(format!("Allow {tool}"), Message::Allow(prompt.id))
                        .keywords(["permission", "yes"]),
                    asking(format!("Deny {tool}"), Message::Deny(prompt.id))
                        .keywords(["permission", "no"]),
                ]),
                Ask::Plan { .. } => actions.push(
                    asking("Approve plan".into(), Message::Approve(prompt.id))
                        .keywords(["plan", "yes"]),
                ),
                Ask::Question { questions, at, .. } => {
                    let question = questions.get(*at).or(questions.first());
                    let title = question.map_or_else(|| "Claude asks".into(), question_title);
                    actions.push(
                        asking(
                            format!("Answer: {title}"),
                            Message::Show(Page::Prompt(prompt.id)),
                        )
                        .keywords(["question"]),
                    );
                }
            }
        }
        for session in &self.sessions {
            let label = session.label();
            let mood = self.mood_of(session);
            actions.push(
                ui::action(
                    format!("Open {label}"),
                    Message::Show(Page::Chat(session.id.clone())),
                )
                .subtitle(format!("Claude Code · {}", session.status_text()))
                .keywords(words.into_iter().chain(["chat"]))
                .leading(mascot(mood)),
            );
            actions.push(
                ui::action(
                    format!("Show the terminal of {label}"),
                    Message::Focus(session.id.clone()),
                )
                .subtitle(directory(&session.cwd))
                .keywords(words.into_iter().chain(["terminal", "focus"]))
                .leading(mascot(mood)),
            );
        }
        actions
    }
}

impl Claude {
    fn handle(&mut self, message: Message) -> Effect<Message> {
        match message {
            Message::Hooks(event) => self.heard(event),
            Message::Sessions(live) => self.sessions_changed(live),
            Message::Signal(signal) => self.signalled(signal),
            Message::Posted(key, id) => self.posted(key, id),
            Message::Chat(id, chat) => {
                if self.talking.as_ref() == Some(&id) {
                    self.chat = Some((id, chat));
                }
                Effect::none()
            }
            Message::Allow(id) => self.decide(id, json!({"behavior": "allow"})),
            Message::AlwaysAllow(id) => {
                let always = self.prompt(id).and_then(|prompt| match &prompt.ask {
                    Ask::Permission { always, .. } => always.clone(),
                    _ => None,
                });
                match always {
                    Some(rules) => self.decide(
                        id,
                        json!({"behavior": "allow", "updatedPermissions": rules}),
                    ),
                    None => self.decide(id, json!({"behavior": "allow"})),
                }
            }
            // Like Esc in the terminal: Claude stops and waits for you.
            Message::Deny(id) => self.decide(
                id,
                json!({"behavior": "deny", "message": "The user said no.", "interrupt": true}),
            ),
            Message::Approve(id) => self.approve(id),
            Message::Pick(id, question, option) => self.pick(id, question, option),
            Message::Submit(id) => self.submit(id),
            Message::Turn(id, question) => {
                if let Some(Ask::Question { questions, at, .. }) = self
                    .prompts
                    .iter_mut()
                    .find(|p| p.id == id)
                    .map(|p| &mut p.ask)
                    && question < questions.len()
                {
                    *at = question;
                }
                self.drop_draft(&Target::Prompt(id));
                Effect::none()
            }
            Message::Draft(target, text) => {
                self.draft = Some((target, text));
                Effect::none()
            }
            Message::SendReply => {
                let Some((target, text)) = self.draft.take() else {
                    return Effect::none();
                };
                if text.trim().is_empty() {
                    self.draft = Some((target, text));
                    return Effect::none();
                }
                self.answer(target, text)
            }
            Message::Show(page) => self.show(page),
            Message::Back => {
                let session = match &self.page {
                    Page::Prompt(id) => self
                        .prompt(*id)
                        .map(|prompt| prompt.session.clone())
                        .filter(|session| self.session(session).is_some()),
                    _ => None,
                };
                self.page = Page::Sessions;
                self.draft = None;
                match session {
                    Some(session) => self.show(Page::Chat(session)),
                    None => Effect::none(),
                }
            }
            Message::Focus(id) => {
                let Some(session) = self.session(&id) else {
                    return Effect::none();
                };
                // The terminal shows the title; it tells its tabs apart.
                let pid = session.pid;
                let title = session.title.clone();
                self.notes
                    .retain(|note| !(note.session == id && matches!(note.kind, NoteKind::Done(_))));
                Effect::task(
                    Task::future(async move {
                        if let Err(error) = focus(pid, title).await {
                            eprintln!("eave-claude: can't show the session: {error}");
                        }
                    })
                    .discard(),
                )
                .close_popover()
                .close_dialog()
            }
            Message::Unasked(id) => {
                let waiting = match self.session(&id).map(|session| &session.status) {
                    Some(Status::Waiting(what)) => what.clone(),
                    _ => return Effect::none(),
                };
                let asked = self.prompts.iter().any(|prompt| prompt.session == id);
                let said = self
                    .notes
                    .iter()
                    .any(|note| note.session == id && matches!(note.kind, NoteKind::Waiting(_)));
                if !asked && !said {
                    self.note(id, NoteKind::Waiting(waiting));
                    return Effect::none().nudge(Nudge::Down);
                }
                Effect::none()
            }
            Message::Opened => {
                self.open = true;
                Effect::none()
            }
            Message::Tick => Effect::none(),
            Message::Closed => {
                self.open = false;
                self.page = Page::Sessions;
                // What's typed in the chat stays with it.
                if !self.dialog_open {
                    self.draft = None;
                }
                Effect::none()
            }
            Message::DialogOpened => {
                self.dialog_open = true;
                Effect::none()
            }
            // It keeps showing the chat while it goes back.
            Message::DialogClosed => {
                self.dialog_open = false;
                self.draft = None;
                Effect::none()
            }
        }
    }

    /// Goes to `page`, opening the popover if it's closed. A chat opens in the dialog, and the
    /// popover it may come out of folds away. A prompt of a session in the list shows in that
    /// session's chat.
    fn show(&mut self, page: Page) -> Effect<Message> {
        let page = match page {
            Page::Prompt(id) => match self.prompt(id) {
                Some(prompt) if self.session(&prompt.session).is_some() => {
                    Page::Chat(prompt.session.clone())
                }
                _ => Page::Prompt(id),
            },
            page => page,
        };
        if let Page::Chat(id) = page {
            if self.talking.as_ref() != Some(&id) {
                self.chat = None;
                self.draft = None;
            }
            self.talking = Some(id);
            return Effect::none().open_dialog();
        }
        if self.page != page {
            self.draft = None;
        }
        self.page = page;
        // Asking the host to open an open popover closes it first, and closing resets the page:
        // within the popover, just change pages.
        if self.open {
            Effect::none()
        } else {
            Effect::none().open_popover()
        }
    }

    fn heard(&mut self, event: hooks::Event) -> Effect<Message> {
        match event {
            hooks::Event::Listening => self.failed = None,
            hooks::Event::Failed(error) => {
                eprintln!("eave-claude: {error}");
                self.failed = Some(error);
            }
            hooks::Event::Hook(hook) => self.hooked(*hook),
            hooks::Event::Asked(id, hook) => return self.asked(id, *hook),
            hooks::Event::Settled(id) => self.forget(id),
        }
        Effect::none()
    }

    fn hooked(&mut self, hook: hooks::Hook) {
        self.remember_mode(&hook);
        let id = hook.session_id;
        match hook.hook_event_name.as_str() {
            "UserPromptSubmit" => {
                self.failed_turns.remove(&id);
                self.settle_session(&id);
                self.notes.retain(|note| note.session != id);
                if let (Some(session), Some(title)) = (self.session_mut(&id), hook.session_title) {
                    session.title = Some(title);
                }
            }
            "Stop" if hook.agent_id.is_none() => {
                self.failed_turns.remove(&id);
                self.settle_session(&id);
                let reply = hook.last_assistant_message.unwrap_or_default();
                if let Some(session) = self.session_mut(&id) {
                    session.last_reply = Some(reply.clone()).filter(|r| !r.is_empty());
                }
                if self.config.done && !hook.stop_hook_active {
                    self.notes.retain(|note| note.session != id);
                    self.note(id, NoteKind::Done(reply));
                }
            }
            "StopFailure" => {
                self.failed_turns.insert(id.clone());
                self.settle_session(&id);
                let error = match hook.error {
                    Value::String(error) => error,
                    Value::Null => String::new(),
                    error => error.to_string(),
                };
                self.notes.retain(|note| note.session != id);
                self.note(id, NoteKind::Failed(error));
            }
            "SessionEnd" => {
                self.settle_session(&id);
                self.notes.retain(|note| {
                    !(note.session == id && matches!(note.kind, NoteKind::Waiting(_)))
                });
            }
            _ => {}
        }
    }

    /// Keeps whether a session is in plan mode, and the mode it works in outside it, to go back
    /// to after a plan.
    fn remember_mode(&mut self, hook: &hooks::Hook) {
        let mode = hook.permission_mode.as_str();
        let id = &hook.session_id;
        if mode.is_empty() || id.is_empty() {
            return;
        }
        if mode == "plan" {
            self.planning.insert(id.clone());
        } else {
            self.planning.remove(id);
            self.modes.insert(id.clone(), mode.to_string());
        }
    }

    fn asked(&mut self, id: u64, hook: hooks::Hook) -> Effect<Message> {
        self.remember_mode(&hook);
        let ask = ask(&hook);
        let session = hook.session_id;
        self.notes
            .retain(|note| !(note.session == session && matches!(note.kind, NoteKind::Waiting(_))));
        let seen_waiting = self
            .session(&session)
            .is_some_and(|s| matches!(s.status, Status::Waiting(_)));
        self.prompts.insert(
            0,
            Prompt {
                id,
                session,
                cwd: hook.cwd,
                ask,
                seen_waiting,
                hidden: false,
            },
        );
        Effect::none().nudge(Nudge::Down)
    }

    fn sessions_changed(&mut self, live: Vec<sessions::Live>) -> Effect<Message> {
        let mut unasked = Vec::new();
        let mut old = std::mem::take(&mut self.sessions);
        for live in live {
            let previous = old
                .iter()
                .position(|s| s.id == live.id)
                .map(|at| old.swap_remove(at));
            let was_waiting = previous
                .as_ref()
                .is_some_and(|s| matches!(s.status, Status::Waiting(_)));
            if matches!(live.status, Status::Waiting(_)) && !was_waiting {
                unasked.push(live.id.clone());
            }
            let last_reply = previous.and_then(|s| s.last_reply);
            self.sessions.push(Session {
                id: live.id,
                pid: live.pid,
                cwd: live.cwd,
                name: live.name,
                title: live.title,
                status: live.status,
                socket: live.socket,
                last_reply,
                started: live.started,
                since: live.since,
            });
        }
        // Gone: their prompts can't be answered any more.
        for gone in old {
            self.settle_session(&gone.id);
            self.notes.retain(|note| note.session != gone.id);
            self.modes.remove(&gone.id);
            self.planning.remove(&gone.id);
            self.failed_turns.remove(&gone.id);
        }
        // A prompt whose session stopped waiting was answered in the terminal.
        let mut stale = Vec::new();
        for prompt in &mut self.prompts {
            let waiting = self
                .sessions
                .iter()
                .find(|s| s.id == prompt.session)
                .map(|s| matches!(s.status, Status::Waiting(_)));
            match waiting {
                Some(true) => prompt.seen_waiting = true,
                Some(false) if prompt.seen_waiting => stale.push(prompt.id),
                _ => {}
            }
        }
        for id in stale {
            hooks::withdraw(id);
            self.forget(id);
        }
        let sessions = &self.sessions;
        self.notes.retain(|note| {
            !matches!(note.kind, NoteKind::Waiting(_))
                || sessions
                    .iter()
                    .any(|s| s.id == note.session && matches!(s.status, Status::Waiting(_)))
        });
        if matches!(&self.talking, Some(id) if self.session(id).is_none()) {
            self.talking = None;
            self.chat = None;
        }
        Effect::task(Task::batch(unasked.into_iter().map(|id| {
            Task::future(async move {
                sleep(UNASKED).await;
                Message::Unasked(id)
            })
        })))
    }

    /// Answers prompt `id` with `decision`, and lets it go.
    fn decide(&mut self, id: u64, decision: Value) -> Effect<Message> {
        if !hooks::answer(id, hooks::decision(decision)) {
            eprintln!("eave-claude: prompt {id} was already answered, or Claude stopped waiting");
        }
        self.forget(id);
        Effect::none()
    }

    /// Approves plan `id`: Claude Code wants its input back for this tool, and goes on in the
    /// mode the config says, else the one the session was in before planning, as when you
    /// approve in the terminal.
    fn approve(&mut self, id: u64) -> Effect<Message> {
        let Some(prompt) = self.prompt(id) else {
            return Effect::none();
        };
        let Ask::Plan {
            input, suggestions, ..
        } = &prompt.ask
        else {
            return self.decide(id, json!({"behavior": "allow"}));
        };
        let suggested = suggestions
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|rule| {
                (rule["type"] == "setMode")
                    .then(|| rule["mode"].as_str())
                    .flatten()
                    .filter(|mode| *mode != "plan")
            });
        let mode = self
            .config
            .plan
            .as_deref()
            .or_else(|| self.modes.get(&prompt.session).map(String::as_str))
            .or(suggested)
            .unwrap_or("default");
        let decision = json!({
            "behavior": "allow",
            "updatedInput": input,
            "updatedPermissions": [{"type": "setMode", "mode": mode, "destination": "session"}],
        });
        // Out of plan mode once approved, before a hook says so.
        if let Some(session) = self.prompt(id).map(|prompt| prompt.session.clone()) {
            self.planning.remove(&session);
        }
        self.decide(id, decision)
    }

    fn pick(&mut self, id: u64, question: usize, option: usize) -> Effect<Message> {
        let Some(Ask::Question {
            questions,
            picks,
            own,
            ..
        }) = self
            .prompts
            .iter_mut()
            .find(|p| p.id == id)
            .map(|p| &mut p.ask)
        else {
            return Effect::none();
        };
        let Some(asked) = questions.get(question) else {
            return Effect::none();
        };
        if option >= asked.options.len() {
            return Effect::none();
        }
        own[question] = None;
        let picked = &mut picks[question];
        if asked.multiple {
            match picked.iter().position(|o| *o == option) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(option),
            }
            return Effect::none();
        }
        *picked = vec![option];
        self.answered_one(id, question)
    }

    /// Question `question` of prompt `id` has its answer: on to the next one without, or send
    /// them all once the last one has it.
    fn answered_one(&mut self, id: u64, question: usize) -> Effect<Message> {
        let Some(Ask::Question {
            questions,
            picks,
            own,
            at,
            ..
        }) = self
            .prompts
            .iter_mut()
            .find(|p| p.id == id)
            .map(|p| &mut p.ask)
        else {
            return Effect::none();
        };
        let unanswered = |q: &usize| picks[*q].is_empty() && own[*q].is_none();
        let last = questions.len() - 1;
        match (0..questions.len()).find(unanswered) {
            None if question == last => return self.submit(id),
            // The next one, or the first one left behind.
            _ if question < last => *at = question + 1,
            Some(first) => *at = first,
            None => {}
        }
        self.drop_draft(&Target::Prompt(id));
        Effect::none()
    }

    /// Sends the options picked, once every question has one.
    fn submit(&mut self, id: u64) -> Effect<Message> {
        let Some(Ask::Question {
            questions,
            input,
            picks,
            own,
            ..
        }) = self.prompt(id).map(|p| &p.ask)
        else {
            return Effect::none();
        };
        let answers: Option<serde_json::Map<_, _>> = questions
            .iter()
            .zip(picks.iter().zip(own))
            .map(|(question, (picked, own))| {
                let answer = match own {
                    Some(own) => own.clone(),
                    None if picked.is_empty() => return None,
                    None => {
                        let labels: Vec<_> = picked
                            .iter()
                            .map(|o| question.options[*o].0.as_str())
                            .collect();
                        labels.join(", ")
                    }
                };
                Some((question.text.clone(), Value::String(answer)))
            })
            .collect();
        let Some(answers) = answers else {
            return Effect::none();
        };
        let decision = answered(input, answers);
        self.decide(id, decision)
    }

    /// Sends `text`, typed in the popover or into a notification, where it goes.
    fn answer(&mut self, target: Target, text: String) -> Effect<Message> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Effect::none();
        }
        match target {
            Target::Prompt(id) => {
                // An answer of your own, to the question showing.
                if let Some(Ask::Question { picks, own, at, .. }) = self
                    .prompts
                    .iter_mut()
                    .find(|p| p.id == id)
                    .map(|p| &mut p.ask)
                {
                    let question = *at;
                    picks[question].clear();
                    own[question] = Some(text);
                    return self.answered_one(id, question);
                }
                let decision = match self.prompt(id).map(|p| &p.ask) {
                    // What you'd have Claude do instead, or what to change in the plan: Claude
                    // reads it and carries on.
                    Some(Ask::Permission { .. } | Ask::Plan { .. }) => {
                        json!({"behavior": "deny", "message": text})
                    }
                    Some(Ask::Question { .. }) | None => return Effect::none(),
                };
                self.decide(id, decision)
            }
            Target::Session(id) => {
                let Some(session) = self.session(&id) else {
                    return Effect::none();
                };
                let Some(socket) = session.socket.clone() else {
                    return Effect::none();
                };
                let pid = session.pid;
                self.notes.retain(|note| note.session != id);
                let sessions = sessions::config_dir().map(|dir| dir.join("sessions"));
                Effect::task(
                    Task::future(async move {
                        if let Err(error) = peer::send(pid, socket, sessions, text).await {
                            eprintln!("eave-claude: couldn't send the message: {error}");
                        }
                    })
                    .discard(),
                )
            }
        }
    }

    /// The prompt is gone: answered here or elsewhere, or no longer waited on.
    fn forget(&mut self, id: u64) {
        self.prompts.retain(|prompt| prompt.id != id);
        self.drop_draft(&Target::Prompt(id));
        if self.page == Page::Prompt(id) {
            self.page = Page::Sessions;
        }
    }

    /// Lets go of every prompt of a session that moved on.
    fn settle_session(&mut self, session: &str) {
        let ids: Vec<_> = self
            .prompts
            .iter()
            .filter(|prompt| prompt.session == session)
            .map(|prompt| prompt.id)
            .collect();
        for id in ids {
            hooks::withdraw(id);
            self.forget(id);
        }
    }

    fn drop_draft(&mut self, target: &Target) {
        if self.draft.as_ref().is_some_and(|(t, _)| t == target) {
            self.draft = None;
        }
    }

    fn note(&mut self, session: String, kind: NoteKind) {
        let key = self.next_note;
        self.next_note += 1;
        self.notes.insert(
            0,
            Note {
                key,
                session,
                kind,
                hidden: false,
            },
        );
    }

    /// What the mascot makes of `session`.
    fn facts(&self, session: &Session) -> Facts {
        Facts {
            asked: matches!(session.status, Status::Waiting(_))
                || self.prompts.iter().any(|p| p.session == session.id),
            busy: session.status == Status::Busy,
            plan: self.planning.contains(&session.id),
            failed: self.failed_turns.contains(&session.id),
            started: session.started,
            since: session.since,
        }
    }

    fn mood_of(&self, session: &Session) -> Mood {
        mood::mood(&self.facts(session), self.clock)
    }

    /// The most pressing mood of all: the bar's.
    fn mood(&self) -> Mood {
        let stray = self
            .prompts
            .iter()
            .any(|p| self.session(&p.session).is_none());
        let moods = self.sessions.iter().map(|s| self.mood_of(s));
        let mood = moods.max().unwrap_or(Mood::Coffee);
        if stray { Mood::Question } else { mood }
    }

    /// When time alone next changes a session's mood, in milliseconds since the epoch.
    fn next_change(&self) -> Option<u64> {
        let mut boundary = None;
        self.sessions
            .iter()
            .filter_map(|session| {
                let boundary = *boundary.get_or_insert_with(mood::next_boundary);
                mood::next_change(&self.facts(session), self.clock, boundary)
            })
            .min()
    }

    fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn session_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    fn prompt(&self, id: u64) -> Option<&Prompt> {
        self.prompts.iter().find(|p| p.id == id)
    }

    /// How many sessions wait for you, here or in the terminal.
    fn waiting(&self) -> usize {
        let mut waiting: Vec<&str> = self.prompts.iter().map(|p| p.session.as_str()).collect();
        waiting.extend(
            self.sessions
                .iter()
                .filter(|s| matches!(s.status, Status::Waiting(_)))
                .map(|s| s.id.as_str()),
        );
        waiting.sort_unstable();
        waiting.dedup();
        waiting.len()
    }

    /// What to call a session: its title, its name, or its directory.
    fn label(&self, session: &str, cwd: &str) -> String {
        match self.session(session) {
            Some(session) => session.label(),
            None => directory(cwd),
        }
    }

    fn draft(&self, target: &Target) -> &str {
        self.draft
            .as_ref()
            .filter(|(t, _)| t == target)
            .map_or("", |(_, text)| text.as_str())
    }

    /// The prompt a session waits on that the popover shows: the oldest.
    fn pending(&self, session: &str) -> Option<&Prompt> {
        self.prompts.iter().rev().find(|p| p.session == session)
    }
}

impl Session {
    fn label(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| directory(&self.cwd))
    }

    fn status_text(&self) -> String {
        match &self.status {
            Status::Busy => "working".into(),
            Status::Idle => "idle".into(),
            Status::Shell => "idle, a shell runs".into(),
            Status::Waiting(what) if what.is_empty() => "waiting for you".into(),
            Status::Waiting(what) => format!("waiting: {what}"),
        }
    }
}

// The notifications.
impl Claude {
    /// The notifications that should show, by what they're for.
    fn wanted(&self) -> Vec<(Key, notify::Note)> {
        let prompts = self
            .prompts
            .iter()
            .filter(|prompt| !prompt.hidden)
            .map(|prompt| (Key::Prompt(prompt.id), self.prompt_note(prompt)));
        let notes = self
            .notes
            .iter()
            .filter(|note| !note.hidden)
            .map(|note| (Key::Note(note.key), self.note_note(note)));
        prompts.chain(notes).collect()
    }

    /// Brings the notifications on the bus in line with [`wanted`](Self::wanted): new ones go up,
    /// ones no longer wanted are closed.
    fn post(&mut self) -> Task<Message> {
        let wanted = self.wanted();
        let mut tasks = Vec::new();
        self.posted.retain_mut(|posted| {
            if wanted.iter().any(|(key, _)| *key == posted.key) {
                return true;
            }
            match posted.id {
                Some(id) => {
                    tasks.push(close(id));
                    false
                }
                None => {
                    posted.gone = true;
                    true
                }
            }
        });
        for (key, note) in wanted {
            let mut replaces = 0;
            if let Some(posted) = self.posted.iter_mut().find(|posted| posted.key == key) {
                // Up and unchanged, or still on its way up.
                match posted.id {
                    Some(id) if posted.note != note => replaces = id,
                    _ => continue,
                }
                posted.note = note.clone();
            } else {
                self.posted.push(Posted {
                    key,
                    id: None,
                    gone: false,
                    note: note.clone(),
                });
            }
            tasks.push(Task::future(async move {
                match notify::send(note, replaces).await {
                    Ok(id) => Message::Posted(key, Some(id)),
                    Err(error) => {
                        eprintln!("eave-claude: can't show a notification: {error}");
                        Message::Posted(key, None)
                    }
                }
            }));
        }
        Task::batch(tasks)
    }

    fn posted(&mut self, key: Key, id: Option<u32>) -> Effect<Message> {
        let Some(at) = self.posted.iter().position(|posted| posted.key == key) else {
            return Effect::none();
        };
        match id {
            Some(id) if self.posted[at].gone => {
                self.posted.remove(at);
                Effect::task(close(id))
            }
            // Couldn't: left without an id, so it isn't tried again and again.
            id => {
                self.posted[at].id = id;
                Effect::none()
            }
        }
    }

    fn signalled(&mut self, signal: notify::Signal) -> Effect<Message> {
        let id = match &signal {
            notify::Signal::Action(id, _)
            | notify::Signal::Picked(id, _)
            | notify::Signal::Replied(id, _)
            | notify::Signal::Closed(id, _) => *id,
        };
        let Some(at) = self.posted.iter().position(|posted| posted.id == Some(id)) else {
            return Effect::none();
        };
        let key = self.posted[at].key;
        // One of several questions stays up on eave while it's answered: it moves on in place.
        let stepping = self.posted[at]
            .note
            .question
            .as_ref()
            .is_some_and(|question| question.step.1 > 1);
        match signal {
            notify::Signal::Action(_, action)
                if stepping && (action.starts_with("pick:") || action == notify::BACK) =>
            {
                self.acted(key, &action)
            }
            notify::Signal::Action(_, action) => {
                // The server takes it away after an action.
                self.posted.remove(at);
                let effect = self.acted(key, &action);
                self.hidden(key);
                effect
            }
            notify::Signal::Picked(_, keys) => {
                if !stepping {
                    self.posted.remove(at);
                }
                match key {
                    Key::Prompt(prompt) => self.picked(prompt, &keys),
                    Key::Note(_) => Effect::none(),
                }
            }
            notify::Signal::Replied(_, text) => {
                self.posted.remove(at);
                let target = match key {
                    Key::Prompt(prompt) => Some(Target::Prompt(prompt)),
                    Key::Note(_) => self.note_session(key).map(Target::Session),
                };
                self.hidden(key);
                match target {
                    Some(target) => self.answer(target, text),
                    None => Effect::none(),
                }
            }
            notify::Signal::Closed(..) => {
                self.posted.remove(at);
                self.hidden(key);
                Effect::none()
            }
        }
    }

    /// Its notification is gone: a prompt still waits in the popover, a note is done with.
    /// What the session whose chat shows asks or says needs no notification: you're reading it.
    /// It stays hidden after the chat closes, so it doesn't pop up then. Any notification already
    /// up for it goes away.
    fn quiet_chat(&mut self) {
        let (true, Some(id)) = (self.dialog_open, &self.talking) else {
            return;
        };
        for prompt in self.prompts.iter_mut().filter(|p| p.session == *id) {
            prompt.hidden = true;
        }
        for note in self.notes.iter_mut().filter(|n| n.session == *id) {
            note.hidden = true;
        }
    }

    fn hidden(&mut self, key: Key) {
        match key {
            Key::Prompt(id) => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == id) {
                    prompt.hidden = true;
                }
            }
            Key::Note(note) => self.notes.retain(|n| n.key != note),
        }
    }

    /// An action picked on the notification for `key`.
    fn acted(&mut self, key: Key, action: &str) -> Effect<Message> {
        match key {
            Key::Prompt(id) => match action {
                "allow" => self.handle(Message::Allow(id)),
                "always" => self.handle(Message::AlwaysAllow(id)),
                "deny" => self.handle(Message::Deny(id)),
                "approve" => self.approve(id),
                "default" | "answer" => self.show(Page::Prompt(id)),
                notify::BACK => match self.asking(id) {
                    Some(at) if at > 0 => self.handle(Message::Turn(id, at - 1)),
                    _ => Effect::none(),
                },
                pick => match pick.strip_prefix("pick:").and_then(|n| n.parse().ok()) {
                    Some(option) => {
                        let at = self.asking(id).unwrap_or(0);
                        self.pick(id, at, option)
                    }
                    None => Effect::none(),
                },
            },
            Key::Note(_) => {
                let Some(session) = self.note_session(key) else {
                    return Effect::none();
                };
                match action {
                    "show" => self.handle(Message::Focus(session)),
                    _ if self.session(&session).is_some() => self.show(Page::Chat(session)),
                    _ => Effect::none(),
                }
            }
        }
    }

    /// The question prompt `id` asks now, if it asks questions.
    fn asking(&self, id: u64) -> Option<usize> {
        match &self.prompt(id)?.ask {
            Ask::Question { at, .. } => Some(*at),
            _ => None,
        }
    }

    /// Several options sent from a notification, by key, for the question it asks now.
    fn picked(&mut self, id: u64, keys: &[String]) -> Effect<Message> {
        let options: Vec<usize> = keys
            .iter()
            .filter_map(|key| key.strip_prefix("pick:")?.parse().ok())
            .collect();
        let Some(Ask::Question {
            questions,
            picks,
            own,
            at,
            ..
        }) = self
            .prompts
            .iter_mut()
            .find(|p| p.id == id)
            .map(|p| &mut p.ask)
        else {
            return Effect::none();
        };
        let question = *at;
        let count = questions[question].options.len();
        let options: Vec<usize> = options.into_iter().filter(|o| *o < count).collect();
        if options.is_empty() {
            return Effect::none();
        }
        picks[question] = options;
        own[question] = None;
        self.answered_one(id, question)
    }

    /// The session a note is about.
    fn note_session(&self, key: Key) -> Option<String> {
        let Key::Note(note) = key else {
            return None;
        };
        self.notes
            .iter()
            .find(|n| n.key == note)
            .map(|n| n.session.clone())
    }

    fn prompt_note(&self, prompt: &Prompt) -> notify::Note {
        let label = self.label(&prompt.session, &prompt.cwd);
        let open = ("default".to_string(), "Open".to_string());
        let mut question = None;
        let (summary, body, mut actions, reply) = match &prompt.ask {
            Ask::Permission {
                tool,
                what,
                why,
                always,
            } => {
                let mut actions = vec![("allow".to_string(), "Allow".to_string())];
                if always.is_some() {
                    actions.push(("always".into(), "Always allow".into()));
                }
                actions.push(("deny".into(), "Deny".into()));
                // What it's for, when it says, reads better than the tool's name.
                let summary = match why {
                    Some(why) => format!("{}?", why.trim_end_matches(['.', '?'])),
                    None => format!("Allow {tool}?"),
                };
                (
                    summary,
                    code(what),
                    actions,
                    Some("Or tell Claude what to do instead"),
                )
            }
            Ask::Plan { plan, .. } => (
                "Plan ready".into(),
                plan.clone(),
                vec![("approve".into(), "Approve".into())],
                Some("What should change?"),
            ),
            Ask::Question { questions, at, .. } => {
                let asked = &questions[*at];
                question = Some(notify::Asking {
                    text: asked.text.clone(),
                    options: asked.options.clone(),
                    multiple: asked.multiple,
                    step: (*at as u32, questions.len() as u32),
                });
                let first = &questions[0];
                if questions.len() > 1 || !fits(first) {
                    let summary = match questions.len() {
                        1 => question_title(first),
                        n => format!("{n} questions"),
                    };
                    (
                        summary,
                        first.text.clone(),
                        vec![("answer".into(), "Answer…".into())],
                        None,
                    )
                } else {
                    let options = first
                        .options
                        .iter()
                        .enumerate()
                        .map(|(option, (choice, _))| (format!("pick:{option}"), choice.clone()))
                        .collect();
                    (
                        question_title(first),
                        first.text.clone(),
                        options,
                        Some("Your answer"),
                    )
                }
            }
        };
        actions.push(open);
        notify::Note {
            app: label,
            thread: prompt.session.clone(),
            urgency: 1,
            question,
            summary,
            body,
            actions,
            reply: reply.map(str::to_string),
            seconds: None,
            mood: Mood::Question,
        }
    }

    fn note_note(&self, note: &Note) -> notify::Note {
        let session = self.session(&note.session);
        let label = session.map_or_else(|| "Claude".into(), Session::label);
        let mut actions = vec![("default".to_string(), "Open".to_string())];
        let (summary, body, reply, seconds) = match &note.kind {
            NoteKind::Done(reply) => {
                let answerable = session.is_some_and(|s| s.socket.is_some());
                // Its first line says what came of it; the rest is the body.
                let (summary, body) = headline(reply);
                (
                    summary,
                    body,
                    answerable.then(|| "Message Claude".to_string()),
                    Some(self.config.timeout.max(1)),
                )
            }
            NoteKind::Failed(error) => ("Stopped with an error".into(), error.clone(), None, None),
            NoteKind::Waiting(what) => {
                actions.insert(0, ("show".into(), "Show".into()));
                (
                    "Waiting for you in the terminal".into(),
                    what.clone(),
                    None,
                    None,
                )
            }
        };
        // A turn that failed waits for you: it's news that can't wait.
        let urgency = if matches!(note.kind, NoteKind::Failed(_)) {
            2
        } else {
            1
        };
        let mood = match note.kind {
            NoteKind::Done(_) => Mood::Happy,
            NoteKind::Failed(_) | NoteKind::Waiting(_) => Mood::Question,
        };
        notify::Note {
            app: label,
            thread: note.session.clone(),
            urgency,
            question: None,
            summary,
            body,
            actions,
            reply,
            seconds,
            mood,
        }
    }
}

/// The longest a reply's first line is as a notification's summary; past it, it's cut at a word.
const HEADLINE: usize = 80;

/// A reply's first line, without Markdown's heading and emphasis marks, as a summary, and the rest
/// as the body. A first line too long for a summary leaves it all in the body, under "Done".
fn headline(reply: &str) -> (String, String) {
    let reply = reply.trim();
    let (first, rest) = reply.split_once('\n').unwrap_or((reply, ""));
    let first = first
        .trim()
        .trim_start_matches('#')
        .trim()
        .trim_matches('*')
        .trim();
    if first.is_empty() || first.chars().count() > HEADLINE || first.starts_with("```") {
        return ("Done".into(), reply.to_string());
    }
    (first.to_string(), rest.trim().to_string())
}

/// Closes the notification `id`.
fn close(id: u32) -> Task<Message> {
    Task::future(async move {
        if let Err(error) = notify::close(id).await {
            eprintln!("eave-claude: can't close a notification: {error}");
        }
    })
    .discard()
}

/// `what` a tool will do, as Markdown code: one line inline, more in a block.
fn code(what: &str) -> String {
    if what.is_empty() {
        return String::new();
    }
    let fence = if what.contains("```") { "~~~~" } else { "```" };
    if what.contains('\n') || what.contains('`') {
        format!("{fence}\n{what}\n{fence}")
    } else {
        format!("`{what}`")
    }
}

/// `what` a tool will do, as Markdown for the popover: a command as a shell block, to be colored.
fn command(tool: &str, what: &str) -> String {
    match what.strip_prefix("$ ") {
        Some(command) if tool == "Bash" && !command.contains("```") => {
            format!("```sh\n{command}\n```")
        }
        _ => code(what),
    }
}

// The popover.
impl Claude {
    /// Every session, with what it's doing: a click opens its chat.
    fn sessions_page(&self) -> ui::Node<Message> {
        let mut rows = vec![ui::header("Claude Code")];
        if let Some(error) = &self.failed {
            rows.push(ui::label(format!("Can't answer prompts: {error}")).tone(Tone::Warning));
        }
        if self.sessions.is_empty() {
            rows.push(ui::label("No sessions running").tone(Tone::Dim));
        } else {
            rows.push(ui::list(self.sessions.iter().map(|session| {
                let asked = self.prompts.iter().any(|p| p.session == session.id);
                // While the popover is open, every session's mascot acts out its mood.
                let sprite = self.mood_of(session).sprite();
                let row = ui::list_item(session.label())
                    .leading(ui::sprite(sprite.frames, sprite.order).looping())
                    .subtitle(format!(
                        "{} · {}",
                        directory(&session.cwd),
                        session.status_text()
                    ))
                    .on_press(Message::Show(Page::Chat(session.id.clone())));
                if asked || matches!(session.status, Status::Waiting(_)) {
                    row.trailing(ui::badge("needs you").tone(Tone::Warning))
                } else {
                    row
                }
            })));
        }
        let strays: Vec<_> = self
            .prompts
            .iter()
            .filter(|p| self.session(&p.session).is_none())
            .collect();
        if !strays.is_empty() {
            rows.push(ui::section("Other prompts"));
            rows.push(ui::list(strays.into_iter().map(|p| {
                let (title, subtitle) = match &p.ask {
                    Ask::Permission { tool, what, .. } => (format!("Allow {tool}?"), what.clone()),
                    Ask::Plan { plan, .. } => ("Plan ready".into(), plan.clone()),
                    Ask::Question { questions, .. } => {
                        (question_title(&questions[0]), questions[0].text.clone())
                    }
                };
                ui::list_item(title)
                    .subtitle(subtitle.lines().next().unwrap_or_default())
                    .on_press(Message::Show(Page::Prompt(p.id)))
            })));
        }
        ui::column(rows)
    }

    /// A session's conversation, what it's doing now, what it asks, and a field to answer.
    fn chat_page(&self, session: &Session) -> ui::Node<Message> {
        // The session's mascot in its mood, as in the list: it plays when the chat opens or the
        // mood changes, and keeps going only while the session asks you something.
        let mood = self.mood_of(session);
        let sprite = mood.sprite();
        let mascot = ui::sprite(sprite.frames, sprite.order);
        let mascot = if mood == Mood::Question {
            mascot.looping()
        } else {
            mascot
        };
        let header = ui::header(session.label())
            .leading(mascot)
            .subtitle(home(&session.cwd))
            .trailing(ui::button(
                ui::label("Show"),
                Message::Focus(session.id.clone()),
            ));
        let chat = self
            .chat
            .as_ref()
            .filter(|(id, _)| *id == session.id)
            .map(|(_, chat)| chat);
        let pending = self.pending(&session.id);
        let waiting = matches!(pending.map(|p| &p.ask), Some(Ask::Permission { .. }));
        let mut conversation = match chat {
            Some(chat) => conversation(&chat.entries, waiting, self.mood_of(session)),
            None => vec![ui::label("…").tone(Tone::Dim)],
        };
        // A plan to approve is read with the rest.
        if let Some(Ask::Plan { plan, .. }) = pending.map(|p| &p.ask) {
            conversation.push(ui::section("Plan"));
            conversation.push(ui::markdown(plan));
        }
        let mut rows = vec![
            header,
            ui::scroll(ui::column(conversation)).bottom(),
            activity(session, chat),
        ];
        let target = match pending {
            Some(prompt) => {
                rows.push(ui::separator());
                rows.extend(self.prompt_controls(prompt, false));
                Some(Target::Prompt(prompt.id))
            }
            None => session
                .socket
                .is_some()
                .then(|| Target::Session(session.id.clone())),
        };
        if let Some(target) = target {
            let placeholder = match pending.map(|p| &p.ask) {
                Some(Ask::Permission { .. }) => "Or tell Claude what to do instead",
                Some(Ask::Plan { .. }) => "What should change?",
                Some(Ask::Question { .. }) => "Or answer in your own words",
                None => "Message Claude",
            };
            rows.push(self.composer(target, placeholder));
        }
        ui::column(rows)
    }

    /// A prompt from a session not in the list.
    fn prompt_page(&self, prompt: &Prompt) -> ui::Node<Message> {
        let mut rows =
            vec![ui::header(self.label(&prompt.session, &prompt.cwd)).on_back(Message::Back)];
        if let Ask::Plan { plan, .. } = &prompt.ask {
            rows.push(ui::scroll(ui::markdown(plan)));
        }
        rows.extend(self.prompt_controls(prompt, true));
        let placeholder = match &prompt.ask {
            Ask::Permission { .. } => "Or tell Claude what to do instead",
            Ask::Plan { .. } => "What should change?",
            Ask::Question { .. } => "Or answer in your own words",
        };
        rows.push(self.composer(Target::Prompt(prompt.id), placeholder));
        ui::column(rows)
    }

    fn composer(&self, target: Target, placeholder: &str) -> ui::Node<Message> {
        let draft = self.draft(&target).to_string();
        ui::text_area(placeholder, draft, move |text| {
            Message::Draft(target.clone(), text)
        })
        .on_submit(Message::SendReply)
        .trailing(ui::button(ui::icon(ui::Icon::Forward), Message::SendReply))
    }

    /// What a prompt asks, and its buttons.
    fn prompt_controls(&self, prompt: &Prompt, alone: bool) -> Vec<ui::Node<Message>> {
        let id = prompt.id;
        let mut rows = Vec::new();
        match &prompt.ask {
            Ask::Permission {
                tool, what, always, ..
            } => {
                rows.push(ui::section(format!("Allow {tool}?")));
                if !what.is_empty() {
                    rows.push(ui::markdown(command(tool, what)));
                }
                rows.push(ui::grid(
                    2,
                    [
                        ui::button(ui::label("Deny"), Message::Deny(id)),
                        ui::button(ui::label("Allow"), Message::Allow(id)).prominent(),
                    ],
                ));
                if always.is_some() {
                    rows.push(ui::button(
                        ui::label("Allow, and don't ask again"),
                        Message::AlwaysAllow(id),
                    ));
                }
            }
            Ask::Plan { .. } => {
                if alone {
                    rows.push(ui::section("Plan ready"));
                }
                rows.push(ui::button(ui::label("Approve"), Message::Approve(id)).prominent());
            }
            Ask::Question {
                questions,
                picks,
                own,
                at,
                ..
            } => {
                // One at a time, with arrows between them: four lists don't fit.
                let index = *at;
                let question = &questions[index];
                let count = questions.len();
                let mut section = ui::section(question_title(question));
                if count > 1 {
                    let mut steps = Vec::new();
                    if index > 0 {
                        steps.push(ui::button(
                            ui::icon(ui::Icon::Back),
                            Message::Turn(id, index - 1),
                        ));
                    }
                    steps.push(ui::label(format!("{} of {count}", index + 1)).tone(Tone::Dim));
                    if index + 1 < count {
                        steps.push(ui::button(
                            ui::icon(ui::Icon::Forward),
                            Message::Turn(id, index + 1),
                        ));
                    }
                    section = section.trailing(ui::row(steps));
                }
                rows.push(section);
                rows.push(ui::markdown(&question.text));
                rows.push(ui::list(question.options.iter().enumerate().map(
                    |(option, (choice, description))| {
                        let row = ui::list_item(choice)
                            .selected(picks[index].contains(&option))
                            .on_press(Message::Pick(id, index, option));
                        if description.is_empty() {
                            row
                        } else {
                            row.subtitle(description)
                        }
                    },
                )));
                if let Some(own) = &own[index] {
                    rows.push(ui::label(format!("Your answer: {own}")).tone(Tone::Accent));
                }
                let all = (0..count).all(|q| !picks[q].is_empty() || own[q].is_some());
                if (count > 1 || question.multiple) && (all || index + 1 == count) {
                    rows.push(ui::button(ui::label("Send"), Message::Submit(id)).prominent());
                }
            }
        }
        rows
    }
}

/// The conversation, a turn at a time: when you spoke, then a timeline of what you said and what
/// Claude said and did, each step marked with how it went. The latest turn's first step from
/// Claude carries the mascot, in `mood`. While a permission is `waiting`, the tool it's for is
/// marked as waiting for you.
fn conversation(entries: &[Entry], waiting: bool, mood: Mood) -> Vec<ui::Node<Message>> {
    // A turn starts where you speak after Claude, or at the start of what was read.
    let mut turns: Vec<Vec<&Entry>> = Vec::new();
    for entry in entries {
        let yours = matches!(entry, Entry::You { .. });
        let after_claude = turns
            .last()
            .and_then(|turn| turn.last())
            .is_some_and(|last| !matches!(last, Entry::You { .. }));
        match turns.last_mut() {
            Some(turn) if !(yours && after_claude) => turn.push(entry),
            _ => turns.push(vec![entry]),
        }
    }
    // Only the call asked about waits: the last one under way.
    let held = entries
        .iter()
        .rfind(|entry| matches!(entry, Entry::Tool { done: false, .. }))
        .filter(|_| waiting);
    let latest = turns.len().saturating_sub(1);
    let today = chrono::Local::now().date_naive();
    let mut nodes = Vec::new();
    for (index, turn) in turns.into_iter().enumerate() {
        let at = turn.iter().find_map(|entry| match entry {
            Entry::You { at, .. } => *at,
            _ => None,
        });
        match at {
            Some(at) => nodes.push(ui::divider(when(at, today))),
            None if index > 0 => nodes.push(ui::separator()),
            None => {}
        }
        let mut mascot = index == latest;
        let steps = turn.into_iter().map(|entry| {
            let step = match entry {
                Entry::You { text, .. } => {
                    return ui::step(ui::Mark::You, ui::markdown(text).tone(Tone::Accent));
                }
                Entry::Claude(text) => ui::step(ui::Mark::Note, ui::markdown(text)),
                Entry::Tool {
                    name,
                    what,
                    done,
                    failed,
                    result,
                    diff,
                    ..
                } => {
                    let mark = match (done, failed) {
                        (_, true) => ui::Mark::Failed,
                        (true, false) => ui::Mark::Done,
                        (false, false) if held.is_some_and(|held| std::ptr::eq(held, entry)) => {
                            ui::Mark::Waiting
                        }
                        (false, false) => ui::Mark::Running,
                    };
                    ui::step(mark, tool(name, what, *failed, result, diff.as_deref()))
                }
                Entry::Aside(text) => ui::step(ui::Mark::Note, ui::label(text).tone(Tone::Dim)),
            };
            if std::mem::take(&mut mascot) {
                let sprite = mood.sprite();
                step.leading(ui::sprite(sprite.frames, sprite.order))
            } else {
                step
            }
        });
        nodes.push(ui::timeline(steps.collect::<Vec<_>>()));
    }
    if nodes.is_empty() {
        nodes.push(ui::label("Nothing said yet").tone(Tone::Dim));
    }
    nodes
}

/// A tool call in a line: its name, what it was for, and what came of it. An error goes under it,
/// and so does the change it made to a file.
fn tool(
    name: &str,
    what: &str,
    failed: bool,
    result: &str,
    diff: Option<&[(ui::Change, String)]>,
) -> ui::Node<Message> {
    let target = first_line(what);
    let target = target.strip_prefix("$ ").unwrap_or(target);
    let mut line = vec![ui::label(name)];
    if target.is_empty() {
        line.push(ui::fill());
    } else {
        line.push(ui::literal(target).tone(Tone::Dim));
    }
    if !failed && !result.is_empty() {
        line.push(ui::label(result).tone(Tone::Dim));
    }
    let mut rows = vec![ui::row(line)];
    if failed && !result.is_empty() {
        rows.push(ui::label(result).tone(Tone::Critical));
    }
    if let Some(diff) = diff.filter(|diff| !diff.is_empty()) {
        rows.push(ui::diff(target, diff.to_vec()));
    }
    match rows.len() {
        1 => rows.remove(0),
        _ => ui::column(rows),
    }
}

/// When a turn started: the time today, with the day this week, and the date before that.
fn when(at: chrono::DateTime<chrono::Local>, today: chrono::NaiveDate) -> String {
    let days = (today - at.date_naive()).num_days();
    let format = match days {
        0 => "%H:%M",
        1..=6 => "%a %H:%M",
        _ => "%-d %b %H:%M",
    };
    at.format(format).to_string()
}

/// What the session is doing now, in a line.
fn activity(session: &Session, chat: Option<&Chat>) -> ui::Node<Message> {
    let running = chat.and_then(Chat::running);
    let mut text = match (&session.status, running) {
        (Status::Busy, Some((name, what))) if !what.is_empty() => {
            format!("Working · {name}: {}", first_line(what))
        }
        (Status::Busy, Some((name, _))) => format!("Working · {name}"),
        (Status::Busy, None) => "Working…".into(),
        (Status::Waiting(_), _) => "Waiting for you".into(),
        (Status::Shell, _) => "Idle, a shell runs".into(),
        (Status::Idle, _) => "Idle".into(),
    };
    if chat.and_then(|chat| chat.mode.as_deref()) == Some("plan") {
        text.push_str(" · plan mode");
    }
    let tone = match session.status {
        Status::Busy => Tone::Accent,
        Status::Waiting(_) => Tone::Warning,
        _ => Tone::Dim,
    };
    ui::label(text).tone(tone)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// What a permission prompt, a question or a plan asks, from its hook.
fn ask(hook: &hooks::Hook) -> Ask {
    let input = &hook.tool_input;
    match hook.tool_name.as_str() {
        "AskUserQuestion" => {
            let questions: Vec<Question> = input["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(question)
                .collect();
            if !questions.is_empty() {
                let count = questions.len();
                return Ask::Question {
                    questions,
                    input: input.clone(),
                    picks: vec![Vec::new(); count],
                    own: vec![None; count],
                    at: 0,
                };
            }
        }
        "ExitPlanMode" => {
            return Ask::Plan {
                plan: text(&input["plan"]).trim().to_string(),
                input: input.clone(),
                suggestions: hook.permission_suggestions.clone(),
            };
        }
        _ => {}
    }
    let always = match &hook.permission_suggestions {
        Value::Array(rules) if !rules.is_empty() => Some(hook.permission_suggestions.clone()),
        _ => None,
    };
    let why = match hook.tool_name.as_str() {
        "Bash" => Some(text(&input["description"])).filter(|why| !why.is_empty()),
        _ => None,
    };
    Ask::Permission {
        tool: tool_name(&hook.tool_name),
        what: describe(&hook.tool_name, input, &hook.cwd),
        why,
        always,
    }
}

fn question(value: &Value) -> Option<Question> {
    let options: Vec<_> = value["options"]
        .as_array()?
        .iter()
        .map(|option| (text(&option["label"]), text(&option["description"])))
        .filter(|(label, _)| !label.is_empty())
        .collect();
    let question = Question {
        text: text(&value["question"]),
        header: text(&value["header"]),
        multiple: value["multiSelect"].as_bool().unwrap_or(false),
        options,
    };
    (!question.text.is_empty() && !question.options.is_empty()).then_some(question)
}

fn question_title(question: &Question) -> String {
    if question.header.is_empty() {
        "Claude asks".into()
    } else {
        question.header.clone()
    }
}

/// Whether a question's options fit as a notice's buttons, beside "Other…".
fn fits(question: &Question) -> bool {
    let length: usize = question
        .options
        .iter()
        .map(|(l, _)| l.chars().count())
        .sum();
    !question.multiple && question.options.len() <= 2 && length <= 24
}

/// The decision that answers an AskUserQuestion: its input, with `answers` from each question's
/// text to the labels picked, or the answer typed.
fn answered(input: &Value, answers: serde_json::Map<String, Value>) -> Value {
    let mut input = input.clone();
    if let Value::Object(fields) = &mut input {
        fields.insert("answers".into(), Value::Object(answers));
    }
    json!({"behavior": "allow", "updatedInput": input})
}

/// `mcp__server__tool` as `server: tool`; others as they are.
pub(crate) fn tool_name(tool: &str) -> String {
    match tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, tool)) => format!("{server}: {tool}"),
        None => tool.into(),
    }
}

/// What a tool is about to do, in a line or two.
pub(crate) fn describe(tool: &str, input: &Value, cwd: &str) -> String {
    let path = |key: &str| relative(&text(&input[key]), cwd);
    let described = match tool {
        "Bash" => format!("$ {}", text(&input["command"])),
        "Write" | "Edit" | "MultiEdit" | "Read" => path("file_path"),
        "NotebookEdit" => path("notebook_path"),
        "WebFetch" => text(&input["url"]),
        "WebSearch" => text(&input["query"]),
        "Skill" => text(&input["skill"]),
        "Agent" | "Task" => text(&input["description"]),
        _ => String::new(),
    };
    if !described.trim().is_empty() {
        return shorten(&described, 400);
    }
    match input {
        Value::Object(fields) if fields.is_empty() => String::new(),
        Value::Null => String::new(),
        input => shorten(&input.to_string(), 400),
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// `path`, from `cwd` when it's inside it.
fn relative(path: &str, cwd: &str) -> String {
    path.strip_prefix(cwd)
        .and_then(|rest| rest.strip_prefix('/'))
        .map_or_else(|| home(path), str::to_string)
}

/// `path` with your home as `~`.
fn home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => path.into(),
        },
        _ => path.into(),
    }
}

/// The last part of a path: the project a session works in.
fn directory(cwd: &str) -> String {
    let name = cwd
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if name.is_empty() {
        home(cwd)
    } else {
        name.into()
    }
}

pub(crate) fn shorten(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.into(),
    }
}

/// Focuses the window of the terminal session `pid` runs in. A terminal that runs every window
/// in one process (kitty, foot's server) has several: the one showing the session's title wins.
async fn focus(pid: u32, title: Option<String>) -> io::Result<()> {
    #[derive(Deserialize)]
    struct Client {
        address: String,
        pid: i64,
        title: String,
    }
    let clients: Vec<Client> = eave_hyprland::query("clients").await?;
    let ancestors = sessions::ancestors(pid);
    let theirs: Vec<_> = clients
        .iter()
        .filter(|c| u32::try_from(c.pid).is_ok_and(|pid| ancestors.contains(&pid)))
        .collect();
    let titled = title.and_then(|title| theirs.iter().find(|c| c.title.contains(&title)));
    let Some(client) = titled.or(theirs.first()) else {
        return Err(io::Error::other("its terminal isn't a window here"));
    };
    let address = &client.address;
    eave_hyprland::dispatch(
        &format!("hl.dsp.focus({{ window = \"address:{address}\" }})"),
        &format!("focuswindow address:{address}"),
    )
    .await
}

#[cfg(test)]
mod tests;
