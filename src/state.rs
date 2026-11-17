//! Session state built from the event stream (DESIGN.md §6).

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::event::{Agent, Event, Kind};

const DUPLICATE_WINDOW_MS: u64 = 2_000;
/// How long an ended session stays, so it can be seen to have ended.
const ENDED_VISIBLE_MS: u64 = 60 * 1000;
const STALE_MS: u64 = 12 * 60 * 60 * 1000;
/// How long a session with nothing for the user (idle, or finished and seen)
/// stays listed after its last event. It comes back with its next one.
const QUIET_VISIBLE_MS: u64 = 30 * 60 * 1000;
const LOG_LINES: usize = 200;
const HISTORY_LEN: usize = 256;
/// Events kept per session, for the event log when it follows a selection.
const SESSION_LOG_LINES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    // Declared in display order: the list is sorted by this.
    Waiting,
    Done,
    Working,
    Idle,
    Ended,
}

pub type Key = (Agent, String);

#[derive(Debug, Clone)]
pub struct Session {
    pub agent: Agent,
    pub id: String,
    pub cwd: String,
    pub state: State,
    /// When the session entered its current state.
    pub since: u64,
    pub last_at: u64,
    /// The last prompt, to recognise the session by what it was asked.
    pub prompt: Option<String>,
    /// The last reply or the question it is waiting on.
    pub detail: Option<String>,
    pub host_pid: Option<u32>,
    pub host_exe: Option<String>,
    pub agent_pid: Option<u32>,
    /// WezTerm pane id.
    pub pane: Option<String>,
    /// A finished session stays highlighted until it is seen.
    pub seen: bool,
    /// When the session ended, if it has.
    pub ended_at: Option<u64>,
    /// When the session last finished a turn.
    finished_at: Option<u64>,
    /// State changes, oldest first, for the timeline.
    history: Vec<(u64, State)>,
    /// This session's own entries of the event log, oldest first.
    log: VecDeque<Event>,
    last_kind: Kind,
    last_turn: Option<String>,
}

/// One column of a session's timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Before the session started, or after it ended.
    None,
    Idle,
    Working,
    /// It finished during this slot.
    Finished,
    Waiting,
}

impl Session {
    /// When the dashboard first heard of the session.
    pub fn first_at(&self) -> u64 {
        self.history.first().map_or(self.last_at, |h| h.0)
    }

    /// What the session did in each of `slots` equal slices of time ending
    /// at `now`, oldest first. Within a slice, waiting outranks finishing,
    /// which outranks working: the rarer, more important moment shows.
    pub fn timeline(&self, now: u64, slots: usize, slot_ms: u64) -> Vec<Slot> {
        let first = self.history.first().map_or(now, |h| h.0);
        (0..slots)
            .map(|i| {
                let end = now.saturating_sub((slots - 1 - i) as u64 * slot_ms);
                let start = end.saturating_sub(slot_ms);
                if end <= first || self.ended_at.is_some_and(|at| start >= at) {
                    return Slot::None;
                }
                let before = self
                    .history
                    .iter()
                    .rev()
                    .find(|h| h.0 <= start)
                    .map(|h| h.1);
                let during = self
                    .history
                    .iter()
                    .filter(|h| h.0 > start && h.0 <= end)
                    .map(|h| h.1);
                let mut slot = Slot::None;
                for state in before.into_iter().chain(during.clone()) {
                    let here = match state {
                        State::Waiting => Slot::Waiting,
                        State::Working => Slot::Working,
                        State::Idle | State::Done => Slot::Idle,
                        State::Ended => Slot::None,
                    };
                    slot = slot.max_by_rank(here);
                }
                if during.clone().any(|s| s == State::Done) && slot != Slot::Waiting {
                    slot = Slot::Finished;
                }
                slot
            })
            .collect()
    }

    pub fn key(&self) -> Key {
        (self.agent, self.id.clone())
    }

    /// Last path segment of the working directory.
    pub fn folder(&self) -> &str {
        folder_of(&self.cwd)
    }

    pub fn wants_attention(&self) -> bool {
        self.state == State::Waiting || (self.state == State::Done && !self.seen)
    }

    fn sort_rank(&self) -> (u8, State) {
        // Seen "done" rows sink to the idle group.
        let group = match self.state {
            State::Waiting => 0,
            State::Done if !self.seen => 1,
            State::Working => 2,
            State::Done | State::Idle => 3,
            State::Ended => 4,
        };
        (group, self.state)
    }
}

impl Slot {
    fn rank(self) -> u8 {
        match self {
            Slot::None => 0,
            Slot::Idle => 1,
            Slot::Working => 2,
            Slot::Finished => 3,
            Slot::Waiting => 4,
        }
    }

    fn max_by_rank(self, other: Slot) -> Slot {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

pub fn folder_of(path: &str) -> &str {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
}

/// How the session list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// In the order sessions appeared: rows never move.
    #[default]
    Fixed,
    /// Waiting, then unseen finishes, then working, then the rest.
    Urgency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkKind {
    Seen,
    Hidden,
}

/// Something the user did to a row, kept in `marks.jsonl` so a restart does
/// not bring back what was already dealt with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub mark: MarkKind,
    pub agent: Agent,
    pub session: String,
    /// The session's last event when the mark was made.
    pub at: u64,
}

#[derive(Default)]
pub struct Board {
    sessions: HashMap<Key, Session>,
    dismissed: HashMap<Key, u64>,
    last_by_agent: HashMap<Agent, u64>,
    log: VecDeque<Event>,
}

impl Board {
    /// Applies one event. Returns true when a session has just started to want
    /// the user: it began waiting, or it finished.
    pub fn apply(&mut self, event: &Event) -> bool {
        let key = (event.agent, event.session.clone());
        // An end for a session never seen before has nothing to show: Copilot
        // ends short-lived helper sessions of its own this way.
        if event.kind == Kind::End && !self.sessions.contains_key(&key) {
            return false;
        }
        // The same hook defined at two config levels fires twice (Codex).
        if let Some(prev) = self.sessions.get(&key) {
            let duplicate = prev.last_kind == event.kind
                && prev.last_turn == event.turn
                && event.at.abs_diff(prev.last_at) <= DUPLICATE_WINDOW_MS;
            if duplicate {
                return false;
            }
        }
        let entry = self.sessions.entry(key.clone()).or_insert_with(|| Session {
            agent: event.agent,
            id: event.session.clone(),
            cwd: event.cwd.clone(),
            state: State::Idle,
            since: event.at,
            last_at: event.at,
            prompt: None,
            detail: None,
            host_pid: None,
            host_exe: None,
            agent_pid: None,
            pane: None,
            seen: true,
            ended_at: None,
            finished_at: None,
            history: vec![(event.at, State::Idle)],
            log: VecDeque::new(),
            last_kind: Kind::Start,
            last_turn: None,
        });

        self.last_by_agent
            .entry(event.agent)
            .and_modify(|t| *t = (*t).max(event.at))
            .or_insert(event.at);
        // Every tool call ends with a "resumed" event; listing them all would
        // push the prompts and finishes out of the log. Keep only the ones
        // that end a wait, which mean the request was answered.
        let was_waiting = entry.state == State::Waiting;
        // A start says nothing the first prompt does not (Copilot even repeats
        // the prompt in it), so it only feeds the state, not the log.
        if event.kind != Kind::Start && (event.kind != Kind::Resumed || was_waiting) {
            entry.log.push_back(event.clone());
            if entry.log.len() > SESSION_LOG_LINES {
                entry.log.pop_front();
            }
            self.log.push_back(event.clone());
            if self.log.len() > LOG_LINES {
                self.log.pop_front();
            }
        }
        // New activity brings a dismissed session back.
        if event.kind == Kind::Prompt {
            self.dismissed.remove(&key);
        }

        if !event.cwd.is_empty() {
            entry.cwd = event.cwd.clone();
        }
        if event.host_pid.is_some() {
            entry.host_pid = event.host_pid;
            entry.host_exe = event.host_exe.clone();
            entry.agent_pid = event.agent_pid;
            entry.pane = event.pane.clone();
        }
        entry.last_at = entry.last_at.max(event.at);
        entry.last_kind = event.kind;
        entry.last_turn = event.turn.clone();

        match event.kind {
            Kind::End => {
                entry.ended_at = Some(event.at);
                // A session with no window is a script running the agent
                // headless (`claude -p` from a scheduled job): nobody is
                // waiting to see it finish, so it goes like any ended one.
                if entry.host_pid.is_none() {
                    entry.seen = true;
                }
                // A finish nobody has looked at yet outlives the session: an
                // interactive one-shot run ends right after it finishes.
                if entry.state == State::Done && !entry.seen {
                    return false;
                }
            }
            Kind::Start | Kind::Prompt => entry.ended_at = None,
            _ => {}
        }
        let next = match event.kind {
            Kind::Start => State::Idle,
            Kind::Prompt | Kind::Resumed => State::Working,
            Kind::Waiting => State::Waiting,
            Kind::Stop => State::Done,
            Kind::End => State::Ended,
        };
        match event.kind {
            // The row shows what the person last asked, not a notice the
            // agent's tooling submitted.
            Kind::Prompt if event.automatic => entry.detail = None,
            Kind::Prompt => {
                entry.prompt = event.text.clone().or(entry.prompt.take());
                entry.detail = None;
            }
            Kind::Start if event.text.is_some() => entry.prompt = event.text.clone(),
            Kind::Waiting => entry.detail = event.text.clone(),
            Kind::Stop => {
                entry.detail = event.text.clone();
                entry.finished_at = Some(event.at);
            }
            _ => {}
        }
        // A tool finishing does not mean the session is no longer waiting on
        // something else, but it does mean the request it waited on was answered.
        if event.kind == Kind::Resumed
            && entry.state != State::Waiting
            && entry.state != State::Working
        {
            return false;
        }

        let alert = next != entry.state && matches!(next, State::Waiting | State::Done);
        if next != entry.state {
            entry.state = next;
            entry.since = event.at;
            entry.history.push((event.at, next));
            if entry.history.len() > HISTORY_LEN {
                entry.history.remove(0);
            }
        }
        if next == State::Done {
            entry.seen = false;
        } else if next != State::Ended {
            entry.seen = true;
        }
        alert
    }

    /// Sessions waiting on the user that have not ended.
    pub fn waiting(&self) -> impl Iterator<Item = &Session> {
        self.sessions
            .values()
            .filter(|s| s.state == State::Waiting && s.ended_at.is_none())
    }

    /// How many sessions of `agent` are waiting or working.
    pub fn busy(&self, agent: Agent) -> usize {
        self.sessions
            .values()
            .filter(|s| {
                s.agent == agent
                    && s.ended_at.is_none()
                    && matches!(s.state, State::Waiting | State::Working)
            })
            .count()
    }

    /// A waiting session's approved command has started (watch.rs). It is
    /// working again, as `PostToolUse` would make it once the command ends,
    /// and the log shows the wait as answered. Nothing is written to the
    /// event file: the hooks remain its only source.
    pub fn command_started(&mut self, key: &Key, at: u64) {
        let Some(s) = self.sessions.get_mut(key) else {
            return;
        };
        if s.state != State::Waiting {
            return;
        }
        s.state = State::Working;
        s.since = at;
        s.history.push((at, State::Working));
        if s.history.len() > HISTORY_LEN {
            s.history.remove(0);
        }
        let event = Event {
            v: crate::event::FORMAT_VERSION,
            at,
            agent: s.agent,
            kind: Kind::Resumed,
            event: "command started".to_string(),
            session: s.id.clone(),
            turn: None,
            cwd: s.cwd.clone(),
            host_pid: None,
            host_exe: None,
            agent_pid: None,
            pane: None,
            text: None,
            automatic: false,
        };
        s.log.push_back(event.clone());
        if s.log.len() > SESSION_LOG_LINES {
            s.log.pop_front();
        }
        self.log.push_back(event);
        if self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    /// Rows to show, most urgent first.
    pub fn rows(&self, now: u64) -> Vec<&Session> {
        self.rows_in(now, Order::Urgency)
    }

    /// Rows to show, in the given order.
    pub fn rows_in(&self, now: u64, order: Order) -> Vec<&Session> {
        let mut rows: Vec<&Session> = self
            .sessions
            .values()
            .filter(|s| now.saturating_sub(s.last_at) < STALE_MS)
            .filter(|s| {
                let quiet = s.state == State::Idle || (s.state == State::Done && s.seen);
                !quiet || now.saturating_sub(s.last_at) < QUIET_VISIBLE_MS
            })
            .filter(|s| {
                s.ended_at.is_none_or(|at| {
                    s.wants_attention() || now.saturating_sub(at) < ENDED_VISIBLE_MS
                })
            })
            .filter(|s| {
                self.dismissed
                    .get(&s.key())
                    .is_none_or(|&at| s.last_at > at)
            })
            .collect();
        match order {
            Order::Urgency => rows.sort_by(|a, b| {
                a.sort_rank()
                    .cmp(&b.sort_rank())
                    .then(b.last_at.cmp(&a.last_at))
            }),
            // Oldest first, so a new session is added at the bottom and no
            // row moves under the pointer.
            Order::Fixed => rows.sort_by(|a, b| {
                a.first_at()
                    .cmp(&b.first_at())
                    .then(a.agent.name().cmp(b.agent.name()))
                    .then(a.id.cmp(&b.id))
            }),
        }
        rows
    }

    /// Marks every finished session as seen. Returns the marks to store.
    pub fn acknowledge_all(&mut self) -> Vec<Mark> {
        let keys: Vec<Key> = self
            .sessions
            .values()
            .filter(|s| s.state == State::Done && !s.seen)
            .map(Session::key)
            .collect();
        keys.iter().filter_map(|k| self.acknowledge(k)).collect()
    }

    /// Marks a finished session as seen. Returns the mark to store, so it
    /// survives a restart, when anything changed.
    pub fn acknowledge(&mut self, key: &Key) -> Option<Mark> {
        let s = self.sessions.get_mut(key)?;
        if s.seen {
            return None;
        }
        s.seen = true;
        Some(Mark {
            mark: MarkKind::Seen,
            agent: s.agent,
            session: s.id.clone(),
            at: s.last_at,
        })
    }

    /// Hides a row until the session does something new. Returns the mark to store.
    pub fn dismiss(&mut self, key: &Key) -> Option<Mark> {
        let s = self.sessions.get(key)?;
        self.dismissed.insert(key.clone(), s.last_at);
        Some(Mark {
            mark: MarkKind::Hidden,
            agent: s.agent,
            session: s.id.clone(),
            at: s.last_at,
        })
    }

    /// Re-applies a stored mark after replaying events. A mark only covers
    /// what had happened when it was made: a later finish shows again.
    pub fn apply_mark(&mut self, mark: &Mark) {
        let key = (mark.agent, mark.session.clone());
        match mark.mark {
            MarkKind::Seen => {
                if let Some(s) = self.sessions.get_mut(&key) {
                    // Only a finish after the mark makes it unseen again;
                    // an end or a tool event since then does not.
                    if s.finished_at.is_none_or(|at| at <= mark.at) {
                        s.seen = true;
                    }
                }
            }
            MarkKind::Hidden => {
                self.dismissed.insert(key, mark.at);
            }
        }
    }

    pub fn last_seen(&self, agent: Agent) -> Option<u64> {
        self.last_by_agent.get(&agent).copied()
    }

    pub fn log(&self) -> impl DoubleEndedIterator<Item = &Event> {
        self.log.iter()
    }

    /// One session's entries of the event log, oldest first.
    pub fn session_log(&self, key: &Key) -> impl DoubleEndedIterator<Item = &Event> {
        self.sessions
            .get(key)
            .into_iter()
            .flat_map(|s| s.log.iter())
    }

    pub fn attention_count(&self, now: u64) -> usize {
        self.rows(now)
            .iter()
            .filter(|s| s.wants_attention())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::FORMAT_VERSION;

    fn ev(agent: Agent, session: &str, kind: Kind, at: u64) -> Event {
        Event {
            v: FORMAT_VERSION,
            at,
            agent,
            kind,
            event: format!("{kind:?}"),
            session: session.into(),
            turn: None,
            cwd: format!("C:\\work\\{session}-proj"),
            host_pid: None,
            host_exe: None,
            agent_pid: None,
            pane: None,
            text: None,
            automatic: false,
        }
    }

    fn states(board: &Board, now: u64) -> Vec<(String, State)> {
        board
            .rows(now)
            .iter()
            .map(|s| (s.id.clone(), s.state))
            .collect()
    }

    #[test]
    fn a_turn_goes_working_then_done_and_alerts_once() {
        let mut b = Board::default();
        assert!(!b.apply(&ev(Agent::Claude, "a", Kind::Start, 0)));
        assert!(!b.apply(&ev(Agent::Claude, "a", Kind::Prompt, 10_000)));
        assert_eq!(states(&b, 10_000), [("a".into(), State::Working)]);
        assert!(b.apply(&ev(Agent::Claude, "a", Kind::Stop, 20_000)));
        assert_eq!(b.attention_count(20_000), 1);
        b.acknowledge(&(Agent::Claude, "a".into()));
        assert_eq!(b.attention_count(20_000), 0);
    }

    #[test]
    fn quiet_rows_leave_the_list_and_come_back_with_new_activity() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "idle", Kind::Start, 0));
        b.apply(&ev(Agent::Claude, "seen", Kind::Stop, 0));
        b.acknowledge(&(Agent::Claude, "seen".into()));
        b.apply(&ev(Agent::Claude, "unseen", Kind::Stop, 0));
        assert_eq!(states(&b, QUIET_VISIBLE_MS - 1).len(), 3);
        // Idle and seen rows go; a finish nobody has looked at stays.
        let left: Vec<String> = states(&b, QUIET_VISIBLE_MS)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(left, ["unseen"]);
        b.apply(&ev(
            Agent::Claude,
            "idle",
            Kind::Prompt,
            QUIET_VISIBLE_MS + 1,
        ));
        assert_eq!(states(&b, QUIET_VISIBLE_MS + 1).len(), 2);
    }

    #[test]
    fn a_notice_from_the_tooling_does_not_replace_the_persons_prompt() {
        let mut b = Board::default();
        let mut asked = ev(Agent::Claude, "a", Kind::Prompt, 1_000);
        asked.text = Some("fix the build".into());
        b.apply(&asked);
        let mut notice = ev(Agent::Claude, "a", Kind::Prompt, 5_000);
        notice.text = Some("Background command finished".into());
        notice.automatic = true;
        b.apply(&notice);
        let rows = b.rows(5_000);
        assert_eq!(rows[0].prompt.as_deref(), Some("fix the build"));
        assert_eq!(rows[0].state, State::Working);
    }

    #[test]
    fn an_approved_command_starting_ends_the_wait_once() {
        let mut b = Board::default();
        let key = (Agent::Copilot, "a".to_string());
        b.apply(&ev(Agent::Copilot, "a", Kind::Prompt, 1_000));
        b.apply(&ev(Agent::Copilot, "a", Kind::Waiting, 2_000));
        assert_eq!(b.waiting().count(), 1);
        assert_eq!(b.busy(Agent::Copilot), 1);
        b.command_started(&key, 9_000);
        assert_eq!(states(&b, 9_000), [("a".into(), State::Working)]);
        assert_eq!(b.waiting().count(), 0);
        // The tool finishing later neither alerts nor logs a second answer.
        assert!(!b.apply(&ev(Agent::Copilot, "a", Kind::Resumed, 24_000)));
        let kinds: Vec<Kind> = b.session_log(&key).map(|e| e.kind).collect();
        assert_eq!(kinds, [Kind::Prompt, Kind::Waiting, Kind::Resumed]);
        // Only a waiting session is changed.
        b.command_started(&key, 25_000);
        assert_eq!(b.session_log(&key).count(), 3);
    }

    #[test]
    fn waiting_then_tool_finishing_is_working_again() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Codex, "a", Kind::Prompt, 0));
        assert!(b.apply(&ev(Agent::Codex, "a", Kind::Waiting, 5_000)));
        assert_eq!(states(&b, 5_000)[0].1, State::Waiting);
        assert!(!b.apply(&ev(Agent::Codex, "a", Kind::Resumed, 9_000)));
        assert_eq!(states(&b, 9_000)[0].1, State::Working);
    }

    #[test]
    fn every_unseen_finish_is_marked_seen_at_once() {
        let mut b = Board::default();
        for id in ["a", "b"] {
            b.apply(&ev(Agent::Claude, id, Kind::Prompt, 0));
            b.apply(&ev(Agent::Claude, id, Kind::Stop, 1_000));
        }
        assert_eq!(b.acknowledge_all().len(), 2);
        assert_eq!(b.attention_count(2_000), 0);
    }

    #[test]
    fn a_late_tool_event_does_not_reopen_a_finished_session() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "a", Kind::Prompt, 0));
        b.apply(&ev(Agent::Claude, "a", Kind::Stop, 5_000));
        b.apply(&ev(Agent::Claude, "a", Kind::Resumed, 9_000));
        assert_eq!(states(&b, 9_000)[0].1, State::Done);
    }

    #[test]
    fn duplicates_from_two_config_levels_count_once() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Codex, "a", Kind::Prompt, 0));
        assert!(b.apply(&ev(Agent::Codex, "a", Kind::Stop, 5_000)));
        assert!(!b.apply(&ev(Agent::Codex, "a", Kind::Stop, 5_001)));
        assert_eq!(b.log().count(), 2);
    }

    #[test]
    fn waiting_sorts_above_done_above_working() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "work", Kind::Prompt, 30_000));
        b.apply(&ev(Agent::Copilot, "done", Kind::Stop, 20_000));
        b.apply(&ev(Agent::Codex, "wait", Kind::Waiting, 10_000));
        let order: Vec<String> = states(&b, 40_000).into_iter().map(|(id, _)| id).collect();
        assert_eq!(order, ["wait", "done", "work"]);
    }

    #[test]
    fn fixed_order_keeps_rows_where_they_appeared() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "first", Kind::Prompt, 0));
        b.apply(&ev(Agent::Codex, "second", Kind::Prompt, 1_000));
        b.apply(&ev(Agent::Copilot, "third", Kind::Prompt, 2_000));
        b.apply(&ev(Agent::Copilot, "third", Kind::Waiting, 3_000));
        b.apply(&ev(Agent::Codex, "second", Kind::Stop, 4_000));
        let ids = |order| {
            b.rows_in(5_000, order)
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(Order::Fixed), ["first", "second", "third"]);
        assert_eq!(ids(Order::Urgency), ["third", "second", "first"]);
    }

    #[test]
    fn ended_and_stale_sessions_drop_out() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "ended", Kind::Start, 0));
        b.apply(&ev(Agent::Claude, "ended", Kind::End, 0));
        b.apply(&ev(Agent::Claude, "old", Kind::Prompt, 0));
        assert_eq!(states(&b, ENDED_VISIBLE_MS - 1).len(), 2);
        assert_eq!(states(&b, ENDED_VISIBLE_MS).len(), 1);
        assert_eq!(states(&b, STALE_MS + 1).len(), 0);
    }

    #[test]
    fn a_headless_run_goes_when_it_ends() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "script", Kind::Prompt, 0));
        b.apply(&ev(Agent::Claude, "script", Kind::Stop, 5_000));
        b.apply(&ev(Agent::Claude, "script", Kind::End, 6_000));
        assert!(b.rows(6_000 + ENDED_VISIBLE_MS).is_empty());
    }

    #[test]
    fn an_unseen_finish_outlives_the_session() {
        let mut b = Board::default();
        let windowed = |kind, at| Event {
            host_pid: Some(42),
            host_exe: Some("Code".into()),
            ..ev(Agent::Claude, "a", kind, at)
        };
        b.apply(&windowed(Kind::Prompt, 0));
        assert!(b.apply(&windowed(Kind::Stop, 5_000)));
        b.apply(&windowed(Kind::End, 6_000));
        let later = 6_000 + ENDED_VISIBLE_MS * 10;
        assert_eq!(states(&b, later), [("a".into(), State::Done)]);
        b.acknowledge(&(Agent::Claude, "a".into()));
        assert!(b.rows(later).is_empty(), "once seen, an ended session goes");
    }

    #[test]
    fn the_timeline_shows_each_minute_at_its_most_important() {
        const MIN: u64 = 60_000;
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "a", Kind::Prompt, 2 * MIN)); // working from minute 2
        b.apply(&ev(Agent::Claude, "a", Kind::Waiting, 4 * MIN + 10)); // waiting in minute 4
        b.apply(&ev(Agent::Claude, "a", Kind::Resumed, 5 * MIN + 10));
        b.apply(&ev(Agent::Claude, "a", Kind::Stop, 6 * MIN + 10)); // finished in minute 6
        let session = b.rows(8 * MIN)[0];
        let slots = session.timeline(8 * MIN, 8, MIN);
        use Slot::*;
        assert_eq!(
            slots,
            [None, None, Working, Working, Waiting, Waiting, Finished, Idle]
        );
    }

    #[test]
    fn the_timeline_is_blank_after_the_end() {
        const MIN: u64 = 60_000;
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "a", Kind::Prompt, 0));
        b.apply(&ev(Agent::Claude, "a", Kind::End, MIN + 10));
        let session = b.sessions.values().next().unwrap();
        assert_eq!(
            session.timeline(4 * MIN, 4, MIN),
            [Slot::Working, Slot::Working, Slot::None, Slot::None]
        );
    }

    #[test]
    fn the_log_skips_routine_tool_events_but_keeps_answered_waits() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "a", Kind::Prompt, 0));
        b.apply(&ev(Agent::Claude, "a", Kind::Resumed, 5_000));
        b.apply(&ev(Agent::Claude, "a", Kind::Waiting, 10_000));
        b.apply(&ev(Agent::Claude, "a", Kind::Resumed, 15_000));
        let kinds: Vec<Kind> = b.log().map(|e| e.kind).collect();
        assert_eq!(kinds, [Kind::Prompt, Kind::Waiting, Kind::Resumed]);
    }

    #[test]
    fn marks_survive_a_replay_but_not_a_later_finish() {
        let events = [
            ev(Agent::Claude, "a", Kind::Prompt, 0),
            ev(Agent::Claude, "a", Kind::Stop, 5_000),
        ];
        let mut first = Board::default();
        events.iter().for_each(|e| {
            first.apply(e);
        });
        let seen = first.acknowledge(&(Agent::Claude, "a".into())).unwrap();
        assert_eq!(
            first.acknowledge(&(Agent::Claude, "a".into())),
            None,
            "nothing new to store"
        );

        let mut replayed = Board::default();
        events.iter().for_each(|e| {
            replayed.apply(e);
        });
        replayed.apply_mark(&seen);
        assert_eq!(replayed.attention_count(6_000), 0);

        replayed.apply(&ev(Agent::Claude, "a", Kind::Prompt, 7_000));
        replayed.apply(&ev(Agent::Claude, "a", Kind::Stop, 9_000));
        assert_eq!(replayed.attention_count(9_000), 1);
    }

    #[test]
    fn an_end_alone_does_not_make_a_row() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Copilot, "helper", Kind::End, 0));
        assert!(b.rows(1_000).is_empty());
        assert_eq!(b.log().count(), 0);
    }

    #[test]
    fn each_session_keeps_its_own_log() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "a", Kind::Prompt, 0));
        b.apply(&ev(Agent::Codex, "b", Kind::Prompt, 1_000));
        b.apply(&ev(Agent::Claude, "a", Kind::Stop, 5_000));
        let a: Vec<Kind> = b
            .session_log(&(Agent::Claude, "a".into()))
            .map(|e| e.kind)
            .collect();
        assert_eq!(a, [Kind::Prompt, Kind::Stop]);
        assert_eq!(b.session_log(&(Agent::Codex, "missing".into())).count(), 0);
    }

    #[test]
    fn an_end_after_a_seen_mark_keeps_it_seen() {
        let windowed = |kind, at| Event {
            host_pid: Some(42),
            host_exe: Some("WindowsTerminal".into()),
            ..ev(Agent::Codex, "a", kind, at)
        };
        let mut first = Board::default();
        first.apply(&windowed(Kind::Prompt, 0));
        first.apply(&windowed(Kind::Stop, 5_000));
        let seen = first.acknowledge(&(Agent::Codex, "a".into())).unwrap();

        // Later the session ends; on the next start-up it is replayed with the mark.
        let mut replayed = Board::default();
        for e in [
            windowed(Kind::Prompt, 0),
            windowed(Kind::Stop, 5_000),
            windowed(Kind::End, 60_000),
        ] {
            replayed.apply(&e);
        }
        replayed.apply_mark(&seen);
        assert_eq!(replayed.attention_count(61_000), 0);
    }

    #[test]
    fn dismissed_rows_return_on_new_activity() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "a", Kind::Stop, 0));
        b.dismiss(&(Agent::Claude, "a".into()));
        assert!(b.rows(1_000).is_empty());
        b.apply(&ev(Agent::Claude, "a", Kind::Prompt, 5_000));
        assert_eq!(b.rows(5_000).len(), 1);
    }

    #[test]
    fn last_seen_is_per_agent() {
        let mut b = Board::default();
        b.apply(&ev(Agent::Claude, "a", Kind::Prompt, 7));
        assert_eq!(b.last_seen(Agent::Claude), Some(7));
        assert_eq!(b.last_seen(Agent::Codex), None);
    }

    #[test]
    fn folder_is_the_last_segment() {
        assert_eq!(folder_of("C:\\Users\\me\\proj\\"), "proj");
        assert_eq!(folder_of("/home/me/proj"), "proj");
    }
}
