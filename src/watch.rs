//! Noticing that an approved command has started (DESIGN.md §6).
//!
//! No agent sends a hook between the user approving a tool call and the
//! command running: the next event is `PostToolUse`, once it has finished.
//! What does show is the command's process. So while a session waits, new
//! processes are watched, and one that keeps running under the agent is taken
//! as the wait being answered.

use std::collections::HashMap;

use crate::host::same_dir;
use crate::state::Key;

/// A new process counts once it has outlived hook runs: bellpanel's own takes
/// under 0.3 s through either agent's shell (spike/FINDINGS.md). Another
/// tool's hook that runs longer than this could be taken for a command.
const MIN_AGE_MS: u64 = 2_000;
/// Processes started this soon after a wait began are the hooks reporting
/// it, not an answer: nobody approves that fast.
const GRACE_MS: u64 = 1_500;
/// How deep to look for the agent among a process's ancestors.
const MAX_DEPTH: usize = 16;

/// pid -> (parent pid, executable stem), as `host::processes` returns it.
pub type Processes = HashMap<u32, (u32, String)>;

/// A session waiting on the user.
pub struct Waiter<'a> {
    pub key: Key,
    pub agent_exe: &'a str,
    pub agent_pid: Option<u32>,
    pub cwd: &'a str,
    /// When the wait began.
    pub since: u64,
    /// No other session of this agent is waiting or working, so a command
    /// under the agent whose folder cannot be read can only be this one's.
    pub alone: bool,
}

/// Remembers when each process was first seen while any session waits.
#[derive(Default)]
pub struct CommandWatch {
    /// pid -> first seen; 0 for processes already running when watching began.
    first_seen: HashMap<u32, u64>,
    watching: bool,
}

impl CommandWatch {
    /// Forget everything, once no session waits any more.
    pub fn stop(&mut self) {
        self.first_seen.clear();
        self.watching = false;
    }

    /// The waiters whose command has started, given the processes running at
    /// `now`. `cwd_of` reads a process's current directory, if it can.
    pub fn poll(
        &mut self,
        now: u64,
        procs: &Processes,
        waiters: &[Waiter],
        cwd_of: impl Fn(u32) -> Option<String>,
    ) -> Vec<Key> {
        self.first_seen.retain(|pid, _| procs.contains_key(pid));
        let seen_at = if self.watching { now } else { 0 };
        for &pid in procs.keys() {
            self.first_seen.entry(pid).or_insert(seen_at);
        }
        self.watching = true;

        let mut started = Vec::new();
        for waiter in waiters {
            let found = self.first_seen.iter().any(|(&pid, &seen)| {
                seen != 0
                    && seen >= waiter.since + GRACE_MS
                    && now.saturating_sub(seen) >= MIN_AGE_MS
                    && runs_command_for(pid, procs, waiter, &cwd_of)
            });
            if found {
                started.push(waiter.key.clone());
            }
        }
        started
    }
}

/// Whether `pid` is a command the waiter's agent runs for it.
fn runs_command_for(
    pid: u32,
    procs: &Processes,
    waiter: &Waiter,
    cwd_of: &impl Fn(u32) -> Option<String>,
) -> bool {
    let Some((_, exe)) = procs.get(&pid) else {
        return false;
    };
    // A new agent process is another session starting, not a command.
    if exe.eq_ignore_ascii_case(waiter.agent_exe) {
        return false;
    }
    let ancestors = ancestors(pid, procs);
    // Claude Code and Copilot CLI run commands as children of the session's
    // own process.
    if waiter
        .agent_pid
        .is_some_and(|agent| ancestors.contains(&agent))
    {
        return true;
    }
    // Codex runs them under its background app-server, shared by every Codex
    // session, so the folder tells which session it is.
    let under_agent = ancestors.iter().any(|a| {
        procs
            .get(a)
            .is_some_and(|(_, exe)| exe.eq_ignore_ascii_case(waiter.agent_exe))
    });
    if !under_agent {
        return false;
    }
    match cwd_of(pid) {
        Some(dir) => same_dir(&dir, waiter.cwd),
        // A sandboxed command may not let its folder be read.
        None => waiter.alone,
    }
}

/// `pid`'s parent, grandparent and so on, as far as they still run.
fn ancestors(pid: u32, procs: &Processes) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = pid;
    while let Some(&(parent, _)) = procs.get(&current) {
        if parent == 0 || chain.contains(&parent) || chain.len() >= MAX_DEPTH {
            break;
        }
        chain.push(parent);
        current = parent;
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Agent;

    fn procs(list: &[(u32, u32, &str)]) -> Processes {
        list.iter()
            .map(|&(pid, parent, exe)| (pid, (parent, exe.to_string())))
            .collect()
    }

    fn key(id: &str) -> Key {
        (Agent::Claude, id.to_string())
    }

    fn waiter<'a>(agent_exe: &'a str, agent_pid: Option<u32>, cwd: &'a str) -> Waiter<'a> {
        Waiter {
            key: key("a"),
            agent_exe,
            agent_pid,
            cwd,
            since: 1_000,
            alone: true,
        }
    }

    const BASE: [(u32, u32, &str); 3] = [
        (10, 1, "WindowsTerminal"),
        (11, 10, "pwsh"),
        (12, 11, "claude"),
    ];

    /// Runs `poll` at each time, with `later` processes present from `appear` on.
    fn run(
        w: &Waiter,
        later: &[(u32, u32, &str)],
        appear: u64,
        until: u64,
        cwd: Option<&str>,
    ) -> bool {
        let mut watch = CommandWatch::default();
        let cwd = cwd.map(str::to_string);
        let mut hit = false;
        let mut now = 1_000;
        while now <= until {
            let mut list = BASE.to_vec();
            if now >= appear {
                list.extend_from_slice(later);
            }
            hit |= !watch
                .poll(now, &procs(&list), std::slice::from_ref(w), |_| cwd.clone())
                .is_empty();
            now += 1_000;
        }
        hit
    }

    #[test]
    fn a_command_under_the_agent_that_keeps_running_ends_the_wait() {
        let w = waiter("claude", Some(12), r"C:\p");
        assert!(run(&w, &[(20, 12, "pwsh")], 5_000, 8_000, None));
    }

    #[test]
    fn a_short_lived_hook_does_not() {
        let w = waiter("claude", Some(12), r"C:\p");
        // Present for one poll only, then gone.
        let mut watch = CommandWatch::default();
        let mut list = BASE.to_vec();
        watch.poll(1_000, &procs(&list), std::slice::from_ref(&w), |_| None);
        list.push((20, 12, "pwsh"));
        assert!(watch
            .poll(5_000, &procs(&list), std::slice::from_ref(&w), |_| None)
            .is_empty());
        list.pop();
        assert!(watch
            .poll(8_000, &procs(&list), std::slice::from_ref(&w), |_| None)
            .is_empty());
    }

    #[test]
    fn processes_running_before_the_watch_began_never_count() {
        let w = waiter("claude", Some(12), r"C:\p");
        assert!(!run(&w, &[(20, 12, "pwsh")], 0, 8_000, None));
    }

    #[test]
    fn hooks_reporting_the_wait_itself_do_not_count() {
        let w = waiter("claude", Some(12), r"C:\p");
        // Appears right as the wait began and runs long, like a slow hook.
        assert!(!run(&w, &[(20, 12, "pwsh")], 2_000, 8_000, None));
    }

    #[test]
    fn another_sessions_command_does_not_count() {
        let w = waiter("claude", Some(12), r"C:\p");
        let other = [(30, 10, "pwsh"), (31, 30, "claude"), (32, 31, "pwsh")];
        assert!(!run(&w, &other, 5_000, 8_000, Some(r"C:\elsewhere")));
    }

    #[test]
    fn codex_commands_under_the_shared_app_server_are_matched_by_folder() {
        // The TUI is 12; commands run under a parentless app-server, 40.
        let w = waiter("codex", Some(12), r"C:\p");
        let server = [
            (40, 0, "codex"),
            (41, 40, "codex-command-runner-0.169.0"),
            (42, 41, "powershell"),
        ];
        assert!(run(&w, &server, 5_000, 8_000, Some(r"c:\P\")));
        assert!(!run(&w, &server, 5_000, 8_000, Some(r"C:\other")));
    }

    #[test]
    fn an_unreadable_folder_counts_only_when_the_session_is_alone() {
        let server = [(40, 0, "codex"), (42, 40, "powershell")];
        let alone = waiter("codex", Some(12), r"C:\p");
        assert!(run(&alone, &server, 5_000, 8_000, None));
        let shared = Waiter {
            alone: false,
            ..waiter("codex", Some(12), r"C:\p")
        };
        assert!(!run(&shared, &server, 5_000, 8_000, None));
    }

    #[test]
    fn a_new_agent_process_is_not_a_command() {
        let w = waiter("codex", None, r"C:\p");
        assert!(!run(&w, &[(40, 12, "codex")], 5_000, 8_000, Some(r"C:\p")));
    }
}
