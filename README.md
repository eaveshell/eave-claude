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

- **In the bar:** the Claude icon while sessions run, dimmed while none works or waits, with a
  count of the ones waiting for you.
- **Permission prompts:** Allow, Deny (Claude stops, like Esc in the terminal), or Instead… to
  tell Claude what to do instead. The popover also has Allow, and don't ask again.
- **Questions** (AskUserQuestion): a question with two short options gets them as buttons, and
  Other… for an answer of your own. Longer ones open the popover, one list per question.
- **Plans:** Approve, or Change… to send it back with what should change.
- **Done:** when a session finishes its turn, its last reply, and Reply to answer it. Clicking it
  focuses the session's terminal.
- **The popover:** your sessions, with what each is doing. Each one's page has what it waits for,
  its last reply, a field to message it, and Show to focus its terminal.

Whatever you answer in the bar, the terminal shows the same prompt meanwhile, and whichever you
answer first wins. Options: `done` (say so when a turn finishes, default `true`) and `timeout`
(seconds that shows, default 8).

The bar shows the icon called `claude` in your icon theme, and a C if there is none. To see the
logo, put one at `~/.local/share/icons/hicolor/scalable/apps/claude.svg`.

## How it works

| From | What |
|---|---|
| `~/.claude/sessions/<pid>.json` | Which sessions run, what they're doing, and whether a dialog waits in their terminal. Claude Code writes one per process; the bar watches the directory with inotify. |
| the Claude Code plugin (`claude-plugin/`) | `PermissionRequest`, `UserPromptSubmit`, `Stop`, `StopFailure` and `SessionEnd` hooks. Each runs `curl`, posting the hook to `$XDG_RUNTIME_DIR/eave-claude.sock`. A permission request waits there for your answer, which goes back as the hook's decision. When the bar isn't running, curl fails quietly and the terminal works as before. |
| each session's messaging socket | Replies you type. Claude Code sessions take messages from each other there; no hook can add what you type to a session. Claude sees them as from another session, and the message says it's you. |

The session files, the transcript's title and the messaging socket are Claude Code's own and
undocumented (written against 2.1.293), so the bar reads them loosely and skips what it can't.
The hooks are documented.

## Developing

```fish
cargo test
```

To try a change in a bar, point config.toml at the checkout: `widget = "~/code/eave-claude"`.
To try the Claude Code plugin without installing it: `claude --plugin-dir ~/code/eave-claude/claude-plugin`.
