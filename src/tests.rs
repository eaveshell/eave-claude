use std::path::PathBuf;

use eave_api::ui::NoticeParts;
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
        started: 1,
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

fn notices(widget: &Claude) -> Vec<NoticeParts<Message>> {
    widget
        .notices()
        .into_iter()
        .map(Notice::into_parts)
        .collect()
}

fn labels(notice: &NoticeParts<Message>) -> Vec<&str> {
    notice
        .actions
        .iter()
        .map(|(label, _)| label.as_str())
        .collect()
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
    assert_eq!(shown[0].body.as_deref(), Some("$ rm -rf target"));
    assert_eq!(shown[0].app.as_deref(), Some("Bar plugin"));
    assert_eq!(labels(&shown[0]), ["Allow", "Deny", "Instead…"]);
    assert_eq!(widget.waiting(), 1);

    let _ = widget.update(Message::Allow(10_001));
    assert_eq!(decision(&mut answer), json!({"behavior": "allow"}));
    assert!(widget.notices().is_empty());
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
        notices(&widget)[0].body.as_deref(),
        Some("a.rs"),
        "relative to the session"
    );
    let _ = widget.update(Message::Deny(10_002));
    let denied = decision(&mut answer);
    assert_eq!(
        (&denied["behavior"], &denied["interrupt"]),
        (&json!("deny"), &json!(true))
    );

    let mut answer = ask(&mut widget, 10_003, "s", "Write", json!({}));
    let _ = widget.update(Message::StartReply(Target::Prompt(10_003)));
    assert!(
        notices(&widget)[0].reply.is_some(),
        "the field shows once asked"
    );
    let _ = widget.update(Message::Draft(
        Target::Prompt(10_003),
        "  use b.rs  ".into(),
    ));
    let _ = widget.update(Message::SendReply);
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
    assert_eq!(labels(&shown[0]), ["Apple", "Pear", "Other…"]);
    let _ = widget.update(Message::Pick(10_005, 0, 1));
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
    assert_eq!(labels(&shown[0]), ["Answer…"], "the popover asks them");

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
        (shown[0].summary.as_str(), shown[0].body.as_deref()),
        ("Plan ready", Some("# Plan\n1. do it"))
    );
    assert_eq!(labels(&shown[0]), ["Approve", "Change…"]);
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
    assert_eq!(shown[0].summary, "Done");
    assert_eq!(shown[0].body.as_deref(), Some("All tests pass."));
    assert_eq!(labels(&shown[0]), ["Reply"]);
    assert!(shown[0].expires.is_some());
    assert_eq!(
        widget.sessions[0].last_reply.as_deref(),
        Some("All tests pass.")
    );

    let _ = widget.update(Message::StartReply(Target::Session("s".into())));
    let shown = notices(&widget);
    assert!(shown[0].reply.is_some());
    assert!(shown[0].expires.is_none(), "it waits while you type");

    // A subagent finishing isn't the session finishing.
    let mut widget = claude();
    let sub = hook("s", "Stop", json!({"agent_id": "a1"}));
    let _ = widget.update(Message::Hooks(hooks::Event::Hook(Box::new(sub))));
    assert!(widget.notices().is_empty());
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
    assert!(widget.notices().is_empty());
}

#[test]
fn waiting_without_a_hook_says_so_in_a_moment() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live(
        "s",
        Status::Waiting("input needed".into()),
    )]));
    assert!(widget.notices().is_empty(), "not yet: a hook may still ask");
    let _ = widget.update(Message::Unasked("s".into()));
    let shown = notices(&widget);
    assert_eq!(shown[0].summary, "Waiting for you in the terminal");
    assert_eq!(shown[0].body.as_deref(), Some("input needed"));

    let _ = widget.update(Message::Sessions(vec![live("s", Status::Idle)]));
    assert!(widget.notices().is_empty(), "answered in the terminal");

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
    let _ = widget.update(Message::Show(Page::Session("s".into())));
    let _ = widget.update(Message::Sessions(Vec::new()));
    assert!(widget.prompts.is_empty());
    assert_eq!(widget.page, Page::Sessions);
    assert!(
        widget.item().into_parts().content.is_none(),
        "hidden with nothing running"
    );
}

#[test]
fn hiding_a_prompt_keeps_it_in_the_popover() {
    let mut widget = claude();
    let _answer = ask(&mut widget, 10_014, "s", "Bash", json!({}));
    let _ = widget.update(Message::Hide(10_014));
    assert!(widget.notices().is_empty());
    assert_eq!(widget.prompts.len(), 1);
    assert_eq!(widget.waiting(), 1);
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
    assert_eq!((config.done, config.timeout), (true, 3));
    assert!(toml::from_str::<Config>("nope = 1").is_err());
}

#[test]
fn pages_change_within_the_open_popover() {
    let mut widget = claude();
    let _ = widget.update(Message::Sessions(vec![live("s", Status::Idle)]));
    let asks_to_open =
        |widget: &mut Claude, page| widget.update(Message::Show(page)).into_parts().1.is_some();
    // From a notice: the popover opens on the page.
    assert!(asks_to_open(&mut widget, Page::Session("s".into())));
    let _ = widget.update(Message::Opened);
    let _ = widget.update(Message::Back);
    // Within it: opening it again would close it first, and lose the page.
    assert!(!asks_to_open(&mut widget, Page::Session("s".into())));
    assert_eq!(widget.page, Page::Session("s".into()));
    let _ = widget.update(Message::Closed);
    assert_eq!(widget.page, Page::Sessions);
    assert!(asks_to_open(&mut widget, Page::Sessions));
}

#[test]
fn questions_come_one_at_a_time() {
    let mut widget = claude();
    let input = json!({"questions": [
        {"question": "One?", "options": [{"label": "A"}, {"label": "B"}]},
        {"question": "Two?", "options": [{"label": "C"}, {"label": "D"}]},
        {"question": "Three?", "options": [{"label": "E"}, {"label": "F"}]},
    ]});
    let mut answer = ask(&mut widget, 10_015, "s", "AskUserQuestion", input);
    let at = |widget: &Claude| match &widget.prompts[0].ask {
        Ask::Question { at, .. } => *at,
        _ => unreachable!(),
    };
    assert_eq!(at(&widget), 0);
    // Picking moves on to the next one.
    let _ = widget.update(Message::Pick(10_015, 0, 1));
    assert_eq!(at(&widget), 1);
    // Typing answers the one showing, and moves on too.
    let _ = widget.update(Message::Draft(Target::Prompt(10_015), "my own".into()));
    let _ = widget.update(Message::SendReply);
    assert_eq!(at(&widget), 2);
    // The arrows go back, and a change there doesn't send while one is left.
    let _ = widget.update(Message::Turn(10_015, 0));
    let _ = widget.update(Message::Pick(10_015, 0, 0));
    assert_eq!(at(&widget), 1);
    assert!(answer.try_recv().is_err());
    let _ = widget.update(Message::Turn(10_015, 9));
    assert_eq!(at(&widget), 1, "no such question");
    // The last one answered sends them all.
    let _ = widget.update(Message::Turn(10_015, 2));
    let _ = widget.update(Message::Pick(10_015, 2, 1));
    assert_eq!(
        decision(&mut answer)["updatedInput"]["answers"],
        json!({"One?": "A", "Two?": "my own", "Three?": "F"})
    );
}
