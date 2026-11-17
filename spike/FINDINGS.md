# Spike findings

Measured 2026-11-15 on Windows 11 with Claude Code 2.1.325, GitHub Copilot CLI
1.0.112 and codex-cli 0.157.1. Every hook ran `node log-event.mjs`, which
appends the raw stdin payload to `events.jsonl` and prints nothing.

## Claude Code

Project hooks in `.claude/settings.local.json` fired under `claude -p`:
`SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd`. Payload is
snake_case: `session_id`, `transcript_path`, `cwd`, `hook_event_name`.

## Copilot CLI

- **Repository hooks are skipped in an untrusted folder.** Neither
  `.github/hooks/*.json` nor `.claude/settings*.json` fired. The reason is only
  in `~/.copilot/logs/`: "Skipped repository hooks ... because the folder ... is
  not trusted". Nothing is printed to the terminal.
- User hooks in `~/.copilot/hooks/*.json` fire anywhere: `sessionStart`,
  `userPromptSubmitted`, `agentStop`, `sessionEnd`.
- Payload is camelCase when the event is configured in camelCase: `sessionId`,
  `cwd`, `timestamp` (epoch ms), `transcriptPath`, `stopReason: "end_turn"`,
  `reason: "complete"` on `sessionEnd`.
- The payload arrives on stdin (the docs never say so).
- The hook process gets `COPILOT_CLI`, `COPILOT_HOOK_DIR`,
  `COPILOT_PROJECT_DIR`. Use these, not the config file the hook came from, to
  tell which agent called: Copilot also reads Claude's `.claude/settings*.json`
  hooks in a trusted folder, so a Claude hook can be fired by Copilot.

## Codex

- `~/.codex/hooks.json` is read (a `SessionEnd` timeout of 10 was clamped to
  3 with a warning), but **no hook ran under `codex exec`**: untrusted hooks are
  skipped silently. Trust them once with `/hooks` in interactive `codex`.
  Trust is stored per hook as `[hooks.state.'<file>:<event>:0:0'] trusted_hash`
  in `~/.codex/config.toml`, so editing a hook's command untrusts it again.
- Once trusted, `codex exec` fires `SessionStart` (`source: "startup"`),
  `UserPromptSubmit` (`turn_id`, `prompt`), `Stop` (`turn_id`,
  `last_assistant_message`), `SessionEnd` (`reason: "other"`). Payload is
  snake_case like Claude's, plus `model` and `permission_mode`.
- **A hook defined in both `~/.codex/hooks.json` and `<repo>/.codex/hooks.json`
  runs twice**, about 1 ms apart. Install user-level only, and dedupe on
  `(session_id, hook_event_name, turn_id)` anyway.
- Environment has `CODEX_MANAGED_BY_NPM` / `CODEX_MANAGED_PACKAGE_ROOT`, which
  only an npm install sets; not a reliable marker. Tell Codex apart by payload
  instead (`model` and `permission_mode` together).
- Two Codex installs are on this machine: `codex` on the Git Bash PATH is
  0.157.1, the one in Windows Terminal is 0.169.0.
- Interactive `codex` may stop at "Background server has incompatible feature
  settings"; "Run without daemon this time" leaves the shared daemon alone.

## Telling the agents apart

| | Payload case | Marker |
| --- | --- | --- |
| Claude Code | snake_case | env `CLAUDE_PROJECT_DIR` |
| Copilot CLI | camelCase (`sessionId`) | env `COPILOT_CLI` |
| Codex | snake_case | payload has `model` and `permission_mode` |

Check in the order Copilot, Codex, Claude. `CLAUDE_*` variables leak into any
agent started from inside a Claude Code session (a Copilot run started that
way carried them all), so a Claude marker alone proves nothing.

## Focusing a window from a TUI inside terminal-pane

`focus-probe` runs as Obsidian -> pty-host -> conpty -> probe, and is clicked
while Obsidian is in front.

- **Plain `SetForegroundWindow` fails**: 31 of 32 clicks returned false and
  nothing moved. The foreground lock does not count the probe as the
  foreground process, even though the click came through Obsidian.
- **`AttachThreadInput` to the foreground thread, then `BringWindowToTop` +
  `SetForegroundWindow`** always returns true.
- **Act on mouse-up, not mouse-down.** On mouse-down only 5 of 13 clicks were
  still in front 300 ms later: Obsidian took focus back when the button was
  released. On mouse-up, about 30 consecutive clicks stayed in front at 1 s.
  Early clicks still missed now and then, so check `GetForegroundWindow`
  afterwards and retry once.
- Keyboard input goes to the focused window, so it is a real activation.
- UWP windows (`ApplicationFrameHost`, e.g. Settings) report success but do not
  come forward. Not a target, so not pursued.

## Marking the terminal-pane tab

- BEL and `OSC 777;notify` written by the TUI reach xterm.js through conpty
  (checked with node-pty: one standalone BEL after `b`, one OSC 777 after `n`).
- terminal-pane marks the tab only when that terminal **does not have the
  keyboard** or Obsidian is in the background. A test run with the probe still
  focused shows nothing, which looked like a failure and was not.
- Confirmed in Obsidian DevTools: BEL -> `termFocus: false` -> `attention: true`
  -> one `.terminal-pane-tab-mark` drawn.

## Side findings in terminal-pane (separate from this project)

- Some PSReadLine token colors (numbers, part of a command name) render in the
  background color, so typed or pasted text looks missing.

## Not yet measured

- Claude Code's and Codex's waiting events in real use. Copilot's were seen
  later: a `notification` of type `elicitation_dialog`, then `postToolUse`.

## After installing (2026-11-15)

The tool was still called agentdash here; commands below use the current
name.

- Claude Code's `Stop` payload carries `last_assistant_message` too, not only
  Codex's. Claude sessions already running picked up the new user-level hooks
  without a restart.
- Host detection walks the parent chain until a process with a visible window.
  It stops where a parent has exited: run from a short-lived shell, Copilot's
  chain ends at that shell (`pwsh <- copilot <- node <- sh`, sh's parent gone),
  so no host is recorded and clicking falls back to the folder name in window
  titles. Set `BELLPANEL_TRACE=1` to log the chain to `hook-errors.log`.

## Codex runs hooks in its background server (2026-11-15)

- An interactive `codex` in Windows Terminal is `codex <- node <- pwsh <-
  WindowsTerminal`, but its hooks run in `codex app-server --managed-daemon`,
  which has no parent. Walking up from the hook finds no window, so every
  Codex event was recorded without a host and clicking did nothing.
- Fix: when the walk fails, find the agent's own process (`codex.exe`) whose
  current directory equals the hook's `cwd`, and walk up from that. The
  directory is read from the process's PEB (`ProcessParameters` +0x20,
  `CurrentDirectory.DosPath` +0x38 on x64). Checked with
  `bellpanel which-host codex %USERPROFILE%` -> `WindowsTerminal`.
- The click does the same lookup for sessions recorded without a host, so
  earlier rows open too while their agent is still running.

## Nothing reports an approved command starting (2026-11-17)

Measured with every tool event logged (`spike/sandbox`) and new shell
processes sampled every 200 ms, asking each agent to run
`Start-Sleep 15; echo done`.

- Copilot CLI 1.0.112: `preToolUse` (powershell), then `permissionRequest`,
  then `notification` of type `permission_prompt`, all within 2 s and
  **before** the prompt is answered. Approving sends nothing. The command's
  `pwsh` starts as a child of `copilot.exe` the moment it is approved, and
  `postToolUse` arrives only when it ends, 15 s later. A session waits on
  screen for as long as the command runs.
- In a trusted folder Copilot also ran Claude's project hooks from
  `.claude/settings.local.json`, about 0.4 s after its own.
- Claude Code 2.1.325 and Codex 0.169.0 did not ask: the command was allowed
  without a prompt, so their approval was not seen. Where their commands run
  was:
  - Claude Code: `pwsh` as a child of `claude.exe`, the process the hook
    records as `agent_pid`.
  - Codex: `powershell` under `codex-command-runner-0.169.0.exe` under the
    parentless `codex.exe` app-server, not under the TUI recorded as
    `agent_pid`. The app-server is shared by every Codex session.
- Hooks also run as children of the agent (`pwsh -nop -nol -c ...` for
  Copilot, Git Bash for Claude). Only their start was sampled here; their
  length was timed the next day (2026-11-18) by running bellpanel's hook
  with the same command lines by hand, 5 runs each: 260 to 300 ms through
  `pwsh -nop -nol -c`, about 80 ms through Git Bash. Other tools' hooks can
  take longer.

## Nerd Font icons on Windows (2026-11-15)

- Icons used: `cod-claude` U+EC82 and `cod-openai` U+EC81 (Nerd Fonts 3.5.0+,
  absent from 3.4.0's `glyphnames.json`),
  `cod-copilot` U+EC1E (earlier). Checked against `glyphnames.json` 3.5.1 and
  the installed fonts' cmap with fontTools.
- winget's `DEVCOM.JetBrainsMonoNerdFont` offered only 3.3.0.
- Installing 3.5.1 "for all users" over a 3.3.0 that apps had open replaced
  the Mono and Propo files but not the plain ones: Windows wrote the new
  plain files as `JetBrainsMonoNerdFont-*_0.ttf` and pointed the
  `HKLM\...\Fonts` entries at them, while the running session kept the old
  files loaded, so the terminal still drew 3.3.0.
- Without restarting: `RemoveFontResourceW` on each old file (repeated until
  it returns false), `AddFontResourceW` on each `_0` file, then broadcast
  `WM_FONTCHANGE`, and restart the app. 16 weights swapped this way; after
  reopening Obsidian, terminal-pane drew the 3.5.1 icons, full size.
- Mono shrinks icons to one cell; the plain family draws them larger.
