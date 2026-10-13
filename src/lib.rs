//! Claude Code, in the bar. The item shows when sessions run, and counts the ones waiting for
//! you. A permission prompt, a question or a plan to approve becomes a notification you can
//! answer, and a session that finishes its turn says so and takes a reply. The popover lists the
//! sessions, and each one's page has its prompts, its last reply and a field to message it.
//!
//! Three sources, each for what only it knows:
//! - `~/.claude/sessions/` ([`sessions`]): which sessions run and whether they wait for you.
//! - The eave plugin for Claude Code ([`hooks`]): what they ask, and the answers.
//! - Each session's messaging socket ([`peer`]): what you type to them.

mod hooks;
mod peer;
mod sessions;
mod title;

use std::io;
use std::path::PathBuf;

use eave_api::time::{Duration, sleep};
use eave_api::ui::{self, Item, Notice, Panel, Reply, Tone};
use eave_api::{Effect, Nudge, Registry, Subscription, Task, Widget};
use serde::Deserialize;
use serde_json::{Value, json};

use sessions::Status;

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            done: true,
            timeout: 8,
        }
    }
}

/// How long a session waits in the terminal before the bar says so, when no hook asked: a hook's
/// prompt usually arrives with the dialog, and would say it better.
const UNASKED: Duration = Duration::from_millis(1500);
/// Notices that aren't prompts take keys from here up, so they never meet a prompt's id.
const NOTE_KEYS: u64 = 1 << 48;

pub struct Claude {
    config: Config,
    /// Why prompts can't be answered here, if they can't.
    failed: Option<String>,
    /// Newest first.
    sessions: Vec<Session>,
    /// Waiting for an answer, newest first.
    prompts: Vec<Prompt>,
    /// Notices that aren't prompts, newest first.
    notes: Vec<Note>,
    next_note: u64,
    page: Page,
    /// What's being typed, and where it goes.
    draft: Option<(Target, String)>,
    /// Whether the popover shows.
    open: bool,
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
}

struct Prompt {
    /// The hook server's id for it, and its notice's key.
    id: u64,
    session: String,
    cwd: String,
    ask: Ask,
    /// The session file said it waits, so it no longer waiting means this was answered elsewhere.
    seen_waiting: bool,
    /// Its notice was closed. It still waits, in the popover.
    hidden: bool,
}

enum Ask {
    /// May it use a tool.
    Permission {
        tool: String,
        what: String,
        /// The rules the terminal offers as "don't ask again".
        always: Option<Value>,
    },
    /// AskUserQuestion: up to four questions, each with options, and what's picked so far.
    Question {
        questions: Vec<Question>,
        input: Value,
        picks: Vec<Vec<usize>>,
    },
    /// ExitPlanMode: a plan to approve.
    Plan { plan: String },
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
}

enum NoteKind {
    /// Its turn ended, with this reply.
    Done(String),
    Failed(String),
    /// A dialog waits in the terminal that no hook asked about: what it waits for.
    Waiting(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Page {
    Sessions,
    Session(String),
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
    Allow(u64),
    AlwaysAllow(u64),
    Deny(u64),
    /// An option of a question: prompt, question, option.
    Pick(u64, usize, usize),
    /// Sends the options picked.
    Submit(u64),
    StartReply(Target),
    Draft(Target, String),
    SendReply,
    CancelReply,
    /// Closes a prompt's notice; it still waits in the popover.
    Hide(u64),
    Expired(u64),
    Dismiss(u64),
    /// Goes to a page, opening the popover if it's closed.
    Show(Page),
    Back,
    /// Focuses the terminal a session runs in.
    Focus(String),
    /// A session has waited a moment: say so if no hook asked.
    Unasked(String),
    Opened,
    Closed,
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
            next_note: NOTE_KEYS,
            page: Page::Sessions,
            draft: None,
            open: false,
        };
        (claude, Task::none())
    }

    fn update(&mut self, message: Message) -> Effect<Message> {
        match message {
            Message::Hooks(event) => self.heard(event),
            Message::Sessions(live) => self.sessions_changed(live),
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
            Message::Pick(id, question, option) => self.pick(id, question, option),
            Message::Submit(id) => self.submit(id),
            Message::StartReply(target) => {
                self.draft = Some((target, String::new()));
                Effect::none()
            }
            Message::Draft(target, text) => {
                self.draft = Some((target, text));
                Effect::none()
            }
            Message::SendReply => self.send_reply(),
            Message::CancelReply => {
                self.draft = None;
                Effect::none()
            }
            Message::Hide(id) => {
                if let Some(prompt) = self.prompts.iter_mut().find(|p| p.id == id) {
                    prompt.hidden = true;
                }
                self.drop_draft(&Target::Prompt(id));
                Effect::none()
            }
            Message::Expired(key) | Message::Dismiss(key) => {
                if let Some(note) = self.notes.iter().find(|note| note.key == key) {
                    let session = Target::Session(note.session.clone());
                    self.drop_draft(&session);
                }
                self.notes.retain(|note| note.key != key);
                Effect::none()
            }
            Message::Show(page) => {
                self.page = page;
                // Asking the host to open an open popover closes it first, and closing resets
                // the page: within the popover, just change pages.
                if self.open {
                    Effect::none()
                } else {
                    Effect::none().open_popover()
                }
            }
            Message::Back => {
                self.page = match &self.page {
                    Page::Prompt(id) => self
                        .prompt(*id)
                        .map(|prompt| prompt.session.clone())
                        .filter(|session| self.session(session).is_some())
                        .map_or(Page::Sessions, Page::Session),
                    _ => Page::Sessions,
                };
                self.draft = None;
                Effect::none()
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
            Message::Closed => {
                self.open = false;
                self.page = Page::Sessions;
                self.draft = None;
                Effect::none()
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run(hooks::watch).map(Message::Hooks),
            sessions::watch().map(Message::Sessions),
        ])
    }

    fn item(&self) -> Item<Message> {
        if self.sessions.is_empty() && self.prompts.is_empty() {
            return Item::hidden();
        }
        let waiting = self.waiting();
        let busy = self.sessions.iter().any(|s| s.status == Status::Busy);
        // An icon called `claude` in your icon theme, if you have one; else a C. Dimmed while
        // nothing works or waits. Not animated while busy: sessions are busy for most of the
        // day, and an icon that keeps moving keeps the bar drawing (about 10% CPU).
        let logo = ui::app_icon("claude");
        let logo = if busy || waiting > 0 {
            logo
        } else {
            logo.tone(Tone::Dim)
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
        let page = match &self.page {
            Page::Session(id) => self.session(id).map(|session| self.session_page(session)),
            Page::Prompt(id) => self.prompt(*id).map(|prompt| self.prompt_page(prompt)),
            Page::Sessions => None,
        };
        let content = page.unwrap_or_else(|| self.sessions_page());
        Some(
            Panel::new(content)
                .on_open(Message::Opened)
                .on_close(Message::Closed),
        )
    }

    fn notices(&self) -> Vec<Notice<Message>> {
        let prompts = self
            .prompts
            .iter()
            .filter(|prompt| !prompt.hidden)
            .map(|prompt| self.prompt_notice(prompt));
        let notes = self.notes.iter().map(|note| self.note_notice(note));
        prompts.chain(notes).collect()
    }
}

impl Claude {
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
        let id = hook.session_id;
        match hook.hook_event_name.as_str() {
            "UserPromptSubmit" => {
                self.settle_session(&id);
                self.notes.retain(|note| note.session != id);
                if let (Some(session), Some(title)) = (self.session_mut(&id), hook.session_title) {
                    session.title = Some(title);
                }
            }
            "Stop" if hook.agent_id.is_none() => {
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

    fn asked(&mut self, id: u64, hook: hooks::Hook) -> Effect<Message> {
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
            });
        }
        // Gone: their prompts can't be answered any more.
        for gone in old {
            self.settle_session(&gone.id);
            self.notes.retain(|note| note.session != gone.id);
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
        if matches!(&self.page, Page::Session(id) if self.session(id).is_none()) {
            self.page = Page::Sessions;
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
            eprintln!("eave-claude: that prompt was already answered");
        }
        self.forget(id);
        Effect::none()
    }

    fn pick(&mut self, id: u64, question: usize, option: usize) -> Effect<Message> {
        let Some(Ask::Question {
            questions, picks, ..
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
        let picked = &mut picks[question];
        if asked.multiple {
            match picked.iter().position(|o| *o == option) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(option),
            }
        } else {
            *picked = vec![option];
        }
        // One question, one answer: nothing else to wait for.
        if questions.len() == 1 && !asked.multiple {
            return self.submit(id);
        }
        Effect::none()
    }

    /// Sends the options picked, once every question has one.
    fn submit(&mut self, id: u64) -> Effect<Message> {
        let Some(Ask::Question {
            questions,
            input,
            picks,
        }) = self.prompt(id).map(|p| &p.ask)
        else {
            return Effect::none();
        };
        if picks.iter().any(Vec::is_empty) {
            return Effect::none();
        }
        let answers = questions
            .iter()
            .zip(picks)
            .map(|(question, picked)| {
                let labels: Vec<_> = picked
                    .iter()
                    .map(|o| question.options[*o].0.as_str())
                    .collect();
                (question.text.clone(), Value::String(labels.join(", ")))
            })
            .collect();
        let decision = answered(input, answers);
        self.decide(id, decision)
    }

    fn send_reply(&mut self) -> Effect<Message> {
        let Some((target, text)) = self.draft.take() else {
            return Effect::none();
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            self.draft = Some((target, text));
            return Effect::none();
        }
        match target {
            Target::Prompt(id) => {
                let decision = match self.prompt(id).map(|p| &p.ask) {
                    // What you'd have Claude do instead, or what to change in the plan: Claude
                    // reads it and carries on.
                    Some(Ask::Permission { .. } | Ask::Plan { .. }) => {
                        json!({"behavior": "deny", "message": text})
                    }
                    Some(Ask::Question {
                        questions, input, ..
                    }) => {
                        let answers = questions
                            .iter()
                            .map(|q| (q.text.clone(), Value::String(text.clone())));
                        answered(input, answers.collect())
                    }
                    None => return Effect::none(),
                };
                let effect = self.decide(id, decision);
                if self.page == Page::Prompt(id) {
                    return self.update(Message::Back);
                }
                effect
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
        self.notes.insert(0, Note { key, session, kind });
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

    fn draft(&self, target: &Target) -> Option<&str> {
        self.draft
            .as_ref()
            .filter(|(t, _)| t == target)
            .map(|(_, text)| text.as_str())
    }

    /// A field on a notice, if it's being typed into.
    fn reply_field(&self, target: Target, placeholder: &str) -> Option<Reply<Message>> {
        let draft = self.draft(&target)?.to_string();
        Some(
            Reply::new(placeholder, draft, move |text| {
                Message::Draft(target.clone(), text)
            })
            .on_submit(Message::SendReply)
            .on_cancel(Message::CancelReply),
        )
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

// The notices.
impl Claude {
    fn prompt_notice(&self, prompt: &Prompt) -> Notice<Message> {
        let id = prompt.id;
        let label = self.label(&prompt.session, &prompt.cwd);
        let target = Target::Prompt(id);
        let notice = |summary: &str| {
            Notice::new(id, summary)
                .app(&label)
                .app_icon("claude")
                .on_press(Message::Show(Page::Prompt(id)))
                .on_dismiss(Message::Hide(id))
        };
        match &prompt.ask {
            Ask::Permission { tool, what, .. } => {
                let notice = notice(&format!("Allow {tool}?"))
                    .body(what)
                    .action("Allow", Message::Allow(id))
                    .action("Deny", Message::Deny(id));
                match self.reply_field(target.clone(), "Or tell Claude what to do instead") {
                    Some(field) => notice.reply(field),
                    None => notice.action("Instead…", Message::StartReply(target)),
                }
            }
            Ask::Plan { plan } => {
                let notice = notice("Plan ready")
                    .body(plan)
                    .action("Approve", Message::Allow(id));
                match self.reply_field(target.clone(), "What should change?") {
                    Some(field) => notice.reply(field),
                    None => notice.action("Change…", Message::StartReply(target)),
                }
            }
            Ask::Question { questions, .. } => {
                let first = &questions[0];
                if questions.len() > 1 || !fits(first) {
                    let summary = match questions.len() {
                        1 => question_title(first),
                        n => format!("{n} questions"),
                    };
                    return notice(&summary)
                        .body(&first.text)
                        .action("Answer…", Message::Show(Page::Prompt(id)));
                }
                let mut notice = notice(&question_title(first)).body(&first.text);
                for (option, (choice, _)) in first.options.iter().enumerate() {
                    notice = notice.action(choice, Message::Pick(id, 0, option));
                }
                match self.reply_field(target.clone(), "Your answer") {
                    Some(field) => notice.reply(field),
                    None => notice.action("Other…", Message::StartReply(target)),
                }
            }
        }
    }

    fn note_notice(&self, note: &Note) -> Notice<Message> {
        let key = note.key;
        let session = self.session(&note.session);
        let label = session.map_or_else(|| "Claude".into(), Session::label);
        let notice = |summary: &str| {
            Notice::new(key, summary)
                .app(&label)
                .app_icon("claude")
                .on_press(Message::Focus(note.session.clone()))
                .on_dismiss(Message::Dismiss(key))
        };
        match &note.kind {
            NoteKind::Done(reply) => {
                let notice = notice("Done");
                let notice = if reply.is_empty() {
                    notice
                } else {
                    notice.body(reply)
                };
                let target = Target::Session(note.session.clone());
                if session.is_none_or(|s| s.socket.is_none()) {
                    return notice.expires(self.timeout(), Message::Expired(key));
                }
                match self.reply_field(target.clone(), "Message Claude") {
                    Some(field) => notice.reply(field),
                    None => notice
                        .action("Reply", Message::StartReply(target))
                        .expires(self.timeout(), Message::Expired(key)),
                }
            }
            NoteKind::Failed(error) => {
                let notice = notice("Stopped with an error");
                if error.is_empty() {
                    notice
                } else {
                    notice.body(error)
                }
            }
            NoteKind::Waiting(what) => notice("Waiting for you in the terminal")
                .body(what)
                .action("Show", Message::Focus(note.session.clone())),
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.timeout.max(1))
    }
}

// The popover.
impl Claude {
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
                let row = ui::list_item(session.label())
                    .subtitle(format!(
                        "{} · {}",
                        directory(&session.cwd),
                        session.status_text()
                    ))
                    .on_press(Message::Show(Page::Session(session.id.clone())));
                if asked || matches!(session.status, Status::Waiting(_)) {
                    row.trailing(ui::badge("needs you").tone(Tone::Warning))
                } else {
                    row
                }
            })));
        }
        // Prompts from sessions not in the list, like ones run with `-p`.
        let strays: Vec<_> = self
            .prompts
            .iter()
            .filter(|p| self.session(&p.session).is_none())
            .collect();
        if !strays.is_empty() {
            rows.push(ui::section("Other prompts"));
            rows.push(ui::list(strays.into_iter().map(|p| self.prompt_row(p))));
        }
        ui::column(rows)
    }

    fn session_page(&self, session: &Session) -> ui::Node<Message> {
        let mut rows = vec![
            ui::header(session.label())
                .on_back(Message::Back)
                .trailing(ui::button(
                    ui::label("Show"),
                    Message::Focus(session.id.clone()),
                )),
            ui::detail("Folder", ui::label(home(&session.cwd))),
            ui::detail("Status", ui::label(session.status_text())),
        ];
        let prompts: Vec<_> = self
            .prompts
            .iter()
            .filter(|p| p.session == session.id)
            .collect();
        if !prompts.is_empty() {
            rows.push(ui::section("Needs you"));
            rows.push(ui::list(prompts.into_iter().map(|p| self.prompt_row(p))));
        }
        if let Some(reply) = &session.last_reply {
            rows.push(ui::section("Last reply"));
            rows.push(ui::label(shorten(reply, 600)).tone(Tone::Dim));
        }
        if session.socket.is_some() {
            let target = Target::Session(session.id.clone());
            let draft = self.draft(&target).unwrap_or_default().to_string();
            rows.push(ui::separator());
            rows.push(
                ui::text_input("Message Claude", draft, move |text| {
                    Message::Draft(target.clone(), text)
                })
                .on_submit(Message::SendReply)
                .trailing(ui::button(ui::icon(ui::Icon::Forward), Message::SendReply)),
            );
        }
        ui::column(rows)
    }

    fn prompt_row(&self, prompt: &Prompt) -> ui::Node<Message> {
        let (title, subtitle) = match &prompt.ask {
            Ask::Permission { tool, what, .. } => (format!("Allow {tool}?"), what.clone()),
            Ask::Plan { plan } => ("Plan ready".into(), plan.clone()),
            Ask::Question { questions, .. } => {
                (question_title(&questions[0]), questions[0].text.clone())
            }
        };
        let subtitle = subtitle.lines().next().unwrap_or_default().to_string();
        ui::list_item(title)
            .subtitle(subtitle)
            .on_press(Message::Show(Page::Prompt(prompt.id)))
    }

    fn prompt_page(&self, prompt: &Prompt) -> ui::Node<Message> {
        let id = prompt.id;
        let target = Target::Prompt(id);
        let draft = self.draft(&target).unwrap_or_default().to_string();
        let field = |placeholder: &str| {
            let target = target.clone();
            ui::text_input(placeholder, draft.clone(), move |text| {
                Message::Draft(target.clone(), text)
            })
            .on_submit(Message::SendReply)
            .trailing(ui::button(ui::icon(ui::Icon::Forward), Message::SendReply))
        };
        let mut rows =
            vec![ui::header(self.label(&prompt.session, &prompt.cwd)).on_back(Message::Back)];
        match &prompt.ask {
            Ask::Permission { tool, what, always } => {
                rows.push(ui::heading(format!("Allow {tool}?")));
                rows.push(ui::label(shorten(what, 800)).tone(Tone::Dim));
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
                rows.push(field("Or tell Claude what to do instead"));
            }
            Ask::Plan { plan } => {
                rows.push(ui::heading("Plan ready"));
                rows.push(ui::label(shorten(plan, 1200)).tone(Tone::Dim));
                rows.push(ui::button(ui::label("Approve"), Message::Allow(id)).prominent());
                rows.push(field("What should change?"));
            }
            Ask::Question {
                questions, picks, ..
            } => {
                for (index, question) in questions.iter().enumerate() {
                    rows.push(ui::section(question_title(question)));
                    rows.push(ui::label(&question.text));
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
                }
                if questions.len() > 1 || questions.iter().any(|q| q.multiple) {
                    rows.push(ui::button(ui::label("Send"), Message::Submit(id)).prominent());
                }
                if questions.len() == 1 {
                    rows.push(field("Or answer in your own words"));
                }
            }
        }
        ui::column(rows)
    }
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
                let picks = vec![Vec::new(); questions.len()];
                return Ask::Question {
                    questions,
                    input: input.clone(),
                    picks,
                };
            }
        }
        "ExitPlanMode" => {
            return Ask::Plan {
                plan: text(&input["plan"]).trim().to_string(),
            };
        }
        _ => {}
    }
    let always = match &hook.permission_suggestions {
        Value::Array(rules) if !rules.is_empty() => Some(hook.permission_suggestions.clone()),
        _ => None,
    };
    Ask::Permission {
        tool: tool_name(&hook.tool_name),
        what: describe(&hook.tool_name, input, &hook.cwd),
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
fn tool_name(tool: &str) -> String {
    match tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, tool)) => format!("{server}: {tool}"),
        None => tool.into(),
    }
}

/// What a tool is about to do, in a line or two.
fn describe(tool: &str, input: &Value, cwd: &str) -> String {
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

fn shorten(text: &str, max: usize) -> String {
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
