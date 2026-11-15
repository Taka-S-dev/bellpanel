# Bellpanel

A terminal dashboard for coding agents running in parallel: **Claude Code**,
**GitHub Copilot CLI** and **Codex**.

Named after the bell panel of a Victorian house: a board in the servants' hall
with a labelled window for every room. When a room rang, its window showed it,
and someone went to that room. Here the rooms are agent sessions, the bell is
the terminal bell, and going to the room is one click.

```
 bellpanel  1 waiting  1 done  1 working    claude 0s  copilot 1s  codex 0s
┌ sessions · last 20 min   ━ working  ━ waiting  ✓ finished  · idle ────────────┐
│● wait  codex   wt   infra                 2m  ······━━━━━━━━━━━━━━  shell      │
│✓ done  claude  code 日本語プロジェクト    0s  ··━━━✓····━━━━━━━━━✓  テストを書 │
│◐ work  copilot wez  api                   1m  ━━━━━━━━━━━━━━━━━━━━  > fix the  │
└────────────────────────────────────────────────────────────────────────────────┘
```

The column after the agent is the app the session runs in: `code` (VS Code),
`wt` (Windows Terminal), `wez` (WezTerm), `obs` (Obsidian).

- Sessions keep their place, in the order they appeared, so a row never moves
  as you reach for it. Press `s` to sort by urgency instead (waiting first,
  then unseen finishes); the choice is remembered.
- Each row has a timeline of the last half hour, one column per minute:
  `━` working, yellow `━` waiting for you, `✓` finished, `·` idle.
- The event log below follows the selected row and reads like that session's
  conversation: your prompts in bold, then what came back. `e` switches it to
  every session's events in one stream, then hides it.
- The terminal bell rings when a session finishes or starts waiting. A
  terminal that turns the bell into a tab mark (terminal-pane in Obsidian
  does) shows which pane wants you.
- Click a session's folder name (underlined, or press Enter) to bring its
  VS Code or terminal window to the front. Clicking elsewhere on a row only
  selects it, so clicking into the pane never switches windows.

It only watches. Approving tool calls stays with each agent, or with
[Signalpost](https://github.com/Taka-S-dev/signalpost) for Claude Code.

## Set up

Needs Rust ([rustup](https://rustup.rs/)) with the MSVC build tools.

```powershell
cargo install --path .
bellpanel install --dry-run   # see what would change
bellpanel install
```

`cargo install` puts `bellpanel` in `~/.cargo/bin`, which rustup adds to
`PATH`, so it runs from any terminal. Open a new terminal if the command is
not found yet. To update after pulling changes, run `cargo install --path .`
and then `bellpanel install` again, so the hook binary copy is refreshed too.

`cargo build --release` builds the same two binaries into `target\release\`
without installing them. To run on a machine without Rust, copy both
`bellpanel.exe` and `bellpanel-hook.exe` into one folder on its `PATH` and
run `bellpanel install` there.

`install` sets up the agents it finds: those with a `~/.claude`, `~/.copilot`
or `~/.codex` folder. The others are skipped and nothing is created for them.
`--agents claude,copilot` picks them yourself.

It copies the hook binary to `%LOCALAPPDATA%\bellpanel\bin\` and adds hooks
to `~/.claude/settings.json`, `~/.codex/hooks.json` and
`~/.copilot/hooks/bellpanel.json`. Other settings and hooks are left as they
were, and a `.bak` is kept of each file it changes. Running it again is safe,
and a file whose content would not change is not rewritten.

Then:

1. Restart running agents so they read the new hooks.
2. **Codex**: open `codex`, run `/hooks` and trust the bellpanel hooks. Codex
   skips untrusted hooks without saying so.
3. Run `bellpanel` in the terminal you keep open.

The header shows when each agent last reported. An agent stuck on `never`
after you have used it has hooks that are missing or not trusted.

`bellpanel uninstall` removes only bellpanel's hooks.

## Keys

| Key | Action |
| --- | --- |
| click the folder name, `Enter` | bring the session's window to the front and mark it seen |
| click elsewhere on a row | select it |
| point at a folder name | it lights up, and the footer says which window a click opens |
| `Space` | mark a finished session as seen |
| `d` | hide the row until the session does something new |
| `e` | event log: the selected session's conversation → every session's events → hidden |
| `s` | switch between arrival order and urgency order |
| `i` | show Nerd Font icons before agent names (needs Nerd Fonts 3.5 or later for Claude's and OpenAI's) |
| `j` / `k`, wheel | move |
| `?` | list these keys |
| `q`, `Esc` | quit |

Seen and hidden rows stay that way after a restart (`marks.jsonl`).

## Icons

`i` puts each agent's icon before its name. They come from a
[Nerd Font](https://www.nerdfonts.com/), which the terminal has to be using:

- **Nerd Fonts 3.5 or later.** Claude's and OpenAI's icons arrived in 3.5.0;
  with an older font they show as empty boxes, and only Copilot's appears.
  winget's `DEVCOM.JetBrainsMonoNerdFont` was still 3.3.0 in November 2026, so
  take the zip from the [releases page](https://github.com/ryanoasis/nerd-fonts/releases/latest).
- **Mono or not.** The `… Nerd Font Mono` family shrinks icons to one cell,
  so they look small. The plain `… Nerd Font` family draws them full size,
  spilling into the next cell; Bellpanel leaves a space there.
- **Replacing an installed version on Windows.** Close every app using the
  font first, and use *Install for all users* if the old one was installed
  that way. A file in use is not overwritten: the new one is added as
  `…_0.ttf` and the old one stays loaded until you sign out or restart.

The setting is remembered; the dashboard starts without icons, since it
cannot tell which font the terminal uses.

## Notes

- Built and tested on Windows 11 only. Bringing windows and tabs forward
  uses Windows APIs.
- A session in a terminal brings its tab forward too. In **WezTerm** this uses
  `wezterm cli` to show the tab and pane the session runs in. **Windows
  Terminal** has no supported way, so Bellpanel reads the session's tab title
  and selects the tab of that name; renamed tabs, two tabs with the same title
  and split panes fall back to just raising the window.
- `bellpanel open <agent> <dir>` does what a click does, for checking from a
  shell; `bellpanel which-host <agent> <dir>` only reports what it would open.
- Events are in `%LOCALAPPDATA%\bellpanel\events.jsonl`. Hook failures go to
  `hook-errors.log` next to it; the hook itself never prints or fails.

Why it is built this way, and the measurements behind it: `docs/DESIGN.md`
and `spike/FINDINGS.md`.
