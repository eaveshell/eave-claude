# eave-claude

[![CI](https://github.com/eaveshell/eave-claude/actions/workflows/ci.yml/badge.svg)](https://github.com/eaveshell/eave-claude/actions/workflows/ci.yml)

Claude Code in [eave](https://github.com/eaveshell/eave), the Hyprland status bar. Your sessions
in the bar, and their permission prompts, questions and plans as notifications you answer
without going back to the terminal.

```toml
# ~/.config/eave/config.toml
[[right]]
widget = "eaveshell/eave-claude"
```

Then, in Claude Code, install the plugin that tells the bar what your sessions ask:

```
/plugin marketplace add eaveshell/eave-claude
/plugin install eave@eave-claude
```

Sessions started afterwards use it. Without the plugin, the bar still lists your sessions and says
when one waits for you in the terminal, but can't answer for it.

- **In the bar:** a little mascot while sessions run, in the mood of the one that most needs
  saying, with a count of the ones waiting for you. It acts out a change of mood, then sits still;
  only a question keeps it going until you answer.

  | Mood | When |
  |---|---|
  | question | a session asks you something: a permission, a question, a plan to approve |
  | planning | working in plan mode |
  | laptop | working |
  | happy | finished its turn, in the last five minutes |
  | coffee | idle a while, by day |
  | sleeping | idle a while, between 22:00 and 07:00 |
- **Permission prompts:** Allow, Always allow (the terminal's "don't ask again"), Deny (Claude
  stops, like Esc in the terminal), or type into the field what Claude should do instead. A
  command that says what it's for asks that: "Run the workspace tests?".
- **Questions** (AskUserQuestion): in eave, answered on the notification. Every option shows, with
  what it means and the one Claude recommends; with several to pick, check them and Send. Several
  questions come one at a time on the same notification, with Back: picking moves on, and
  answering the last one sends them all. The field takes an answer of your own. Other
  notification daemons get two short options as buttons, and anything longer opens the chat.
- **Plans:** the plan, in Markdown, with Approve, or type what should change to send it back.
  Approving goes back to the mode the session was in before planning, as in the terminal, or
  to the mode set in `plan`.
- **Done:** when a session finishes its turn, its last reply, its first line as the title, with a
  field to answer it. A turn that stops with an error is critical: it stays until you see it.
- **The popover:** your sessions, each with its mascot, and what it's doing. A click opens its chat,
  and so do the keys 1 to 9 and 0 for the first ten (see [Keys](#keys)), in a dialog in the middle
  of the screen, which the popover turns into: the
  conversation as it happens, a turn at a time under the time you spoke, its steps strung on a
  line, each marked done, under way, failed or waiting for you. Markdown and all, with code
  colored, the tools it runs with what came of them, the edits it makes as diffs, what it asks
  you, and a field to message it. Show focuses its terminal, and Esc closes it. Clicking a
  notification opens its session's chat, which the notification turns into, and its Show
  button goes to its terminal instead.
- **In the dormer:** allow or deny what a session asks, approve a plan, open a question to
  answer it, and open any session's chat, which the dormer turns into, or show its terminal.

While a session's chat is open, it sends no notifications for that session: you're
reading it already, and what it asks is answered right there. The same goes while its terminal is
in front of you, on its tab: a notification already up goes away when you switch to it, and the
prompt still waits in the popover. Most terminals are told apart by the window's title, which
Claude Code sets to the session's. kitty can say exactly which tab and split you're on, and Show
switches to the session's, with remote control on in `kitty.conf`:

```
allow_remote_control socket-only
listen_on unix:@kitty-{kitty_pid}
```

Whatever you answer in the bar, the terminal shows the same prompt meanwhile, and whichever you
answer first wins. Options: `done` (say so when a turn finishes, default `true`) and `timeout`
(seconds that shows, default 8), `plan` (the mode an approved plan goes on in, like `"auto"`;
unset, the one before planning), and `watching` (no notifications for a session whose terminal
is in front, default `true`).

Notifications go over the session bus like any app's, so they show in eave with
[eave-notifications](https://github.com/eaveshell/eave-notifications), or in mako, dunst or
swaync. They carry the mascot, in the mood that fits (asking, or happy when done), and Markdown bodies, which eave sets
as such. In eave they come from Claude, with the session beside it and the mascot as the icon, and
a session's notifications stack on one card; other daemons show the session as the app. In eave, the field to answer is right on the notification; the bar takes the keyboard
only once you click it.

## Keys

While the popover is open, `1` to `9` and `0` open the first ten sessions' chats, and in a chat
`Ctrl+T` shows its terminal (with an eave newer than eave-api 0.1.8). They're named `chat-1` to
`chat-10` and `show`, so you can give them other keys, or none, in the widget's entry:

```toml
[[right]]
widget = "fran/eave-claude"
keys = { chat-1 = "alt+1", chat-10 = false }
```

To open the popover from anywhere, bind a key in Hyprland: `bind = SUPER, C, event, eave-popover
claude`, or run `eave popover claude`.

## How it works

| From | What |
|---|---|
| `~/.claude/sessions/<pid>.json` | Which sessions run, what they're doing, and whether a dialog waits in their terminal. Claude Code writes one per process; the bar watches the directory with inotify. |
| the Claude Code plugin (`claude-plugin/`) | `PermissionRequest`, `UserPromptSubmit`, `Stop`, `StopFailure` and `SessionEnd` hooks. Each runs `curl`, posting the hook to `$XDG_RUNTIME_DIR/eave-claude.sock`. A permission request waits there for your answer, which goes back as the hook's decision. When the bar isn't running, curl fails quietly and the terminal works as before. |
| the session's transcript (`~/.claude/projects/…/<session>.jsonl`) | The conversation in its chat, read only while the chat shows: its last stretch, then each line Claude Code appends, watched with inotify. |
| the notification server, on the session bus | The notifications, and what you do with them: an action, a reply, a dismissal. |
| each session's messaging socket | Replies you type. Claude Code sessions take messages from each other there; no hook can add what you type to a session. Claude sees them as from another session, and the message says it's you. |

The session files, the transcript and the messaging socket are Claude Code's own and
undocumented (written against 2.1.293), so the bar reads them loosely and skips what it can't.
The hooks are documented.

## The mascot

Its sprites are drawn as text, in the `.px` files of the mascot repository (with its `pixel.py`).
`assets/mascot/export.py` turns them into the PNG frames here and `src/mascot.rs`:

```fish
python3 -I assets/mascot/export.py ~/code/mascot
```

## Developing

```fish
cargo test
```

To try a change in a bar, point config.toml at the checkout: `widget = "~/code/eave-claude"`.
To try the Claude Code plugin without installing it: `claude --plugin-dir ~/code/eave-claude/claude-plugin`.
