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
  | coffee | idle a while, by day (dimmed) |
  | sleeping | idle a while, between 22:00 and 07:00 (dimmed) |
- **Permission prompts:** Allow, Deny (Claude stops, like Esc in the terminal), or type into the
  field what Claude should do instead. The chat also has Allow, and don't ask again.
- **Questions** (AskUserQuestion): a question with two short options gets them as buttons, and
  the field takes an answer of your own. Longer ones open the chat, one question at a time with
  arrows between them: picking an option moves on, and answering the last one sends them all.
- **Plans:** the plan, in Markdown, with Approve, or type what should change to send it back.
  Approving goes back to the mode the session was in before planning, as in the terminal.
- **Done:** when a session finishes its turn, its last reply, with a field to answer it.
- **The popover:** your sessions, each with its mascot, and what it's doing. A click opens its chat: the
  conversation as it happens, Markdown and all, the tools it runs, what it's doing now, what it
  asks you, and a field to message it. Show focuses its terminal. Clicking a notification opens
  its session's chat.

Whatever you answer in the bar, the terminal shows the same prompt meanwhile, and whichever you
answer first wins. Options: `done` (say so when a turn finishes, default `true`) and `timeout`
(seconds that shows, default 8).

Notifications go over the session bus like any app's, so they show in eave with
[eave-notifications](https://github.com/eaveshell/eave-notifications), or in mako, dunst or
swaync. They carry the mascot, in the mood that fits (asking, or happy when done), and Markdown bodies, which eave sets
as such. In eave, the field to answer is right on the notification; the bar takes the keyboard
only once you click it.

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
