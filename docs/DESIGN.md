# Design

Bellpanel is a terminal dashboard for coding agents running in parallel:
Claude Code, GitHub Copilot CLI and Codex. It shows which sessions are working,
waiting for the user, or finished; rings the terminal bell when one finishes or
starts waiting; and brings the session's window to the front when its row is
clicked. It is meant to stay open in a terminal pane: terminal-pane in
Obsidian, a Windows Terminal or WezTerm pane. It is Windows only.

It never answers for an agent. Approving tool calls stays with each agent's own
prompt, or with Signalpost for Claude Code.

It was called agentdash while it was being built. `install` still recognises
hooks and data left under that name and replaces or carries them over.

The facts this design rests on were measured in `spike/` and are written up,
dated, in `spike/FINDINGS.md`.

---

## 1. Shape

```
agent ──hook──▶ bellpanel-hook <agent> <event> ──append──▶ events.jsonl
                                                               │ tail
                                                     bellpanel (TUI)
                                                      ├ session list
                                                      ├ BEL on finish / waiting
                                                      └ click → focus host window
```

Two binaries from one crate:

- **`bellpanel-hook`** runs once per hook call. It reads the payload on stdin,
  turns it into one line of the common event format (§3), appends it, prints
  nothing and exits 0. It must never hold an agent up or change its behaviour.
- **`bellpanel`** is the TUI. It also carries `install` / `uninstall`.

## 2. Why a file and not a server

- Nothing is lost while the TUI is closed. It replays the file on start.
- The hook does not depend on the TUI running, so a missing or crashed TUI
  cannot slow an agent down.
- Signalpost already owns the HTTP-hook-held-open approach for approvals. A
  second server would compete with it for nothing.

Each event is appended with a single `write` to a file opened in append mode,
and lines stay short (texts are cut to 200 characters, a finished turn's
reply to 600). That concurrent hooks
never interleave inside a line is relied on, not measured.

## 3. Event format

One JSON object per line, version 1:

| Field | Meaning |
| --- | --- |
| `v` | format version, `1` |
| `at` | epoch milliseconds when the hook ran |
| `agent` | `claude`, `copilot` or `codex` |
| `kind` | `start`, `prompt`, `waiting`, `resumed`, `stop`, `end` |
| `event` | the agent's own event name, kept for debugging |
| `session` | the agent's session id |
| `turn` | turn id when the agent gives one (Codex) |
| `cwd` | working directory of the session |
| `host_pid`, `host_exe` | the nearest ancestor process owning a visible window (§5) |
| `agent_pid` | the agent's own process, to find its tab (§5) |
| `pane` | `WEZTERM_PANE`, when the window is WezTerm (§5) |
| `text` | prompt or last reply, cut to 200 characters (the reply to 600) |

Unknown events are not written: the hook only registers for the ones below.

| kind | Claude Code | Copilot CLI | Codex |
| --- | --- | --- | --- |
| start | `SessionStart` | `sessionStart` | `SessionStart` |
| prompt | `UserPromptSubmit` | `userPromptSubmitted` | `UserPromptSubmit` |
| waiting | `PermissionRequest`, `Notification` asking for permission | `notification` of type `permission_prompt` / `elicitation_dialog` | `PermissionRequest` |
| resumed | `PostToolUse` | `postToolUse` | `PostToolUse` |
| stop | `Stop` | `agentStop` | `Stop` |
| end | `SessionEnd` | `sessionEnd` | `SessionEnd` |

Copilot's `permissionRequest` is **not** used for waiting: it fires before the
rules engine, so it would also fire for calls that are allowed automatically.

## 4. Telling agents apart

The agent comes from the hook's command line, because each agent's hooks are
installed in that agent's own user-level config. One override: a camelCase
payload (`sessionId`) is always Copilot, since Copilot also reads Claude's
project-level `.claude/settings*.json` hooks in trusted folders.

Environment variables are not used: `CLAUDE_*` and `COPILOT_*` both leak into
any agent started from inside another one.

## 5. Bringing a window forward

The hook records the nearest ancestor process that owns a visible top-level
window (skipping `explorer`), and that process's executable name. For Claude in
VS Code that is `Code`; in Windows Terminal, `WindowsTerminal`; in
terminal-pane, `Obsidian`.

When that walk finds no window, the hook looks for the agent's own process
(`claude`, `copilot`, `codex`) whose current directory, read from its PEB,
is the session's `cwd`, and walks up from that instead. Codex needs this: it
runs hooks in a background app-server with no parent. Two agent processes of
the same kind in one folder cannot be told apart. `bellpanel which-host
<agent> <dir>` shows what this finds.

On click, the TUI lists that process's windows. When there are several (VS Code
runs every window in one process) it picks the one whose title contains the
session's folder name. If the process is gone, or its executable name no
longer matches (a reused pid), it falls back to any editor window titled with
the folder name.

Focusing follows the spike: attach to the foreground thread's input,
`BringWindowToTop`, `SetForegroundWindow`, detach; act on mouse **up**; check
`GetForegroundWindow` afterwards and retry once.

### Tabs

The hook also records the agent's own process (`agent_pid`) and, inside
WezTerm, `WEZTERM_PANE`. The pane id only counts when the window is
`wezterm-gui`: anything started from a WezTerm pane inherits it, VS Code
included.

- **WezTerm**: `wezterm cli activate-tab` for the tab holding the pane (found
  in `wezterm cli list --format json`), then `activate-pane`, with the CLI
  found next to `wezterm-gui.exe` and `--no-auto-start`. Supported and exact.
  But the 20240203 CLI on Windows cannot find the GUI by itself: it connects
  to the bare name `gui-sock-<pid>` and fails ("failed to connect"), from
  anywhere. Measured 2026-11-15: setting `WEZTERM_UNIX_SOCKET` to
  `%USERPROFILE%\.local\share\wezterm\gui-sock-<pid>`, or running in that
  folder, reaches it. Both are done.
- **Windows Terminal**: no supported way. A request to focus a tab by session
  was declined (microsoft/terminal#19783). The workaround follows perch
  (ArcticGizmo/perch#102): read the title of the agent's console, then find
  the `TabItem` of that name through UI Automation and `Select()` it.
  Reading another console's title means `FreeConsole` + `AttachConsole`,
  which the dashboard cannot do to itself, so a helper process
  (`bellpanel console-title <pid>`, started detached) does it. Leading status
  glyphs (`✳`, `◐`) are ignored when matching. No match, or two tabs with the
  same title, leaves the tab alone; the window still comes forward.
  Measured 2026-11-15: titles read back as shown on the tabs
  (`Clarify aaa | <folder>` for Codex, `aaa - GitHub Copilot` for Copilot), and
  selecting either tab reported success.

The tab is selected before the window is raised, so the window comes forward
already showing it.

## 6. Session state

Per session, the latest `kind` decides the state:

| last kind | state | shown as |
| --- | --- | --- |
| start | idle | dim |
| prompt, resumed | working | spinner |
| waiting | **waiting** | top of the list |
| stop | **done** until seen | below waiting |
| end | ended | hidden after 1 minute, unless it finished and is not yet seen |

A session with no window (an agent run by a script, `claude -p`) is hidden a
minute after it ends even when its finish was not seen: nobody is waiting for
it. A session with no event for 12 hours is hidden.

Rows are listed in the order sessions appeared, so none moves while the
pointer is on its way to it; waiting and unseen finishes stand out by colour
and in the header counts. `s` switches to urgency order (the table's order
above), and the choice is kept in `settings.json`.

"Seen" means the row was opened or acknowledged with a key. Seen and hidden
rows are written to `marks.jsonl` and re-applied after the events are
replayed, so a restart does not bring back what was dealt with.

Only the folder name is a link. A click elsewhere on a row selects it: the
pane has to be clicked to get the keyboard, and that click must not switch
windows.

Duplicate lines (same session, kind and turn within 2 s) are dropped: a hook
defined at both user and project level runs twice under Codex.

### An approved command

No agent sends a hook between the user approving a tool call and the command
running; the next is `PostToolUse`, when it ends (`spike/FINDINGS.md`). Left at
that, a session would show *waiting* for as long as the approved command runs.
So while any session waits, the TUI lists processes once a second
(`watch.rs`) and treats a new one as the wait being answered when it:

- appeared after polling began and at least 1.5 s after the wait began (the
  hooks reporting the wait run right then), and has run for 2 s (bellpanel's
  own hook takes under 0.3 s; another tool's hook running longer than 2 s
  during a wait would be taken for the command);
- is not itself the agent's executable (that is a new session);
- has the session's `agent_pid` among its ancestors (Claude Code, Copilot
  CLI), or else has a process of the same agent among them and runs in the
  session's folder, read from its PEB (Codex, whose commands run under its
  shared app-server). When that folder cannot be read, as a sandboxed
  command may not allow, it counts only if no other session of that agent is
  waiting or working.

The session then shows *working*, and its log shows the wait as approved.
This is kept in memory only: after a restart a session still running its
command shows *waiting* until `PostToolUse` arrives. Two sessions of one agent
in one folder cannot be told apart, as in §5.

### Event log

The log under the sessions follows the selected row: that session's last 50
prompts, waits and replies, oldest first, so it reads as the conversation
without opening the window. `e` switches to each listed session's latest
event in the list's order, to see what every terminal is at: the log's height
is shared out between them, short texts leaving their lines to long ones,
each text wraps at the pane's full width under
a line naming the session, Markdown marks and table pipes are dropped, and a finish
with no text (Copilot's) shows the prompt it answered;
then to one stream of every session's events (the last 200); then hides the
log so the sessions get the whole pane. The choice is kept in
`settings.json`. Routine tool completions are left out of all of them; only
one that ends a wait is shown, as "approved".

A seen mark covers the session up to its last *finish*: an end or a tool event
arriving later does not make it unseen again, only a new finish does.

## 7. Bell

The TUI writes BEL when a session moves into *waiting* or *done* while it is
tailing, never while replaying the file at start-up. terminal-pane turns that
into a blinking dot on the tab and a flashing taskbar button, but only when the
dashboard's terminal does not have the keyboard, which is the case it is for.

Once no listed session is waiting, the TUI withdraws the request with
iTerm2's `OSC 1337;RequestAttention=no`, sent once per bell that a wait rang.
A wait answered in the agent's own window, or a row dismissed here, then
clears the tab mark without a visit to the dashboard. Unseen finishes do not
hold it back, since the point is that answering clears the mark; a bell for a
finish alone is never withdrawn. terminal-pane clears its mark
and stops the taskbar flash on it; terminals that do not know the sequence
ignore it. An answered wait is known once the approved command has run for
about two seconds (§6, An approved command).

## 8. Silent failures to surface

Copilot and Codex skip hooks they do not trust, without a word on screen. So
the header shows, per agent, when its last event arrived, and "never" in
place of a time. "codex: never" right after installing means its hooks still
need trusting with `/hooks`.

## 9. Install

`bellpanel install` copies `bellpanel-hook.exe` to
`%LOCALAPPDATA%\bellpanel\bin\` (so rebuilding does not move or lock the path
the agents call), then adds the hooks to the agents it finds, those whose
config folder exists (`--agents` chooses instead):

| Agent | File | How |
| --- | --- | --- |
| Claude Code | `~/.claude/settings.json` | merged into `hooks`, `.bak` kept |
| Copilot CLI | `~/.copilot/hooks/bellpanel.json` | a file of its own |
| Codex | `~/.codex/hooks.json` | merged into `hooks`, `.bak` kept |

It is idempotent: earlier bellpanel entries (and the spike's `log-event.mjs`
entries) are removed before the new ones go in. `--dry-run` prints the result
without writing. `uninstall` removes only bellpanel's entries.

The command path uses forward slashes and is quoted only when it contains a
space, because Claude Code runs hook commands through Git Bash on Windows,
where unquoted backslashes are eaten.

## 10. Data location

`%LOCALAPPDATA%\bellpanel\events.jsonl`, or `$BELLPANEL_HOME` when set (tests
use this). The TUI reads at most the last 2 MiB on start. Past 10 MiB the hook moves the
file to `events.1.jsonl`, replacing the one before, and starts a new one; the
TUI notices the file changing under it by its first bytes, since a new file
can be as large as the old one by the time it looks.
