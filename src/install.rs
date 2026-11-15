//! Adding and removing bellpanel's hooks in each agent's user config
//! (DESIGN.md §9). The JSON edits are pure functions; `plan` and `apply` do the I/O.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::event::Agent;

pub const CLAUDE_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "Notification",
    "PostToolUse",
    "Stop",
    "SessionEnd",
];
pub const COPILOT_EVENTS: [&str; 6] = [
    "sessionStart",
    "userPromptSubmitted",
    "notification",
    "postToolUse",
    "agentStop",
    "sessionEnd",
];
pub const CODEX_EVENTS: [&str; 6] = [
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
    "SessionEnd",
];

/// Codex caps `SessionEnd` at 3 s; the hook takes milliseconds anyway.
const TIMEOUT_SECS: u64 = 3;

/// Commands that are ours to replace: this tool's, its earlier name's
/// (agentdash), and the spike's logger.
fn is_ours(command: &str) -> bool {
    ["bellpanel-hook", "agentdash-hook", "log-event.mjs"]
        .iter()
        .any(|name| command.contains(name))
}

/// Forward slashes survive Git Bash, which Claude Code runs hooks through on
/// Windows; quotes only when needed, since a quoted first word is a string,
/// not a command, in PowerShell.
pub fn command_line(exe: &Path, agent: Agent, event: &str) -> String {
    let path = exe.to_string_lossy().replace('\\', "/");
    let program = if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path
    };
    format!("{program} {} {event}", agent.name())
}

fn object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("just made an object")
}

/// Removes our commands from a Claude/Codex-style `hooks` table, dropping the
/// groups and events that removal leaves empty. Anything that was already
/// empty, or never held ours, is kept exactly as it was.
fn strip_grouped(hooks: &mut Map<String, Value>) {
    let mut emptied = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let Some(list) = groups.as_array_mut() else {
            continue;
        };
        let mut touched = false;
        list.retain_mut(|group| {
            let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = inner.len();
            inner.retain(|h| {
                !h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_ours)
            });
            let removed = inner.len() != before;
            touched |= removed;
            !(removed && inner.is_empty())
        });
        if touched && list.is_empty() {
            emptied.push(event.clone());
        }
    }
    for event in emptied {
        hooks.remove(&event);
    }
}

/// Claude Code's `settings.json` and Codex's `hooks.json` share one shape:
/// `{"hooks": {"<Event>": [{"hooks": [{"type": "command", ...}]}]}}`.
pub fn with_grouped_hooks(
    mut config: Value,
    exe: Option<&Path>,
    agent: Agent,
    events: &[&str],
) -> Value {
    let root = object(&mut config);
    // Keep a `hooks` table that was already there, even an empty one.
    let had_table = root
        .get("hooks")
        .is_some_and(|h| h.as_object().is_some_and(Map::is_empty));
    let hooks = object(root.entry("hooks").or_insert_with(|| json!({})));
    strip_grouped(hooks);
    if let Some(exe) = exe {
        for event in events {
            let entry = json!({ "hooks": [{
                "type": "command",
                "command": command_line(exe, agent, event),
                "timeout": TIMEOUT_SECS,
            }]});
            match hooks.entry(event.to_string()).or_insert_with(|| json!([])) {
                Value::Array(list) => list.push(entry),
                other => *other = json!([entry]),
            }
        }
    }
    if hooks.is_empty() && !had_table {
        root.remove("hooks");
    }
    config
}

/// Copilot reads every `*.json` in `~/.copilot/hooks/`, so it gets a file of its own.
pub fn copilot_file(exe: &Path) -> Value {
    let path = exe.to_string_lossy().replace('\\', "/");
    let mut hooks = Map::new();
    for event in COPILOT_EVENTS {
        hooks.insert(
            event.to_string(),
            json!([{
                "type": "command",
                "bash": format!("'{path}' copilot {event}"),
                "powershell": format!("& '{path}' copilot {event}"),
                "timeoutSec": TIMEOUT_SECS,
            }]),
        );
    }
    json!({ "version": 1, "hooks": hooks })
}

pub struct Change {
    pub path: PathBuf,
    pub before: Option<String>,
    /// `None` deletes the file.
    pub after: Option<String>,
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

pub fn installed_hook_path() -> PathBuf {
    crate::store::data_dir().join("bin").join(if cfg!(windows) {
        "bellpanel-hook.exe"
    } else {
        "bellpanel-hook"
    })
}

fn read_json(path: &Path) -> io::Result<(Option<String>, Value)> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: not valid JSON ({e}); left untouched", path.display()),
                )
            })?;
            Ok((Some(text), value))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((None, json!({}))),
        Err(e) => Err(e),
    }
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("JSON values serialize");
    text.push('\n');
    text
}

/// The folder each agent keeps its user config in.
fn config_dir(home: &Path, agent: Agent) -> PathBuf {
    home.join(match agent {
        Agent::Claude => ".claude",
        Agent::Copilot => ".copilot",
        Agent::Codex => ".codex",
    })
}

/// Agents that look installed: their config folder exists. Install only
/// touches these, so nobody gets a `~/.codex` for a tool they never used.
pub fn present_agents() -> Vec<Agent> {
    let home = home();
    Agent::ALL
        .into_iter()
        .filter(|&a| config_dir(&home, a).is_dir())
        .collect()
}

/// Agents whose config currently calls this tool's hook.
pub fn installed_agents() -> Vec<Agent> {
    let home = home();
    let calls_us =
        |path: PathBuf| fs::read_to_string(path).is_ok_and(|text| text.contains("bellpanel-hook"));
    Agent::ALL
        .into_iter()
        .filter(|&agent| match agent {
            Agent::Claude => calls_us(config_dir(&home, agent).join("settings.json")),
            Agent::Codex => calls_us(config_dir(&home, agent).join("hooks.json")),
            Agent::Copilot => calls_us(
                config_dir(&home, agent)
                    .join("hooks")
                    .join("bellpanel.json"),
            ),
        })
        .collect()
}

/// What installing (`exe` = Some) for `agents`, or uninstalling (`exe` = None)
/// from every agent, would change.
pub fn plan(exe: Option<&Path>, agents: &[Agent]) -> io::Result<Vec<Change>> {
    let home = home();
    let mut changes = Vec::new();
    let wanted = |agent: Agent| exe.is_none() || agents.contains(&agent);

    for (agent, path, events) in [
        (
            Agent::Claude,
            config_dir(&home, Agent::Claude).join("settings.json"),
            &CLAUDE_EVENTS[..],
        ),
        (
            Agent::Codex,
            config_dir(&home, Agent::Codex).join("hooks.json"),
            &CODEX_EVENTS[..],
        ),
    ] {
        if !wanted(agent) {
            continue;
        }
        let (before, value) = read_json(&path)?;
        // Nothing to remove from a file that is not there.
        if exe.is_none() && before.is_none() {
            continue;
        }
        let updated = with_grouped_hooks(value.clone(), exe, agent, events);
        // Same content, maybe different layout: leave the user's file as it is.
        if before.is_some() && updated == value {
            continue;
        }
        // Uninstalling from a file only we wrote leaves nothing worth keeping.
        let after = if exe.is_none() && agent == Agent::Codex && updated == json!({}) {
            None
        } else {
            Some(pretty(&updated))
        };
        if before.is_some() || after.is_some() {
            changes.push(Change {
                path,
                before,
                after,
            });
        }
    }

    let copilot_dir = config_dir(&home, Agent::Copilot).join("hooks");
    let copilot = copilot_dir.join("bellpanel.json");
    let before = fs::read_to_string(&copilot).ok();
    let after = exe
        .filter(|_| wanted(Agent::Copilot))
        .map(|exe| pretty(&copilot_file(exe)));
    if (before.is_some() && exe.is_none()) || after.is_some() {
        changes.push(Change {
            path: copilot,
            before,
            after,
        });
    }
    // The file the tool wrote under its earlier name would fire alongside the new one.
    let legacy = copilot_dir.join("agentdash.json");
    if let Ok(before) = fs::read_to_string(&legacy) {
        changes.push(Change {
            path: legacy,
            before: Some(before),
            after: None,
        });
    }

    changes.retain(|c| c.before != c.after);
    Ok(changes)
}

/// Writes the changes, keeping a `.bak` of every file that existed.
pub fn apply(changes: &[Change]) -> io::Result<()> {
    for change in changes {
        if change.before.is_some() {
            let mut bak = change.path.clone().into_os_string();
            bak.push(".bak");
            fs::copy(&change.path, PathBuf::from(bak))?;
        }
        match &change.after {
            Some(text) => {
                if let Some(dir) = change.path.parent() {
                    fs::create_dir_all(dir)?;
                }
                fs::write(&change.path, text)?;
            }
            None => fs::remove_file(&change.path)?,
        }
    }
    Ok(())
}

/// Files kept by the tool under its earlier name, `agentdash`.
const LEGACY_FILES: [&str; 4] = [
    "events.jsonl",
    "events.1.jsonl",
    "marks.jsonl",
    "settings.json",
];

/// Copies events, marks and settings from the earlier name's data folder,
/// when the new one does not have them yet. Returns the files copied.
pub fn migrate_legacy_data() -> io::Result<Vec<PathBuf>> {
    let (Some(old), new) = (crate::store::legacy_data_dir(), crate::store::data_dir()) else {
        return Ok(Vec::new());
    };
    let mut copied = Vec::new();
    for name in LEGACY_FILES {
        let (from, to) = (old.join(name), new.join(name));
        if from.exists() && !to.exists() {
            fs::create_dir_all(&new)?;
            fs::copy(&from, &to)?;
            copied.push(to);
        }
    }
    Ok(copied)
}

/// Copies the hook binary next to the data, so rebuilding never moves or locks
/// the path the agents call. A running copy cannot be overwritten on Windows,
/// but it can be renamed out of the way.
pub fn copy_hook_binary() -> io::Result<PathBuf> {
    let name = if cfg!(windows) {
        "bellpanel-hook.exe"
    } else {
        "bellpanel-hook"
    };
    let source = std::env::current_exe()?.with_file_name(name);
    if !source.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} not found; build both binaries", source.display()),
        ));
    }
    let target = installed_hook_path();
    fs::create_dir_all(target.parent().expect("has a parent"))?;
    if target.exists() {
        let old = target.with_extension("old");
        let _ = fs::remove_file(&old);
        fs::rename(&target, &old)?;
    }
    fs::copy(&source, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Users\me\AppData\Local\bellpanel\bin\bellpanel-hook.exe")
    }

    #[test]
    fn command_lines_use_forward_slashes_and_quote_only_spaces() {
        assert_eq!(
            command_line(&exe(), Agent::Claude, "Stop"),
            "C:/Users/me/AppData/Local/bellpanel/bin/bellpanel-hook.exe claude Stop"
        );
        assert_eq!(
            command_line(Path::new(r"C:\My Tools\h.exe"), Agent::Codex, "Stop"),
            "\"C:/My Tools/h.exe\" codex Stop"
        );
    }

    #[test]
    fn install_keeps_other_settings_and_hooks() {
        let settings = json!({
            "model": "opus",
            "hooks": { "PermissionRequest": [{ "hooks": [{ "type": "http", "url": "http://127.0.0.1:1/hook/t/permission" }] }] }
        });
        let out = with_grouped_hooks(settings, Some(&exe()), Agent::Claude, &CLAUDE_EVENTS);
        assert_eq!(out["model"], "opus");
        let permission = out["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(permission.len(), 2, "Signalpost's hook stays next to ours");
        assert_eq!(permission[0]["hooks"][0]["type"], "http");
        assert_eq!(out["hooks"].as_object().unwrap().len(), CLAUDE_EVENTS.len());
    }

    #[test]
    fn install_twice_is_install_once() {
        let once = with_grouped_hooks(json!({}), Some(&exe()), Agent::Codex, &CODEX_EVENTS);
        let twice = with_grouped_hooks(once.clone(), Some(&exe()), Agent::Codex, &CODEX_EVENTS);
        assert_eq!(once, twice);
    }

    #[test]
    fn install_replaces_the_spike_logger() {
        let spike = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "node \"C:/x/spike/log-event.mjs\" codex Stop"}]}]}});
        let out = with_grouped_hooks(spike, Some(&exe()), Agent::Codex, &CODEX_EVENTS);
        let stop = out["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(stop[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("bellpanel-hook"));
    }

    #[test]
    fn uninstall_removes_only_ours_and_empty_tables() {
        let mixed = json!({"hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "other-tool"}]}],
            "SessionEnd": [{"hooks": [{"type": "command", "command": "C:/a/bellpanel-hook.exe claude SessionEnd"}]}]
        }});
        let out = with_grouped_hooks(mixed, None, Agent::Claude, &CLAUDE_EVENTS);
        assert_eq!(
            out,
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "other-tool"}]}]}})
        );
        assert_eq!(
            with_grouped_hooks(out.clone(), Some(&exe()), Agent::Claude, &CLAUDE_EVENTS)["hooks"]
                ["Stop"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let only_ours = with_grouped_hooks(json!({}), Some(&exe()), Agent::Codex, &CODEX_EVENTS);
        assert_eq!(
            with_grouped_hooks(only_ours, None, Agent::Codex, &CODEX_EVENTS),
            json!({})
        );
    }

    #[test]
    fn empty_entries_that_were_not_ours_are_left_alone() {
        // Found in a real settings.json: an empty event list left by another tool.
        let settings =
            json!({"hooks": {"PermissionDenied": [], "Stop": [{"matcher": "", "hooks": []}]}});
        let installed = with_grouped_hooks(
            settings.clone(),
            Some(&exe()),
            Agent::Claude,
            &CLAUDE_EVENTS,
        );
        assert_eq!(installed["hooks"]["PermissionDenied"], json!([]));
        assert_eq!(
            with_grouped_hooks(installed, None, Agent::Claude, &CLAUDE_EVENTS),
            settings
        );
        assert_eq!(
            with_grouped_hooks(json!({"hooks": {}}), None, Agent::Claude, &CLAUDE_EVENTS),
            json!({"hooks": {}})
        );
    }

    #[test]
    fn install_replaces_hooks_from_the_earlier_name() {
        let old = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "C:/x/agentdash/bin/agentdash-hook.exe claude Stop"}]}]}});
        let out = with_grouped_hooks(old, Some(&exe()), Agent::Claude, &CLAUDE_EVENTS);
        let stop = out["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(stop[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("bellpanel-hook"));
    }

    #[test]
    fn copilot_gets_a_command_per_shell() {
        let file = copilot_file(&exe());
        assert_eq!(file["version"], 1);
        let stop = &file["hooks"]["agentStop"][0];
        assert_eq!(
            stop["powershell"],
            "& 'C:/Users/me/AppData/Local/bellpanel/bin/bellpanel-hook.exe' copilot agentStop"
        );
        assert!(stop["bash"].as_str().unwrap().starts_with('\''));
    }
}
