use std::path::PathBuf;

use serde_json::{Value, json};

use super::*;

fn claude() -> Claude {
    Claude::new(Config::default()).0
}

fn live(id: &str, status: Status) -> sessions::Live {
    sessions::Live {
        pid: 4242,
        id: id.into(),
        cwd: "/home/f/code/eave".into(),
        name: Some("eave-01".into()),
        title: Some("Bar plugin".into()),
        status,
        socket: Some(PathBuf::from("/run/user/1/cc-socks/4242.sock")),
        kitty: None,
        started: 1,
        since: 1,
    }
}

fn hook(session: &str, event: &str, extra: Value) -> hooks::Hook {
    let mut value =
        json!({"hook_event_name": event, "session_id": session, "cwd": "/home/f/code/eave"});
    if let (Value::Object(fields), Value::Object(more)) = (&mut value, extra) {
        fields.extend(more);
    }
    serde_json::from_value(value).unwrap()
}

/// A prompt from a hook, as id `id`, with its answer caught.
fn ask(
    widget: &mut Claude,
    id: u64,
    session: &str,
    tool: &str,
    input: Value,
) -> tokio::sync::oneshot::Receiver<String> {
    let answer = hooks::expect(id);
    let hook = hook(
        session,
        "PermissionRequest",
        json!({"tool_name": tool, "tool_input": input, "permission_suggestions": [{"type": "addRules"}]}),
    );
    let _ = widget.update(Message::Hooks(hooks::Event::Asked(id, Box::new(hook))));
    answer
}

fn decision(answer: &mut tokio::sync::oneshot::Receiver<String>) -> Value {
    let body: Value = serde_json::from_str(&answer.try_recv().expect("answered")).unwrap();
    assert_eq!(
        body["hookSpecificOutput"]["hookEventName"],
        "PermissionRequest"
    );
    body["hookSpecificOutput"]["decision"].clone()
}

fn notices(widget: &Claude) -> Vec<notify::Note> {
    widget.wanted().into_iter().map(|(_, note)| note).collect()
}

fn labels(notice: &notify::Note) -> Vec<&str> {
    notice
        .actions
        .iter()
        .map(|(_, label)| label.as_str())
        .collect()
}

/// The notification for `key` went up as `id`.
fn posted(widget: &mut Claude, key: Key, id: u32) {
    let _ = widget.update(Message::Posted(key, Some(id)));
}

fn signal(widget: &mut Claude, signal: notify::Signal) -> Effect<Message> {
    widget.update(Message::Signal(signal))
}

#[test]
fn a_permission_prompt_is_a_notice_that_answers() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    let mut answer = ask(
        &mut widget,
        10_001,
        "s",
        "Bash",
        json!({"command": "rm -rf target"}),
    );
    let shown = notices(&widget);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].summary, "Allow Bash?");
    assert_eq!(shown[0].body, "`$ rm -rf target`");
    assert_eq!(shown[0].app, "Bar plugin");
    assert_eq!(
        labels(&shown[0]),
        ["Allow", "Always allow", "Deny", "Show", "Open"],
        "always, as the terminal suggested rules"
    );
    assert_eq!(
        shown[0].reply.as_deref(),
        Some("Or tell Claude what to do instead")
    );
    assert_eq!(shown[0].seconds, None, "it waits");
    assert_eq!(widget.waiting(), 1);

    // Its Allow, picked on the notification.
    posted(&mut widget, Key::Prompt(10_001), 7);
    let _ = signal(&mut widget, notify::Signal::Action(7, "allow".into()));
    assert_eq!(decision(&mut answer), json!({"behavior": "allow"}));
    assert!(widget.wanted().is_empty());
    assert!(widget.posted.is_empty(), "the server took it away itself");
}

#[test]
fn denying_stops_claude_and_instead_tells_it_why() {
    let mut widget = claude();
    let mut answer = ask(
        &mut widget,
        10_002,
        "s",
        "Write",
        json!({"file_path": "/home/f/code/eave/a.rs"}),
    );
    assert_eq!(
        notices(&widget)[0].body,
        "`a.rs`",
        "relative to the session"
    );
    let _ = widget.update(Message::Deny(10_002));
    let denied = decision(&mut answer);
    assert_eq!(
        (&denied["behavior"], &denied["interrupt"]),
        (&json!("deny"), &json!(true))
    );

    // Typed into the notification.
    let mut answer = ask(&mut widget, 10_003, "s", "Write", json!({}));
    posted(&mut widget, Key::Prompt(10_003), 8);
    let _ = signal(
        &mut widget,
        notify::Signal::Replied(8, "  use b.rs  ".into()),
    );
    assert_eq!(
        decision(&mut answer),
        json!({"behavior": "deny", "message": "use b.rs"})
    );
}

#[test]
fn always_allow_sends_the_suggested_rules() {
    let mut widget = claude();
    let mut answer = ask(&mut widget, 10_004, "s", "Bash", json!({"command": "ls"}));
    let _ = widget.update(Message::AlwaysAllow(10_004));
    assert_eq!(
        decision(&mut answer),
        json!({"behavior": "allow", "updatedPermissions": [{"type": "addRules"}]})
    );
}

#[test]
fn a_short_question_answers_from_the_notice() {
    let mut widget = claude();
    let input = json!({"questions": [{"question": "Which fruit?", "header": "Fruit", "multiSelect": false,
        "options": [{"label": "Apple", "description": "red"}, {"label": "Pear", "description": "green"}]}]});
    let mut answer = ask(&mut widget, 10_005, "s", "AskUserQuestion", input.clone());
    let shown = notices(&widget);
    assert_eq!(shown[0].summary, "Fruit");
    assert_eq!(labels(&shown[0]), ["Apple", "Pear", "Show", "Open"]);
    assert_eq!(shown[0].reply.as_deref(), Some("Your answer"));
    posted(&mut widget, Key::Prompt(10_005), 9);
    let _ = signal(&mut widget, notify::Signal::Action(9, "pick:1".into()));
    let picked = decision(&mut answer);
    assert_eq!(picked["behavior"], "allow");
    assert_eq!(
        picked["updatedInput"]["answers"],
        json!({"Which fruit?": "Pear"})
    );
    assert_eq!(
        picked["updatedInput"]["questions"], input["questions"],
        "the input, kept"
    );
    assert!(widget.prompts.is_empty());
}

#[test]
fn an_answer_of_your_own_answers_the_question_showing() {
    let mut widget = claude();
    let input =
        json!({"questions": [{"question": "Name?", "options": [{"label": "A"}, {"label": "B"}]}]});
    let mut answer = ask(&mut widget, 10_006, "s", "AskUserQuestion", input);
    let _ = widget.update(Message::Draft(Target::Prompt(10_006), "Zed".into()));
    let _ = widget.update(Message::SendReply);
    assert_eq!(
        decision(&mut answer)["updatedInput"]["answers"],
        json!({"Name?": "Zed"})
    );
}

#[test]
fn several_questions_wait_for_send() {
    let mut widget = claude();
    let input = json!({"questions": [
        {"question": "Colors?", "multiSelect": true, "options": [{"label": "Red"}, {"label": "Blue"}, {"label": "Green"}]},
        {"question": "Size?", "options": [{"label": "S"}, {"label": "L"}]},
    ]});
    let mut answer = ask(&mut widget, 10_007, "s", "AskUserQuestion", input);
    let shown = notices(&widget);
    assert_eq!(shown[0].summary, "2 questions");
    assert_eq!(
        labels(&shown[0]),
        ["Answer…", "Show", "Open"],
        "the popover asks them"
    );
    assert_eq!(shown[0].reply, None);

    let _ = widget.update(Message::Pick(10_007, 0, 0));
    let _ = widget.update(Message::Pick(10_007, 0, 2));
    let _ = widget.update(Message::Pick(10_007, 0, 0));
    let _ = widget.update(Message::Pick(10_007, 0, 1));
    let _ = widget.update(Message::Submit(10_007));
    assert!(answer.try_recv().is_err(), "Size has no answer yet");
    let _ = widget.update(Message::Pick(10_007, 1, 1));
    let _ = widget.update(Message::Submit(10_007));
    assert_eq!(
        decision(&mut answer)["updatedInput"]["answers"],
        json!({"Colors?": "Green, Blue", "Size?": "L"})
    );
}

#[test]
fn a_plan_is_approved_or_sent_back() {
    let mut widget = claude();
    let mut answer = ask(
        &mut widget,
        10_008,
        "s",
        "ExitPlanMode",
        json!({"plan": "# Plan\n1. do it"}),
    );
    let shown = notices(&widget);
    assert_eq!(
        (shown[0].summary.as_str(), shown[0].body.as_str()),
        ("Plan ready", "# Plan\n1. do it")
    );
    assert_eq!(labels(&shown[0]), ["Approve", "Show", "Open"]);
    assert_eq!(shown[0].reply.as_deref(), Some("What should change?"));
    let _ = widget.update(Message::Draft(
        Target::Prompt(10_008),
        "smaller steps".into(),
    ));
    let _ = widget.update(Message::SendReply);
    assert_eq!(
        decision(&mut answer),
        json!({"behavior": "deny", "message": "smaller steps"})
    );
}

#[test]
fn approving_a_plan_gives_its_input_back_and_leaves_plan_mode() {
    let mut widget = claude();
    // The session worked in auto mode before planning.
    let submit = hook("s", "UserPromptSubmit", json!({"permission_mode": "auto"}));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(submit))));
    let input = json!({"plan": "# Plan", "planFilePath": "/p.md"});
    let mut answer = ask(&mut widget, 10_016, "s", "ExitPlanMode", input.clone());
    posted(&mut widget, Key::Prompt(10_016), 11);
    let _ = signal(&mut widget, notify::Signal::Action(11, "approve".into()));
    assert_eq!(
        decision(&mut answer),
        json!({
            "behavior": "allow",
            "updatedInput": input,
            "updatedPermissions": [{"type": "setMode", "mode": "auto", "destination": "session"}],
        })
    );

    // Without a mode seen, what Claude Code suggests, else the default.
    let mut widget = claude();
    let answer = hooks::expect(10_017);
    let plan = hook(
        "t",
        "PermissionRequest",
        json!({"tool_name": "ExitPlanMode", "tool_input": {"plan": "p"}, "permission_mode": "plan",
            "permission_suggestions": [{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}),
    );
    let _ = widget.update(Message::Hooks(hooks::Event::Asked(10_017, Box::new(plan))));
    let _ = widget.update(Message::Approve(10_017));
    let mut answer = answer;
    assert_eq!(
        decision(&mut answer)["updatedPermissions"][0]["mode"],
        "acceptEdits"
    );
    let mut widget = claude();
    let mut answer = ask(
        &mut widget,
        10_018,
        "u",
        "ExitPlanMode",
        json!({"plan": "p"}),
    );
    let _ = widget.update(Message::Approve(10_018));
    assert_eq!(
        decision(&mut answer)["updatedPermissions"][0]["mode"],
        "default"
    );
}

#[test]
fn an_approved_plan_goes_on_in_the_configured_mode() {
    let mut widget = Claude::new(Config {
        plan: Some("auto".into()),
        ..Config::default()
    })
    .0;
    let submit = hook(
        "s",
        "UserPromptSubmit",
        json!({"permission_mode": "default"}),
    );
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(submit))));
    let mut answer = ask(
        &mut widget,
        10_019,
        "s",
        "ExitPlanMode",
        json!({"plan": "p"}),
    );
    let _ = widget.update(Message::Approve(10_019));
    assert_eq!(
        decision(&mut answer)["updatedPermissions"][0]["mode"],
        "auto"
    );
}

#[test]
fn notifications_go_up_once_and_close_when_answered_elsewhere() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    let _answer = ask(&mut widget, 10_022, "s", "Bash", json!({}));
    assert_eq!(widget.posted.len(), 1, "sent");
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    assert_eq!(widget.posted.len(), 1, "not again");
    // Answered in the terminal before its id came: closed as soon as it does.
    let _ = widget.update(Message::Hooks(hooks::Event::Settled(10_022)));
    assert!(widget.posted[0].gone);
    let closes = widget.update(Message::Posted(Key::Prompt(10_022), Some(3)));
    assert!(widget.posted.is_empty());
    let _ = closes;

    // Dismissed: the prompt still waits in the popover, without a notification.
    let _answer = ask(&mut widget, 10_020, "s", "Bash", json!({}));
    posted(&mut widget, Key::Prompt(10_020), 4);
    let _ = signal(&mut widget, notify::Signal::Closed(4, 2));
    assert!(widget.wanted().is_empty());
    assert_eq!(widget.prompts.len(), 1);
    assert!(widget.posted.is_empty());

    // Someone else's notification is none of ours.
    let _ = signal(&mut widget, notify::Signal::Action(99, "allow".into()));
    assert_eq!(widget.prompts.len(), 1);
}

#[test]
fn a_click_on_a_notification_opens_its_chat() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    let _answer = ask(&mut widget, 10_021, "s", "Bash", json!({}));
    posted(&mut widget, Key::Prompt(10_021), 5);
    let effect = signal(&mut widget, notify::Signal::Action(5, "default".into()));
    assert!(effect.dialog_request().is_some(), "opens the dialog");
    assert_eq!(widget.talking.as_deref(), Some("s"));
    assert_eq!(widget.prompts.len(), 1, "still to answer, in the chat");
    assert!(widget.prompts[0].hidden);
}

#[test]
fn a_prompt_answered_in_the_terminal_goes_away() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    let mut answer = ask(&mut widget, 10_009, "s", "Bash", json!({"command": "ls"}));
    // The file may still say busy when the hook arrives: that's not an answer.
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    assert_eq!(widget.prompts.len(), 1);
    let _ = widget.update(Message::Sessions(vec![live(
        "s",
        Status::Waiting("permission prompt".into()),
    )]));
    assert_eq!(widget.prompts.len(), 1);
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    assert!(widget.prompts.is_empty());
    assert!(
        matches!(
            answer.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ),
        "withdrawn: no opinion"
    );
}

#[test]
fn moving_on_settles_the_session_s_prompts() {
    let mut widget = claude();
    let _a = ask(&mut widget, 10_010, "s", "Bash", json!({}));
    let _b = ask(&mut widget, 10_011, "other", "Bash", json!({}));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(hook(
        "s",
        "UserPromptSubmit",
        json!({}),
    )))));
    let left: Vec<_> = widget.prompts.iter().map(|p| p.id).collect();
    assert_eq!(left, [10_011]);
    let _ = widget.update(Message::Hooks(hooks::Event::Settled(10_011)));
    assert!(widget.prompts.is_empty());
}

#[test]
fn a_finished_turn_says_so_and_takes_a_reply() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Idle)]));
    let stop = hook(
        "s",
        "Stop",
        json!({"last_assistant_message": "All tests pass."}),
    );
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(stop))));
    let shown = notices(&widget);
    assert_eq!(
        shown[0].summary, "All tests pass.",
        "its first line says it"
    );
    assert_eq!(shown[0].body, "");
    assert_eq!(labels(&shown[0]), ["Show", "Open"]);
    assert_eq!(shown[0].reply.as_deref(), Some("Message Claude"));
    assert_eq!(shown[0].seconds, Some(8));
    assert_eq!(
        widget.sessions[0].last_reply.as_deref(),
        Some("All tests pass.")
    );
    // Clicked: its chat.
    let key = widget.wanted()[0].0;
    posted(&mut widget, key, 6);
    let _ = signal(&mut widget, notify::Signal::Action(6, "default".into()));
    assert_eq!(widget.talking.as_deref(), Some("s"));
    assert!(widget.wanted().is_empty());

    // A subagent finishing isn't the session finishing.
    let mut widget = claude();
    let sub = hook("s", "Stop", json!({"agent_id": "a1"}));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(sub))));
    assert!(widget.wanted().is_empty());
}

#[test]
fn done_can_be_turned_off() {
    let mut widget = Claude::new(Config {
        done: false,
        ..Config::default()
    })
    .0;
    let stop = hook("s", "Stop", json!({"last_assistant_message": "ok"}));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(stop))));
    assert!(widget.wanted().is_empty());
}

#[test]
fn waiting_without_a_hook_says_so_in_a_moment() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live(
        "s",
        Status::Waiting("input needed".into()),
    )]));
    assert!(widget.wanted().is_empty(), "not yet: a hook may still ask");
    let _ = widget.update(Message::Unasked("s".into()));
    let shown = notices(&widget);
    assert_eq!(shown[0].summary, "Waiting for you in the terminal");
    assert_eq!(shown[0].body, "input needed");
    assert_eq!(labels(&shown[0]), ["Show", "Open"]);

    let _ = widget.update(Message::Sessions(vec![live("s", Status::Idle)]));
    assert!(widget.wanted().is_empty(), "answered in the terminal");

    // When a hook did ask, its prompt says it.
    let _ = widget.update(Message::Sessions(vec![live(
        "s",
        Status::Waiting("permission prompt".into()),
    )]));
    let _answer = ask(&mut widget, 10_012, "s", "Bash", json!({}));
    let _ = widget.update(Message::Unasked("s".into()));
    assert_eq!(notices(&widget).len(), 1);
}

#[test]
fn a_session_that_ends_takes_its_prompts_with_it() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live(
        "s",
        Status::Waiting(String::new()),
    )]));
    let _answer = ask(&mut widget, 10_013, "s", "Bash", json!({}));
    let _ = widget.update(Message::Show(Page::Chat("s".into())));
    let _ = widget.update(Message::Sessions(Vec::new()));
    assert!(widget.prompts.is_empty());
    assert!(widget.talking.is_none() && widget.dialog().is_none());
    assert!(
        widget.item().into_parts().content.is_none(),
        "hidden with nothing running"
    );
}

#[test]
fn a_chat_that_shows_needs_no_notifications() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![
        live("s", Status::Idle),
        live("t", Status::Idle),
    ]));
    let stop = |session| {
        Message::Hooks(hooks::Event::Hook(Box::new(hook(
            session,
            "Stop",
            json!({"last_assistant_message": "Done."}),
        ))))
    };
    // Already up when its chat opens: it goes away.
    let _ = widget.update(stop("s"));
    assert_eq!(widget.wanted().len(), 1);
    let _ = widget.update(Message::Show(Page::Chat("s".into())));
    let _ = widget.update(Message::DialogOpened);
    assert!(widget.wanted().is_empty());
    // Asked or said while it shows: none, and none once it closes.
    let _answer = ask(&mut widget, 10_015, "s", "Bash", json!({}));
    assert!(widget.wanted().is_empty());
    // The prompt is still there to answer, in the chat.
    assert_eq!(widget.pending("s").map(|p| p.id), Some(10_015));
    let _ = widget.update(stop("s"));
    assert!(widget.wanted().is_empty());
    // Another session still gets its own.
    let _ = widget.update(stop("t"));
    assert_eq!(widget.wanted().len(), 1);
    let _ = widget.update(Message::DialogClosed);
    assert_eq!(widget.wanted().len(), 1, "only the other session's");
}

#[test]
fn a_terminal_in_front_needs_no_notifications() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![
        live("s", Status::Idle),
        live("t", Status::Idle),
    ]));
    let stop = |session| {
        Message::Hooks(hooks::Event::Hook(Box::new(hook(
            session,
            "Stop",
            json!({"last_assistant_message": "Done."}),
        ))))
    };
    let front = |widget: &mut Claude, session: Option<&str>| {
        let n = widget.looked;
        let _ = widget.update(Message::Front(n, session.map(Into::into)));
    };
    // Already up when you switch to its tab: it goes away.
    let _ = widget.update(stop("s"));
    assert_eq!(widget.wanted().len(), 1);
    front(&mut widget, Some("s"));
    assert!(widget.wanted().is_empty());
    // Asked while in front: none, but it still waits to be answered.
    let _answer = ask(&mut widget, 10_140, "s", "Bash", json!({}));
    assert!(widget.wanted().is_empty());
    assert_eq!(widget.pending("s").map(|p| p.id), Some(10_140));
    // Another session still gets its own, and none come back when you look away.
    let _ = widget.update(stop("t"));
    front(&mut widget, None);
    assert_eq!(widget.wanted().len(), 1, "only the other session's");
    // An answer from an older look comes too late to count.
    let stale = widget.looked;
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    let _ = widget.update(Message::Front(stale, Some("s".into())));
    assert_eq!(widget.front, None);
}

#[test]
fn watching_can_be_turned_off() {
    let mut widget = Claude::new(toml::from_str("watching = false").unwrap()).0;
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Idle)]));
    let n = widget.looked;
    let _ = widget.update(Message::Front(n, Some("s".into())));
    let _answer = ask(&mut widget, 10_141, "s", "Bash", json!({}));
    assert_eq!(widget.wanted().len(), 1);
}

#[test]
fn a_chat_follows_only_the_session_showing() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    let _ = widget.update(Message::Show(Page::Chat("s".into())));
    let _ = widget.update(Message::DialogOpened);
    let mut chat = Chat::default();
    chat.read_line(r#"{"type":"user","message":{"content":"hi"}}"#, "/");
    let _ = widget.update(Message::Chat("other".into(), chat.clone()));
    assert!(widget.chat.is_none(), "not the one showing");
    let _ = widget.update(Message::Chat("s".into(), chat));
    assert!(widget.chat.is_some());
    // The composer messages the session, or answers what it asks.
    let _ = widget.update(Message::Draft(Target::Session("s".into()), "go".into()));
    let _answer = ask(&mut widget, 10_014, "s", "Bash", json!({}));
    assert_eq!(widget.pending("s").map(|p| p.id), Some(10_014));
    // The popover it came out of folds away: what's typed in the chat stays.
    let _ = widget.update(Message::Closed);
    assert!(widget.draft.is_some());
    // Closed, it shows the chat while it goes back, but no longer follows it.
    let _ = widget.update(Message::DialogClosed);
    assert!(widget.draft.is_none());
    assert!(widget.chat.is_some() && widget.dialog().is_some());
    assert!(!widget.dialog_open);
}

#[test]
fn code_is_inline_or_a_block() {
    assert_eq!(code("$ ls"), "`$ ls`");
    assert_eq!(code("a\nb"), "```\na\nb\n```");
    assert_eq!(code("echo `x`"), "```\necho `x`\n```");
    assert_eq!(code(""), "");
}

#[test]
fn a_command_to_allow_is_a_shell_block() {
    assert_eq!(command("Bash", "$ cargo test"), "```sh\ncargo test\n```");
    assert_eq!(command("Read", "src/lib.rs"), "`src/lib.rs`");
}

#[test]
fn a_chat_reads_as_a_timeline() {
    let at = chrono::Local::now();
    let entries = [
        Entry::You {
            text: "fix it".into(),
            at: Some(at),
        },
        Entry::Claude("Looking.".into()),
        Entry::Tool {
            id: "a".into(),
            name: "Read".into(),
            what: "src/lib.rs".into(),
            done: true,
            failed: false,
            result: "12 lines".into(),
            diff: None,
        },
        Entry::Tool {
            id: "b".into(),
            name: "Bash".into(),
            what: "$ false".into(),
            done: true,
            failed: true,
            result: "exit 1".into(),
            diff: None,
        },
        Entry::you("and now?"),
        Entry::Tool {
            id: "c".into(),
            name: "Bash".into(),
            what: "$ ssh asahi".into(),
            done: false,
            failed: false,
            result: String::new(),
            diff: None,
        },
    ];
    let marks = |nodes: Vec<ui::Node<Message>>| -> Vec<Vec<ui::Mark>> {
        nodes
            .into_iter()
            .filter_map(|node| match node.into_kind() {
                ui::Kind::Timeline(steps) => Some(
                    steps
                        .into_iter()
                        .map(|step| match step.into_kind() {
                            ui::Kind::Step { mark, .. } => mark,
                            _ => panic!("a step that isn't one"),
                        })
                        .collect(),
                ),
                _ => None,
            })
            .collect()
    };
    let nodes = conversation(&entries, true, Mood::Laptop);
    assert!(
        matches!(nodes[0].kind(), ui::Kind::Divider { text } if *text == at.format("%H:%M").to_string()),
        "a turn opens with its time"
    );
    // The second turn has no time: a plain line.
    assert!(matches!(nodes[2].kind(), ui::Kind::Separator));
    use ui::Mark::*;
    assert_eq!(
        marks(nodes),
        [vec![You, Note, Done, Failed], vec![You, Waiting]]
    );
    let nodes = conversation(&entries, false, Mood::Laptop);
    assert_eq!(marks(nodes)[1], [You, Running]);
}

#[test]
fn tools_are_described_briefly() {
    let cwd = "/w";
    assert_eq!(
        describe("Read", &json!({"file_path": "/w/src/a.rs"}), cwd),
        "src/a.rs"
    );
    assert_eq!(
        describe("WebFetch", &json!({"url": "https://x"}), cwd),
        "https://x"
    );
    assert_eq!(describe("Odd", &json!({"a": 1}), cwd), r#"{"a":1}"#);
    assert_eq!(describe("Odd", &json!({}), cwd), "");
    assert_eq!(
        tool_name("mcp__github__create_issue"),
        "github: create_issue"
    );
    assert_eq!(directory("/home/f/code/eave/"), "eave");
    assert_eq!(shorten("abcdef", 3), "abc…");
}

#[test]
fn config_has_defaults() {
    let config: Config = toml::from_str("timeout = 3").unwrap();
    assert_eq!(
        (config.done, config.timeout, config.plan, config.watching),
        (true, 3, None, true)
    );
    let config: Config = toml::from_str(r#"plan = "auto""#).unwrap();
    assert_eq!(config.plan.as_deref(), Some("auto"));
    assert!(toml::from_str::<Config>("nope = 1").is_err());
}

#[test]
fn a_chat_opens_in_the_dialog_and_pages_change_within_the_open_popover() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Idle)]));
    let asks = |widget: &mut Claude, page| {
        let effect = widget.update(Message::Show(page));
        (
            effect.dialog_request().is_some(),
            effect.into_parts().1.is_some(),
        )
    };
    // A chat: in the dialog, which the host opens once.
    assert_eq!(asks(&mut widget, Page::Chat("s".into())), (true, false));
    assert_eq!(widget.page, Page::Sessions);
    // The list: in the popover.
    assert_eq!(asks(&mut widget, Page::Sessions), (false, true));
    let _ = widget.update(Message::Opened);
    // Within it: opening it again would close it first, and lose the page.
    assert_eq!(asks(&mut widget, Page::Sessions), (false, false));
    let _ = widget.update(Message::Closed);
    assert_eq!(widget.page, Page::Sessions);
    // Showing the terminal closes the dialog too.
    let effect = widget.update(Message::Focus("s".into()));
    assert!(effect.dialog_request().is_some());
}

#[test]
fn questions_come_one_at_a_time() {
    let mut widget = claude();
    let input = json!({"questions": [
        {"question": "One?", "options": [{"label": "A"}, {"label": "B"}]},
        {"question": "Two?", "options": [{"label": "C"}, {"label": "D"}]},
        {"question": "Three?", "options": [{"label": "E"}, {"label": "F"}]},
    ]});
    let mut answer = ask(&mut widget, 10_023, "s", "AskUserQuestion", input);
    let at = |widget: &Claude| match &widget.prompts[0].ask {
        Ask::Question { at, .. } => *at,
        _ => unreachable!(),
    };
    assert_eq!(at(&widget), 0);
    // Picking moves on to the next one.
    let _ = widget.update(Message::Pick(10_023, 0, 1));
    assert_eq!(at(&widget), 1);
    // Typing answers the one showing, and moves on too.
    let _ = widget.update(Message::Draft(Target::Prompt(10_023), "my own".into()));
    let _ = widget.update(Message::SendReply);
    assert_eq!(at(&widget), 2);
    // The arrows go back, and a change there doesn't send while one is left.
    let _ = widget.update(Message::Turn(10_023, 0));
    let _ = widget.update(Message::Pick(10_023, 0, 0));
    assert_eq!(at(&widget), 1);
    assert!(answer.try_recv().is_err());
    let _ = widget.update(Message::Turn(10_023, 9));
    assert_eq!(at(&widget), 1, "no such question");
    // The last one answered sends them all.
    let _ = widget.update(Message::Turn(10_023, 2));
    let _ = widget.update(Message::Pick(10_023, 2, 1));
    assert_eq!(
        decision(&mut answer)["updatedInput"]["answers"],
        json!({"One?": "A", "Two?": "my own", "Three?": "F"})
    );
}

#[test]
fn the_mascot_works_plans_asks_and_rests() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    assert_eq!(widget.mood(), Mood::Laptop);

    let plan = hook("s", "UserPromptSubmit", json!({"permission_mode": "plan"}));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(plan))));
    assert_eq!(widget.mood(), Mood::Planning);

    let _answer = ask(&mut widget, 10_030, "s", "Bash", json!({"command": "ls"}));
    assert_eq!(widget.mood(), Mood::Question);
    let _ = widget.update(Message::Deny(10_030));
    assert_eq!(widget.mood(), Mood::Planning);

    // Done a moment ago: pleased, until five minutes have gone by.
    let now = Clock::now().now;
    let mut done = live("s", Status::Idle);
    done.since = now - 1000;
    let _ = widget.update(Message::Sessions(vec![done.clone()]));
    assert_eq!(widget.mood(), Mood::Happy);
    assert_eq!(widget.next_change(), Some(done.since + mood::DONE_FOR));

    // Not after a turn that failed.
    let failed = hook("s", "StopFailure", json!({"error": "overloaded"}));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(failed))));
    assert!(widget.mood() <= Mood::Coffee);

    // Long idle: coffee or sleep, whichever the hour says.
    done.since = 1;
    let _ = widget.update(Message::Sessions(vec![done]));
    assert!(widget.mood() <= Mood::Coffee);
    assert!(widget.next_change().is_some_and(|at| at > now));
}

#[test]
fn a_reply_s_first_line_is_its_headline() {
    assert_eq!(
        headline("## Fixed the reload\n\nThe watcher follows the link."),
        (
            "Fixed the reload".to_string(),
            "The watcher follows the link.".to_string()
        )
    );
    assert_eq!(headline("**Done.**"), ("Done.".to_string(), String::new()));
    let long = "word ".repeat(30);
    assert_eq!(headline(&long).0, "Done", "too long to be one");
    assert_eq!(headline("```rust\nfn a() {}\n```").0, "Done");
}

#[test]
fn a_command_that_says_what_it_s_for_asks_that() {
    let mut widget = claude();
    let _answer = ask(
        &mut widget,
        10_120,
        "s",
        "Bash",
        json!({"command": "cargo test --workspace", "description": "Run the workspace tests."}),
    );
    assert_eq!(notices(&widget)[0].summary, "Run the workspace tests?");
}

#[test]
fn always_allow_from_the_notification_sends_the_suggested_rules() {
    let mut widget = claude();
    let mut answer = ask(&mut widget, 10_121, "s", "Bash", json!({"command": "ls"}));
    posted(&mut widget, Key::Prompt(10_121), 9);
    let _ = signal(&mut widget, notify::Signal::Action(9, "always".into()));
    assert_eq!(
        decision(&mut answer),
        json!({"behavior": "allow", "updatedPermissions": [{"type": "addRules"}]})
    );
}

#[test]
fn show_on_a_prompt_s_notification_leaves_it_to_answer() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Idle)]));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(hook(
        "s",
        "Stop",
        json!({"last_assistant_message": "Done."}),
    )))));
    let mut answer = ask(&mut widget, 10_142, "s", "Bash", json!({"command": "ls"}));
    posted(&mut widget, Key::Prompt(10_142), 12);
    let _ = signal(&mut widget, notify::Signal::Action(12, "show".into()));
    assert!(answer.try_recv().is_err(), "not answered");
    assert_eq!(widget.pending("s").map(|p| p.id), Some(10_142));
    // Its notification closes, as on any action, and its turn's news is old once you're there.
    assert!(widget.wanted().is_empty());
}

#[test]
fn a_failed_turn_is_critical() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Idle)]));
    let failed = hook("s", "StopFailure", json!({"error": "rate limited"}));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(failed))));
    let shown = notices(&widget);
    assert_eq!(shown[0].summary, "Stopped with an error");
    assert_eq!(shown[0].urgency, 2);
}

const EAVE: notify::Caps = notify::Caps {
    threads: true,
    choices: true,
};

#[test]
fn eave_gets_a_question_s_options_and_a_way_back() {
    let mut widget = claude();
    let input = json!({"questions": [
        {"question": "Which renderer?", "header": "Renderer", "options": [
            {"label": "GPU (Recommended)", "description": "The default"},
            {"label": "CPU", "description": "Steadier"},
            {"label": "Both", "description": "Twice the time"}]},
        {"question": "How long?", "options": [{"label": "10 s"}, {"label": "30 s"}]},
    ]});
    let _answer = ask(&mut widget, 10_130, "s", "AskUserQuestion", input);
    let plain = notices(&widget).remove(0);
    assert_eq!(
        labels(&plain),
        ["Answer…", "Show", "Open"],
        "others get the chat"
    );

    let rich = notify::rich(plain.clone(), EAVE);
    assert_eq!(rich.summary, "Which renderer?");
    assert_eq!(rich.body, "");
    assert_eq!(
        labels(&rich),
        ["Show", "Open"],
        "the options are the question's"
    );
    assert_eq!(rich.reply.as_deref(), Some("Or answer in your own words"));
    let question = rich.question.as_ref().expect("asked");
    assert_eq!(question.step, (0, 2));
    assert_eq!(question.options.len(), 3);
    assert_eq!(notify::rich(plain, notify::Caps::default()).question, None);

    // Picked on the card: it stays up, and moves on to the next question in place.
    posted(&mut widget, Key::Prompt(10_130), 11);
    let _ = signal(&mut widget, notify::Signal::Action(11, "pick:1".into()));
    assert_eq!(widget.posted.len(), 1, "still up");
    let step = widget.posted[0].note.question.as_ref().map(|q| q.step);
    assert_eq!(step, Some((1, 2)), "sent again for the next question");
    let rich = notify::rich(widget.posted[0].note.clone(), EAVE);
    assert_eq!(labels(&rich), ["Show", "Open", "Back"]);

    // Back, then on again.
    let _ = signal(&mut widget, notify::Signal::Action(11, notify::BACK.into()));
    let step = widget.posted[0].note.question.as_ref().map(|q| q.step);
    assert_eq!(step, Some((0, 2)));
}

#[test]
fn several_picks_from_the_card_answer_a_question() {
    let mut widget = claude();
    let input = json!({"questions": [
        {"question": "Which hints?", "multiSelect": true, "options": [
            {"label": "image-data"}, {"label": "value"}, {"label": "transient"}]},
    ]});
    let mut answer = ask(&mut widget, 10_131, "s", "AskUserQuestion", input);
    posted(&mut widget, Key::Prompt(10_131), 12);
    let picked = vec![
        "pick:2".to_string(),
        "pick:0".to_string(),
        "pick:9".to_string(),
    ];
    let _ = signal(&mut widget, notify::Signal::Picked(12, picked));
    assert_eq!(
        decision(&mut answer)["updatedInput"]["answers"],
        json!({"Which hints?": "transient, image-data"})
    );
}

#[test]
fn the_dormer_answers_prompts_and_opens_sessions() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Busy)]));
    let titles = |widget: &Claude| -> Vec<String> {
        widget
            .actions()
            .iter()
            .map(|action| action.title().to_string())
            .collect()
    };
    assert_eq!(
        titles(&widget),
        ["Open Bar plugin", "Show the terminal of Bar plugin"]
    );

    let mut answer = ask(
        &mut widget,
        10_100,
        "s",
        "Bash",
        json!({"command": "cargo test"}),
    );
    assert_eq!(titles(&widget)[..2], ["Allow Bash", "Deny Bash"]);
    let allow = widget
        .actions()
        .into_iter()
        .next()
        .unwrap()
        .into_parts()
        .message;
    let _ = widget.update(allow);
    assert_eq!(decision(&mut answer), json!({"behavior": "allow"}));
    assert_eq!(titles(&widget).len(), 2, "answered, so no longer asked");
}
