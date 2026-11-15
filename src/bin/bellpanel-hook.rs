//! Called by an agent's hook: `bellpanel-hook <agent> <event>`, payload on stdin.
//!
//! Appends one event and exits 0 without printing, whatever happens. Any output
//! could be read by the agent as a hook decision, and a non-zero exit can
//! block a turn, so failures go to `hook-errors.log` instead.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bellpanel::event::{normalize, Agent};
use bellpanel::{host, store};

/// Some callers may keep stdin open; never hold the agent up waiting for it.
const STDIN_TIMEOUT: Duration = Duration::from_secs(2);

fn main() {
    if let Err(message) = run() {
        log_error(message);
    }
}

fn log_error(message: String) {
    let dir = store::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("hook-errors.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{} {message}", now_ms());
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_stdin() -> Result<String, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut raw = String::new();
        let _ = tx.send(std::io::stdin().read_to_string(&mut raw).map(|_| raw));
    });
    match rx.recv_timeout(STDIN_TIMEOUT) {
        Ok(Ok(raw)) => Ok(raw),
        Ok(Err(e)) => Err(format!("stdin: {e}")),
        Err(_) => Err("stdin: timed out".into()),
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [agent, event] = args.as_slice() else {
        return Err(format!(
            "usage: bellpanel-hook <agent> <event>, got {args:?}"
        ));
    };
    let agent = Agent::parse(agent).ok_or_else(|| format!("unknown agent {agent}"))?;
    let at = now_ms();
    let raw = read_stdin()?;
    let payload: serde_json::Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("{event}: bad payload: {e}"))?;

    let Some(mut record) = normalize(agent, event, &payload, at) else {
        return Ok(());
    };
    // BELLPANEL_TRACE=1 records the process chain, to see where host
    // detection stops for an agent.
    if std::env::var_os("BELLPANEL_TRACE").is_some() {
        let chain: Vec<String> = host::ancestry()
            .iter()
            .map(|a| format!("{}:{}{}", a.exe, a.pid, if a.has_window { "*" } else { "" }))
            .collect();
        log_error(format!(
            "trace {} {event}: {}",
            agent.name(),
            chain.join(" <- ")
        ));
    }
    if let Some(found) = host::host_process(record.agent.exe(), &record.cwd) {
        // WEZTERM_PANE is inherited by anything started from a WezTerm pane,
        // VS Code included, so it only counts when WezTerm is the window.
        if found.exe.eq_ignore_ascii_case("wezterm-gui") {
            record.pane = std::env::var("WEZTERM_PANE").ok().filter(|p| !p.is_empty());
        }
        record.host_pid = Some(found.pid);
        record.host_exe = Some(found.exe);
        record.agent_pid = found.agent_pid;
    }
    store::append(&store::events_path(), &record).map_err(|e| format!("append: {e}"))
}
